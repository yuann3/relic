//! The seeded investigations of ADR 0016 (R4): twelve questions on the
//! Git-recovery fixture, each put to bounded recall and to the three
//! baselines of docs/research/rlm-model-boundary.md:535-537: FTS-only, the
//! first FTS page of 20 plus one model call, and a fixed hand-coded
//! pipeline. A case is scored on the records its answer cites and on what it
//! says; every model call, egress byte and reported token is counted at the
//! model seam, where it leaves.
//!
//! Here the model is scripted on loopback, which checks the cases, the
//! scoring and the meters. Real numbers come from the ignored test, against
//! the model `relic recall` would ask, configured by the `RELIC_MODEL_*`
//! variables or `~/.config/relic/env`:
//!
//! ```text
//! cargo nextest run --test investigations --run-ignored only --no-capture
//! ```
//!
//! It prints a line per arm and writes JSON lines to
//! `$RELIC_INVESTIGATIONS_OUT`, by default `target/tmp/investigations.jsonl`.

#[expect(
    dead_code,
    reason = "the investigations read the stores, not the repositories"
)]
mod recovery;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use relic::rlm::coordinator::{InvestigationResult, InvestigationStatus, run_investigation};
use relic::rlm::model::chat::{ChatCompletionsClient, ChatConfig};
use relic::rlm::model::egress::{self, EgressRequest, Role, Target};
use relic::rlm::model::{ModelBoundary, ModelError, Prices, Reply};
use relic::rlm::protocol::BudgetV1;
use serde_json::{Value, json};

use recovery::{Later, Recovery, S, Variant};

const SEED: u64 = 7;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Group {
    /// The answer is a search and a hop away, but not on the first page.
    Answerable,
    /// The answer puts records of two legs together, or reads a hook's exit
    /// status off its record.
    Combination,
    /// Nothing recorded answers it, or what is recorded contradicts it.
    Insufficient,
}

impl Group {
    fn name(self) -> &'static str {
        match self {
            Self::Answerable => "answerable",
            Self::Combination => "combination",
            Self::Insufficient => "insufficient",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Arm {
    FtsOnly,
    Top20,
    Pipeline,
    Rlm,
}

const ARMS: [Arm; 4] = [Arm::FtsOnly, Arm::Top20, Arm::Pipeline, Arm::Rlm];

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Self::FtsOnly => "fts_only",
            Self::Top20 => "top20_one_call",
            Self::Pipeline => "fixed_pipeline",
            Self::Rlm => "bounded_rlm",
        }
    }

    /// The RLM's own budget for every arm, but a baseline's root calls are
    /// its fixed turns and the one the model gets: past them it is out of
    /// budget, whatever the model plans.
    fn budget(self) -> BudgetV1 {
        let mut budget = BudgetV1::prototype_default();
        if self != Self::Rlm {
            budget.root_calls = 2;
        }
        budget
    }
}

