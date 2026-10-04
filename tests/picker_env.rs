//! Kept in its own test binary: it sets process environment, which would race
//! with any other test running fzf in parallel.

// Shared helpers; this binary uses only some of them.
#[allow(dead_code)]
mod fake_fzf;

use fake_fzf::{FakeFzf, page};
use relic::picker::pick_with;

#[test]
fn user_fzf_defaults_never_reach_the_picker() {
    // SAFETY: the only test in this binary, so no other thread reads the env.
    unsafe {
        std::env::set_var("FZF_DEFAULT_OPTS", "--multi --preview 'echo {}'");
        std::env::set_var("FZF_DEFAULT_OPTS_FILE", "/tmp/relic-fzf-opts");
        std::env::set_var("RELIC_PICKER_CANARY", "1");
    }
    let fzf = FakeFzf::new("printf '1\\t\\0'");
    assert_eq!(pick_with(&page(&[1]), &fzf.path).unwrap(), Some(1));
    let env = String::from_utf8(fzf.read("env")).unwrap();
    assert!(env.contains("RELIC_PICKER_CANARY=1"), "env was not recorded");
    assert!(!env.contains("FZF_DEFAULT_OPTS"), "{env}");
}
