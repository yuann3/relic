//! Optional fzf picker over one bounded [`SearchPage`].
//!
//! Contract (docs/research/tui-fzf-integration.md, "fzf contract"): fixed argv,
//! no shell, no query or history text in argv, NUL-framed records on stdin,
//! at most one bounded record back on stdout, and the returned id must be a
//! member of the page. Display text from fzf is discarded.
//!
//! fzf draws its UI on the controlling terminal (it opens `/dev/tty` itself)
//! while stdin is our pipe and stdout is captured. A TUI caller must therefore
//! leave raw mode and the alternate screen, and stop its own input reader,
//! before calling [`pick`]; two readers on one terminal corrupt both.

use std::ffi::OsStr;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::app::{self, PageHit, SearchPage};

/// Lowest fzf this picker accepts. Every flag in [`ARGS`] exists from 0.17.4
/// (`--layout`); 0.27.2 additionally fixed `--read0` mis-rendering long lines,
/// and our records run up to [`RECORD_MAX`] bytes. Only 0.74.4 was exercised.
const MINIMUM: (u32, u32, u32) = (0, 27, 2);
const MINIMUM_STR: &str = "0.27.2";

/// Per-record cap on what we send, including the NUL terminator.
const RECORD_MAX: usize = 2048;

/// fzf echoes back one of our records, so a valid answer is never longer.
const OUTPUT_MAX: usize = RECORD_MAX;

/// `--nth` indexes the fields left after `--with-nth`, so `2..` searches source
/// and summary while the timestamp is shown but not matched; digits in a query
/// would otherwise fuzzy-match nearly every timestamp. The id is never shown
/// or searched. The delimiter is a literal tab, which fzf treats as a plain
/// string rather than a regex.
const ARGS: &[&str] = &[
    "--read0",
    "--print0",
    "--no-multi",
    "--delimiter=\t",
    "--with-nth=2..",
    "--nth=2..",
    "--height=80%",
    "--layout=reverse",
    "--border",
];

/// User defaults could add `--multi`, `--print-query`, `--expect` (breaking the
/// one-record protocol) or a `--preview`/`--bind execute` that interpolates
/// hostile history text into a shell command.
const SCRUBBED_ENV: &[&str] = &["FZF_DEFAULT_OPTS", "FZF_DEFAULT_OPTS_FILE"];

#[derive(Debug)]
pub enum PickError {
    /// fzf is not on PATH. Callers fall back to indexed search.
    Unavailable,
    /// fzf is present but older than the tested minimum, or its version is
    /// unrecognisable.
    TooOld {
        found: String,
        minimum: &'static str,
    },
    Io(std::io::Error),
    /// fzf answered with something outside the protocol.
    Protocol(String),
}

impl std::fmt::Display for PickError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable => write!(f, "fzf unavailable; using indexed search"),
            Self::TooOld { found, minimum } => write!(
                f,
                "fzf {found} is older than the supported {minimum}; \
                 install a newer fzf (https://github.com/junegunn/fzf#installation)"
            ),
            Self::Io(e) => write!(f, "fzf failed: {e}"),
            Self::Protocol(m) => write!(f, "fzf returned an invalid selection: {m}"),
        }
    }
}

impl std::error::Error for PickError {}

impl From<std::io::Error> for PickError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// What the user did in fzf. No match and cancel stay apart, as fzf keeps
/// them: a script must tell "nothing matched" from "the user gave up".
#[derive(Debug, PartialEq, Eq)]
pub enum Pick {
    Chosen(i64),
    /// Enter with nothing matching: fzf's exit 1.
    NoMatch,
    /// Esc or Ctrl-C: fzf's exit 130.
    Cancelled,
}

/// Let the user pick one hit with the `fzf` on PATH.
pub fn pick(page: &SearchPage) -> Result<Pick, PickError> {
    let fzf = find_on_path(std::env::var_os("PATH").as_deref()).ok_or(PickError::Unavailable)?;
    pick_with(page, &fzf)
}

