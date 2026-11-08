/// What a source is: a shell history, by its format, a Claude Code session,
/// or a Git repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Zsh,
    Bash,
    /// A repository, imported through `git` rather than read as a file.
    Git,
    /// One Claude Code session's transcript.
    Claude,
    /// One shell session's frames from `relic hook`.
    Hook,
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Zsh => "zsh",
            Self::Bash => "bash",
            Self::Git => "git",
            Self::Claude => "claude",
            Self::Hook => "hook",
        })
    }
}

/// Kind of observation source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ObservationKind {
    ZshCommand = 1,
    BashCommand = 2,
    HookFrame = 3,
    AgentMessage = 4,
    /// A Git ref moved, at the time Git recorded.
    GitReflog = 5,
    /// Where a Git ref pointed when Relic imported the repository.
    GitRef = 6,
    /// A shell command an agent ran: Claude Code's Bash tool.
    AgentCommand = 7,
    /// What such a command printed, as the agent saw it, and whether it
    /// failed.
    AgentOutput = 8,
    /// Another tool call an agent made: its name and what it named.
    AgentTool = 9,
}

impl ObservationKind {
    pub fn as_i64(self) -> i64 {
        self as i64
    }

    pub fn from_i64(v: i64) -> Option<Self> {
        match v {
            1 => Some(Self::ZshCommand),
            2 => Some(Self::BashCommand),
            3 => Some(Self::HookFrame),
            4 => Some(Self::AgentMessage),
            5 => Some(Self::GitReflog),
            6 => Some(Self::GitRef),
            7 => Some(Self::AgentCommand),
            8 => Some(Self::AgentOutput),
            9 => Some(Self::AgentTool),
            _ => None,
        }
    }

    pub fn source_label(self) -> &'static str {
        match self {
            Self::ZshCommand => "zsh",
            Self::BashCommand => "bash",
            Self::HookFrame => "hook",
            Self::AgentMessage => "agent",
            Self::GitReflog | Self::GitRef => "git",
            Self::AgentCommand => "claude",
            Self::AgentOutput => "claude-out",
            Self::AgentTool => "claude-tool",
        }
    }
}

/// Which observation fields a revision's source file itself encodes. A field
/// outside the set is absent for that revision, never inferred. One value per
/// revision instead of a blob per row (ADR 0008); a single row still says
/// "no time" with a NULL `observed_at_ms`. One exception: a Git revision's
/// `GitRef` rows carry the time they were imported, as Git keeps none for
/// where a ref points, while its reflog rows beside them carry Git's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provenance(u8);

impl Provenance {
    pub const COMMAND: Self = Self(1);
    pub const OBSERVED_AT: Self = Self(2);

    pub fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    pub fn has(self, field: Self) -> bool {
        self.0 & field.0 == field.0
    }

    pub fn as_i64(self) -> i64 {
        i64::from(self.0)
    }

    pub fn from_i64(v: i64) -> Self {
        Self(v as u8)
    }
}

/// Bit in `Observation::flags`: `safe_text` is not the source's exact text.
/// The record was cut at the per-record cap, had a dangling zsh Meta byte
/// dropped, or was not valid UTF-8.
pub const FLAG_LOSSY: i64 = 1;

/// A sanitized observation ready for storage. The open revision assigns its
/// revision and ordinal.
#[derive(Debug, Clone)]
pub struct Observation {
    pub kind: ObservationKind,
    pub observed_at_ms: Option<i64>,
    pub safe_text: String,
    pub flags: i64,
    pub locator: Vec<u8>,
}

/// An observation retrieved from the store, with its row ID.
#[derive(Debug, Clone)]
pub struct StoredObservation {
    pub id: i64,
    pub revision_id: i64,
    pub ordinal: i64,
    pub kind: ObservationKind,
    pub observed_at_ms: Option<i64>,
    pub safe_text: String,
    pub flags: i64,
    pub locator: Vec<u8>,
}

/// A ranked search hit from FTS.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub id: i64,
    pub observed_at_ms: Option<i64>,
    /// For an untimed hit: see [`crate::store::Store::observed_no_later_than`].
    pub observed_before_ms: Option<i64>,
    pub kind: ObservationKind,
    pub safe_text: String,
    pub score: f64,
}

/// When a command ran, as far as the source can say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    At(i64),
    /// The source has no time for it, but a later timestamped command in the
    /// same file bounds it.
    NoLaterThan(i64),
    Unknown,
}

impl When {
    pub(crate) fn from_parts(at: Option<i64>, before: Option<i64>) -> Self {
        match (at, before) {
            (Some(ms), _) => Self::At(ms),
            (None, Some(ms)) => Self::NoLaterThan(ms),
            (None, None) => Self::Unknown,
        }
    }

    /// Fixed 17 columns for result lists. The first column marks an
    /// inferred bound, so exact and bounded dates line up.
    pub fn label(self) -> String {
        match self {
            Self::At(ms) => format!(" {}", format_utc(ms)),
            Self::NoLaterThan(ms) => format!("≤{}", format_utc(ms)),
            Self::Unknown => " unknown time    ".into(),
        }
    }

    /// Fixed 12 columns, month-day only, for the timeline beside the year.
    pub fn short(self) -> String {
        match self {
            Self::At(ms) => format!(" {}", &format_utc(ms)[5..]),
            Self::NoLaterThan(ms) => format!("≤{}", &format_utc(ms)[5..]),
            Self::Unknown => " unknown    ".into(),
        }
    }

    /// Spelled out, saying which part is inferred.
    pub fn detail(self) -> String {
        match self {
            Self::At(ms) => format!("{} UTC", format_utc(ms)),
            Self::NoLaterThan(ms) => format!(
                "not recorded; no later than {} UTC (inferred from history order)",
                format_utc(ms)
            ),
            Self::Unknown => "not recorded".into(),
        }
    }
}

/// `YYYY-MM-DD HH:MM` in UTC. Local time needs a tz database, which the
/// prototype does not carry.
pub fn format_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}",
        rem / 3600,
        rem % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_labels_keep_their_columns() {
        let ms = 1_791_017_948_000;
        for w in [When::At(ms), When::NoLaterThan(ms), When::Unknown] {
            assert_eq!(w.label().chars().count(), 17, "{w:?}");
            assert_eq!(w.short().chars().count(), 12, "{w:?}");
        }
        assert_eq!(When::NoLaterThan(ms).label(), "≤2026-10-03 08:59");
        assert_eq!(When::NoLaterThan(ms).short(), "≤10-03 08:59");
        assert!(When::NoLaterThan(ms).detail().contains("inferred"));
    }

    #[test]
    fn utc_formatting() {
        assert_eq!(format_utc(0), "1970-01-01 00:00");
        assert_eq!(format_utc(1_700_000_000_000), "2023-11-14 22:13");
        assert_eq!(format_utc(951_782_400_000), "2000-02-29 00:00");
        assert_eq!(format_utc(-60_000), "1969-12-31 23:59");
    }
}