struct Case {
    id: &'static str,
    group: Group,
    /// Asked of the contradictory variant's store, not the answerable one's.
    contradictory: bool,
    question: &'static str,
    /// The story's span in seconds after [`S`]. A record in it, or a Git ref
    /// to a commit its reflog names, is on topic; anything else cited is not.
    window: (i64, i64),
    /// An answer cites at least one record of each need: a record of that
    /// source holding that text, for any of the alternatives.
    needs: Vec<Vec<(&'static str, String)>>,
    /// An answer, claim or step holds one of these, in any case.
    says: Vec<String>,
}

const SHOP: (i64, i64) = (0, 1000);
const NOTES: (i64, i64) = (4990, 5700);
const BILLING: (i64, i64) = (6900, 7900);
const API: (i64, i64) = (8900, 9950);
const INFRA: (i64, i64) = (10900, 11700);

/// The twelve cases. Git's clock and identity are fixed, so both stores
/// have the same commits and `a`'s shas serve both.
fn cases(a: &Recovery, later: &Later) -> Vec<Case> {
    let short = |sha: &str| sha[..7].to_string();
    let need = |source: &'static str, text: &str| (source, text.to_string());
    // What the user typed: history has it, and the hook with its status.
    let typed = |text: &str| vec![need("zsh", text), need("hook", text)];
    let deleted_coupons = vec![
        need("claude", "git branch -D feature/coupons"),
        need("claude-out", "Deleted branch feature/coupons"),
    ];
    let case = |id, group, question, window, needs, says: &[&str]| Case {
        id,
        group,
        contradictory: false,
        question,
        window,
        needs,
        says: says.iter().map(ToString::to_string).collect(),
    };
    let lost_commit = "How did I get back the commit that git reset --hard threw away in shop?";
    let stash = short(a.stash.as_deref().expect("the sibling is imported"));
    let (deleted, limiter, rejected) = (
        short(&a.deleted),
        short(&later.limiter),
        short(&later.rejected),
    );
    vec![
        // R1's answerable case: the rescue is a branch, which Git still has.
        case(
            "shop-lost-commit",
            Group::Answerable,
            lost_commit,
            SHOP,
            vec![
                typed("git branch rescue HEAD@{1}"),
                vec![need("git", "git ref refs/heads/rescue ")],
            ],
            &["rescue"],
        ),
        case(
            "shop-deleted-branch",
            Group::Answerable,
            "Claude Code deleted a branch in shop. How did I restore it?",
            SHOP,
            vec![
                vec![deleted_coupons[0].clone()],
                typed(&format!("git branch feature/coupons {deleted}")),
            ],
            &[&deleted],
        ),
        case(
            "notes-dropped-stash",
            Group::Answerable,
            "I dropped a stash in notes by mistake. Which commit was it, so I can get it back?",
            NOTES,
            vec![vec![need("git", "refs/stash ")], typed("git stash drop")],
            &[&stash],
        ),
        case(
            "billing-detached-commits",
            Group::Answerable,
            "I made commits after checking out a tag in billing, and they are not on main. Where did they end up?",
            BILLING,
            vec![
                typed("git checkout v1.2"),
                vec![
                    need("git", "refs/heads/wip-2 "),
                    need("zsh", "git branch wip-2 "),
                    need("hook", "git branch wip-2 "),
                ],
            ],
            &["wip-2"],
        ),
        // Only the hook saw which builds failed.
        case(
            "api-build-fix",
            Group::Combination,
            "My Rust project in api would not compile at first. What did I do to fix it?",
            API,
            vec![
                vec![need("hook", "cargo build\nexit 101")],
                typed("cargo update -p ring"),
                vec![need("hook", "cargo build\nexit 0")],
            ],
            &["cargo update"],
        ),
        // Claude Code deleted it, the user cherry-picked it, Git shows where.
        case(
            "infra-deleted-fix",
            Group::Combination,
            "Claude Code deleted the branch with my timeout fix in infra. Is that fix on main now?",
            INFRA,
            vec![
                vec![
                    need("claude", "git branch -D fix/timeout"),
                    need("claude-out", "Deleted branch fix/timeout"),
                ],
                typed(&format!("git cherry-pick {}", short(&later.timeout))),
                vec![need("git", "cherry-pick: raise the request timeout")],
            ],
            &["cherry-pick"],
        ),
        // The reflog still has both; the hook saw the cherry-pick fail.
        case(
            "api-lost-commits",
            Group::Combination,
            "After I reset api I tried to bring two commits back, and it went wrong. Can I still recover them?",
            API,
            vec![
                vec![need("git", "commit: add a rate limiter")],
                vec![need("git", "commit: log rejected requests")],
                vec![need(
                    "hook",
                    &format!("git cherry-pick {limiter} {rejected}\nexit 1 "),
                )],
            ],
            &[&limiter, &rejected],
        ),
        case(
            "shop-switch-failed",
            Group::Combination,
            "My first git switch back to the coupons branch in shop failed. Why?",
            SHOP,
            vec![
                vec![need("hook", "git switch feature/coupons\nexit 128")],
                deleted_coupons.clone(),
            ],
            &["deleted"],
        ),
        // R1's contradictory variant: history has the rescue, the hook saw it
        // fail outside the repository, and Git has no `rescue`.
        Case {
            contradictory: true,
            ..case(
                "shop-rescue-failed",
                Group::Insufficient,
                lost_commit,
                SHOP,
                vec![],
                &[],
            )
        },
        // Never committed or stashed: Git never had the edits.
        case(
            "billing-discarded-edits",
            Group::Insufficient,
            "How did I restore the src/tax.rs edits Claude Code threw away in billing?",
            BILLING,
            vec![],
            &[],
        ),
        // shop never had a stash; notes did, which is not this question.
        case(
            "shop-dropped-stash",
            Group::Insufficient,
            "How did I recover the stash I dropped in shop?",
            SHOP,
            vec![],
            &[],
        ),
        // Relic never records what a terminal printed.
        case(
            "api-conflict-output",
            Group::Insufficient,
            "What did the cherry-pick conflict in api print?",
            API,
            vec![],
            &[],
        ),
    ]
}

/// One stored record, as scoring reads it.
struct Record {
    id: i64,
    source: &'static str,
    at_ms: Option<i64>,
    text: String,
}

fn corpus(db: &Path) -> Vec<Record> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut stmt = conn
        .prepare("SELECT id, kind, observed_at_ms, safe_text FROM observation")
        .unwrap();
    let rows = stmt.query_map([], |r| {
        let kind = relic::domain::ObservationKind::from_i64(r.get(1)?).expect("a known kind");
        Ok(Record {
            id: r.get(0)?,
            source: relic::app::source_label(kind),
            at_ms: r.get(2)?,
            text: r.get(3)?,
        })
    });
    rows.unwrap().map(Result::unwrap).collect()
}

/// What an investigation ended with, as scoring reads it.
struct Answer {
    outcome: &'static str,
    error: Option<String>,
    /// Record ids, in the order first cited.
    cited: Vec<i64>,
    /// The answer, claims and steps, lowercased.
    said: String,
    missing: Vec<String>,
}

