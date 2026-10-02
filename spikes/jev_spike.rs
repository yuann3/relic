//! PROTOTYPE — delete after P1 (#18) is resolved.
//!
//! Proves Jev works for all three Relic integration surfaces:
//!   1. Assess child model — Choice(4) verdict on evidence
//!   2. Search reranking — Score(5) relevance grading
//!   3. Query intent routing — Choice intent classification
//!
//! Run: TYPESAFE_API_KEY=<key> cargo run --bin jev_spike

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;

// ── Thin TypeSafe client ──────────────────────────────────────────

const API_URL: &str = "https://api.typesafe.ai/v1/systemone";
const MODEL: &str = "jev-latest";

#[derive(Debug, Serialize)]
struct SystemOneRequest {
    state: serde_json::Value,
    model: String,
    questions: HashMap<String, Question>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Question {
    Choice {
        instructions: serde_json::Value,
        criteria: HashMap<String, serde_json::Value>,
    },
    Score {
        instructions: serde_json::Value,
        criteria: Vec<serde_json::Value>,
    },
    Noul {
        instructions: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<NoulCriteria>,
    },
}

#[derive(Debug, Serialize)]
struct NoulCriteria {
    r#true: Option<serde_json::Value>,
    r#false: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct SystemOneResponse {
    model: String,
    answers: HashMap<String, Answer>,
    usage: Usage,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Answer {
    Choice {
        choice: String,
        confidence: f64,
        probabilities: HashMap<String, f64>,
    },
    Score {
        score: f64,
        confidence: f64,
        probabilities: HashMap<String, f64>,
    },
    Noul {
        noul: f64,
    },
}

#[derive(Debug, Deserialize)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
}

fn call_jev(api_key: &str, request: &SystemOneRequest) -> Result<SystemOneResponse, String> {
    let body = serde_json::to_value(request).map_err(|e| format!("serialize: {e}"))?;

    let response: SystemOneResponse = ureq::post(API_URL)
        .header("Authorization", &format!("Bearer {api_key}"))
        .send_json(&body)
        .map_err(|e| format!("http: {e}"))?
        .body_mut()
        .read_json()
        .map_err(|e| format!("deserialize: {e}"))?;

    Ok(response)
}

// ── Surface 1: Assess child model ─────────────────────────────────

fn test_assess_verdict(api_key: &str) {
    println!("\n{}", "=".repeat(60));
    println!("SURFACE 1: Assess child model — Choice(4) verdict");
    println!("{}", "=".repeat(60));

    let evidence_items = vec![
        ("item_0", "certbot renew --nginx", "find observed certificate renewal steps"),
        ("item_1", "apt-get update && apt-get upgrade -y", "find observed certificate renewal steps"),
        ("item_2", "certbot certificates\nFound the following certificates:\n  Certificate Name: example.com\n  Expiry Date: 2027-03-15", "find observed certificate renewal steps"),
        ("item_3", "git commit -m 'update readme'", "find observed certificate renewal steps"),
    ];

    let mut criteria = HashMap::new();
    criteria.insert("supports".to_string(), serde_json::json!("The evidence directly supports the objective with an observed action, command, or outcome"));
    criteria.insert("contradicts".to_string(), serde_json::json!("The evidence shows a failure, rollback, or outcome contrary to the objective"));
    criteria.insert("unclear".to_string(), serde_json::json!("The evidence is ambiguous, truncated, or insufficient to judge relevance"));
    criteria.insert("irrelevant".to_string(), serde_json::json!("The evidence is entirely unrelated to the objective"));

    // Batch: one question per evidence item, shared objective in state
    let mut questions = HashMap::new();
    for (item_id, evidence_text, objective) in &evidence_items {
        questions.insert(
            item_id.to_string(),
            Question::Choice {
                instructions: serde_json::json!(format!(
                    "Evidence: '{}'\n\nDoes this evidence support or contradict the stated objective?",
                    evidence_text
                )),
                criteria: criteria.clone(),
            },
        );
        // Use the objective as shared state (same for all items in this batch)
        let _ = objective;
    }

    let request = SystemOneRequest {
        state: serde_json::json!({
            "objective": evidence_items[0].2
        }),
        model: MODEL.to_string(),
        questions,
    };

    let start = Instant::now();
    match call_jev(api_key, &request) {
        Ok(resp) => {
            let elapsed = start.elapsed();
            println!("  Model: {}", resp.model);
            println!("  Latency: {:.0}ms", elapsed.as_millis());
            println!("  Tokens: {} in, {} out", resp.usage.input_tokens, resp.usage.output_tokens);
            println!();

            for (item_id, evidence_text, _) in &evidence_items {
                if let Some(answer) = resp.answers.get(*item_id) {
                    match answer {
                        Answer::Choice { choice, confidence, probabilities } => {
                            // Map to Relic's Confidence enum
                            let relic_confidence = if *confidence > 0.7 {
                                "High"
                            } else if *confidence > 0.4 {
                                "Medium"
                            } else {
                                "Low"
                            };

                            println!("  {item_id}: '{}'", &evidence_text[..evidence_text.len().min(50)]);
                            println!("    verdict={choice}, confidence={confidence:.2} → {relic_confidence}");
                            println!("    probabilities: {:?}", probabilities);
                        }
                        _ => println!("  {item_id}: unexpected answer type"),
                    }
                }
            }
        }
        Err(e) => println!("  ERROR: {e}"),
    }
}

// ── Surface 2: Search reranking ───────────────────────────────────

fn test_search_reranking(api_key: &str) {
    println!("\n{}", "=".repeat(60));
    println!("SURFACE 2: Search reranking — Score(5) relevance");
    println!("{}", "=".repeat(60));

    let query = "how to renew SSL certificate";
    let bm25_results = vec![
        ("hit_0", "openssl req -new -key server.key -out server.csr"),
        ("hit_1", "certbot renew --nginx --dry-run"),
        ("hit_2", "cat /etc/nginx/sites-available/default"),
        ("hit_3", "certbot certificates"),
        ("hit_4", "systemctl restart nginx"),
        ("hit_5", "apt install python3-certbot-nginx"),
    ];

    let mut questions = HashMap::new();
    for (hit_id, hit_text) in &bm25_results {
        questions.insert(
            hit_id.to_string(),
            Question::Score {
                instructions: serde_json::json!(format!(
                    "Rate how relevant this command is to the search query.\n\nCommand: '{}'",
                    hit_text
                )),
                criteria: vec![
                    serde_json::json!("Completely irrelevant; no connection to the query"),
                    serde_json::json!("Tangentially related; mentions similar topics"),
                    serde_json::json!("Partially relevant; addresses part of the query"),
                    serde_json::json!("Directly relevant; addresses the query with specific information"),
                    serde_json::json!("Highly relevant; directly answers the query with concrete evidence"),
                ],
            },
        );
    }

    let request = SystemOneRequest {
        state: serde_json::json!({ "search_query": query }),
        model: MODEL.to_string(),
        questions,
    };

    let start = Instant::now();
    match call_jev(api_key, &request) {
        Ok(resp) => {
            let elapsed = start.elapsed();
            println!("  Query: '{query}'");
            println!("  Latency: {:.0}ms", elapsed.as_millis());
            println!("  Tokens: {} in, {} out", resp.usage.input_tokens, resp.usage.output_tokens);
            println!();

            // Collect scores and sort
            let mut scored: Vec<(&str, &str, f64, f64)> = Vec::new();
            for (hit_id, hit_text) in &bm25_results {
                if let Some(Answer::Score { score, confidence, .. }) = resp.answers.get(*hit_id) {
                    scored.push((hit_id, hit_text, *score, *confidence));
                }
            }

            println!("  BM25 order → Jev reranked order:");
            println!("  {:<6} {:<50} {:>5} {:>6}", "id", "command", "score", "conf");
            println!("  {}", "-".repeat(70));

            // Show BM25 order with scores
            for (id, text, score, conf) in &scored {
                println!("  {:<6} {:<50} {:>5.2} {:>6.2}", id, &text[..text.len().min(50)], score, conf);
            }

            // Show reranked order
            scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap());
            println!();
            println!("  Reranked (by Jev score):");
            for (i, (id, text, score, _)) in scored.iter().enumerate() {
                println!("  {}. {id}: '{}' (score={score:.2})", i + 1, &text[..text.len().min(50)]);
            }
        }
        Err(e) => println!("  ERROR: {e}"),
    }
}