/// [`pick`] with an explicit fzf binary.
pub fn pick_with(page: &SearchPage, fzf: &Path) -> Result<Pick, PickError> {
    check_version(fzf)?;
    let records = encode(&page.hits);
    let child = command(fzf)
        .args(ARGS)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(spawn_error)?;
    let mut child = Reaped(child);
    let mut stdin = child.0.stdin.take().expect("stdin is piped");
    let mut stdout = child.0.stdout.take().expect("stdout is piped");

    // Feeding stdin from this thread while fzf blocks on a full stdout pipe
    // (or never reads) would deadlock; a writer thread keeps both moving.
    let writer = std::thread::spawn(move || stdin.write_all(&records));

    let mut out = Vec::new();
    let read = (&mut stdout).take(OUTPUT_MAX as u64 + 1).read_to_end(&mut out);
    drop(stdout);
    let overflow = out.len() > OUTPUT_MAX;
    if overflow || read.is_err() {
        // Stop a child that is still talking; we are not listening any more.
        let _ = child.0.kill();
    }
    let status = child.0.wait()?;
    let written = writer.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic));

    read?;
    if overflow {
        return Err(PickError::Protocol(format!("more than {OUTPUT_MAX} bytes on stdout")));
    }
    // fzf may legitimately exit before draining the page (early pick, cancel).
    match written {
        Err(e) if e.kind() != ErrorKind::BrokenPipe => return Err(e.into()),
        _ => {}
    }
    match status.code() {
        Some(0) => parse_selection(&out, page).map(Pick::Chosen),
        Some(1) => Ok(Pick::NoMatch),
        Some(130) => Ok(Pick::Cancelled),
        _ => Err(PickError::Protocol(format!("fzf exited with {status}"))),
    }
}

/// Kills and reaps the child if it is dropped before an explicit `wait`, so
/// early returns and panics never leave a zombie or a live fzf on the tty.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        if let Ok(None) = self.0.try_wait() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn command(fzf: &Path) -> Command {
    let mut cmd = Command::new(fzf);
    for k in SCRUBBED_ENV {
        cmd.env_remove(k);
    }
    cmd
}

fn spawn_error(e: std::io::Error) -> PickError {
    match e.kind() {
        ErrorKind::NotFound => PickError::Unavailable,
        _ => PickError::Io(e),
    }
}

/// Empty PATH entries mean the current directory; skipping them keeps a
/// planted `./fzf` from running.
fn find_on_path(path: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("fzf"))
        .find(|p| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

fn check_version(fzf: &Path) -> Result<(), PickError> {
    let out = command(fzf)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(spawn_error)?;
    let text = String::from_utf8_lossy(&out.stdout);
    let token = text.split_whitespace().next().filter(|_| out.status.success());
    match token.and_then(parse_version) {
        Some(v) if v >= MINIMUM => Ok(()),
        _ => Err(PickError::TooOld {
            found: token
                .map_or_else(|| "unknown".into(), |t| app::display_line(t).chars().take(32).collect()),
            minimum: MINIMUM_STR,
        }),
    }
}

/// `MAJOR.MINOR[.PATCH][-suffix]`, digits only; anything else is unknown.
fn parse_version(token: &str) -> Option<(u32, u32, u32)> {
    let core = token.split_once('-').map_or(token, |(core, _)| core);
    let mut parts = core.split('.').map(|p| {
        if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) { p.parse().ok() } else { None }
    });
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next().unwrap_or(Some(0))?;
    parts.next().is_none().then_some((major, minor, patch))
}

/// `<id>\t<YYYY-MM-DD HH:MM>\t<source>\t<summary>\0` per hit.
fn encode(hits: &[PageHit]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(hits.len() * 96);
    for h in hits {
        let start = buf.len();
        let when = h.when().label();
        let when = when.trim();
        write!(buf, "{}\t{when}\t{}\t", h.id, app::display_line(h.source)).expect("Vec write");
        let mut budget = RECORD_MAX - 1 - (buf.len() - start);
        let mut utf8 = [0; 4];
        for c in h.text.chars().map(app::display_char) {
            if c.len_utf8() > budget {
                break;
            }
            budget -= c.len_utf8();
            buf.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
        }
        buf.push(0);
    }
    buf
}

