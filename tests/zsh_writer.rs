//! A history written by the real zsh, metafication included, imports as the
//! text the user typed.

use std::process::{Command, Stdio};

use relic::import::{SourceKind, import_file};
use relic::redact::Redactor;
use relic::store::Store;

#[test]
fn cjk_history_written_by_real_zsh_is_searchable() {
    let dir = tempfile::tempdir().unwrap();
    let hist = dir.path().join("zsh_history");
    // -i: a non-interactive zsh keeps no history, so `fc -W` writes nothing.
    let written = Command::new("zsh")
        .args(["-f", "-i", "-c"])
        .arg(r#"HISTFILE=$1; HISTSIZE=100; SAVEHIST=100; setopt extended_history; print -s "echo 部署完成"; print -s "git tag 日本語"; fc -W"#)
        .arg("zsh")
        .arg(&hist)
        .stdin(Stdio::null())
        .output();
    let written = match written {
        Ok(out) => out,
        Err(e) => {
            eprintln!("SKIPPED cjk_history_written_by_real_zsh_is_searchable: cannot run zsh: {e}");
            return;
        }
    };
    assert!(written.status.success(), "{}", String::from_utf8_lossy(&written.stderr));
    let bytes = std::fs::read(&hist).unwrap();
    // U+90E8 is E9 83 A8; zsh metafies its 0x83. If this zsh writes plain
    // UTF-8 instead, unmetafying it would corrupt the text: fail loudly.
    assert!(
        bytes.windows(4).any(|w| w == [0xE9, 0x83, 0xA3, 0xA8]),
        "zsh did not metafy its history: {bytes:02X?}"
    );

    let mut store = Store::open(&dir.path().join("relic.db")).unwrap();
    let r = import_file(&mut store, &hist, SourceKind::Zsh, &Redactor::new()).unwrap();
    assert_eq!((r.records_stored, r.lossy_records, r.dangling_meta), (2, 0, 0));

    let hits = store.search("部署完成", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].safe_text, "echo 部署完成");
    assert!(hits[0].observed_at_ms.is_some());
    assert_eq!(store.search("日本語", 10).unwrap()[0].safe_text, "git tag 日本語");
}