impl Answer {
    fn of(r: &InvestigationResult) -> Self {
        let (outcome, error) = match &r.status {
            InvestigationStatus::Completed => ("answered", None),
            InvestigationStatus::Partial => ("partial", None),
            InvestigationStatus::Abstained => ("abstained", None),
            InvestigationStatus::BudgetExhausted => ("budget_exhausted", None),
            InvestigationStatus::Cancelled => ("cancelled", None),
            InvestigationStatus::Failed(e) => ("failed", Some(e.clone())),
        };
        let mut answer = Self {
            outcome,
            error,
            cited: Vec::new(),
            said: String::new(),
            missing: Vec::new(),
        };
        let Some(f) = &r.finish else { return answer };
        let texts = [&f.answer]
            .into_iter()
            .chain(f.claims.iter().map(|c| &c.text))
            .chain(f.procedure.iter().map(|s| &s.text));
        answer.said = texts
            .map(|t| t.to_lowercase())
            .collect::<Vec<_>>()
            .join("\n");
        let cites = f
            .claims
            .iter()
            .flat_map(|c| c.supports.iter().chain(&c.contradicts))
            .chain(f.procedure.iter().flat_map(|s| &s.supports));
        for o in cites.filter_map(|c| r.cited.get(c)) {
            if !answer.cited.contains(&o.id) {
                answer.cited.push(o.id);
            }
        }
        answer.missing = f.missing.clone();
        answer
    }
}

impl Case {
    /// The ids each need resolves to in `corpus`.
    fn needed(&self, corpus: &[Record]) -> Vec<Vec<i64>> {
        let resolve = |(source, text): &(&str, String)| {
            let ids: Vec<i64> = corpus
                .iter()
                .filter(|r| r.source == *source && r.text.contains(text.as_str()))
                .map(|r| r.id)
                .collect();
            assert!(
                !ids.is_empty(),
                "{}: no {source} record holds {text:?}",
                self.id
            );
            ids
        };
        self.needs
            .iter()
            .map(|alternatives| alternatives.iter().flat_map(resolve).collect())
            .collect()
    }

    fn on_topic(&self, corpus: &[Record]) -> HashSet<i64> {
        let (from, to) = ((S + self.window.0) * 1000, (S + self.window.1) * 1000);
        let inside = |r: &Record| r.at_ms.is_some_and(|t| (from..=to).contains(&t));
        // `git reflog <ref> <sha> …` and `git ref <ref> <sha> …`. A ref's time
        // is when it was imported, so its commit says whose story it is.
        fn sha(r: &Record) -> Option<&str> {
            (r.source == "git")
                .then(|| r.text.split_whitespace().nth(3))
                .flatten()
        }
        let shas: HashSet<&str> = corpus
            .iter()
            .filter(|r| inside(r))
            .filter_map(sha)
            .collect();
        corpus
            .iter()
            .filter(|r| inside(r) || sha(r).is_some_and(|s| shas.contains(s)))
            .map(|r| r.id)
            .collect()
    }

    /// Whether `a` answers the case, and whether it cites only what is on
    /// topic and everything it needs.
    fn mark(
        &self,
        arm: Arm,
        needed: &[Vec<i64>],
        on_topic: &HashSet<i64>,
        a: &Answer,
    ) -> (bool, bool) {
        let needs_met = needed
            .iter()
            .all(|ids| ids.iter().any(|id| a.cited.contains(id)));
        // FTS-only's answer is the records it found: there is no text to judge.
        let says = arm == Arm::FtsOnly
            || self.says.is_empty()
            || self.says.iter().any(|s| a.said.contains(&s.to_lowercase()));
        let correct = match self.group {
            // No claim of success, and it says what is missing (research,
            // prototype acceptance plan 2).
            Group::Insufficient => {
                matches!(a.outcome, "abstained" | "partial") && !a.missing.is_empty()
            }
            _ => a.outcome == "answered" && needs_met && says,
        };
        let exact = a.cited.iter().all(|id| on_topic.contains(id))
            && (self.group == Group::Insufficient || needs_met);
        (correct, exact)
    }
}

/// The baselines' query: the question's words of three letters or more, at
/// most the 12 a search takes. The search matches any of them and keeps the
/// most recently imported, as every search surface does (ADR 0006).
fn terms(question: &str) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for word in question
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(str::to_lowercase)
    {
        if !terms.contains(&word) {
            terms.push(word);
        }
    }
    terms.truncate(12);
    terms
}

/// A root turn the host takes for an arm, so it never leaves.
enum Turn {
    Plan(Value),
    /// FTS-only's answer: every record its search found.
    CiteAll,
}

