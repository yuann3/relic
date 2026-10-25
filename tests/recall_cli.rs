//! `relic recall` as a process against a scripted Chat Completions server on
//! loopback: its relic.recall/v1 contract and exit codes (ADR 0011).

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// Every request body the server was sent, in arrival order.
type Bodies = Arc<Mutex<Vec<String>>>;

/// What the model writes for one request, or `None` for an HTTP 500.
type Script = dyn Fn(&Value) -> Option<Value> + Send + Sync;

/// A Chat Completions server answering each request with `script`, which
/// sees the request's user message: the root state or the child packet.
fn serve(script: impl Fn(&Value) -> Option<Value> + Send + Sync + 'static) -> (String, Bodies) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let bodies = Bodies::default();
    let (seen, script): (Bodies, Arc<Script>) = (Arc::clone(&bodies), Arc::new(script));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (seen, script) = (Arc::clone(&seen), Arc::clone(&script));
            std::thread::spawn(move || answer(stream, &seen, &*script));
        }
    });
    (url, bodies)
}

fn answer(mut stream: TcpStream, seen: &Bodies, script: &Script) {
    let mut reader = BufReader::new(&stream);
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
    let body = String::from_utf8(body).unwrap();
    seen.lock().unwrap().push(body.clone());
    let request: Value = serde_json::from_str(&body).unwrap();
    let user: Value = serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    let (status, body) = match script(&user) {
        Some(content) => {
            let reply = json!({"model": "scripted", "choices": [{"message": {"content": content.to_string()}, "finish_reason": "stop"}]});
            ("200 OK", reply.to_string())
        }
        None => ("500 Internal Server Error", "{}".to_string()),
    };
    let head = format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
    let _ = stream.write_all(format!("{head}{body}").as_bytes());
}

fn plan(ops: Value) -> Value {
    json!({"version": 1, "expected_state_version": 0, "kind": "plan", "finish": null, "plan": {"ops": ops}})
}

fn finish(state_version: u64, finish: Value) -> Value {
    json!({"version": 1, "expected_state_version": state_version, "kind": "finish", "plan": null, "finish": finish})
}

/// The inspected items of a root state.
fn inspected(state: &Value) -> &Vec<Value> {
    state["last_inspection"]["items"].as_array().unwrap()
}

/// The evidence id of the inspected item whose text holds `needle`.
fn cite(state: &Value, needle: &str) -> Value {
    let item = inspected(state).iter().find(|i| i["safe_text"].as_str().unwrap().contains(needle));
    item.unwrap_or_else(|| panic!("no inspected item holds {needle:?}: {state}"))["evidence_id"].clone()
}

/// Search `certbot` and inspect what it found, then answer with `answer`.
fn search_then(answer: impl Fn(&Value) -> Value + Send + Sync + 'static) -> impl Fn(&Value) -> Option<Value> + Send + Sync {
    move |state| {
        Some(match state["state_version"].as_u64().unwrap() {
            0 => plan(json!([
                {"op": "search", "op_id": "s1", "terms": ["certbot"], "match": "any", "limit": 20},
                {"op": "inspect", "op_id": "i1", "input": {"output": "s1"}, "limit": 20, "max_item_bytes": 2048},
            ])),
            v => finish(v, answer(state)),
        })
    }
}

fn relic(args: &[&str], db: &Path, url: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relic"))
        .args(args)
        .arg("--db")
        .arg(db)
        .env_remove("RELIC_MODEL_API_KEY")
        .env("RELIC_MODEL_PROFILE", "vllm")
        .env("RELIC_MODEL_URL", url)
        .env("RELIC_MODEL_NAME", "scripted")
        .output()
        .unwrap()
}