fn parse_selection(out: &[u8], page: &SearchPage) -> Result<i64, PickError> {
    let protocol = |m: &str| Err(PickError::Protocol(m.into()));
    let Some((&0, record)) = out.split_last() else {
        return protocol("no NUL-terminated record");
    };
    if record.contains(&0) {
        return protocol("more than one record");
    }
    let id = record.split(|&b| b == b'\t').next().unwrap_or_default();
    // Only the exact bytes Relic wrote count: parse() alone would also accept
    // "+12" or "012", which match no record that was sent.
    let Some(id) = std::str::from_utf8(id)
        .ok()
        .and_then(|s| s.parse::<i64>().ok().filter(|n| n.to_string() == s))
    else {
        return protocol("record does not start with a numeric id");
    };
    if !page.contains(id) {
        return protocol("id is not on the page");
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: i64, text: &str) -> PageHit {
        PageHit {
            id,
            observed_at_ms: Some(1_700_000_000_000),
            observed_before_ms: None,
            source: "zsh",
            text: text.into(),
        }
    }

    fn page(hits: Vec<PageHit>) -> SearchPage {
        SearchPage {
            schema: app::SEARCH_SCHEMA,
            query: "q".into(),
            limit: 10,
            truncated: false,
            deferred: None,
            words_dropped: 0,
            order: app::Order::RecentlyImported,
            hits,
        }
    }

    #[test]
    fn version_parsing_is_conservative() {
        assert_eq!(parse_version("0.74.4"), Some((0, 74, 4)));
        assert_eq!(parse_version("0.27"), Some((0, 27, 0)));
        assert_eq!(parse_version("0.75.0-rc1"), Some((0, 75, 0)));
        for bad in ["", "v0.74.4", "0", "0.74.x", "0..4", "0.74.4.1", "devel", "+1.2.3"] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
        assert!(parse_version("0.27.1").unwrap() < MINIMUM);
        assert!(parse_version("0.27.2").unwrap() >= MINIMUM);
        assert!(parse_version("1.0").unwrap() >= MINIMUM);
    }

    #[test]
    fn path_lookup_skips_cwd_and_non_executables() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(a.path().join("fzf"), "").unwrap(); // mode 644
        let exe = b.path().join("fzf");
        std::fs::write(&exe, "").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path = std::env::join_paths([a.path(), b.path()]).unwrap();
        assert_eq!(find_on_path(Some(&path)), Some(exe));
        assert_eq!(find_on_path(Some(a.path().as_os_str())), None);
        assert_eq!(find_on_path(None), None);
        // A bare ":" is two empty entries, i.e. the cwd twice.
        assert_eq!(find_on_path(Some(OsStr::new(":"))), None);
    }

    /// Our encoder -> the real fzf with our exact flags -> our decoder. Skips
    /// when fzf is not installed.
    #[test]
    fn real_fzf_accepts_the_argv_and_round_trips_a_record() {
        let Some(fzf) = find_on_path(std::env::var_os("PATH").as_deref()) else {
            eprintln!("skipped: no fzf on PATH");
            return;
        };
        check_version(&fzf).unwrap();
        let p =
            page(vec![hit(41, "make check"), hit(42, "cargo\tbuild\n--release"), hit(43, "ls")]);
        let filter = |q: &str| {
            let mut child = command(&fzf)
                .args(ARGS)
                .arg(format!("--filter={q}"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child.stdin.take().unwrap().write_all(&encode(&p.hits)).unwrap();
            child.wait_with_output().unwrap()
        };
        let out = filter("cargo release");
        assert!(out.status.success());
        assert_eq!(parse_selection(&out.stdout, &p).unwrap(), 42);
        // Ids and timestamps are not searchable; the source is.
        assert_eq!(filter("41").status.code(), Some(1));
        assert_eq!(filter("2023").status.code(), Some(1));
        assert_eq!(filter("zsh").stdout.iter().filter(|&&b| b == 0).count(), 3);
    }
}
