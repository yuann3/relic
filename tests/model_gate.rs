//! The Ctrl-C gate of ADR 0010, as a process against a server that never
//! answers: Ctrl-C during a root call, and during a batch of four child
//! calls, dies by SIGINT (130 to a shell) within 250 ms, says `Cancelled`,
//! and commits nothing that arrives late. If it fails, the transport
//! decision reopens (reqwest), not a workaround.
//!
//! The process under test is `relic recall` itself, over the real
//! `ChatCompletionsClient`, with SIGINT as the cancel flag.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::process::ExitStatusExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// A config directory with no `relic/env`: the developer's own model settings
/// must not reach a test that scripts the model.
const NO_SETTINGS: &str = "/nonexistent";

const GATE: Duration = Duration::from_millis(250);

/// Reads one HTTP request off `stream`; true if it was a child call.
fn read_request(stream: &TcpStream) -> bool {
    let mut reader = BufReader::new(stream);
    let mut length = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            length = v.trim().parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).unwrap();
    // Only a child call's user message holds a packet.
    let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let user: serde_json::Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    user.get("packet").is_some()
}

fn completion(content: &serde_json::Value) -> String {
    let body = serde_json::json!({"model": "gate", "choices": [{"message": {"content": content.to_string()}, "finish_reason": "stop"}]}).to_string();
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// Answers the root call with `plan`, or holds it if there is none. Child
/// calls are answered `child_after` after they arrive, valid every time,
/// so a reply the cancel failed to fence would show as an event.
fn server(plan: Option<serde_json::Value>, child_after: Duration) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let children = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&children);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let plan = plan.clone();
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || {
                if read_request(&stream) {
                    seen.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(child_after);
                    // Every item of every packet, judged: a fenced reply
                    // would pass the strict checks if it were let in.
                    let all =
                        serde_json::json!({"version": 1, "packet_id": "pkt_00", "assessments": []});
                    let _ = stream.write_all(completion(&all).as_bytes());
                } else if let Some(plan) = plan {
                    let _ = stream.write_all(completion(&plan).as_bytes());
                } else {
                    std::thread::sleep(Duration::from_secs(30));
                }
            });
        }
    });
    (url, children)
}

/// A store with 16 commands that match `generic`.
fn store(dir: &std::path::Path) -> std::path::PathBuf {
    let hist = dir.join("zsh_history");
    let lines: String = (0..16)
        .map(|i| format!(": {}:0;generic step {i}\n", 1_700_000_000 + i))
        .collect();
    std::fs::write(&hist, lines).unwrap();
    let db = dir.join("relic.db");
    let out = Command::new(env!("CARGO_BIN_EXE_relic"))
        .arg("import")
        .arg(&hist)
        .arg("--db")
        .arg(&db)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    db
}

/// Runs a recall until `said` holds for what it printed and `ready` holds,
/// sends SIGINT, and returns the signal it died by, the time from SIGINT to exit,
/// what it said and the last line of its recall log.
fn interrupt(
    url: &str,
    said_enough: impl Fn(&str) -> bool,
    ready: impl Fn() -> bool,
) -> (Option<i32>, Duration, String, serde_json::Value) {
    let dir = tempfile::tempdir().unwrap();
    let db = store(dir.path());
    let mut child = Command::new(env!("CARGO_BIN_EXE_relic"))
        .args(["recall", "which command renewed the certificate?", "--db"])
        .arg(&db)
        .env("XDG_CONFIG_HOME", NO_SETTINGS)
        .env("RELIC_MODEL_PROFILE", "llama_cpp")
        .env("RELIC_MODEL_URL", url)
        .env("RELIC_MODEL_NAME", "gate")
        .env_remove("RELIC_MODEL_API_KEY")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let mut said = String::new();
    let start = Instant::now();
    loop {
        let line = rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("not ready:\n{said}"));
        said.push_str(&line);
        said.push('\n');
        if said_enough(&said) {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "{said}");
    }
    while !ready() {
        std::thread::sleep(Duration::from_millis(1));
    }
    // The request bytes are on the wire and the call is blocked reading.
    std::thread::sleep(Duration::from_millis(20));
    let sent = Instant::now();
    // SAFETY: signals our own child process.
    unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) };
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            sent.elapsed() < Duration::from_secs(5),
            "still running 5 s after SIGINT:\n{said}"
        );
        std::thread::sleep(Duration::from_millis(1));
    };
    let took = sent.elapsed();
    said.extend(rx.try_iter().map(|l| l + "\n"));
    let log = std::fs::read_to_string(dir.path().join("relic.db.recall.log")).unwrap();
    let last = serde_json::from_str(log.lines().last().unwrap()).unwrap();
    (status.signal(), took, said, last)
}

#[test]
fn ctrl_c_during_a_root_call_dies_by_sigint_within_250_ms() {
    let (url, _) = server(None, Duration::ZERO);
    let (signal, took, said, last) = interrupt(&url, |said| said.contains("root call 1"), || true);
    // Not exit(130): a shell stops a script around Relic only for a child
    // that died by the signal.
    assert_eq!(signal, Some(libc::SIGINT), "{said}");
    assert!(said.contains("recall cancelled"), "{said}");
    assert_eq!(
        (&last["event"], &last["outcome"]),
        (
            &serde_json::json!("finish"),
            &serde_json::json!("cancelled")
        ),
        "{last}"
    );
    assert!(took < GATE, "{took:?} from SIGINT to exit\n{said}");
}

#[test]
fn ctrl_c_during_four_child_calls_dies_by_sigint_within_250_ms_and_commits_none() {
    let plan = serde_json::json!({"version": 1, "expected_state_version": 0, "kind": "plan", "finish": null, "plan": {"ops": [
        {"op": "search", "op_id": "s1", "terms": ["generic"], "match": "any", "limit": 16},
        {"op": "assess", "op_id": "a1", "input": {"output": "s1"}, "objective": "renewal", "max_items": 16},
    ]}});
    // Replies land 150 ms after each call arrives: after SIGINT, and before
    // the gate would close if the process were still waiting.
    let (url, children) = server(Some(plan), Duration::from_millis(150));
    let four = |said: &str| said.matches("child call for").count() == 4;
    let (signal, took, said, last) = interrupt(&url, four, || children.load(Ordering::SeqCst) == 4);
    // Not exit(130): a shell stops a script around Relic only for a child
    // that died by the signal.
    assert_eq!(signal, Some(libc::SIGINT), "{said}");
    assert!(said.contains("recall cancelled"), "{said}");
    assert_eq!(
        (&last["event"], &last["outcome"]),
        (
            &serde_json::json!("finish"),
            &serde_json::json!("cancelled")
        ),
        "{last}"
    );
    assert!(took < GATE, "{took:?} from SIGINT to exit\n{said}");
    assert!(
        !said.contains("child reply from"),
        "a child reply was let in after cancel:\n{said}"
    );
}