impl Turn {
    fn action(&self, state: &Value) -> Value {
        let v = &state["state_version"];
        match self {
            Self::Plan(ops) => {
                json!({"version": 1, "expected_state_version": v, "kind": "plan", "finish": null, "plan": {"ops": ops}})
            }
            Self::CiteAll => {
                let items = state["last_inspection"]["items"]
                    .as_array()
                    .into_iter()
                    .flatten();
                let ids: Vec<&Value> = items.map(|i| &i["evidence_id"]).collect();
                let finish = match ids.is_empty() {
                    true => {
                        json!({"status": "abstained", "answer": "", "claims": [], "missing": ["a record matching the question"]})
                    }
                    false => {
                        json!({"status": "answered", "answer": "These records match the question.",
                        "claims": [{"text": "these records match the question", "supports": ids}]})
                    }
                };
                json!({"version": 1, "expected_state_version": v, "kind": "finish", "plan": null, "finish": finish})
            }
        }
    }
}

/// What reached the provider for one investigation.
#[derive(Default, Clone)]
struct Meter {
    /// Requests sent, a retry included.
    calls: u64,
    egress_bytes: u64,
    prompt_tokens: u64,
    completion_tokens: u64,
    /// Calls whose reply reported no usage: the token counts leave them out.
    calls_without_usage: u64,
    /// What the provider says answered.
    returned: Vec<String>,
}

/// The model one arm's investigation talks to: the host takes the arm's
/// fixed root turns itself, and everything else goes to `model`, metered.
struct Metered {
    model: Arc<dyn ModelBoundary>,
    turns: Mutex<VecDeque<Turn>>,
    meter: Mutex<Meter>,
}

impl Metered {
    fn new(arm: Arm, question: &str, model: Arc<dyn ModelBoundary>) -> Self {
        let search = json!({"op": "search", "op_id": "s1", "terms": terms(question), "match": "any", "limit": 20});
        let inspect = |input: &str| json!({"op": "inspect", "op_id": "i1", "input": {"output": input}, "limit": 20, "max_item_bytes": 1024});
        let turns = match arm {
            Arm::FtsOnly => vec![Turn::Plan(json!([search, inspect("s1")])), Turn::CiteAll],
            Arm::Top20 => vec![Turn::Plan(json!([search, inspect("s1")]))],
            // The research's fixed shape: search, expand, assess, select,
            // inspect, then the model finishes. 48 items is 12 child calls
            // of 4, within the 16 the budget allows.
            Arm::Pipeline => vec![Turn::Plan(json!([
                search,
                {"op": "expand", "op_id": "e1", "input": {"output": "s1"}, "scope": "same_source", "before": 2, "after": 2, "limit": 48},
                {"op": "assess", "op_id": "a1", "input": {"output": "e1"}, "objective": question, "max_items": 48},
                {"op": "select", "op_id": "k1", "input": {"output": "a1"}, "verdicts": ["supports"], "limit": 20},
                inspect("k1"),
            ]))],
            Arm::Rlm => vec![],
        };
        Self {
            model,
            turns: Mutex::new(turns.into()),
            meter: Mutex::default(),
        }
    }
}

impl ModelBoundary for Metered {
    fn target(&self) -> Target {
        self.model.target()
    }

    fn prices(&self) -> Option<Prices> {
        self.model.prices()
    }

    fn call(&self, req: &EgressRequest, deadline: Instant) -> Result<Reply, ModelError> {
        if req.role() == Role::Root
            && let Some(turn) = self.turns.lock().unwrap().pop_front()
        {
            let state: Value = serde_json::from_str(req.user()).expect("a root state is JSON");
            return Ok(Reply {
                body: turn.action(&state).to_string(),
                ..Reply::default()
            });
        }
        let reply = self.model.call(req, deadline);
        let mut m = self.meter.lock().unwrap();
        m.calls += 1;
        m.egress_bytes += req.bytes() as u64;
        match reply.as_ref().ok().and_then(|r| r.usage) {
            Some(u) => {
                m.prompt_tokens += u.prompt_tokens;
                m.completion_tokens += u.completion_tokens;
            }
            None => m.calls_without_usage += 1,
        }
        if let Ok(Reply {
            returned_model: Some(name),
            ..
        }) = &reply
            && !m.returned.contains(name)
        {
            m.returned.push(name.clone());
        }
        reply
    }
}

/// Both stores, as the cases need them: same seed, same commits, apart from
/// whether the shop rescue worked.
struct Bench {
    stores: [Recovery; 2],
    later: Later,
    cases: Vec<Case>,
}

fn bench() -> Bench {
    let store = |variant| {
        let mut r = Recovery::build(variant, SEED);
        r.import_sibling();
        let later = r.import_investigations();
        (r, later)
    };
    let ((a, later), (b, _)) = (store(Variant::Answerable), store(Variant::Contradictory));
    let cases = cases(&a, &later);
    Bench {
        stores: [a, b],
        later,
        cases,
    }
}

