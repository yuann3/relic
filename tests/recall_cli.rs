//! `relic recall` as a process against a scripted Chat Completions server on
//! loopback: its relic.recall/v1 contract and exit codes (ADR 0011), and the
//! privacy canary gate of ADR 0004.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// Every request body the server was sent, in arrival order.
type Bodies = Arc<Mutex<Vec<String>>>;

/// What the model writes for one request, or `None` for an HTTP 500 that
/// says why.
type Script = dyn Fn(&Value) -> Option<Value> + Send + Sync;

/// A Chat Completions server answering each request with `script`, which
/// sees the request's user message: the root state or the child packet.
fn serve(script: impl Fn(&Value) -> Option<Value> + Send + Sync + 'static) -> (String, Bodies) {
    serve_as("scripted", script)
}

/// [`serve`], saying `model` answered.
fn serve_as(model: &'static str, script: impl Fn(&Value) -> Option<Value> + Send + Sync + 'static) -> (String, Bodies) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let bodies = Bodies::default();
    let (seen, script): (Bodies, Arc<Script>) = (Arc::clone(&bodies), Arc::new(script));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (seen, script) = (Arc::clone(&seen), Arc::clone(&script));
            std::thread::spawn(move || answer(stream, model, &seen, &*script));
        }
    });
    (url, bodies)
}

fn answer(mut stream: TcpStream, model: &str, seen: &Bodies, script: &Script) {
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
    let user = user_message(&body);
    let (status, body) = match script(&user) {
        Some(content) => {
            let reply = json!({"model": model, "choices": [{"message": {"content": content.to_string()}, "finish_reason": "stop"}]});
            ("200 OK", reply.to_string())
        }
        None => ("500 Internal Server Error", json!({"error": {"message": "the scripted model is down"}}).to_string()),
    };
    let head = format!("HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len());
    let _ = stream.write_all(format!("{head}{body}").as_bytes());
}

/// The user message of a request body: the root state or the child packet.
fn user_message(body: &str) -> Value {
    let request: Value = serde_json::from_str(body).unwrap();
    serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap()
}

fn is_child(user: &Value) -> bool {
    user.get("packet").is_some()
}

/// A child that says every item of its packet supports the objective.
fn supports_all(user: &Value, claim: &str) -> Value {
    let packet = &user["packet"];
    let items = packet["items"].as_array().unwrap();
    let assessments: Vec<Value> = items
        .iter()
        .map(|i| json!({"item_id": i["item_id"], "verdict": "supports", "confidence": "high",
            "claims": [{"text": claim, "supports": [i["evidence_id"]]}]}))
        .collect();
    json!({"version": 1, "packet_id": packet["packet_id"], "assessments": assessments})
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
        .env("RELIC_MODEL_PROFILE", "llama_cpp")
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
    // A model on loopback bills nothing; the server named itself.
    assert_eq!((&report["cost_microusd"], &report["models"]), (&json!(0), &json!(["scripted"])));
    // The payload audit: each request's size and destination, on stderr.
    let origin = url.trim_end_matches("/v1");
    assert_eq!(stderr.matches("send a root request of ").count(), 2, "{stderr}");
    assert!(stderr.contains(&format!("to {origin} (llama_cpp)")), "{stderr}");

    // The log: what was sent and how it ended, under one recall id, and no
    // evidence text, which would outlive `forget`.
    let log_path = dir.path().join("relic.db.recall.log");
    let log = std::fs::read_to_string(&log_path).unwrap();
    let lines: Vec<Value> = log.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    let events: Vec<&str> = lines.iter().map(|l| l["event"].as_str().unwrap()).collect();
    assert_eq!(events, ["start", "send", "reply", "send", "reply", "finish"], "{log}");
    assert!(lines.iter().all(|l| l["schema"] == "relic.recall-log/v1" && l["recall"] == lines[0]["recall"]), "{log}");
    assert_eq!((&lines[0]["origin"], &lines[0]["profile"]), (&json!(origin), &json!("llama_cpp")));
    assert_eq!((&lines[1]["role"], &lines[2]["model"]), (&json!("root"), &json!("scripted")));
    let finish = &lines[5];
    assert_eq!((&finish["outcome"], &finish["cited"], &finish["cost_microusd"]), (&json!("answered"), &json!([renew, certs]), &json!(0)));
    assert!(!log.contains("certbot") && !log.contains("renew"), "{log}");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&log_path).unwrap().permissions().mode() & 0o777, 0o600);

    // The same recall as text.
    let out = relic(&["recall", "how", "did", "I", "renew"], &db, &url);
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(text.starts_with("answered: You renewed with certbot.\n"), "{text}");
    assert!(text.contains(&format!("  - certbot renew ran [#{renew}, against #{certs}]\n")), "{text}");
    assert!(text.contains(&format!("  1. Run certbot renew --nginx [#{renew}]\n")), "{text}");
    assert!(text.contains(&format!("  #{renew:<6} ")) && text.contains("zsh   certbot renew --nginx -d example.com"), "{text}");
    assert!(text.ends_with("spent: 2 root + 0 child calls, at most $0.0000, answered by scripted\n"), "{text}");
    let log = std::fs::read_to_string(&log_path).unwrap();
    let recalls: std::collections::HashSet<&str> = log.lines().map(|l| l.split("\"recall\":\"").nth(1).unwrap().split('"').next().unwrap()).collect();
    assert_eq!((log.lines().count(), recalls.len()), (12, 2), "a second recall appends under its own id:\n{log}");
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
    let said = "model endpoint answered HTTP 500: the scripted model is down";
    assert!(report["error"].as_str().unwrap().contains(said) && stderr.contains(said), "{report}\n{stderr}");

    // A remote model without prices: its cost cannot be capped, so recall
    // refuses before any call unless told the cost may be unknown.
    let remote = |extra: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_relic"))
            .args([&["recall", "renew", "--json", "--db", db.to_str().unwrap()][..], extra].concat())
            .env("RELIC_MODEL_PROFILE", "llama_cpp")
            .env("RELIC_MODEL_URL", "https://127.0.0.1:9/v1")
            .env("RELIC_MODEL_NAME", "remote")
            .env_remove("RELIC_MODEL_PRICE_IN")
            .env_remove("RELIC_MODEL_PRICE_OUT")
            .output()
            .unwrap();
        (out.status.code(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
    };
    let (code, stdout, stderr) = remote(&[]);
    assert_eq!(code, Some(2));
    assert!(stdout.is_empty() && stderr.contains("--allow-unknown-cost") && !stderr.contains("send a root"), "{stderr}");
    let (code, stdout, stderr) = remote(&["--allow-unknown-cost"]);
    assert_eq!(code, Some(2), "{stderr}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!((report["outcome"].as_str(), &report["cost_microusd"]), (Some("failed"), &Value::Null), "{report}");
    assert!(stderr.contains("send a root request") && stderr.contains("could not reach the model"), "{stderr}");

    let out = Command::new(env!("CARGO_BIN_EXE_relic"))
        .args(["recall", "renew", "--db"])
        .arg(&db)
        .env("RELIC_MODEL_PROFILE", "llama_cpp")
        .env("RELIC_MODEL_URL", "http://127.0.0.1:9/v1")
        .env_remove("RELIC_MODEL_NAME")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2));
    assert!(stderr.contains("RELIC_MODEL_NAME is required") && out.stdout.is_empty(), "{stderr}");

    // A log that cannot be opened: nothing leaves unaudited.
    let log = dir.path().join("relic.db.recall.log");
    std::fs::remove_file(&log).unwrap();
    std::fs::create_dir(&log).unwrap();
    let (url, bodies) = serve(search_then(partial));
    let out = relic(&["recall", "renew", "--json"], &db, &url);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!((out.status.code(), report["outcome"].as_str()), (Some(2), Some("failed")), "{report}");
    assert!(report["error"].as_str().unwrap().contains("could not write the recall log"), "{report}");
    assert!(bodies.lock().unwrap().is_empty(), "a request left without its audit");
}

