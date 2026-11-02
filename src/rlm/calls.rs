//! Admitting model calls: every fence, the shared retry rule, and up to
//! `max_concurrency` children at once (ADR 0010).
//!
//! Each call runs on its own thread, so a stop returns at once instead of
//! waiting on a blocking socket: a call still in flight finishes into a
//! closed channel, and nothing it returns is committed. The CLI then exits
//! and the OS closes the socket (research, cancellation 6).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::redact::Redactor;
use crate::rlm::budget::{BudgetExhausted, BudgetLedger, Reservation};
use crate::rlm::coordinator::Progress;
use crate::rlm::model::egress::{EgressError, EgressRequest, Role};
use crate::rlm::model::{ModelBoundary, ModelError, Prices, Reply};

/// The longest `Retry-After` a 429 may ask for and still get its one retry.
const RETRY_AFTER_MAX: Duration = Duration::from_secs(10);
/// How often a wait for replies looks at the cancel flag.
const POLL: Duration = Duration::from_millis(10);

/// One logical call to admit.
pub struct Job {
    pub role: Role,
    pub payload: Value,
    /// Emitted when the call is admitted.
    pub admitted: Progress,
}

/// Each job's reply, in job order, or why it has none.
pub type Replies = Vec<Result<Reply, String>>;

/// Why admission stopped for good.
#[derive(Debug, PartialEq)]
pub enum Stop {
    Cancelled,
    Budget(BudgetExhausted),
}

/// What admitting calls needs from an investigation.
pub struct Calls<'r> {
    pub model: &'r Arc<dyn ModelBoundary>,
    pub ledger: &'r mut BudgetLedger,
    pub cancel: &'r Arc<AtomicBool>,
    pub progress: &'r mut dyn FnMut(Progress),
    pub deadline: Instant,
    pub concurrency: usize,
    /// Child calls admitted so far.
    pub child_calls: &'r mut u16,
}

/// A call in flight: what to resend on a retry and what to give back.
struct Flight {
    req: EgressRequest,
    attempt: u8,
    tokens: u64,
    cost: u64,
}

/// Every job's reply, in job order, or why it has none. Stops at once on
/// cancel, budget exhaustion or the wall-clock deadline.
pub fn call_all(c: &mut Calls, jobs: Vec<Job>) -> Result<Replies, Stop> {
    let mut results: Vec<Option<Result<Reply, String>>> = jobs.iter().map(|_| None).collect();
    let mut flights: Vec<Option<Flight>> = jobs.iter().map(|_| None).collect();
    let (tx, rx) = mpsc::channel::<(usize, Result<Reply, ModelError>)>();
    let mut pending = jobs.into_iter().enumerate();
    let mut in_flight = 0;
    loop {
        while in_flight < c.concurrency.max(1) {
            let Some((i, job)) = pending.next() else { break };
            match admit(c, job)? {
                // A stop raised while its audit was emitted holds it back.
                Ok(_) if c.cancel.load(Ordering::Relaxed) => return Err(Stop::Cancelled),
                Ok(flight) => {
                    match send(c, &tx, i, &flight.req, Duration::ZERO) {
                        Ok(()) => in_flight += 1,
                        Err(e) => results[i] = Some(Err(e)),
                    }
                    flights[i] = Some(flight);
                }
                Err(e) => results[i] = Some(Err(e)),
            }
        }
        if in_flight == 0 {
            break;
        }
        let got = rx.recv_timeout(POLL);
        // A reply that lands after cancel is dropped, never committed.
        if c.cancel.load(Ordering::Relaxed) {
            return Err(Stop::Cancelled);
        }
        let Ok((i, outcome)) = got else { continue };
        in_flight -= 1;
        let flight = flights[i].as_mut().expect("a reply is for a call that was sent");
        match outcome {
            Ok(reply) => {
                release_unspent(c, flight, &reply);
                // Provider text: redacted once, here, for every sink after.
                let returned_model = reply.returned_model.as_deref().map(|m| Redactor::new().redact(m));
                (c.progress)(Progress::Replied { role: flight.req.role(), returned_model });
                results[i] = Some(Ok(reply));
            }
            Err(e) => match retry(c, flight, &e) {
                Some(delay) => match send(c, &tx, i, &flight.req, delay) {
                    Ok(()) => in_flight += 1,
                    Err(e) => results[i] = Some(Err(e)),
                },
                None => results[i] = Some(Err(e.to_string())),
            },
        }
    }
    Ok(results.into_iter().map(|r| r.expect("every job ends with a result")).collect())
}

