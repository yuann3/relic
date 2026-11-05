//! A validated procedure as a portable Agent Skill: one `SKILL.md` with the
//! spec's frontmatter only (https://agentskills.io/specification), no
//! scripts and no `allowed-tools`.
//!
//! The standard gives the body to the model as-is, but harnesses do not:
//! Claude Code runs `` !`cmd` `` and rewrites `$1` and `$ARGUMENTS`, Goose
//! rewrites `$1` with no escape, Letta rewrites `<SKILL_DIR>`. No escape
//! works in all of them, so text that would trip one never enters the body:
//! a recorded command is shown by id for `relic show`, and step prose or a
//! description is refused (docs/research/validation-skill-packaging.md).

use crate::app::When;

/// One procedure step: what the model wrote, and the records it cites.
pub struct Step<'a> {
    pub text: &'a str,
    pub records: Vec<Record<'a>>,
}

/// A cited record, as the store holds it.
pub struct Record<'a> {
    pub id: i64,
    pub source: &'static str,
    pub observed_at_ms: Option<i64>,
    pub text: &'a str,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CompileError {
    /// Not a name the Agent Skills spec allows.
    Name(String),
    Description(String),
    NoSteps,
    /// Step `n` (from 1) holds text some harness would rewrite or run.
    StepText { n: usize, hazard: &'static str },
}

impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Name(why) => write!(f, "skill name: {why}"),
            Self::Description(why) => write!(f, "description: {why}"),
            Self::NoSteps => f.write_str("the report has no procedure steps to compile"),
            Self::StepText { n, hazard } => write!(
                f,
                "step {n} contains {hazard}, which some agents rewrite or run when they load a skill; \
                 recall again for a procedure that quotes commands only by citing them"
            ),
        }
    }
}

impl std::error::Error for CompileError {}

/// The first thing in `text` that some harness rewrites or runs.
pub fn hazard(text: &str) -> Option<&'static str> {
    let bytes = text.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        let rest = &text[i + 1..];
        match b {
            b'$' if rest.starts_with(|c: char| c.is_ascii_digit()) => return Some("`$` before a digit"),
            b'$' if rest.starts_with("ARGUMENTS") => return Some("`$ARGUMENTS`"),
            b'$' if rest.starts_with("{CLAUDE_") || rest.starts_with("{DROID_") => return Some("a `${…}` agent variable"),
            // Claude Code: only at the start of a line or after whitespace.
            b'!' if rest.starts_with('`') && (i == 0 || bytes[i - 1].is_ascii_whitespace()) => {
                return Some("`!` before a backtick");
            }
            _ => {}
        }
    }
    if text.lines().any(|l| l.trim_start().starts_with("```!")) {
        return Some("a ```! block");
    }
    ["<SKILL_DIR>", "#file:", "#tool:", "{{"].into_iter().find(|h| text.contains(h))
}

/// The spec's rule: 1-64 of `a-z0-9-`, no hyphen at either end, no `--`.
pub fn check_name(name: &str) -> Result<(), CompileError> {
    let why = if name.is_empty() || name.len() > 64 {
        "must be 1-64 characters"
    } else if !name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
        "may hold only a-z, 0-9 and -"
    } else if name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        "must not start or end with - or hold --"
    } else {
        return Ok(());
    };
    Err(CompileError::Name(format!("{name:?} {why}")))
}

fn check_description(description: &str) -> Result<(), CompileError> {
    let why = match description.chars().count() {
        0 => "is empty",
        n if n > 1024 => "is over 1024 characters",
        // Goose splits frontmatter on `---`.
        _ if description.contains('\n') || description.contains("---") => "must be one line without ---",
        _ => match hazard(description) {
            Some(h) => h,
            None => return Ok(()),
        },
    };
    Err(CompileError::Description(why.into()))
}

