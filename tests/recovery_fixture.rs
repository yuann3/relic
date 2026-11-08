//! The Git-recovery acceptance fixture (ADR 0016, R1) holds what the task
//! needs in every leg, the same on every build, and a verifier tells its
//! answerable variant from its contradictory one.

mod recovery;

use std::path::Path;
use std::process::Command;

use recovery::{NOISE, Recovery, Variant};

const ZSH: i64 = 1;
const HOOK: i64 = 3;
const REFLOG: i64 = 5;
const REF: i64 = 6;
const AGENT_COMMAND: i64 = 7;
const AGENT_OUTPUT: i64 = 8;

/// Every stored record, in id order: kind, time and text.
fn rows(db: &Path) -> Vec<(i64, Option<i64>, String)> {
    let conn = rusqlite::Connection::open(db).unwrap();
    let mut stmt = conn.prepare("SELECT kind, observed_at_ms, safe_text FROM observation ORDER BY id").unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap().map(Result::unwrap).collect()
}

fn texts(rows: &[(i64, Option<i64>, String)], kind: i64) -> Vec<&str> {
    rows.iter().filter(|r| r.0 == kind).map(|r| r.2.as_str()).collect()
}

/// `git rev-parse --verify -q <rev>` in `repo`: what it names, if anything.
fn verify(repo: &Path, rev: &str) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(repo).args(["rev-parse", "--verify", "-q", rev]).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
    out.status.success().then(|| String::from_utf8(out.stdout).unwrap().trim().to_string())
}

#[test]
fn the_same_seed_builds_the_same_store_and_another_seed_other_noise() {
    // A ref's time is when relic imported it: the one field that moves.
    let stable = |r: &Recovery| -> Vec<_> { rows(&r.db).into_iter().map(|(k, t, s)| (k, if k == REF { None } else { t }, s)).collect() };
    let (a, b, c) = (Recovery::build(Variant::Answerable, 7), Recovery::build(Variant::Answerable, 7), Recovery::build(Variant::Answerable, 8));
    assert_eq!((&a.lost, &a.deleted), (&b.lost, &b.deleted));
    // With the seeded investigations' legs too.
    let (x, y) = (a.import_investigations(), b.import_investigations());
    assert_eq!([x.tax, x.limiter, x.rejected, x.timeout, x.picked], [y.tax, y.limiter, y.rejected, y.timeout, y.picked]);
    c.import_investigations();
    assert_eq!(stable(&a), stable(&b));
    let (a, c) = (stable(&a), stable(&c));
    assert_ne!(texts(&a, ZSH)[..NOISE], texts(&c, ZSH)[..NOISE]);
    assert_eq!(a[NOISE..], c[NOISE..], "the story does not depend on the noise");
}

#[test]
fn every_leg_holds_what_the_recovery_needs() {
    let r = Recovery::build(Variant::Answerable, 7);
    let rows = rows(&r.db);
    let short = &r.deleted[..7];
    assert_eq!(texts(&rows, ZSH).len(), NOISE + 11);
    assert!(texts(&rows, ZSH).contains(&"git branch rescue HEAD@{1}"));
    let hooks = texts(&rows, HOOK);
    assert!(hooks.contains(&"git reset --hard HEAD~1\nexit 0 · took 0.5 s · cwd: /home/dev/src/shop"), "{hooks:#?}");
    assert!(hooks.contains(&"git branch rescue HEAD@{1}\nexit 0 · took 0.5 s · cwd: /home/dev/src/shop"), "{hooks:#?}");
    assert!(hooks.contains(&"git switch feature/coupons\nexit 128 · took 0.5 s · cwd: /home/dev/src/shop"), "{hooks:#?}");
    let reflog = texts(&rows, REFLOG);
    assert!(reflog.contains(&format!("git reflog refs/heads/main {} commit: WIP: round prices to the cent", r.lost).as_str()), "{reflog:#?}");
    assert!(reflog.contains(&format!("git reflog HEAD {} commit: coupons: apply discount codes", r.deleted).as_str()), "{reflog:#?}");
    assert!(texts(&rows, REF).contains(&format!("git ref refs/heads/rescue {} WIP: round prices to the cent", r.lost).as_str()));
    // What the agent ran and what it printed, never the prompt.
    assert_eq!(texts(&rows, AGENT_COMMAND), ["git branch --merged main", "git branch -D feature/coupons"]);
    assert_eq!(texts(&rows, AGENT_OUTPUT), ["* main".to_string(), format!("Deleted branch feature/coupons (was {short}).")]);
    assert!(!rows.iter().any(|r| r.2.contains("tidy up")));
    assert_eq!(verify(&r.shop, "rescue").as_deref(), Some(r.lost.as_str()));
    assert_eq!(verify(&r.shop, "feature/coupons").as_deref(), Some(r.deleted.as_str()));
}

#[test]
fn the_contradictory_variant_recorded_the_rescue_and_its_failure_and_has_no_rescue() {
    let r = Recovery::build(Variant::Contradictory, 7);
    let rows = rows(&r.db);
    assert!(texts(&rows, ZSH).contains(&"git branch rescue HEAD@{1}"));
    assert!(texts(&rows, HOOK).contains(&"git branch rescue HEAD@{1}\nexit 128 · took 0.5 s · cwd: /home/dev"));
    assert!(!rows.iter().any(|r| r.0 >= REFLOG && r.0 <= REF && r.2.contains("rescue")));
    assert_eq!(verify(&r.shop, "rescue"), None);
}

#[test]
fn the_sibling_is_held_out_until_it_is_imported_and_its_dropped_stash_stays_citable() {
    let mut r = Recovery::build(Variant::Answerable, 7);
    let mentions = |r: &Recovery| rows(&r.db).into_iter().filter(|r| r.2.contains("stash") && r.2.contains("release post")).count();
    assert_eq!(mentions(&r), 0);
    r.import_sibling();
    let stash = r.stash.clone().unwrap();
    let rows = rows(&r.db);
    assert!(texts(&rows, REFLOG).contains(&format!("git reflog refs/stash {stash} On main: release post draft").as_str()), "{rows:#?}");
    assert!(texts(&rows, HOOK).contains(&"git stash drop\nexit 0 · took 0.5 s · cwd: /home/dev/src/notes"));
    // Git dropped the stash; its commit is still there for the verifier.
    assert_eq!(verify(&r.notes, "refs/stash"), None);
    assert_eq!(verify(&r.notes, &format!("{stash}^{{commit}}")).as_deref(), Some(stash.as_str()));
}