/// Reserve everything one logical call can spend, before anything leaves.
fn admit(c: &mut Calls, job: Job) -> Result<Result<Flight, String>, Stop> {
    if c.cancel.load(Ordering::Relaxed) {
        return Err(Stop::Cancelled);
    }
    if Instant::now() >= c.deadline {
        return Err(Stop::Budget(BudgetExhausted::WallClock));
    }
    let logical = match job.role {
        Role::Root => Reservation { root_calls: 1, ..Default::default() },
        Role::Child => Reservation { child_calls: 1, ..Default::default() },
    };
    c.ledger.reserve(&logical).map_err(Stop::Budget)?;
    let target = c.model.target();
    let mut audit = None;
    let req = match EgressRequest::new(job.role, job.payload, &target, c.ledger, &mut |a| audit = Some(a)) {
        Ok(req) => req,
        Err(EgressError::Budget(b)) => return Err(Stop::Budget(b)),
        Err(e) => return Ok(Err(e.to_string())),
    };
    let tokens = u64::from(job.role.max_output_tokens());
    // No tokenizer: a token is at least a byte, so bytes bound the input.
    let cost = c.model.prices().map_or(0, |p| p.cost(req.bytes() as u64, tokens));
    c.ledger
        .reserve(&Reservation { provider_attempts: 1, output_tokens: tokens, cost_microusd: cost, ..Default::default() })
        .map_err(Stop::Budget)?;
    if job.role == Role::Child {
        *c.child_calls += 1;
    }
    (c.progress)(job.admitted);
    // After the admission it belongs to, and only for a request every fence
    // let through: the audit says what is about to leave.
    if let Some(audit) = audit {
        (c.progress)(Progress::Egress(audit));
    }
    Ok(Ok(Flight { req, attempt: 1, tokens, cost }))
}

/// The one retry the rule allows, if this failure earns it and the budget
/// and deadline still have room. Every retry leaves a trace.
fn retry(c: &mut Calls, flight: &mut Flight, e: &ModelError) -> Option<Duration> {
    let delay = e.retry_after(RETRY_AFTER_MAX).filter(|_| flight.attempt == 1)?;
    if Instant::now() + delay >= c.deadline || c.ledger.reserve(&Reservation { provider_attempts: 1, ..Default::default() }).is_err() {
        return None;
    }
    flight.attempt += 1;
    (c.progress)(Progress::Retry { role: flight.req.role(), attempt: flight.attempt, reason: e.to_string() });
    Some(delay)
}

fn send(
    c: &Calls,
    tx: &mpsc::Sender<(usize, Result<Reply, ModelError>)>,
    i: usize,
    req: &EgressRequest,
    delay: Duration,
) -> Result<(), String> {
    let (model, req, tx, deadline, cancel) = (Arc::clone(c.model), req.clone(), tx.clone(), c.deadline, Arc::clone(c.cancel));
    let builder = std::thread::Builder::new().name("relic-model".into());
    crate::signal::spawn(builder, move || {
        std::thread::sleep(delay);
        // A stop while waiting out a Retry-After: `call_all` has returned,
        // and nothing may leave after it.
        if !cancel.load(Ordering::Relaxed) {
            let _ = tx.send((i, model.call(&req, deadline)));
        }
    })
    .map(drop)
    .map_err(|e| format!("could not start a model call: {e}"))
}