// ── Surface 3: Query intent routing ──────────────────────────────

fn test_intent_routing(api_key: &str) {
    println!("\n{}", "=".repeat(60));
    println!("SURFACE 3: Query intent routing — Choice");
    println!("{}", "=".repeat(60));

    let test_queries = vec![
        "how did I renew the SSL certificate last time",
        "git log",
        "what failed yesterday evening",
        "show me the docker compose setup",
        "did I ever run terraform destroy",
    ];

    let mut criteria = HashMap::new();
    criteria.insert("procedure_recall".to_string(), serde_json::json!("User wants to recall a multi-step procedure they performed before"));
    criteria.insert("exact_lookup".to_string(), serde_json::json!("User wants to find a specific command or output they ran"));
    criteria.insert("investigation".to_string(), serde_json::json!("User wants to understand what happened during a time period or incident"));
    criteria.insert("exploration".to_string(), serde_json::json!("User wants to browse or discover what exists in their history"));

    for query in &test_queries {
        let mut questions = HashMap::new();
        questions.insert(
            "intent".to_string(),
            Question::Choice {
                instructions: serde_json::json!("What is the user's intent with this query?"),
                criteria: criteria.clone(),
            },
        );

        let request = SystemOneRequest {
            state: serde_json::json!({ "user_query": query }),
            model: MODEL.to_string(),
            questions,
        };

        let start = Instant::now();
        match call_jev(api_key, &request) {
            Ok(resp) => {
                let elapsed = start.elapsed();
                if let Some(Answer::Choice { choice, confidence, probabilities }) = resp.answers.get("intent") {
                    println!("  '{query}'");
                    println!("    → {choice} (confidence={confidence:.2}, {:.0}ms)", elapsed.as_millis());
                    println!("    probabilities: {:?}", probabilities);
                    println!();
                }
            }
            Err(e) => println!("  '{query}' → ERROR: {e}\n"),
        }
    }
}

// ── Main ──────────────────────────────────────────────────────────

fn main() {
    let api_key = match std::env::var("TYPESAFE_API_KEY") {
        Ok(key) if !key.is_empty() => key,
        _ => {
            eprintln!("Set TYPESAFE_API_KEY to run this prototype.");
            eprintln!("Get a key at https://app.typesafe.ai");
            std::process::exit(1);
        }
    };

    println!("Jev Spike — proving all three integration surfaces");
    println!("API: {API_URL}");
    println!("Model: {MODEL}");

    test_assess_verdict(&api_key);
    test_search_reranking(&api_key);
    test_intent_routing(&api_key);

    println!("\n{}", "=".repeat(60));
    println!("DONE — all three surfaces tested.");
    println!("{}", "=".repeat(60));
}
