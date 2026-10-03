//! Scripted stand-in for recall progress. The real coordinator (src/rlm) has
//! no progress or cancellation hooks yet, so this emits the typed events the
//! workspace will consume, on the same thread/channel/fencing path, without
//! calling any model.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::SyncSender;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::model::{RecallEvent, UiAction};

pub fn spawn(
    op: u64,
    query: String,
    cite: i64,
    cancel: Arc<AtomicBool>,
    tx: SyncSender<UiAction>,
) -> std::io::Result<JoinHandle<()>> {
    std::thread::Builder::new()
        .name(format!("relic-recall-{op}"))
        .spawn(move || script(op, &query, cite, &cancel, &tx))
}

fn script(op: u64, query: &str, cite: i64, cancel: &AtomicBool, tx: &SyncSender<UiAction>) {
    let send =
        |event| !cancel.load(Ordering::SeqCst) && tx.send(UiAction::Recall { op, event }).is_ok();
    // Sleeps in short slices so cancellation is noticed within ~25 ms.
    let wait = |ms: u64| {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            if cancel.load(Ordering::SeqCst) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        true
    };
    let q: String = query.chars().take(40).collect();
    let steps: [(String, u64, &str, u32, u32); 6] = [
        (
            "plan: turn the question into searches".into(),
            500,
            "2 searches planned",
            1,
            0,
        ),
        (
            format!("search: \"{q}\""),
            600,
            "1 page of indexed hits",
            1,
            1,
        ),
        ("assess batch 1/3".into(), 700, "12 read, 3 kept", 1, 2),
        ("assess batch 2/3".into(), 700, "12 read, 1 kept", 2, 2),
        ("assess batch 3/3".into(), 700, "9 read, 0 kept", 2, 3),
        ("finish: cite evidence".into(), 500, "1 citation", 2, 3),
    ];
    if !send(RecallEvent::Started { at: Instant::now() }) {
        return;
    }
    for (label, ms, outcome, root, child) in steps {
        if !send(RecallEvent::StepStarted(label)) || !wait(ms) {
            return;
        }
        if !send(RecallEvent::Calls { root, child }) || !send(RecallEvent::StepDone(outcome.into()))
        {
            return;
        }
    }
    send(RecallEvent::Finished {
        claim: format!(
            "Scripted finding for \"{q}\": #{cite} is the closest indexed match. Fake text; no model was called."
        ),
        cites: vec![cite],
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::model::{RecallStatus, UiModel, update};

    fn collect(rx: &std::sync::mpsc::Receiver<UiAction>) -> Vec<RecallEvent> {
        rx.iter()
            .map(|a| match a {
                UiAction::Recall { event, .. } => event,
                other => panic!("{other:?}"),
            })
            .collect()
    }

    #[test]
    fn cancel_stops_the_script_promptly() {
        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        let cancel = Arc::new(AtomicBool::new(false));
        let h = spawn(1, "git".into(), 7, Arc::clone(&cancel), tx).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        let t = Instant::now();
        cancel.store(true, Ordering::SeqCst);
        h.join().unwrap();
        assert!(
            t.elapsed() < Duration::from_millis(200),
            "took {:?}",
            t.elapsed()
        );
        let events = collect(&rx);
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, RecallEvent::Finished { .. })),
            "{events:?}"
        );
    }

    #[test]
    fn full_script_drives_the_model_to_a_cited_finding() {
        use crate::app::{PageHit, SEARCH_SCHEMA, SearchPage};
        use crate::ui::model::{Key, UiEffect};

        let mut m = UiModel::new(false);
        update(&mut m, UiAction::Key(Key::Char('c')));
        let hit = PageHit {
            id: 42,
            observed_at_ms: None,
            source: "zsh",
            text: "cargo".into(),
        };
        let page = SearchPage {
            schema: SEARCH_SCHEMA,
            query: "c".into(),
            limit: 200,
            truncated: false,
            deferred: None,
            hits: vec![hit],
        };
        let generation = m.search.generation;
        update(
            &mut m,
            UiAction::SearchDone {
                generation,
                result: Ok(page),
                elapsed: Duration::ZERO,
            },
        );
        update(&mut m, UiAction::Key(Key::Ctrl('r')));
        let e = update(&mut m, UiAction::Key(Key::Char('y')));
        let [
            UiEffect::StartRecall {
                op,
                ref query,
                cite,
            },
        ] = e[..]
        else {
            panic!("{e:?}")
        };

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        script(op, query, cite, &AtomicBool::new(false), &tx);
        drop(tx);
        for a in rx.iter() {
            update(&mut m, a);
        }
        assert_eq!(m.recall.status, RecallStatus::Finished);
        assert_eq!((m.recall.root_calls, m.recall.child_calls), (2, 3));
        assert_eq!(m.recall.steps.len(), 6);
        assert_eq!(m.recall.finding.as_ref().unwrap().cites, [42]);
    }
}