/// Exactly which relic ran: its `--version` and the commit it was built from.
fn versions() -> Value {
    let stdout = |cmd: &mut Command| {
        String::from_utf8(cmd.output().unwrap().stdout)
            .unwrap()
            .trim()
            .to_string()
    };
    let repo = env!("CARGO_MANIFEST_DIR");
    json!({
        "version": stdout(Command::new(env!("CARGO_BIN_EXE_relic")).arg("--version")),
        "commit": stdout(Command::new("git").args(["-C", repo, "rev-parse", "HEAD"])),
        "dirty": !stdout(Command::new("git").args(["-C", repo, "status", "--porcelain", "--untracked-files=no"])).is_empty(),
    })
}

/// Every case under every arm, written to `out` as JSON lines: the exact
/// versions, a line per case and arm, a line per arm, then the verdict.
/// `model(contradictory)` answers for that store.
fn run(
    bench: &Bench,
    out: &Path,
    configured: &str,
    model: &dyn Fn(bool) -> Arc<dyn ModelBoundary>,
) -> Vec<Value> {
    let corpora = bench.stores.each_ref().map(|r| corpus(&r.db));
    let mut rows = Vec::new();
    let mut returned: Vec<String> = Vec::new();
    for case in &bench.cases {
        let i = usize::from(case.contradictory);
        let (needed, on_topic) = (case.needed(&corpora[i]), case.on_topic(&corpora[i]));
        let store = relic::store::Store::open_read_only(&bench.stores[i].db).unwrap();
        for arm in ARMS {
            let metered = Arc::new(Metered::new(arm, case.question, model(case.contradictory)));
            let t = Instant::now();
            let r = run_investigation(
                case.question,
                None,
                &store,
                metered.clone(),
                arm.budget(),
                &Arc::default(),
                &mut |_| {},
            );
            let wall_ms = t.elapsed().as_millis() as u64;
            let m = metered.meter.lock().unwrap().clone();
            for name in m.returned {
                if !returned.contains(&name) {
                    returned.push(name);
                }
            }
            let answer = Answer::of(&r);
            let (correct, exact) = case.mark(arm, &needed, &on_topic, &answer);
            // A real model takes minutes over the 48 runs; without a line
            // each, the run looks hung.
            eprintln!(
                "{} {:<15} correct {correct:<5} {} calls {:.1} s",
                case.id,
                arm.name(),
                m.calls,
                wall_ms as f64 / 1e3
            );
            rows.push(json!({
                "case": case.id, "group": case.group.name(), "arm": arm.name(), "outcome": answer.outcome, "error": answer.error,
                "correct": correct, "exact": exact, "cited": answer.cited, "needed": needed, "missing": answer.missing,
                "calls": m.calls, "egress_bytes": m.egress_bytes, "prompt_tokens": m.prompt_tokens,
                "completion_tokens": m.completion_tokens, "calls_without_usage": m.calls_without_usage,
                "turns": r.turns, "child_calls": r.child_calls, "cost_microusd": r.cost_microusd, "wall_ms": wall_ms,
            }));
        }
    }
    let target = model(false).target();
    let header = json!({
        "schema": "relic.investigations/v1", "seed": SEED, "relic": versions(),
        "model": {"configured": configured, "profile": target.profile, "origin": target.origin, "returned": returned},
    });
    let summaries = ARMS.map(|arm| summary(arm, &rows));
    // The RLM label (research, prototype acceptance plan 2): it answers more
    // of the eight cases built to need a join than every baseline, and
    // claims success on none of the four insufficient ones.
    let joined = |s: &Value| {
        s["correct"]["answerable"].as_u64().unwrap() + s["correct"]["combination"].as_u64().unwrap()
    };
    let rlm = &summaries[3];
    let beats = summaries[..3].iter().all(|b| joined(rlm) > joined(b))
        && rlm["correct"]["insufficient"] == 4;
    let verdict = json!({"verdict": {"bounded_rlm_beats_baselines": beats}});

    for s in &summaries {
        let of = |g: Group| format!("{}/4", s["correct"][g.name()]);
        println!(
            "{:<15} answerable {}  combination {}  insufficient {}  exact {:>2}/12  calls {:>3}  egress {:>7} B  tokens {}+{} ({} calls unreported)",
            s["arm"].as_str().unwrap(),
            of(Group::Answerable),
            of(Group::Combination),
            of(Group::Insufficient),
            s["exact"],
            s["calls"],
            s["egress_bytes"],
            s["prompt_tokens"],
            s["completion_tokens"],
            s["calls_without_usage"],
        );
    }
    println!("bounded RLM beats the baselines: {beats}");
    let lines: Vec<Value> = [header]
        .into_iter()
        .chain(rows)
        .chain(summaries)
        .chain([verdict])
        .collect();
    std::fs::write(
        out,
        lines.iter().map(|l| format!("{l}\n")).collect::<String>(),
    )
    .unwrap();
    lines
}