/// ADR 0004: canaries in every field of a history, then import, search and
/// recall against a scripted model; no canary in any byte of the state dir
/// or of anything sent or printed. Every canary starts `Zq9Canary`.
#[test]
fn privacy_canary_gate() {
    let dir = tempfile::tempdir().unwrap();
    let (histories, state) = (dir.path().join("in"), dir.path().join("state"));
    std::fs::create_dir_all(&histories).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    let db = state.join("relic.db");
    // One carrier per line: env assignment, bearer header, URL password, a
    // flag value, piped sudo password and a continuation line; the timestamp
    // and duration fields are numbers only.
    let zsh_commands = [
        "certbot renew --nginx -d example.com",
        "PGPASSWORD=Zq9CanaryPg1 certbot certificates",
        "curl -H 'Authorization: Bearer Zq9CanaryBearer2abcdefghijk' https://acme.test/certbot",
        "git clone https://bob:Zq9CanaryUrl3@git.test/certbot.git",
        "certbot register --password Zq9CanaryFlag4",
        "echo Zq9CanaryPipe5 | sudo -S certbot renew",
        "certbot renew \\\n--token=Zq9CanaryCont6",
    ];
    let zsh: String = zsh_commands.iter().enumerate().map(|(i, c)| format!(": {}:{i};{c}\n", 1_700_000_000 + i)).collect();
    std::fs::write(histories.join("zsh_history"), zsh).unwrap();
    let bash_commands = ["certbot renew --api-key=Zq9CanaryBash7"];
    std::fs::write(histories.join("bash_history"), format!("#1700000100\n{}\n", bash_commands[0])).unwrap();
    import(&db, &histories.join("zsh_history"), "zsh");
    import(&db, &histories.join("bash_history"), "bash");
    // A repository whose commit subject, branch name and stash message
    // carry secrets: the Git fields relic stores.
    let repo = dir.path().join("repo");
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .envs([("GIT_AUTHOR_NAME", "t"), ("GIT_AUTHOR_EMAIL", "t@t"), ("GIT_COMMITTER_NAME", "t"), ("GIT_COMMITTER_EMAIL", "t@t")])
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    std::fs::create_dir(&repo).unwrap();
    git(&["init", "-q", "-b", "main"]);
    git(&["commit", "-q", "--allow-empty", "-m", "certbot renew; password=Zq9CanaryGit13"]);
    git(&["branch", "certbot/api_key=Zq9CanaryRef14"]);
    std::fs::write(repo.join("f"), "x").unwrap();
    git(&["add", "f"]);
    git(&["stash", "-q", "-m", "certbot wip token=Zq9CanaryStash15"]);
    let out = relic(&["import", repo.to_str().unwrap()], &db, "");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let redactor = relic::redact::Redactor::new();
    for raw in zsh_commands.iter().chain(&bash_commands) {
        let safe = redactor.import(raw);
        assert_eq!(redactor.redact(&safe), safe, "redact(redact(x)) != redact(x) for {raw:?}");
    }

    // The model echoes secrets back too, in its answer, its child claims and
    // the name the provider says answered.
    let (url, bodies) = serve_as("token=Zq9CanaryModel9abcdefgh", |user| {
        if is_child(user) {
            return Some(supports_all(user, "it ran with token=Zq9CanaryChild8"));
        }
        Some(match user["state_version"].as_u64().unwrap() {
            0 => plan(json!([
                {"op": "search", "op_id": "s1", "terms": ["certbot"], "match": "any", "limit": 20},
                {"op": "assess", "op_id": "a1", "input": {"output": "s1"}, "objective": "renewal; password=Zq9CanaryObjective9", "max_items": 20},
                {"op": "select", "op_id": "k1", "input": {"output": "a1"}, "verdicts": ["supports"], "limit": 20},
                {"op": "inspect", "op_id": "i1", "input": {"output": "k1"}, "limit": 20, "max_item_bytes": 2048},
            ])),
            v => {
                let all: Vec<Value> = inspected(user).iter().map(|i| i["evidence_id"].clone()).collect();
                finish(v, json!({"status": "answered", "answer": "certbot renew, then mysql --password=Zq9CanaryAnswer10",
                    "claims": [{"text": "renewed; api_key=Zq9CanaryClaim11", "supports": all}],
                    "procedure": [{"text": "certbot renew", "supports": [cite(user, "renew --nginx")]}]}))
            }
        })
    });
    let recall = relic(&["recall", "how", "did", "I", "renew?", "PGPASSWORD=Zq9CanaryQuestion12", "--json"], &db, &url);
    let search = relic(&["search", "certbot", "--json"], &db, &url);
    assert_eq!((recall.status.code(), search.status.code()), (Some(0), Some(0)), "{}", String::from_utf8_lossy(&recall.stderr));

    // Not vacuous: every record went out, redacted, to the root and to
    // children, and the answer came back with its markers.
    let bodies = bodies.lock().unwrap().clone();
    let children = bodies.iter().filter(|b| is_child(&user_message(b))).count();
    assert!(bodies.len() >= 3 && children >= 1, "{} requests, {children} child", bodies.len());
    let sent = bodies.concat();
    for marker in ["[REDACTED:password]", "[REDACTED:token]", "[REDACTED:key]"] {
        assert!(sent.contains(marker), "no {marker} in what was sent");
    }
    let report: Value = serde_json::from_slice(&recall.stdout).unwrap();
    let hits = serde_json::from_slice::<Value>(&search.stdout).unwrap()["hits"].as_array().unwrap().len();
    let git_rows = report["evidence"].as_array().unwrap().iter().filter(|e| e["source"] == "git").count();
    assert!(git_rows >= 6, "the commit, branch and stash went out: {report}");
    assert_eq!(report["evidence"].as_array().unwrap().len(), hits, "{report}");
    assert!(report["answer"].as_str().unwrap().ends_with("--password=[REDACTED:password]"), "{report}");

    let mut haystacks: Vec<(String, Vec<u8>)> = vec![
        ("recall stdout".into(), recall.stdout),
        ("recall stderr".into(), recall.stderr),
        ("search stdout".into(), search.stdout),
        ("search stderr".into(), search.stderr),
    ];
    haystacks.extend(bodies.into_iter().enumerate().map(|(i, b)| (format!("request {i}"), b.into_bytes())));
    let files: Vec<PathBuf> = std::fs::read_dir(&state).unwrap().map(|e| e.unwrap().path()).collect();
    for wanted in ["relic.db", "relic.db-wal", "relic.db.recall.log"] {
        assert!(files.iter().any(|f| f.ends_with(wanted)), "no {wanted} to check in {files:?}");
    }
    haystacks.extend(files.into_iter().map(|f| (f.display().to_string(), std::fs::read(&f).unwrap())));
    for (name, bytes) in &haystacks {
        // FTS keeps lowercased terms.
        let folded = bytes.to_ascii_lowercase();
        assert!(!folded.windows(9).any(|w| w == b"zq9canary"), "a canary in {name}");
    }
}