/// Usage the provider reports is authoritative; without it the whole
/// reservation stays spent (research, budget and cost lease).
fn release_unspent(c: &mut Calls, flight: &Flight, reply: &Reply) {
    let Some(usage) = reply.usage else { return };
    let tokens = flight.tokens.saturating_sub(usage.completion_tokens);
    let spent = c.model.prices().map_or(0, |p: Prices| p.cost(usage.prompt_tokens, usage.completion_tokens));
    c.ledger.release_output(tokens, flight.cost.saturating_sub(spent));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rlm::model::egress::Target;
    use crate::rlm::model::Usage;
    use crate::rlm::protocol::BudgetV1;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;

    /// Answers each call with the next scripted outcome, after `delay`;
    /// counts calls and the most in flight at once.
    #[derive(Default)]
    struct Script {
        outcomes: Mutex<Vec<Result<Reply, ModelError>>>,
        delay: Duration,
        prices: Option<Prices>,
        calls: AtomicUsize,
        live: AtomicUsize,
        most_live: AtomicUsize,
    }

    impl ModelBoundary for Script {
        fn target(&self) -> Target {
            Target { profile: "script".into(), origin: "script:".into() }
        }

        fn call(&self, req: &EgressRequest, _deadline: Instant) -> Result<Reply, ModelError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let live = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_live.fetch_max(live, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            self.live.fetch_sub(1, Ordering::SeqCst);
            let mut outcomes = self.outcomes.lock().unwrap();
            if outcomes.is_empty() { Ok(Reply { body: req.user().to_string(), ..Reply::default() }) } else { outcomes.remove(0) }
        }

        fn prices(&self) -> Option<Prices> {
            self.prices
        }
    }

    fn script(outcomes: Vec<Result<Reply, ModelError>>) -> Arc<Script> {
        Arc::new(Script { outcomes: Mutex::new(outcomes), ..Script::default() })
    }

    fn jobs(n: usize) -> Vec<Job> {
        (0..n)
            .map(|i| Job { role: Role::Child, payload: serde_json::json!({"n": i}), admitted: Progress::Note(format!("job {i}")) })
            .collect()
    }

    /// `call_all` over `jobs` with `budget`; the outcome and every event.
    fn run(
        model: Arc<Script>,
        budget: BudgetV1,
        jobs: Vec<Job>,
        cancel: &Arc<AtomicBool>,
    ) -> (Result<Replies, Stop>, Vec<Progress>, BudgetLedger, u16) {
        let model: Arc<dyn ModelBoundary> = model;
        let mut ledger = BudgetLedger::new(budget.clone());
        let mut events = Vec::new();
        let mut child_calls = 0;
        let mut c = Calls {
            model: &model,
            ledger: &mut ledger,
            cancel,
            progress: &mut |p| events.push(p),
            deadline: Instant::now() + Duration::from_millis(budget.wall_ms),
            concurrency: usize::from(budget.max_concurrency),
            child_calls: &mut child_calls,
        };
        let out = call_all(&mut c, jobs);
        (out, events, ledger, child_calls)
    }

    fn retries(events: &[Progress]) -> usize {
        events.iter().filter(|e| matches!(e, Progress::Retry { .. })).count()
    }

    #[test]
    fn each_admission_comes_before_its_audit_and_a_fenced_request_has_none() {
        let ok = || Ok(Reply { body: "ok".into(), ..Reply::default() });
        let label = |e: &Progress| match e {
            Progress::Note(n) => Some(n.clone()),
            Progress::Egress(a) => Some(format!("send {}", a.bytes)),
            _ => None,
        };
        let budget = BudgetV1 { max_concurrency: 1, ..BudgetV1::prototype_default() };
        let (out, events, _, _) = run(script(vec![ok(), ok()]), budget.clone(), jobs(2), &Arc::default());
        assert!(out.is_ok());
        let seen: Vec<String> = events.iter().filter_map(label).collect();
        let send = seen[1].clone();
        assert!(send.starts_with("send "), "{seen:?}");
        assert_eq!(seen, ["job 0".to_string(), send.clone(), "job 1".into(), send]);

        // Out of output tokens after the bytes were counted: nothing leaves,
        // so nothing is audited as leaving.
        let budget = BudgetV1 { reserved_output_tokens: 0, ..budget };
        let (out, events, _, _) = run(script(vec![ok()]), budget, jobs(1), &Arc::default());
        assert!(out.is_err());
        assert!(events.iter().all(|e| label(e).is_none()), "{events:?}");
    }

    #[test]
    fn only_a_failure_before_send_or_a_bounded_429_is_retried_and_only_once() {
        let ok = || Ok(Reply { body: "ok".into(), ..Reply::default() });
        let soon = ModelError::RateLimited { retry_after: Some(Duration::from_millis(5)) };
        let cases = [
            (vec![Err(ModelError::BeforeSend("refused".into())), ok()], true, 1),
            (vec![Err(soon.clone()), ok()], true, 1),
            // Asked to wait past the bound, or not told how long.
            (vec![Err(ModelError::RateLimited { retry_after: Some(Duration::from_secs(60)) })], false, 0),
            (vec![Err(ModelError::RateLimited { retry_after: None })], false, 0),
            // It may have run: never sent twice.
            (vec![Err(ModelError::Status { code: 500, message: None })], false, 0),
            (vec![Err(ModelError::Status { code: 529, message: None })], false, 0),
            (vec![Err(ModelError::Ambiguous("reset".into()))], false, 0),
            (vec![Err(ModelError::Truncated)], false, 0),
            // A retry that fails too is the end of it.
            (vec![Err(soon.clone()), Err(soon)], false, 1),
        ];
        for (outcomes, succeeds, expected_retries) in cases {
            let label = format!("{outcomes:?}");
            let model = script(outcomes);
            let (out, events, ledger, _) = run(model.clone(), BudgetV1::prototype_default(), jobs(1), &Arc::default());
            let reply = out.unwrap().remove(0);
            assert_eq!(reply.is_ok(), succeeds, "{label}");
            assert_eq!(retries(&events), expected_retries, "{label}");
            let attempts = 1 + expected_retries;
            assert_eq!(model.calls.load(Ordering::SeqCst), attempts, "{label}");
            let spent = BudgetV1::prototype_default().provider_attempts - ledger.remaining().provider_attempts;
            assert_eq!(usize::from(spent), attempts, "{label}: every attempt is paid for");
        }
    }

    #[test]
    fn at_most_max_concurrency_children_are_in_flight_and_replies_keep_job_order() {
        let model = Arc::new(Script { delay: Duration::from_millis(30), ..Script::default() });
        let (out, _, _, _) = run(model.clone(), BudgetV1::prototype_default(), jobs(6), &Arc::default());
        assert_eq!(model.most_live.load(Ordering::SeqCst), 4);
        // The script echoes each request, so a reply filed under the wrong
        // job shows here.
        let bodies: Vec<String> = out.unwrap().into_iter().map(|r| r.unwrap().body).collect();
        let expected: Vec<String> = (0..6).map(|i| format!("{{\"n\":{i}}}")).collect();
        assert_eq!(bodies, expected);
    }

    #[test]
    fn cancel_returns_while_calls_are_still_blocked() {
        let model = Arc::new(Script { delay: Duration::from_secs(5), ..Script::default() });
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            flag.store(true, Ordering::Relaxed);
        });
        let t = Instant::now();
        let (out, _, _, _) = run(model.clone(), BudgetV1::prototype_default(), jobs(4), &cancel);
        assert_eq!(out.unwrap_err(), Stop::Cancelled);
        assert!(t.elapsed() < Duration::from_millis(250), "{:?}", t.elapsed());
        // 50 ms is long enough for four threads to reach the model.
        assert_eq!(model.calls.load(Ordering::SeqCst), 4, "all four were in flight");
    }

    #[test]
    fn a_retry_waiting_out_its_retry_after_is_never_sent_once_cancelled() {
        let model = script(vec![Err(ModelError::RateLimited { retry_after: Some(Duration::from_millis(300)) })]);
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cancel);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            flag.store(true, Ordering::Relaxed);
        });
        let (out, _, _, _) = run(model.clone(), BudgetV1::prototype_default(), jobs(1), &cancel);
        assert_eq!(out.unwrap_err(), Stop::Cancelled);
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(model.calls.load(Ordering::SeqCst), 1, "the retry stayed home");
    }

    #[test]
    fn every_fence_stops_admission() {
        let fence = |edit: &dyn Fn(&mut BudgetV1), prices: Option<Prices>| {
            let mut budget = BudgetV1::prototype_default();
            edit(&mut budget);
            let model = Arc::new(Script { prices, ..Script::default() });
            let (out, _, _, admitted) = run(model, budget, jobs(3), &Arc::default());
            (out.unwrap_err(), admitted)
        };
        use BudgetExhausted::*;
        // Two calls fit and are admitted; the third never is.
        assert_eq!(fence(&|b| b.child_calls = 2, None), (Stop::Budget(ChildCalls), 2));
        assert_eq!(fence(&|b| b.provider_attempts = 2, None), (Stop::Budget(ProviderAttempts), 2));
        assert_eq!(fence(&|b| b.reserved_output_tokens = 768 * 2, None), (Stop::Budget(OutputTokens), 2));
        assert_eq!(fence(&|b| b.wall_ms = 0, None), (Stop::Budget(WallClock), 0));
        // At $1 per million input tokens, a request of about 2 KB reserves
        // about 2000 micro-dollars: one fits in 3000, two do not.
        let dollar = Some(Prices { input_per_mtok: 1_000_000, output_per_mtok: 0 });
        assert_eq!(fence(&|b| b.max_cost_microusd = Some(3000), dollar), (Stop::Budget(Cost), 1));
        // Without prices the cost is unknown, not fenced.
        let (out, _, _, _) = run(script(vec![]), BudgetV1 { max_cost_microusd: Some(0), ..BudgetV1::prototype_default() }, jobs(3), &Arc::default());
        assert!(out.is_ok());
    }

    #[test]
    fn reported_usage_gives_back_what_a_call_did_not_spend() {
        let usage = |completion| Ok(Reply { usage: Some(Usage { prompt_tokens: 10, completion_tokens: completion }), ..Reply::default() });
        let (_, _, ledger, _) = run(script(vec![usage(100), Ok(Reply::default())]), BudgetV1::prototype_default(), jobs(2), &Arc::default());
        // 768 reserved each; the first reported 100, the second nothing.
        assert_eq!(BudgetV1::prototype_default().reserved_output_tokens - ledger.remaining().reserved_output_tokens, 100 + 768);
    }
}