fn summary(arm: Arm, rows: &[Value]) -> Value {
    let mine: Vec<&Value> = rows.iter().filter(|r| r["arm"] == arm.name()).collect();
    let correct = |g: Group| {
        mine.iter()
            .filter(|r| r["group"] == g.name() && r["correct"] == true)
            .count()
    };
    let sum = |field: &str| mine.iter().map(|r| r[field].as_u64().unwrap()).sum::<u64>();
    json!({
        "arm": arm.name(),
        "correct": {"answerable": correct(Group::Answerable), "combination": correct(Group::Combination), "insufficient": correct(Group::Insufficient)},
        "exact": mine.iter().filter(|r| r["exact"] == true).count(),
        "calls": sum("calls"), "egress_bytes": sum("egress_bytes"), "prompt_tokens": sum("prompt_tokens"),
        "completion_tokens": sum("completion_tokens"), "calls_without_usage": sum("calls_without_usage"), "wall_ms": sum("wall_ms"),
    })
}

#[test]
#[ignore = "asks the model RELIC_MODEL_* configures: real calls, time and money"]
fn against_the_configured_model() {
    let client = ChatCompletionsClient::from_env().unwrap_or_else(|e| panic!("{e}"));
    let configured = client.model().to_string();
    let model: Arc<dyn ModelBoundary> = Arc::new(client);
    let out = std::env::var_os("RELIC_INVESTIGATIONS_OUT").map_or_else(
        || Path::new(env!("CARGO_TARGET_TMPDIR")).join("investigations.jsonl"),
        PathBuf::from,
    );
    run(&bench(), &out, &configured, &|_| Arc::clone(&model));
    println!("results: {}", out.display());
}

/// How the scripted model plays one question: the terms its root searches,
/// the texts it takes for on topic, and its answer, or `None` to abstain
/// saying `missing`.
struct Script {
    hints: &'static [&'static str],
    cite: Vec<String>,
    answer: Option<String>,
    missing: Vec<&'static str>,
}

fn script(hints: &'static [&'static str], cite: &[&str], answer: Option<&str>) -> Script {
    let cite = cite.iter().map(ToString::to_string).collect();
    Script {
        hints,
        cite,
        answer: answer.map(String::from),
        missing: vec!["a record of how"],
    }
}

/// Each request the scripted server saw: its egress bytes, counted from
/// what arrived, and the prompt and completion tokens it reported.
type Served = Arc<Mutex<Vec<[u64; 3]>>>;

/// A Chat Completions server on loopback playing `scripts`, keyed by
/// question, and reporting usage.
fn serve(scripts: HashMap<&'static str, Script>) -> (String, Served) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let served = Served::default();
    let (seen, scripts) = (Arc::clone(&served), Arc::new(scripts));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { return };
            let (seen, scripts) = (Arc::clone(&seen), Arc::clone(&scripts));
            std::thread::spawn(move || answer(stream, &seen, &scripts));
        }
    });
    (url, served)
}

fn answer(mut stream: TcpStream, seen: &Served, scripts: &HashMap<&'static str, Script>) {
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
    let request: Value = serde_json::from_slice(&body).unwrap();
    let user: Value =
        serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
    let role = if user.get("packet").is_some() {
        Role::Child
    } else {
        Role::Root
    };
    let content = scripted(&user, scripts).to_string();
    let (prompt, completion) = (body.len() as u64 / 4, content.len() as u64 / 4);
    seen.lock().unwrap().push([
        egress::request_bytes(role, &user) as u64,
        prompt,
        completion,
    ]);
    let reply = json!({"model": "scripted", "choices": [{"message": {"content": content}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion}})
    .to_string();
    let head = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        reply.len()
    );
    let _ = stream.write_all(format!("{head}{reply}").as_bytes());
}

fn scripted(user: &Value, scripts: &HashMap<&'static str, Script>) -> Value {
    let on_topic = |script: &Script, item: &Value| {
        script
            .cite
            .iter()
            .any(|c| item["safe_text"].as_str().unwrap().contains(c.as_str()))
    };
    if let Some(packet) = user.get("packet") {
        // The pipeline's helpers: its objective is the question.
        let script = &scripts[user["objective"].as_str().unwrap()];
        let assessments: Vec<Value> = packet["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                let (verdict, claims) = match on_topic(script, i) {
                    true => ("supports", json!([{"text": "on topic", "supports": [i["evidence_id"]]}])),
                    false => ("irrelevant", json!([])),
                };
                json!({"item_id": i["item_id"], "verdict": verdict, "confidence": "high", "claims": claims})
            })
            .collect();
        return json!({"version": 1, "packet_id": packet["packet_id"], "assessments": assessments});
    }
    let script = &scripts[user["question"].as_str().unwrap()];
    let v = user["state_version"].as_u64().unwrap();
    let items = user["last_inspection"]["items"]
        .as_array()
        .into_iter()
        .flatten();
    let cited: Vec<&Value> = items
        .filter(|i| on_topic(script, i))
        .map(|i| &i["evidence_id"])
        .collect();
    // With an answer in mind and nothing on topic in front of it, it looks
    // again once, as a real model may: a baseline's budget stops it.
    if v == 0 || (v == 1 && cited.is_empty() && script.answer.is_some()) {
        return json!({"version": 1, "expected_state_version": v, "kind": "plan", "finish": null, "plan": {"ops": [
            {"op": "search", "op_id": format!("s{v}"), "terms": script.hints, "match": "any", "limit": 50},
            {"op": "inspect", "op_id": format!("i{v}"), "input": {"output": format!("s{v}")}, "limit": 50, "max_item_bytes": 2048},
        ]}});
    }
    let finish = match &script.answer {
        Some(answer) if !cited.is_empty() => json!({"status": "answered", "answer": answer,
            "claims": [{"text": answer, "supports": cited}], "procedure": [{"text": answer, "supports": [cited[0]]}]}),
        _ => json!({"status": "abstained", "answer": "", "claims": [], "missing": script.missing}),
    };
    json!({"version": 1, "expected_state_version": v, "kind": "finish", "plan": null, "finish": finish})
}

