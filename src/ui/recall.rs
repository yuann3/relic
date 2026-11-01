//! A recall on its own thread: [`app::recall`] over the configured model,
//! its progress turned into the steps and counters the Recall pane shows.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::SyncSender;
use std::thread::JoinHandle;
use std::time::Instant;

use super::model::{RecallEvent, UiAction};
use crate::app;
use crate::rlm::coordinator::Progress;
use crate::rlm::model::ModelBoundary;
use crate::store::Store;

pub fn spawn(
    op: u64,
    question: String,
    store: Store,
    model: Arc<dyn ModelBoundary>,
    cancel: Arc<AtomicBool>,
    tx: SyncSender<UiAction>,
) -> std::io::Result<JoinHandle<()>> {
    let builder = std::thread::Builder::new().name(format!("relic-recall-{op}"));
    crate::signal::spawn(builder, move || run(op, &question, &store, model, &cancel, &tx))
}

fn run(op: u64, question: &str, store: &Store, model: Arc<dyn ModelBoundary>, cancel: &Arc<AtomicBool>, tx: &SyncSender<UiAction>) {
    // Fails only once the workspace is gone, which has cancelled the recall.
    let send = |event| drop(tx.send(UiAction::Recall { op, event }));
    send(RecallEvent::Started { at: Instant::now() });
    let (mut root, mut child, mut op_open) = (0, 0, false);
    let mut progress = |p: Progress| match &p {
        Progress::RootCall { .. } | Progress::Op { .. } => {
            // An operation has no event of its own that ends it: the next
            // step does.
            if std::mem::take(&mut op_open) {
                send(RecallEvent::StepDone(String::new()));
            }
            if matches!(p, Progress::RootCall { .. }) {
                root += 1;
                send(RecallEvent::Calls { root, child });
            } else {
                op_open = true;
            }
            send(RecallEvent::StepStarted(p.to_string()));
        }
        Progress::ChildCall { .. } => {
            child += 1;
            send(RecallEvent::Calls { root, child });
            send(RecallEvent::StepStarted(p.to_string()));
        }
        Progress::Retry { .. } => send(RecallEvent::StepStarted(p.to_string())),
        Progress::Replied { .. } => send(RecallEvent::StepDone(p.to_string())),
        // Said after the operation it is about.
        Progress::Note(_) => {
            op_open = false;
            send(RecallEvent::StepDone(p.to_string()));
        }
        // The CLI's payload audit; the confirmation already named where
        // requests go.
        Progress::Egress(_) => {}
    };
    let report = app::recall(question, store, model, cancel, &mut progress);
    send(RecallEvent::Finished(report));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::RecallOutcome;
    use crate::rlm::model::FakeModel;
    use crate::ui::model::{Key, RecallStatus, StepState, UiEffect, UiModel, update};

    #[test]
    fn a_recall_drives_the_pane_to_a_report_citing_the_record() {
        let mut store = Store::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zsh_history");
        std::fs::write(&path, ": 1700000000:0;cd /etc/nginx\n: 1700000001:0;certbot renew\n").unwrap();
        crate::import::import_file(&mut store, &path, crate::import::SourceKind::Zsh, &crate::redact::Redactor::new()).unwrap();
        let renew = store.search("certbot", 10).unwrap()[0].id;

        let model = Arc::new(FakeModel::new());
        model.add_root_raw(
            serde_json::json!({"version": 1, "expected_state_version": 0, "kind": "plan", "finish": null, "plan": {"ops": [
                {"op": "search", "op_id": "s1", "terms": ["certbot"], "match": "any", "limit": 5},
                {"op": "inspect", "op_id": "i1", "input": {"output": "s1"}, "limit": 5, "max_item_bytes": 512},
            ]}})
            .to_string(),
        );
        model.add_root_fn(|state| {
            let cite = state["last_inspection"]["items"][0]["evidence_id"].clone();
            serde_json::json!({"version": 1, "expected_state_version": 1, "kind": "finish", "plan": null, "finish": {
                "status": "answered", "answer": "certbot", "claims": [{"text": "it ran", "supports": [cite]}]}})
            .to_string()
        });

        let mut m = UiModel::new(false);
        m.recall_model = Ok("fake: (fake)".into());
        m.query = "how did I renew".into();
        update(&mut m, UiAction::Key(Key::Ctrl('r')));
        let e = update(&mut m, UiAction::Key(Key::Char('y')));
        let [UiEffect::StartRecall { op, ref question }] = e[..] else { panic!("{e:?}") };

        let (tx, rx) = std::sync::mpsc::sync_channel(64);
        run(op, question, &store, model, &Arc::default(), &tx);
        drop(tx);
        for a in rx.iter() {
            update(&mut m, a);
        }
        let r = &m.recall;
        assert_eq!(r.status, RecallStatus::Finished);
        assert_eq!((r.root_calls, r.child_calls), (2, 0));
        let labels: Vec<&str> = r.steps.iter().map(|s| s.label.as_str()).collect();
        assert_eq!(labels, ["root call 1: plan the next step", "run s1", "run i1", "root call 2: plan the next step"]);
        assert!(r.steps.iter().all(|s| s.state == StepState::Done), "{:?}", r.steps);
        let outcomes: Vec<Option<&str>> = r.steps.iter().map(|s| s.outcome.as_deref()).collect();
        assert_eq!(outcomes, [Some("root reply from fake"), None, None, Some("root reply from fake")]);
        let report = r.report.as_ref().unwrap();
        assert_eq!((report.outcome, report.claims[0].supports.as_slice()), (RecallOutcome::Answered, &[renew][..]));
        m.focus = crate::ui::model::Focus::Recall;
        let e = update(&mut m, UiAction::Key(Key::Enter));
        assert!(matches!(e[..], [UiEffect::OpenEvidence { id }] if id == renew), "{e:?}");
    }
}
