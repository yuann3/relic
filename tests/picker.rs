//! The fzf picker against fake fzf executables that misbehave on purpose.

mod fake_fzf;

use std::time::Duration;

use fake_fzf::{FakeFzf, page};
use relic::picker::{Pick, PickError, pick_with};

fn protocol_error(r: Result<Pick, PickError>) -> String {
    match r {
        Err(PickError::Protocol(m)) => m,
        other => panic!("expected a protocol error, got {other:?}"),
    }
}

#[test]
fn returns_the_selected_page_member() {
    let fzf =
        FakeFzf::new("cat > \"$D/stdin\"; printf '12\\t2023-11-14 22:13\\tzsh\\tanything\\0'");
    assert_eq!(pick_with(&page(&[11, 12, 13]), &fzf.path).unwrap(), Pick::Chosen(12));
    fzf.assert_reaped();
}

#[test]
fn sends_fixed_argv_and_never_the_query() {
    let fzf = FakeFzf::new("cat > \"$D/stdin\"; printf '11\\t\\0'");
    let mut p = page(&[11]);
    p.query = "deploy --prod $(rm -rf ~)".into();
    pick_with(&p, &fzf.path).unwrap();
    let argv = fzf.argv();
    assert_eq!(
        argv,
        [
            "--read0",
            "--print0",
            "--no-multi",
            "--delimiter=\t",
            "--with-nth=2..",
            "--nth=2..",
            "--height=80%",
            "--layout=reverse",
            "--border",
        ]
    );
    assert!(!argv.iter().any(|a| a.contains("deploy")));
}

#[test]
fn records_are_nul_framed_sanitized_and_capped() {
    let fzf = FakeFzf::new("cat > \"$D/stdin\"; exit 130");
    let mut p = page(&[1, 2, 3, 4]);
    p.hits[0].text = "a\tb\nc\rd\0e\x1b[31mf\u{7f}g\u{85}h\u{202e}i\u{2028}j".into();
    p.hits[1].observed_at_ms = None;
    p.hits[2].text = "€".repeat(1000); // 3 bytes each: the cap falls mid-char
    p.hits[3].text = "plain".into();
    assert_eq!(pick_with(&p, &fzf.path).unwrap(), Pick::Cancelled);

    let stdin = fzf.read("stdin");
    assert_eq!(stdin.last(), Some(&0));
    let records: Vec<&[u8]> = stdin[..stdin.len() - 1].split(|&b| b == 0).collect();
    assert_eq!(records.len(), 4);
    let fields = |r: &[u8]| -> Vec<String> {
        String::from_utf8(r.to_vec()).unwrap().split('\t').map(String::from).collect()
    };
    assert_eq!(fields(records[0]), ["1", "2023-11-14 22:13", "zsh", "a b c d?e?[31mf?g?h?i j"]);
    assert_eq!(fields(records[1])[1], "unknown time");
    assert!(records[2].len() < 2048, "record plus NUL fits 2 KiB: {}", records[2].len());
    assert!(records[2].len() > 2048 - 4, "cap wastes at most one partial char");
    assert!(fields(records[2])[3].chars().all(|c| c == '€'));
    assert_eq!(fields(records[3]), ["4", "2023-11-14 22:13", "zsh", "plain"]);
    assert!(records.iter().all(|r| !r.contains(&b'\n')));
}

#[test]
fn cancel_and_no_match_are_distinct_and_not_errors() {
    for (code, want) in [(130, Pick::Cancelled), (1, Pick::NoMatch)] {
        let fzf = FakeFzf::new(&format!("exit {code}"));
        assert_eq!(pick_with(&page(&[1]), &fzf.path).unwrap(), want, "exit {code}");
        fzf.assert_reaped();
    }
}

#[test]
fn fzf_failure_is_a_protocol_error() {
    let fzf = FakeFzf::new("printf '1\\t\\0'; exit 2");
    assert!(protocol_error(pick_with(&page(&[1]), &fzf.path)).contains("exit status: 2"));
    fzf.assert_reaped();
}

#[test]
fn rejects_malformed_selections() {
    let cases = [
        ("printf '99\\tx\\0'", "not on the page"),
        ("printf '1\\tx\\0'; printf '2\\tx\\0'", "more than one record"),
        ("printf '1\\tx'", "no NUL-terminated"),
        ("true", "no NUL-terminated"),
        ("printf 'abc\\tx\\0'", "numeric id"),
        ("printf '\\377\\tx\\0'", "numeric id"),
        ("printf ' 1\\tx\\0'", "numeric id"),
        ("printf '+1\\tx\\0'", "numeric id"),
        ("printf '01\\tx\\0'", "numeric id"),
    ];
    for (body, want) in cases {
        let fzf = FakeFzf::new(body);
        let msg = protocol_error(pick_with(&page(&[1, 2]), &fzf.path));
        assert!(msg.contains(want), "{body}: {msg}");
        fzf.assert_reaped();
    }
}

#[test]
fn early_exit_without_reading_stdin_still_selects() {
    // 5,000 hits of 400 bytes overflow any pipe buffer; fzf leaves first.
    let fzf = FakeFzf::new("printf '4999\\t\\0'");
    let mut p = page(&(0..5000).collect::<Vec<_>>());
    for h in &mut p.hits {
        h.text = "x".repeat(400);
    }
    assert_eq!(
        fake_fzf::within(Duration::from_secs(10), move || pick_with(&p, &fzf.path)).unwrap(),
        Pick::Chosen(4999)
    );
}

#[test]
fn oversize_output_is_cut_off_and_the_child_reaped() {
    let bodies = [
        // Never reads stdin and never stops writing: both pipes fill at once.
        "exec yes 1",
        // Overflows, then lingers; only a kill ends it.
        "head -c 4096 /dev/zero; exec sleep 60",
    ];
    for body in bodies {
        let fzf = FakeFzf::new(body);
        let mut p = page(&(0..5000).collect::<Vec<_>>());
        for h in &mut p.hits {
            h.text = "x".repeat(400);
        }
        let path = fzf.path.clone();
        let r = fake_fzf::within(Duration::from_secs(10), move || pick_with(&p, &path));
        assert!(protocol_error(r).contains("more than 2048 bytes"), "{body}");
        fzf.assert_reaped();
    }
}

#[test]
fn version_gate() {
    for (version, ok) in [
        ("0.27.2", true),
        ("0.74.4 (Homebrew)", true),
        ("0.27.1", false),
        ("0.16.4", false),
        ("garbage", false),
        ("", false),
    ] {
        let fzf = FakeFzf::with_version(version, "printf '1\\t\\0'");
        match pick_with(&page(&[1]), &fzf.path) {
            Ok(Pick::Chosen(1)) if ok => {}
            Err(PickError::TooOld { found, minimum }) if !ok => {
                assert_eq!(minimum, "0.27.2");
                assert_eq!(
                    found,
                    version.split(' ').next().filter(|v| !v.is_empty()).unwrap_or("unknown")
                );
                assert!(!fzf.dir.path().join("argv").exists(), "picker ran despite {version:?}");
            }
            other => panic!("{version:?}: {other:?}"),
        }
    }
}

#[test]
fn failing_version_probe_is_unknown() {
    let fzf = FakeFzf::new_raw("echo 0.74.4; exit 2");
    match pick_with(&page(&[1]), &fzf.path) {
        Err(PickError::TooOld { found, .. }) => assert_eq!(found, "unknown"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn missing_binary_is_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let r = pick_with(&page(&[1]), &dir.path().join("fzf"));
    assert!(matches!(r, Err(PickError::Unavailable)), "{r:?}");
}