/// The scripts for the answerable store and the contradictory one. The
/// RLM's searches find each story; the baselines' come from the question.
/// Some answers are wrong on purpose, which the marks must catch; unless
/// `honest`, two insufficient ones are too.
fn scripts(bench: &Bench, honest: bool) -> [HashMap<&'static str, Script>; 2] {
    let q = |id: &str| bench.cases.iter().find(|c| c.id == id).unwrap().question;
    let short = |sha: &str| sha[..7].to_string();
    let a = &bench.stores[0];
    let (deleted, stash) = (short(&a.deleted), short(a.stash.as_deref().unwrap()));
    let (timeout, limiter, rejected) = (
        short(&bench.later.timeout),
        short(&bench.later.limiter),
        short(&bench.later.rejected),
    );
    let answerable = HashMap::from([
        // Also cites a site record it found: correct, not exact.
        (
            q("shop-lost-commit"),
            script(
                &["rescue", "spacing"],
                &["git branch rescue", "refs/heads/rescue", "header-spacing"],
                Some("Put a branch named rescue on HEAD@{1}."),
            ),
        ),
        (
            q("shop-deleted-branch"),
            script(
                &["coupons"],
                &[
                    "git branch -D feature/coupons",
                    &format!("git branch feature/coupons {deleted}"),
                ],
                Some(&format!("git branch feature/coupons {deleted}")),
            ),
        ),
        (
            q("notes-dropped-stash"),
            script(
                &["stash"],
                &["refs/stash ", "git stash drop"],
                Some(&format!("It was {stash}: git stash apply {stash}")),
            ),
        ),
        (
            q("billing-detached-commits"),
            script(
                &["v1", "wip"],
                &["git checkout v1.2", "wip-2 "],
                Some("They are on wip-2."),
            ),
        ),
        // Cites what it needs but says the wrong thing: not correct.
        (
            q("api-build-fix"),
            script(
                &["cargo"],
                &["cargo build\nexit", "cargo update"],
                Some("You ran cargo clean, then built again."),
            ),
        ),
        (
            q("infra-deleted-fix"),
            script(
                &["timeout", "pick"],
                &[
                    "git branch -D fix/timeout",
                    &format!("git cherry-pick {timeout}"),
                    "cherry-pick: raise",
                ],
                Some("Yes: you cherry-picked it onto main."),
            ),
        ),
        (
            q("api-lost-commits"),
            script(
                &["limiter", "rejected", "pick"],
                &[
                    "commit: add a rate limiter",
                    "commit: log rejected requests",
                    &format!("git cherry-pick {limiter}"),
                ],
                Some(&format!("Yes: git branch keep {rejected} holds both.")),
            ),
        ),
        // Right, but cites only the failed switch, not the deletion.
        (
            q("shop-switch-failed"),
            script(
                &["coupons"],
                &["git switch feature/coupons\nexit 128"],
                Some("Claude Code had deleted the branch."),
            ),
        ),
        (q("billing-discarded-edits"), script(&["tax"], &[], None)),
        // Answers a question about shop from notes' stash.
        (
            q("shop-dropped-stash"),
            script(
                &["stash"],
                &["refs/stash "],
                (!honest).then_some("git stash apply it from the reflog."),
            ),
        ),
        (q("api-conflict-output"), script(&["pick"], &[], None)),
    ]);
    // Abstains without saying what is missing.
    let rescue = Script {
        missing: if honest {
            vec!["a rescue that worked"]
        } else {
            vec![]
        },
        ..script(&["rescue"], &[], None)
    };
    let contradictory = HashMap::from([(q("shop-rescue-failed"), rescue)]);
    [answerable, contradictory]
}

fn scripted_client(url: &str) -> Arc<dyn ModelBoundary> {
    let vars = HashMap::from([
        ("RELIC_MODEL_PROFILE", "llama_cpp"),
        ("RELIC_MODEL_URL", url),
        ("RELIC_MODEL_NAME", "scripted"),
    ]);
    Arc::new(
        ChatCompletionsClient::new(
            ChatConfig::from_env(|k| vars.get(k).map(ToString::to_string)).unwrap(),
        )
        .unwrap(),
    )
}