fn import(db: &Path, hist: &Path, shell: &str) {
    let out = relic(&["import", hist.to_str().unwrap(), "--shell", shell], db, "");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

fn store_with(dir: &Path, commands: &[&str]) -> PathBuf {
    let hist = dir.join("zsh_history");
    let lines: String = commands.iter().enumerate().map(|(i, c)| format!(": {}:0;{c}\n", 1_700_000_000 + i)).collect();
    std::fs::write(&hist, lines).unwrap();
    let db = dir.join("relic.db");
    import(&db, &hist, "zsh");
    db
}

/// The id `relic search` gives the one `certbot` record holding `text`.
fn record_id(db: &Path, text: &str) -> i64 {
    let out = relic(&["search", "--json", "certbot"], db, "");
    let page: Value = serde_json::from_slice(&out.stdout).unwrap();
    let hits: Vec<&Value> = page["hits"].as_array().unwrap().iter().filter(|h| h["text"] == text).collect();
    assert_eq!(hits.len(), 1, "{page}");
    hits[0]["id"].as_i64().unwrap()
}

const CERTBOT: &[&str] = &["ls", "certbot renew --nginx -d example.com", "certbot certificates", "git status"];

#[test]
fn an_answer_cites_the_records_it_rests_on_by_id() {
    let dir = tempfile::tempdir().unwrap();
    let db = store_with(dir.path(), CERTBOT);
    let (url, bodies) = serve(search_then(|state| {
        json!({"status": "answered", "answer": "You renewed with certbot.",
            "claims": [{"text": "certbot renew ran", "supports": [cite(state, "renew")], "contradicts": [cite(state, "certificates")]}],
            "procedure": [{"text": "Run certbot renew --nginx", "supports": [cite(state, "renew")]}]})
    }));
    let out = relic(&["recall", "how", "did", "I", "renew", "--json"], &db, &url);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    let (renew, certs) = (record_id(&db, CERTBOT[1]), record_id(&db, CERTBOT[2]));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["schema"], "relic.recall/v1");
    assert_eq!(report["outcome"], "answered");
    assert_eq!(report["question"], "how did I renew");
    assert_eq!(report["claims"], json!([{"text": "certbot renew ran", "supports": [renew], "contradicts": [certs]}]));
    assert_eq!(report["procedure"], json!([{"text": "Run certbot renew --nginx", "supports": [renew]}]));
    let evidence: Vec<(i64, &str)> = report["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["id"].as_i64().unwrap(), e["text"].as_str().unwrap()))
        .collect();
    assert_eq!(evidence, [(renew, CERTBOT[1]), (certs, CERTBOT[2])]);
    assert_eq!((report["root_calls"].as_u64(), bodies.lock().unwrap().len()), (Some(2), 2));
    // The payload audit: each request's size and destination, on stderr.
    let origin = url.trim_end_matches("/v1");
    assert_eq!(stderr.matches("send a root request of ").count(), 2, "{stderr}");
    assert!(stderr.contains(&format!("to {origin} (vllm)")), "{stderr}");

    // The same recall as text.
    let out = relic(&["recall", "how", "did", "I", "renew"], &db, &url);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(text.starts_with("answered: You renewed with certbot.\n"), "{text}");
    assert!(text.contains(&format!("  - certbot renew ran [#{renew}, against #{certs}]\n")), "{text}");
    assert!(text.contains(&format!("  1. Run certbot renew --nginx [#{renew}]\n")), "{text}");
    assert!(text.contains(&format!("  #{renew:<6} ")) && text.contains("zsh   certbot renew --nginx -d example.com"), "{text}");
}

#[test]
fn exit_codes_say_answered_nothing_citable_failed_and_unconfigured_apart() {
    let dir = tempfile::tempdir().unwrap();
    let db = store_with(dir.path(), CERTBOT);
    let run = |script: Box<Script>, args: &[&str]| {
        let (url, _) = serve(script);
        let out = relic(&[&["recall", "renew"][..], args].concat(), &db, &url);
        (out.status.code(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
    };

    let partial = |_: &Value| json!({"status": "partial", "answer": "maybe certbot", "claims": [], "missing": ["the date"]});
    let (code, stdout, _) = run(Box::new(search_then(partial)), &[]);
    assert_eq!(code, Some(0));
    assert!(stdout.starts_with("partly answered: maybe certbot\n") && stdout.contains("missing:\n  - the date\n"), "{stdout}");

    let abstain = |_: &Value| json!({"status": "abstained", "answer": "", "claims": [], "missing": ["no renewal found"]});
    let (code, stdout, _) = run(Box::new(search_then(abstain)), &["--json"]);
    assert_eq!(code, Some(1), "{stdout}");
    assert_eq!(serde_json::from_str::<Value>(&stdout).unwrap()["outcome"], "abstained");

    // Citing what it was never shown is no evidence: the finish gate
    // turns it into an abstention, exit 1 like any other.
    let forged = |_: &Value| json!({"status": "answered", "answer": "yes", "claims": [{"text": "x", "supports": ["ev1_00000000deadbeef"]}]});
    let (code, stdout, stderr) = run(Box::new(search_then(forged)), &["--json"]);
    assert_eq!(code, Some(1), "{stderr}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!((report["outcome"].as_str(), report["claims"].as_array().unwrap().len()), (Some("abstained"), 0));
    assert!(stderr.contains("note: the finish gate refused the answer"), "{stderr}");

    let (code, stdout, stderr) = run(Box::new(|_: &Value| None), &["--json"]);
    assert_eq!(code, Some(2), "{stderr}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["outcome"], "failed");
    assert!(report["error"].as_str().unwrap().contains("HTTP 500") && stderr.contains("recall failed"), "{report}");

    let out = Command::new(env!("CARGO_BIN_EXE_relic"))
        .args(["recall", "renew", "--db"])
        .arg(&db)
        .env("RELIC_MODEL_PROFILE", "vllm")
        .env("RELIC_MODEL_URL", "http://127.0.0.1:9/v1")
        .env_remove("RELIC_MODEL_NAME")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr.contains("RELIC_MODEL_NAME is required") && out.stdout.is_empty(), "{stderr}");
}
