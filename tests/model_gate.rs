//! The Ctrl-C gate of ADR 0010, as a process against a server that never
//! answers: Ctrl-C during a root call, and during a batch of four child
//! calls, exits 130 within 250 ms, says `Cancelled`, and commits nothing
//! that arrives late. If it fails, the transport decision reopens (reqwest),
//! not a workaround.
//!
//! The process under test is this test binary run again with `GATE_CHILD`
//! set: it drives `run_investigation` over the real `ChatCompletionsClient`
//! and turns SIGINT into the cancel flag, as the recall command will.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use relic::rlm::coordinator::{InvestigationStatus, run_investigation};
use relic::rlm::model::chat::{ChatCompletionsClient, ChatConfig, OutputMode, Profile, TokenField};
use relic::rlm::protocol::BudgetV1;
use relic::store::Store;

const GATE: Duration = Duration::from_millis(250);

static CANCEL: OnceLock<Arc<AtomicBool>> = OnceLock::new();

extern "C" fn on_sigint(_: libc::c_int) {
    // An atomic load and an atomic store: async-signal-safe.
    if let Some(cancel) = CANCEL.get() {
        cancel.store(true, Ordering::SeqCst);
    }
}

/// The process under test. Not a test unless `GATE_CHILD` names a case.
#[test]
fn gate_child() {
    let (Ok(case), Ok(url), Ok(db)) = (std::env::var("GATE_CHILD"), std::env::var("GATE_URL"), std::env::var("GATE_DB")) else {
        return;
    };
    let cancel = CANCEL.get_or_init(Arc::default).clone();
    // SAFETY: installs a handler that only stores to an atomic.
    unsafe { libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t) };
    let config = ChatConfig {
        profile: Profile::Vllm,
        base_url: url,
        model: "gate".into(),
        output: OutputMode::JsonSchema,
        token_field: TokenField::MaxTokens,
        api_key: None,
        prices: None,
    };
    let client = Arc::new(ChatCompletionsClient::new(config).unwrap());
    let store = Store::open_read_only(std::path::Path::new(&db)).unwrap();
    let (mut roots, mut children) = (0, 0);
    let mut progress = |p: relic::rlm::coordinator::Progress| {
        use relic::rlm::coordinator::Progress::*;
        eprintln!("event {p:?}");
        match p {
            RootCall { .. } => roots += 1,
            ChildCall { .. } => children += 1,
            _ => {}
        }
        let ready = match case.as_str() {
            "root" => roots == 1 && children == 0,
            _ => children == 4,
        };
        if ready {
            eprintln!("READY");
        }
    };
    let result = run_investigation("which command renewed the certificate?", &store, client, BudgetV1::prototype_default(), &cancel, &mut progress);
    eprintln!("status {:?}", result.status);
    let code = if result.status == InvestigationStatus::Cancelled { 130 } else { 1 };
    // Not a return: the calls still blocked on their sockets must not hold
    // the process open; exiting closes them (research, cancellation 6).
    std::process::exit(code);
}

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
    String::from_utf8_lossy(&body).contains("relic_child_result")
}

fn completion(content: &serde_json::Value) -> String {
    let body = serde_json::json!({"model": "gate", "choices": [{"message": {"content": content.to_string()}, "finish_reason": "stop"}]}).to_string();
    format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len())
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
                    let all = serde_json::json!({"version": 1, "packet_id": "pkt_00", "assessments": []});
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
    let lines: String = (0..16).map(|i| format!(": {}:0;generic step {i}\n", 1_700_000_000 + i)).collect();
    std::fs::write(&hist, lines).unwrap();
    let db = dir.join("relic.db");
    let out = Command::new(env!("CARGO_BIN_EXE_relic")).arg("import").arg(&hist).arg("--db").arg(&db).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    db
}

/// Runs the case until it says READY and `ready` holds, sends SIGINT, and
/// returns the exit code, the time from SIGINT to exit, and what it said.
fn interrupt(case: &str, url: &str, ready: impl Fn() -> bool) -> (Option<i32>, Duration, String) {
    let dir = tempfile::tempdir().unwrap();
    let db = store(dir.path());
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["gate_child", "--exact", "--nocapture"])
        .env("GATE_CHILD", case)
        .env("GATE_URL", url)
        .env("GATE_DB", &db)
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
        let line = rx.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| panic!("no READY from {case}:\n{said}"));
        said.push_str(&line);
        said.push('\n');
        if line == "READY" {
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
        assert!(sent.elapsed() < Duration::from_secs(5), "still running 5 s after SIGINT:\n{said}");
        std::thread::sleep(Duration::from_millis(1));
    };
    let took = sent.elapsed();
    said.extend(rx.try_iter().map(|l| l + "\n"));
    (status.code(), took, said)
}

#[test]
fn ctrl_c_during_a_root_call_exits_130_within_250_ms() {
    let (url, _) = server(None, Duration::ZERO);
    let (code, took, said) = interrupt("root", &url, || true);
    assert_eq!(code, Some(130), "{said}");
    assert!(said.contains("status Cancelled"), "{said}");
    assert!(took < GATE, "{took:?} from SIGINT to exit\n{said}");
}

#[test]
fn ctrl_c_during_four_child_calls_exits_130_within_250_ms_and_commits_none() {
    let plan = serde_json::json!({"version": 1, "expected_state_version": 0, "kind": "plan", "finish": null, "plan": {"ops": [
        {"op": "search", "op_id": "s1", "terms": ["generic"], "match": "any", "limit": 16},
        {"op": "assess", "op_id": "a1", "input": {"output": "s1"}, "objective": "renewal", "max_items": 16},
    ]}});
    // Replies land 150 ms after each call arrives: after SIGINT, and before
    // the gate would close if the process were still waiting.
    let (url, children) = server(Some(plan), Duration::from_millis(150));
    let (code, took, said) = interrupt("children", &url, || children.load(Ordering::SeqCst) == 4);
    assert_eq!(code, Some(130), "{said}");
    assert!(said.contains("status Cancelled"), "{said}");
    assert!(took < GATE, "{took:?} from SIGINT to exit\n{said}");
    assert!(!said.contains("Replied { role: Child"), "a child reply was let in after cancel:\n{said}");
}