/// The harness against a scripted model: the fixture keeps what each case
/// needs off its first FTS page, the marks are the ones the scripted answers
/// were written to earn, and the meters add up to what the server saw.
#[test]
fn the_seeded_investigations_are_scored_by_what_they_cite_and_metered_where_calls_leave() {
    let bench = bench();
    let dir = tempfile::tempdir().unwrap();
    let play = |honest: bool| {
        let [answerable, contradictory] = scripts(&bench, honest);
        let ((url_a, served_a), (url_b, served_b)) = (serve(answerable), serve(contradictory));
        let (a, b) = (scripted_client(&url_a), scripted_client(&url_b));
        let out = dir.path().join("investigations.jsonl");
        let lines = run(&bench, &out, "scripted", &|contradictory| {
            Arc::clone(if contradictory { &b } else { &a })
        });
        assert_eq!(
            std::fs::read_to_string(&out).unwrap(),
            lines.iter().map(|l| format!("{l}\n")).collect::<String>()
        );
        let served: Vec<[u64; 3]> = [served_a, served_b]
            .iter()
            .flat_map(|s| s.lock().unwrap().clone())
            .collect();
        (lines, served)
    };

    let (lines, served) = play(false);
    let header = &lines[0];
    assert!(
        header["relic"]["version"]
            .as_str()
            .unwrap()
            .starts_with(concat!("relic ", env!("CARGO_PKG_VERSION"), " (")),
        "{header}"
    );
    assert_eq!(
        header["relic"]["commit"].as_str().unwrap().len(),
        40,
        "{header}"
    );
    assert_eq!(
        (&header["model"]["configured"], &header["model"]["returned"]),
        (&json!("scripted"), &json!(["scripted"]))
    );
    let rows: Vec<&Value> = lines.iter().filter(|l| l.get("case").is_some()).collect();
    assert_eq!(rows.len(), 12 * ARMS.len());
    let row = |case: &str, arm: Arm| {
        *rows
            .iter()
            .find(|r| r["case"] == case && r["arm"] == arm.name())
            .unwrap()
    };

    let marks = [
        ("shop-lost-commit", true, false),
        ("shop-deleted-branch", true, true),
        ("notes-dropped-stash", true, true),
        ("billing-detached-commits", true, true),
        ("api-build-fix", false, true),
        ("infra-deleted-fix", true, true),
        ("api-lost-commits", true, true),
        ("shop-switch-failed", false, false),
        ("shop-rescue-failed", false, true),
        ("billing-discarded-edits", true, true),
        ("shop-dropped-stash", false, false),
        ("api-conflict-output", true, true),
    ];
    for (case, correct, exact) in marks {
        let r = row(case, Arm::Rlm);
        assert_eq!(
            (r["correct"].as_bool(), r["exact"].as_bool()),
            (Some(correct), Some(exact)),
            "{r}"
        );
    }
    for case in &bench.cases {
        // The first page has none of what a case needs, and FTS-only
        // abstains only on an empty one.
        let fts = row(case.id, Arm::FtsOnly);
        assert_eq!(
            (&fts["calls"], &fts["egress_bytes"], &fts["correct"]),
            (&json!(0), &json!(0), &json!(false)),
            "{fts}"
        );
        let top = row(case.id, Arm::Top20);
        // One call, though the model would have looked again.
        assert_eq!(top["calls"], 1, "{top}");
        assert!(
            case.group == Group::Insufficient || top["correct"] == false,
            "{top}"
        );
        let pipeline = row(case.id, Arm::Pipeline);
        assert_eq!(
            pipeline["calls"].as_u64(),
            Some(pipeline["child_calls"].as_u64().unwrap() + 1),
            "{pipeline}"
        );
    }
    // Every request the server saw, and only those, in the meters.
    let total = |field: &str| rows.iter().map(|r| r[field].as_u64().unwrap()).sum::<u64>();
    assert_eq!(
        (total("calls"), total("calls_without_usage")),
        (served.len() as u64, 0)
    );
    let seen = [0, 1, 2].map(|i| served.iter().map(|s| s[i]).sum::<u64>());
    assert_eq!(
        [
            total("egress_bytes"),
            total("prompt_tokens"),
            total("completion_tokens")
        ],
        seen
    );

    let summary = |lines: &[Value], arm: Arm| {
        lines
            .iter()
            .find(|l| l.get("case").is_none() && l["arm"] == arm.name())
            .unwrap()
            .clone()
    };
    assert_eq!(
        summary(&lines, Arm::Rlm)["correct"],
        json!({"answerable": 4, "combination": 2, "insufficient": 2})
    );
    // It claimed success on an insufficient case.
    assert_eq!(
        lines.last().unwrap(),
        &json!({"verdict": {"bounded_rlm_beats_baselines": false}})
    );
    let (lines, _) = play(true);
    assert_eq!(summary(&lines, Arm::Rlm)["correct"]["insufficient"], 4);
    assert_eq!(
        lines.last().unwrap(),
        &json!({"verdict": {"bounded_rlm_beats_baselines": true}})
    );
}