/// The bundle's one file.
pub fn render(name: &str, description: &str, confirmed_at_ms: i64, steps: &[Step]) -> Result<String, CompileError> {
    check_name(name)?;
    check_description(description)?;
    if steps.is_empty() {
        return Err(CompileError::NoSteps);
    }
    let date = &crate::app::format_utc(confirmed_at_ms)[..10];
    // JSON strings are YAML double-quoted scalars.
    let quoted = serde_json::to_string(description).map_err(|e| CompileError::Description(e.to_string()))?;
    let mut md = format!(
        "---\nname: {name}\ndescription: {quoted}\nmetadata:\n  relic-version: \"{}\"\n  relic-confirmed: \"{date}\"\n---\n\n\
         # {name}\n\n\
         Relic recovered this procedure from the user's own command history. A verifier the user wrote confirmed it worked on {date}.\n\n\
         Before running anything, show the user these steps and ask. Run a step only after the user agrees. \
         A `[REDACTED:<kind>]` marker is a value Relic removed: ask the user for it, never guess it.\n\n## Steps\n",
        env!("CARGO_PKG_VERSION")
    );
    for (n, step) in steps.iter().enumerate() {
        if let Some(hazard) = hazard(step.text) {
            return Err(CompileError::StepText { n: n + 1, hazard });
        }
        md.push_str(&format!("\n{}. {}\n", n + 1, step.text.trim()));
        for r in &step.records {
            let when = r.observed_at_ms.map_or(When::Unknown, When::At).label();
            let when = when.trim();
            if hazard(r.text).is_some() {
                md.push_str(&format!(
                    "\n   Recorded command #{} ({}, {when}) is not copied here, because agents rewrite parts of its text. \
                     Read it exactly with `relic show {}`.\n",
                    r.id, r.source, r.id
                ));
                continue;
            }
            // Longer than any backtick run inside, so the text cannot close it.
            let longest = r.text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
            let fence = "`".repeat(longest.max(2) + 1);
            md.push_str(&format!("\n   Recorded command #{} ({}, {when}):\n\n   {fence}text\n", r.id, r.source));
            for line in r.text.lines() {
                md.push_str(&format!("   {line}\n"));
            }
            md.push_str(&format!("   {fence}\n"));
        }
    }
    md.push_str(
        "\n## Evidence\n\nEach recorded command is a record in the user's Relic store. `relic show <id>` prints it exactly as stored. \
         If it says the record is gone, the user has forgotten it: say so, and do not reconstruct it.\n",
    );
    Ok(md)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hazards_are_what_some_harness_rewrites_or_runs_and_nothing_else() {
        for (text, found) in [
            ("awk '{print $1}' f", true),
            ("echo $ARGUMENTS", true),
            ("cat ${CLAUDE_SKILL_DIR}/x", true),
            ("run !`date`", true),
            ("!`date`", true),
            ("x\n```!\ndate\n```", true),
            ("cd <SKILL_DIR>", true),
            ("see #file:a.rs", true),
            ("echo {{x}}", true),
            // Left alone everywhere.
            ("echo $HOME ${HOME} $PATH", false),
            ("KEY=!`cmd`", false),
            ("git reflog && git branch rescue HEAD@{1}", false),
            ("sudo !!", false),
            ("mysql --password=[REDACTED:password]", false),
        ] {
            assert_eq!(hazard(text).is_some(), found, "{text:?}: {:?}", hazard(text));
        }
    }

    #[test]
    fn names_follow_the_spec() {
        assert!(check_name("git-reflog-recovery").is_ok());
        for bad in ["", "Git", "a_b", "-a", "a-", "a--b", &"a".repeat(65)] {
            assert!(check_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_hazardous_command_is_shown_by_id_and_a_hazardous_step_is_refused() {
        let records = |text| vec![Record { id: 42, source: "zsh", observed_at_ms: Some(1_700_000_000_000), text }];
        let md = render("x", "Use it.", 0, &[Step { text: "List the reflog.", records: records("git reflog ```` HEAD") }]).unwrap();
        assert!(md.contains("   `````text\n   git reflog ```` HEAD\n   `````\n"), "{md}");
        let md = render("x", "Use it.", 0, &[Step { text: "Print column one.", records: records("awk '{print $1}' f") }]).unwrap();
        assert!(md.contains("Recorded command #42 (zsh, ") && md.contains("`relic show 42`") && !md.contains("awk"), "{md}");
        let err = render("x", "Use it.", 0, &[Step { text: "Run awk '{print $1}'.", records: vec![] }]).unwrap_err();
        assert_eq!(err, CompileError::StepText { n: 1, hazard: "`$` before a digit" });
        assert!(matches!(render("x", "a --- b", 0, &[]), Err(CompileError::Description(_))));
    }
}
