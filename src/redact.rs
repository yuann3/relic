//! Redaction: finds the spans of a command that carry a secret and replaces
//! each with a typed marker, `[REDACTED:<kind>]`.
//!
//! Precision-leaning by design (ADR 0003): known secret carriers go, while
//! SHAs, digests, UUIDs and paths stay, because evidence that cannot be
//! searched or cited is worth nothing. No detector proves absence.
//!
//! Detectors only mark byte spans; one splice at the end writes the markers,
//! so no detector ever sees another's output.

/// Stored with every revision. Bump it whenever a change here could redact
/// something an earlier version left in place: the next import then rescrubs
/// what is already stored.
pub const REDACTION_REVISION: &str = "r2";

const MARKER_OPEN: &str = "[REDACTED:";
/// Sits between `MARKER_OPEN` and the kind in a marker the user typed, so it
/// is no longer well-formed and cannot pass for one the redactor wrote. A
/// word, not punctuation: FTS then indexes it apart from a real marker too.
const TYPED: &str = "literal:";

/// What a redacted span held. Kept in the marker because the kind is evidence
/// (a command took a password) and never the secret.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Password,
    Token,
    Key,
    Secret,
}

impl Kind {
    const ALL: [Kind; 4] = [Kind::Password, Kind::Token, Kind::Key, Kind::Secret];

    fn name(self) -> &'static str {
        match self {
            Kind::Password => "password",
            Kind::Token => "token",
            Kind::Key => "key",
            Kind::Secret => "secret",
        }
    }
}

/// Byte ranges of the well-formed markers in `text`, in order.
pub fn markers(text: &str) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
    let mut from = 0;
    std::iter::from_fn(move || {
        loop {
            let start = from + text[from..].find(MARKER_OPEN)?;
            let kind = start + MARKER_OPEN.len();
            from = kind;
            let len = Kind::ALL.iter().map(|k| k.name()).find(|name| text[kind..].starts_with(name)).map(str::len);
            if let Some(len) = len
                && text[kind + len..].starts_with(']')
            {
                from = kind + len + 1;
                return Some(start..from);
            }
        }
    })
}

/// A compiled redaction pipeline. Compile once per process, apply to each record.
pub struct Redactor {
    _private: (), // No config for now; the struct exists for future expansion.
}

impl Redactor {
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Redact raw text nobody has redacted before. Anything in it that looks
    /// like a marker was typed by the user, so it is escaped first; from then
    /// on every well-formed marker in the safe text is one Relic wrote.
    pub fn import(&self, raw: &str) -> String {
        let mut escaped = String::with_capacity(raw.len());
        let mut pos = 0;
        for marker in markers(raw) {
            let kind = marker.start + MARKER_OPEN.len();
            escaped.push_str(&raw[pos..kind]);
            escaped.push_str(TYPED);
            pos = kind;
        }
        escaped.push_str(&raw[pos..]);
        self.redact(&escaped)
    }

    /// Redact text that may already hold markers, leaving those as they are:
    /// `redact(redact(x)) == redact(x)`. Stored safe text goes through here
    /// again on a rescrub and on every model egress, and a redactor that
    /// rewrote its own markers would demote real ones there.
    pub fn redact(&self, input: &str) -> String {
        let cmd = Command::parse(input);
        let mut spans = Vec::new();
        cmd.named_values(&mut spans);
        cmd.flag_values(&mut spans);
        cmd.piped_passwords(&mut spans);
        url_passwords(input, &mut spans);
        bearer_tokens(&cmd, &mut spans);
        private_key_blocks(input, &mut spans);
        token_runs(input, &mut spans);
        splice(input, spans)
    }
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

type Span = (usize, usize, Kind);

/// Write a marker over each span. Overlapping spans become one marker, of
/// the kind found first. A marker already in the text is never covered: the
/// spans stop at its edges.
fn splice(s: &str, mut spans: Vec<Span>) -> String {
    spans.retain(|&(a, b, _)| a < b);
    spans.sort_unstable_by_key(|&(a, b, _)| (a, b));
    let mut kept = markers(s).peekable();
    let mut out = String::with_capacity(s.len());
    let mut pos = 0;
    let mut i = 0;
    while i < spans.len() {
        let (start, mut end, kind) = spans[i];
        i += 1;
        while i < spans.len() && spans[i].0 <= end {
            end = end.max(spans[i].1);
            i += 1;
        }
        let mut start = start.max(pos);
        // Emit the span in pieces around the markers inside it.
        while start < end {
            while kept.peek().is_some_and(|m| m.end <= start) {
                kept.next();
            }
            let piece_end = match kept.peek() {
                Some(m) if m.start <= start => {
                    // Inside a marker: copy it through and resume after it.
                    let m_end = m.end.min(s.len());
                    out.push_str(&s[pos..m_end]);
                    pos = m_end;
                    start = m_end;
                    continue;
                }
                Some(m) if m.start < end => m.start,
                _ => end,
            };
            out.push_str(&s[pos..start]);
            out.push_str(MARKER_OPEN);
            out.push_str(kind.name());
            out.push(']');
            pos = piece_end;
            start = piece_end;
        }
    }
    out.push_str(&s[pos..]);
    out
}

/// Shell quoting state of one byte.
#[derive(Clone, Copy, PartialEq)]
enum Q {
    Plain,
    /// Escaped by a backslash, or inside `$(...)` or backticks: part of the
    /// word around it, never a separator.
    Glued,
    Single,
    Double,
    Open,
    Close,
}

struct Word {
    start: usize,
    end: usize,
    segment: usize,
    /// The target of `<`, `>` or `>>`: a file, never one of the command's
    /// arguments. Not set after `<<<`, whose word is the input itself.
    redirect: bool,
}

/// A command split the way the shell would split it, close enough to find
/// where a value starts and ends. Values are often quoted or continued with
/// a backslash-newline, and cutting them at the first space leaks the rest.
struct Command<'a> {
    s: &'a str,
    q: Vec<Q>,
    words: Vec<Word>,
}

impl<'a> Command<'a> {
    fn parse(s: &'a str) -> Self {
        let b = s.as_bytes();
        let mut q = vec![Q::Plain; b.len()];
        let mut state = Q::Plain;
        let mut i = 0;
        while i < b.len() {
            match (state, b[i]) {
                (Q::Single, b'\'') | (Q::Double, b'"') => {
                    q[i] = Q::Close;
                    state = Q::Plain;
                }
                (Q::Double, b'\\') if i + 1 < b.len() => {
                    q[i] = Q::Double;
                    q[i + 1] = Q::Double;
                    i += 1;
                }
                (Q::Single | Q::Double, _) => q[i] = state,
                (_, b'\\') if i + 1 < b.len() => {
                    q[i + 1] = Q::Glued;
                    i += 1;
                }
                (_, b'$') if b.get(i + 1) == Some(&b'(') => {
                    let mut depth = 0;
                    while i < b.len() {
                        q[i] = Q::Glued;
                        match b[i] {
                            b'(' => depth += 1,
                            b')' if depth == 1 => break,
                            b')' => depth -= 1,
                            _ => {}
                        }
                        i += 1;
                    }
                }
                (_, b'`') => {
                    q[i] = Q::Glued;
                    i += 1;
                    while i < b.len() && b[i] != b'`' {
                        q[i] = Q::Glued;
                        i += 1;
                    }
                    if i < b.len() {
                        q[i] = Q::Glued;
                    }
                }
                (_, b'\'') => {
                    q[i] = Q::Open;
                    state = Q::Single;
                }
                (_, b'"') => {
                    q[i] = Q::Open;
                    state = Q::Double;
                }
                _ => {}
            }
            i += 1;
        }

        let mut words = Vec::new();
        let mut segment = 0;
        let mut start = None;
        // `<` or `>` seen since the last word, and how many `<` in a row.
        let (mut redirect, mut less) = (false, 0);
        for (i, &c) in b.iter().enumerate() {
            let plain = q[i] == Q::Plain;
            let sep = plain && (c.is_ascii_whitespace() || b";|&()<>".contains(&c));
            if sep {
                if let Some(st) = start.take() {
                    words.push(Word { start: st, end: i, segment, redirect: redirect && less != 3 });
                    (redirect, less) = (false, 0);
                }
                redirect |= matches!(c, b'<' | b'>');
                less = if c == b'<' { less + 1 } else if c == b'>' { 0 } else { less };
                if b"\n;|&()".contains(&c) {
                    segment += 1;
                    (redirect, less) = (false, 0);
                }
            } else if start.is_none() {
                start = Some(i);
            }
        }
        if let Some(st) = start {
            words.push(Word { start: st, end: b.len(), segment, redirect: redirect && less != 3 });
        }
        Self { s, q, words }
    }

    fn text(&self, w: &Word) -> &'a str {
        &self.s[w.start..w.end]
    }

    /// The word after `i` in the same simple command, if it is not a flag.
    fn value_word(&self, i: usize) -> Option<&Word> {
        let next = self.words.get(i + 1)?;
        (next.segment == self.words[i].segment && !next.redirect && !self.text(next).starts_with('-'))
            .then_some(next)
    }

    /// Where a value starting at `start` ends. A value that opens a quote
    /// runs to the end of the word, quotes and all. A value inside a quote
    /// opened before its name ends at that quote, or at one of `stop`: `&`
    /// between query parameters (`"user=a&password=b"`), a space between
    /// assignments (`"host=db password=x dbname=app"`).
    fn value_end(&self, start: usize, word_end: usize, stop: &[u8]) -> usize {
        match self.q[start] {
            Q::Single | Q::Double => (start..word_end)
                .find(|&i| self.q[i] == Q::Close || stop.contains(&self.s.as_bytes()[i]))
                .unwrap_or(word_end),
            Q::Close => start,
            _ => word_end,
        }
    }

    /// `NAME=value` and `Name: value` wherever a secret-sounding name starts
    /// inside a word: shell assignments, `--password=x`, `-Ddb.password=x`,
    /// query strings and HTTP headers.
    fn named_values(&self, spans: &mut Vec<Span>) {
        let b = self.s.as_bytes();
        for (wi, w) in self.words.iter().enumerate() {
            let mut i = w.start;
            while i < w.end {
                if !is_name_byte(b[i]) {
                    i += 1;
                    continue;
                }
                let run = i;
                while i < w.end && is_name_byte(b[i]) {
                    i += 1;
                }
                if i >= w.end {
                    break;
                }
                let name = self.s[run..i].trim_start_matches(['-', '.']);
                let Some(secret) = secret_name(name) else {
                    // `--signing-key=<base64>`: see `flag_values`.
                    if b[i] == b'=' && bare_key_flag(&self.s[run..i]) {
                        let end = self.value_end(i + 1, w.end, b"");
                        push_random(self.s, i + 1, end, spans);
                    }
                    continue;
                };
                match b[i] {
                    b'=' => {
                        let start = i + 1;
                        let end = if start < w.end {
                            self.value_end(start, w.end, b"& \t\n")
                        } else {
                            start
                        };
                        push_value(self.s, start, end, secret, spans);
                        i = end.max(start);
                    }
                    // A header starts its word or its quote. Further in, a
                    // colon after such a name is a Rust path (`auth::login`),
                    // an image tag (`org/auth:v2`) or a jq key, not a header;
                    // `scheme://` is a URL.
                    b':' if !matches!(b.get(i + 1), Some(b'/' | b':'))
                        && (run == w.start || self.q[run - 1] == Q::Open) =>
                    {
                        i = self.header_value(wi, i + 1, secret, spans);
                    }
                    // A JSON member: `"password":"x"`, quotes escaped or not.
                    b'"' | b'\\' => {
                        if let Some((start, end)) = json_value(b, i, w.end) {
                            push_value(self.s, start, end, secret, spans);
                            i = end;
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Redacts a header value after its colon, keeping the auth scheme
    /// (`Authorization: Bearer [REDACTED]`) since it says what kind of
    /// credential this was without revealing it.
    fn header_value(&self, wi: usize, after_colon: usize, secret: Secret, spans: &mut Vec<Span>) -> usize {
        let w = &self.words[wi];
        let b = self.s.as_bytes();
        let end = if after_colon < w.end {
            self.value_end(after_colon, w.end, b"")
        } else {
            after_colon
        };
        let mut v = after_colon;
        while v < end && matches!(b[v], b' ' | b'\t') {
            v += 1;
        }
        let scheme_end = (v..end).find(|&i| !b[i].is_ascii_alphabetic()).unwrap_or(end);
        if is_auth_scheme(&self.s[v..scheme_end]) {
            v = scheme_end;
            while v < end && matches!(b[v], b' ' | b'\t') {
                v += 1;
            }
        }
        if v < end {
            push_value(self.s, v, end, secret, spans);
            return end;
        }
        // Unquoted `-H Authorization: Basic xyz` puts the value in later words.
        // Only after a header flag: `grep -n token: values.yaml` has no value.
        if !wi.checked_sub(1).is_some_and(|p| matches!(self.text(&self.words[p]), "-H" | "--header")) {
            return end;
        }
        let mut i = wi;
        while let Some(next) = self.value_word(i) {
            i += 1;
            if !is_auth_scheme(self.text(next)) {
                push_value(self.s, next.start, next.end, secret, spans);
                break;
            }
        }
        end
    }

    /// Flags whose next word is the secret: `--password X`, `--token X`, and
    /// tool-specific short flags such as `mysql -pX` or `sshpass -p X`.
    fn flag_values(&self, spans: &mut Vec<Span>) {
        // Segment of the last `config`/`configure` word, so the setter rule
        // costs one pass rather than a look-back per word.
        let mut config_segment = None;
        for (i, w) in self.words.iter().enumerate() {
            let text = self.text(w);
            if self.openssl_pass(i, spans) {
                continue;
            }
            if let Some(name) = text.strip_prefix('-') {
                let name = name.strip_prefix('-').unwrap_or(name);
                let long = text.starts_with("--") || name.len() >= 4;
                if long
                    && name.bytes().all(is_name_byte)
                    && let Some(v) = self.value_word(i)
                {
                    match secret_flag_name(name) {
                        Some(secret) => push_value(self.s, v.start, v.end, secret, spans),
                        // An unknown `--*-key` flag is too often a file or a
                        // sort key to redact by name, but base64 after it is
                        // a key. Judged as one word here: `token_runs` splits
                        // at `/` and misses base64 that contains one.
                        None if bare_key_flag(name) => push_random(self.s, v.start, v.end, spans),
                        None => {}
                    }
                }
            }
            self.tool_flag(i, spans);
            self.htpasswd_batch(i, spans);
            if config_segment == Some(w.segment) {
                self.config_setter(i, spans);
            }
            if matches!(text, "config" | "configure") {
                config_segment = Some(w.segment);
            }
        }
    }

    fn tool_flag(&self, i: usize, spans: &mut Vec<Span>) {
        let w = &self.words[i];
        let text = self.text(w);
        for tf in TOOL_FLAGS {
            let Some(rest) = text.strip_prefix(tf.flag) else {
                continue;
            };
            if !self.in_tool(i, tf) {
                continue;
            }
            let (start, end) = if rest.is_empty() {
                match (tf.separate, self.value_word(i)) {
                    (true, Some(v)) => (v.start, v.end),
                    _ => continue,
                }
            } else if tf.attached {
                let skip = usize::from(rest.starts_with('='));
                (w.start + tf.flag.len() + skip, w.end)
            } else {
                continue;
            };
            let start = if tf.after_colon {
                // `curl -u user` alone prompts; only `user:pass` holds a secret.
                match self.s[start..end].find(':') {
                    Some(c) => start + c + 1,
                    None => continue,
                }
            } else {
                start
            };
            push_value(self.s, start, end, (Strength::Strong, tf.kind), spans);
        }
    }

    /// `openssl enc -pass pass:SECRET`, and `-passin` / `-passout`. The other
    /// sources (`env:`, `file:`, `fd:`, `stdin`) name where the password is.
    /// Returns whether word `i` was such a flag, so the caller does not also
    /// take the whole source for a password.
    fn openssl_pass(&self, i: usize, spans: &mut Vec<Span>) -> bool {
        if !matches!(self.text(&self.words[i]), "-pass" | "-passin" | "-passout") || !self.in_tool(i, &OPENSSL) {
            return false;
        }
        if let Some(v) = self.value_word(i) {
            let quoted = self.q[v.start] == Q::Open;
            let start = v.start + usize::from(quoted);
            if self.s[start..v.end].starts_with("pass:") {
                let end = self.value_end(start, v.end, b"");
                push_value(self.s, start + 5, end, (Strength::Strong, Kind::Password), spans);
            }
        }
        true
    }

    /// `htpasswd -b file user PASSWORD`, `htpasswd -nb user PASSWORD`: with
    /// the batch flag the password is the command's last argument.
    fn htpasswd_batch(&self, i: usize, spans: &mut Vec<Span>) {
        let w = &self.words[i];
        let text = self.text(w);
        let batch = text.starts_with('-') && !text.starts_with("--") && text.contains('b');
        if !batch || !self.in_tool(i, &HTPASSWD) {
            return;
        }
        let args: Vec<&Word> = self.words[i + 1..]
            .iter()
            .take_while(|next| next.segment == w.segment)
            .filter(|next| !next.redirect && !self.text(next).starts_with('-'))
            .collect();
        // `-n` prints instead of writing a file, so there is one argument fewer.
        if let Some(password) = args.last()
            && args.len() >= 2
        {
            push_value(self.s, password.start, password.end, (Strength::Strong, Kind::Password), spans);
        }
    }

    /// `echo PASSWORD | sudo -S cmd` and `sudo -S cmd <<< PASSWORD`: with
    /// `-S` sudo reads the password from stdin, so what is fed to it is one.
    fn piped_passwords(&self, spans: &mut Vec<Span>) {
        let b = self.s.as_bytes();
        for (i, w) in self.words.iter().enumerate() {
            let segment = |j: usize| self.words[j].segment == w.segment;
            if self.text(w) != "sudo" || !self.sudo_reads_stdin(i) {
                continue;
            }
            // A here-string after it in the same command.
            for j in i + 1..self.words.len() {
                if !segment(j) {
                    break;
                }
                if self.s[self.words[j - 1].end..self.words[j].start].contains("<<<") {
                    let v = &self.words[j];
                    push_value(self.s, v.start, v.end, (Strength::Strong, Kind::Password), spans);
                }
            }
            // An `echo` or `printf` piped into it.
            let Some(feeder_end) = i.checked_sub(1) else { continue };
            let gap = &b[self.words[feeder_end].end..w.start];
            if gap.iter().filter(|&&c| c == b'|').count() != 1 || gap.contains(&b'\n') {
                continue;
            }
            let feeder = self.words[feeder_end].segment;
            let first = self.words[..=feeder_end].iter().position(|f| f.segment == feeder).unwrap_or(feeder_end);
            if !matches!(self.text(&self.words[first]), "echo" | "printf") {
                continue;
            }
            let args: Vec<&Word> = self.words[first + 1..=feeder_end]
                .iter()
                .skip_while(|a| self.text(a).starts_with('-'))
                .collect();
            if let (Some(from), Some(to)) = (args.first(), args.last()) {
                push_value(self.s, from.start, to.end, (Strength::Strong, Kind::Password), spans);
            }
        }
    }

    /// Whether the `sudo` at word `i` has `-S` among its own flags. They end
    /// at the command it runs, whose `-S` means something else (`ls -S`).
    fn sudo_reads_stdin(&self, i: usize) -> bool {
        let mut flags = self.words[i + 1..].iter().take_while(|f| f.segment == self.words[i].segment);
        while let Some(f) = flags.next() {
            let text = self.text(f);
            if !text.starts_with('-') {
                return false;
            }
            if text.starts_with("--") {
                continue;
            }
            if text.contains('S') {
                return true;
            }
            // These take the next word as their value.
            if matches!(text, "-u" | "-g" | "-h" | "-p" | "-C" | "-D" | "-R" | "-T" | "-U" | "-r" | "-t") {
                flags.next();
            }
        }
        false
    }

    /// Whether a word before `i` in the same simple command names one of the
    /// flag's tools (after any `sudo`, path or env prefix), and its
    /// subcommand when the flag only means "password" there (`docker login`
    /// but not `docker run -p 8080:80`).
    fn in_tool(&self, i: usize, tf: &ToolFlag) -> bool {
        let seg = self.words[i].segment;
        let before = self.words[..i].iter().rev().take_while(|w| w.segment == seg);
        let mut seen_sub = tf.subcommand.is_none();
        for w in before {
            let text = self.text(w);
            if Some(text) == tf.subcommand {
                seen_sub = true;
            }
            let base = text.rsplit('/').next().unwrap_or(text);
            if tf.tools.contains(&base) {
                return seen_sub;
            }
        }
        false
    }

    /// `aws configure set aws_secret_access_key X`, `git config user.password X`:
    /// the setting's name is a bare word and its value the next one.
    fn config_setter(&self, i: usize, spans: &mut Vec<Span>) {
        let w = &self.words[i];
        let text = self.text(w);
        let name_start = text.bytes().rposition(|c| !is_name_byte(c)).map_or(0, |p| p + 1);
        if let Some(secret) = secret_name(text[name_start..].trim_start_matches(['-', '.']))
            && let Some(v) = self.value_word(i)
        {
            push_value(self.s, v.start, v.end, secret, spans);
        }
    }
}

struct ToolFlag {
    tools: &'static [&'static str],
    flag: &'static str,
    subcommand: Option<&'static str>,
    /// `-pSECRET`.
    attached: bool,
    /// `-p SECRET`. Off for mysql: there `-p db` prompts and `db` is a database.
    separate: bool,
    after_colon: bool,
    kind: Kind,
}

const MYSQL: &[&str] = &[
    "mysql", "mysqldump", "mysqladmin", "mysqlimport", "mysqlshow", "mysqlcheck", "mysqlpump",
    "mysqlbinlog", "mariadb", "mariadb-dump", "mariadb-admin",
];
const MONGO: &[&str] = &[
    "mongo", "mongosh", "mongodump", "mongorestore", "mongoexport", "mongoimport",
];
const REGISTRY: &[&str] = &["docker", "podman", "nerdctl", "buildah", "helm"];

const fn tf(tools: &'static [&'static str], flag: &'static str, attached: bool, separate: bool) -> ToolFlag {
    ToolFlag { tools, flag, subcommand: None, attached, separate, after_colon: false, kind: Kind::Password }
}

const OPENSSL: ToolFlag = tf(&["openssl"], "", false, false);
const HTPASSWD: ToolFlag = tf(&["htpasswd", "htpasswd2"], "", false, false);

const TOOL_FLAGS: &[ToolFlag] = &[
    tf(MYSQL, "-p", true, false),
    tf(&["7z", "7za", "7zr", "7zz"], "-p", true, false),
    tf(&["sshpass"], "-p", true, true),
    tf(MONGO, "-p", true, true),
    tf(&["redis-cli"], "-a", true, true),
    tf(&["zip", "unzip", "sqlcmd", "bcp"], "-P", true, true),
    tf(&["plink", "pscp", "psftp"], "-pw", false, true),
    ToolFlag { subcommand: Some("login"), ..tf(REGISTRY, "-p", true, true) },
    ToolFlag { after_colon: true, ..tf(&["curl"], "-u", true, true) },
    ToolFlag { after_colon: true, ..tf(&["curl"], "-U", true, true) },
    ToolFlag { after_colon: true, ..tf(&["curl"], "--user", false, true) },
    ToolFlag { after_colon: true, ..tf(&["curl"], "--proxy-user", false, true) },
    // `-key`, `-keyform` and the like start with `-k` but are not attached values.
    tf(&["openssl"], "-k", false, true),
    // `gh variable set --body` is not a secret; `gh secret set --body` is.
    ToolFlag { subcommand: Some("secret"), kind: Kind::Secret, ..tf(&["gh"], "--body", true, true) },
    ToolFlag { subcommand: Some("secret"), kind: Kind::Secret, ..tf(&["gh"], "-b", false, true) },
];

/// Name bytes for variables, flags, properties and header names.
fn is_name_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-')
}

#[derive(Clone, Copy, PartialEq)]
enum Strength {
    /// The name could hold a count or a switch (`MAX_TOKEN=4096`), so a bare
    /// number is not taken for a secret.
    Weak,
    Strong,
}

/// Whether a variable, flag, property or header name holds a credential.
///
/// The name is split into words (`_ . -` and camelCase) and only its LAST
/// word decides: a name is about what its last word names. `API_KEY_FILE`,
/// `TOKEN_URL` and `SECRET_NAME` hold paths and labels; `TOKENIZERS_*` and
/// `MAX_TOKENS` hold no token at all. Substring matching redacted all of them.
fn secret_name(name: &str) -> Option<Secret> {
    // Conventional credential carriers whose last word says nothing.
    if name.eq_ignore_ascii_case("DATABASE_URL") || name.eq_ignore_ascii_case("SECRET_KEY_BASE") {
        return Some((Strength::Strong, Kind::Secret));
    }
    // The shell's working directory, not a password.
    if name.eq_ignore_ascii_case("PWD") {
        return None;
    }
    let words = name_words(name);
    let last = words.last()?.as_str();
    let prev = words.len().checked_sub(2).map(|i| words[i].as_str());
    match last {
        "PASSWORD" | "PASSWD" | "PASS" | "PWD" | "PASSPHRASE" | "SSHPASS" => {
            Some((Strength::Strong, Kind::Password))
        }
        "SECRET" | "CREDENTIAL" | "CREDENTIALS" | "CREDS" => Some((Strength::Strong, Kind::Secret)),
        // PGPASSWORD, MYSQLPASSWORD. Not a bare `*PASS` suffix: BYPASS, COMPASS.
        w if w.ends_with("PASSWORD") || w.ends_with("PASSWD") => Some((Strength::Strong, Kind::Password)),
        "TOKEN" | "PAT" | "AUTH" | "AUTHORIZATION" | "BEARER" | "COOKIE" => Some((Strength::Weak, Kind::Token)),
        // KEY alone is too common (SORT_KEY, --key client.pem); it needs a
        // qualifier, as a separate word or fused (APIKEY).
        "KEY" if prev.is_some_and(|p| KEY_QUALIFIERS.contains(&p)) => Some((Strength::Weak, Kind::Key)),
        w if w.strip_suffix("KEY").is_some_and(|p| KEY_QUALIFIERS.contains(&p)) => {
            Some((Strength::Weak, Kind::Key))
        }
        _ => None,
    }
}

/// A `--*-key` or `--*-secret` flag that `secret_name` does not know.
fn bare_key_flag(name: &str) -> bool {
    secret_name(name).is_none()
        && name_words(name.trim_start_matches('-')).last().is_some_and(|w| w == "KEY" || w == "SECRET")
}

/// Redact a flag's value when it reads as random base64, `/` and all.
fn push_random(s: &str, start: usize, end: usize, spans: &mut Vec<Span>) {
    if start < end && looks_random(unquote(&s[start..end])) {
        push_value(s, start, end, (Strength::Strong, Kind::Key), spans);
    }
}

const KEY_QUALIFIERS: &[&str] = &[
    "API", "SECRET", "ACCESS", "PRIVATE", "CLIENT", "SIGNING", "ENCRYPTION", "MASTER", "AUTH",
    "SESSION", "LICENSE",
];

/// A flag name whose next word is a credential. Switches that only mention
/// one (`--no-password`, `--ask-pass`, `--password-stdin`) take no value.
fn secret_flag_name(name: &str) -> Option<Secret> {
    let words = name_words(name);
    if matches!(words.first().map(String::as_str), Some("NO" | "ASK")) {
        return None;
    }
    secret_name(name)
}

fn name_words(name: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in name.chars() {
        if matches!(c, '_' | '.' | '-') {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_ascii_uppercase() && prev_lower && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
        cur.push(c.to_ascii_uppercase());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn is_auth_scheme(word: &str) -> bool {
    ["basic", "bearer", "token", "digest", "negotiate", "ntlm", "bot", "apikey"]
        .iter()
        .any(|s| word.eq_ignore_ascii_case(s))
}

/// How sure a name is that its value is a credential, and of what kind.
type Secret = (Strength, Kind);

fn push_value(s: &str, start: usize, end: usize, (strength, kind): Secret, spans: &mut Vec<Span>) {
    if start < end && !obviously_not_secret(&s[start..end], strength) {
        spans.push((start, end, kind));
    }
}

/// Values that name where a secret lives, or switch something on, rather
/// than being one. Keeping them keeps commands like
/// `GITHUB_TOKEN=$(gh auth token)` findable and leaks nothing: the other
/// detectors still scan the text.
fn obviously_not_secret(raw: &str, strength: Strength) -> bool {
    let v = unquote(raw);
    v.is_empty()
        || ["true", "false", "yes", "no", "on", "off", "none", "null"]
            .iter()
            .any(|b| v.eq_ignore_ascii_case(b))
        || is_reference(v)
        || ["~/", "./", "../"].iter().any(|p| v.starts_with(p))
        // A path, unless it could be base64 and reads as random: base64
        // starts with `/` one time in 64, and a secret must not get through
        // by doing so. A `.`, `_` or `-` anywhere rules base64 out.
        || (v.starts_with('/')
            && !(v.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'/' | b'+' | b'=')) && looks_random(v)))
        || (strength == Strength::Weak && v.bytes().all(|c| c.is_ascii_digit()))
}

fn unquote(v: &str) -> &str {
    let b = v.as_bytes();
    if b.len() >= 2 && (b[0] == b'\'' || b[0] == b'"') && b[b.len() - 1] == b[0] {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

/// `$VAR`, `${VAR}`, `$(cmd)` or `` `cmd` ``, and nothing else.
fn is_reference(v: &str) -> bool {
    (v.starts_with("${") && v.ends_with('}'))
        || (v.starts_with("$(") && v.ends_with(')'))
        || (v.len() >= 2 && v.starts_with('`') && v.ends_with('`'))
        || v.strip_prefix('$').is_some_and(|n| {
            !n.is_empty() && n.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        })
}

/// The value of a JSON member whose name ends just before `at`:
/// `"name": "value"` or `"name": 42`, with the quotes backslash-escaped when
/// the JSON sits inside a double-quoted shell word.
fn json_value(b: &[u8], at: usize, limit: usize) -> Option<(usize, usize)> {
    let quote = |i: usize| match (b.get(i), b.get(i + 1)) {
        (Some(b'"'), _) => Some(i + 1),
        (Some(b'\\'), Some(b'"')) => Some(i + 2),
        _ => None,
    };
    let spaces = |mut i: usize| {
        while i < limit && matches!(b[i], b' ' | b'\t') {
            i += 1;
        }
        i
    };
    let colon = spaces(quote(at)?);
    if colon >= limit || b[colon] != b':' {
        return None;
    }
    let start = spaces(colon + 1);
    match quote(start) {
        Some(inner) => {
            let close = (inner..limit).find(|&i| b[i] == b'"')?;
            Some((inner, if b[close - 1] == b'\\' { close - 1 } else { close }))
        }
        None => Some((start, (start..limit).find(|&i| b",} \t\n'\"".contains(&b[i])).unwrap_or(limit))),
    }
}

/// `scheme://user:PASSWORD@host`. Only the password goes; the user and host
/// are what you search for.
fn url_passwords(s: &str, spans: &mut Vec<Span>) {
    let b = s.as_bytes();
    let mut from = 0;
    while let Some(off) = s[from..].find("://") {
        let auth = from + off + 3;
        let auth_end = (auth..b.len())
            .find(|&i| b"/?#'\"`".contains(&b[i]) || b[i].is_ascii_whitespace())
            .unwrap_or(b.len());
        // The last '@' ends the userinfo; an unencoded '@' in a password
        // still belongs to it.
        if let Some(at) = s[auth..auth_end].rfind('@')
            && let Some(colon) = s[auth..auth + at].find(':')
        {
            push_value(s, auth + colon + 1, auth + at, (Strength::Strong, Kind::Password), spans);
        }
        from = auth;
    }
}

/// `Bearer <token>` outside a recognised header (`--header "Bearer x"`,
/// `Authorization:Bearer` split by the shell), however short the token.
fn bearer_tokens(cmd: &Command, spans: &mut Vec<Span>) {
    let s = cmd.s;
    let b = s.as_bytes();
    let lower = s.to_ascii_lowercase();
    let mut from = 0;
    while let Some(off) = lower[from..].find("bearer") {
        let at = from + off;
        from = at + 6;
        if at > 0 && b[at - 1].is_ascii_alphanumeric() {
            continue;
        }
        let mut v = from;
        while v < b.len() && matches!(b[v], b' ' | b'\t') {
            v += 1;
        }
        if v == from || v >= b.len() {
            continue;
        }
        // A quote right after `Bearer ` that ends the string it sits in
        // (`grep "Bearer " src/`) closes; only an opening one holds a token.
        if matches!(b[v], b'\'' | b'"') && cmd.q[v] != Q::Open {
            continue;
        }
        let (start, stop): (usize, &[u8]) = if matches!(b[v], b'\'' | b'"') {
            (v + 1, &b[v..=v])
        } else {
            (v, b" \t\n'\"`;|&<>)")
        };
        let end = (start..b.len()).find(|&i| stop.contains(&b[i])).unwrap_or(b.len());
        let token = &s[start..end];
        // "the bearer token is ..." in a commit message is prose.
        if !token.bytes().all(|c| c.is_ascii_lowercase()) {
            push_value(s, start, end, (Strength::Strong, Kind::Token), spans);
        }
    }
}

/// Keeps the BEGIN/END armour lines, which say what the block was.
fn private_key_blocks(s: &str, spans: &mut Vec<Span>) {
    let mut from = 0;
    while let Some(off) = s[from..].find("-----BEGIN ") {
        let header = from + off + "-----BEGIN ".len();
        let Some(close) = s[header..].find("-----") else {
            return;
        };
        let body = header + close + 5;
        from = body;
        if !s[header..header + close].contains("PRIVATE KEY") {
            continue;
        }
        let end = s[body..].find("-----END").map_or(s.len(), |e| body + e);
        spans.push((body, end, Kind::Key));
        from = end;
    }
}

/// Bare tokens: known vendor formats by prefix, JWTs, and a generic
/// random-looking-string check. Runs last and only over what the structural
/// detectors left: a value they already took keeps its own kind, and the flag
/// it is attached to (`-p` in `-pS3cretPw9x…`) is not swallowed with it.
fn token_runs(s: &str, spans: &mut Vec<Span>) {
    let b = s.as_bytes();
    let mut taken = vec![false; b.len()];
    for &(start, end, _) in spans.iter() {
        taken[start..end].fill(true);
    }
    let in_run = |i: usize| is_token_byte(b[i]) && !taken[i];
    let mut i = 0;
    while i < b.len() {
        if !in_run(i) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && in_run(i) {
            i += 1;
        }
        if let Some(end) = jwt_end(b, start, i) {
            spans.push((start, end, Kind::Token));
            i = end;
        } else if has_vendor_prefix(&s[start..i]) {
            spans.push((start, i, Kind::Token));
        } else if looks_random(&s[start..i]) {
            spans.push((start, i, Kind::Secret));
        }
    }
}

/// Base64/base64url characters, minus `/` and `=`. With `/` in the set a
/// whole path (`/usr/local/lib/python3/site-packages`) was one long "token";
/// `=` would glue a variable name to its value. The cost: a base64 secret
/// containing `/` is judged in pieces, so the generic check can miss it, and
/// only the name, flag and header rules catch it.
fn is_token_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'+')
}

fn has_vendor_prefix(t: &str) -> bool {
    let rest = |p: &str, min: usize| t.strip_prefix(p).is_some_and(|r| r.len() >= min);
    let aws = (t.starts_with("AKIA") || t.starts_with("ASIA"))
        && t.len() == 20
        && t.bytes().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    aws || ["ghp_", "gho_", "ghu_", "ghs_", "ghr_", "github_pat_", "glpat-", "npm_"]
        .iter()
        .any(|p| rest(p, 20))
        || ["xoxa-", "xoxb-", "xoxp-", "xoxr-", "xoxs-", "xapp-"].iter().any(|p| rest(p, 10))
        || ["sk_live_", "sk_test_", "rk_live_", "rk_test_"].iter().any(|p| rest(p, 16))
        || rest("AIza", 30)
        // OpenAI/Anthropic keys; the digit keeps `sk-some-long-package-name` out.
        || (rest("sk-", 20) && t.bytes().any(|c| c.is_ascii_digit()))
}

/// `header.payload.signature`, where header and payload are base64url JSON
/// objects and so both start with `eyJ` (`{"`).
fn jwt_end(b: &[u8], start: usize, end: usize) -> Option<usize> {
    let seg = |from: usize| (from..b.len()).find(|&i| !is_b64url(b[i])).unwrap_or(b.len());
    if !b[start..end].starts_with(b"eyJ") {
        return None;
    }
    let h = seg(start);
    if b.get(h) != Some(&b'.') || !b[h + 1..].starts_with(b"eyJ") {
        return None;
    }
    let p = seg(h + 1);
    if b.get(p) != Some(&b'.') {
        return None;
    }
    Some(seg(p + 1))
}

fn is_b64url(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
}

/// Long, uses upper, lower AND digits, and switches between them about as
/// often as random text does.
///
/// Two classes are not enough: git SHAs and digests are lower+digit, paths
/// and package names upper+lower, and none are secrets. Identifiers that do
/// mix all three (`TestParseRFC3339WithTimezone`, `Python3BuildEnvironment`)
/// are made of words, so their same-class runs are long: 2.5 characters or
/// more on average. Random base62 averages about 1.6, and fewer than 1% of
/// random 24-character strings reach 2.5. Pure-hex secrets are left to the
/// context rules, because a bare hex string is far more often a hash.
fn looks_random(t: &str) -> bool {
    if t.len() < 20 {
        return false;
    }
    let (mut upper, mut lower, mut digit) = (false, false, false);
    let mut alnum = 0usize;
    let mut runs = 0usize;
    let mut prev = 0u8;
    for c in t.bytes() {
        let class = if c.is_ascii_uppercase() {
            upper = true;
            1
        } else if c.is_ascii_lowercase() {
            lower = true;
            2
        } else if c.is_ascii_digit() {
            digit = true;
            3
        } else {
            prev = 0;
            continue;
        };
        alnum += 1;
        if class != prev {
            runs += 1;
        }
        prev = class;
    }
    upper && lower && digit && alnum * 2 < runs * 5
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each case is a real-world-shaped command and the exact safe text an
    /// import must produce. Must-keep cases (input == output) guard precision.
    const CASES: &[(&str, &str)] = &[
        ("curl https://admin:hunter2pass@example.com/api", "curl https://admin:[REDACTED:password]@example.com/api"),
        ("psql postgres://user:pgpass123@db.internal:5432/app", "psql postgres://user:[REDACTED:password]@db.internal:5432/app"),
        ("git clone https://oauth2:p@ss@gitlab.example.com/o/r.git", "git clone https://oauth2:[REDACTED:password]@gitlab.example.com/o/r.git"),
        ("redis-cli -u redis://default:r3disPw@cache:6379", "redis-cli -u redis://default:[REDACTED:password]@cache:6379"),
        ("mysql -u root -pS3cretPassw0rd db", "mysql -u root -p[REDACTED:password] db"),
        ("mysqldump -uroot -p'my pass' shop > shop.sql", "mysqldump -uroot -p[REDACTED:password] shop > shop.sql"),
        ("mysql -u root -p shop", "mysql -u root -p shop"),
        ("mysqladmin --password=MyPassw0rd! status", "mysqladmin --password=[REDACTED:password] status"),
        ("mysql --password hunter2 -e 'select 1'", "mysql --password [REDACTED:password] -e 'select 1'"),
        ("docker login -p MyDockerPass123 registry", "docker login -p [REDACTED:password] registry"),
        ("docker login -u bob --password s3cr3t ghcr.io", "docker login -u bob --password [REDACTED:password] ghcr.io"),
        ("docker run -p 8080:80 nginx", "docker run -p 8080:80 nginx"),
        ("echo $PW | docker login --password-stdin -u bob", "echo $PW | docker login --password-stdin -u bob"),
        ("sshpass -p 'hunter2' ssh host", "sshpass -p [REDACTED:password] ssh host"),
        ("sudo sshpass -phunter2 scp f host:", "sudo sshpass -p[REDACTED:password] scp f host:"),
        ("redis-cli -a r3dis! ping", "redis-cli -a [REDACTED:password] ping"),
        ("curl -u admin:Sup3rS3cret https://x.test", "curl -u admin:[REDACTED:password] https://x.test"),
        ("curl -u admin https://x.test", "curl -u admin https://x.test"),
        ("curl https://bob:S3cr3tPw@api.example.com/x", "curl https://bob:[REDACTED:password]@api.example.com/x"),
        ("git clone ssh://git@github.com:22/o/r.git", "git clone ssh://git@github.com:22/o/r.git"),
        ("zip -P zipPass archive.zip f", "zip -P [REDACTED:password] archive.zip f"),
        ("ansible-playbook --ask-pass site.yml", "ansible-playbook --ask-pass site.yml"),
        ("export password=lowercasesecret1", "export password=[REDACTED:password]"),
        ("export Api_Key=MixedCaseKey123", "export Api_Key=[REDACTED:key]"),
        ("db_pass=x9 ./migrate", "db_pass=[REDACTED:password] ./migrate"),
        ("token=abc ./run", "token=[REDACTED:token] ./run"),
        ("export GH_PAT=abc123secretvalue", "export GH_PAT=[REDACTED:token]"),
        ("export DB_PASS=dbpasssecret; ./deploy", "export DB_PASS=[REDACTED:password]; ./deploy"),
        ("OPENAI_API_KEY=sk-1234567890abcdef cargo run", "OPENAI_API_KEY=[REDACTED:key] cargo run"),
        ("MYSQL_PWD=mysqlsecret mysql", "MYSQL_PWD=[REDACTED:password] mysql"),
        ("PGPASSWORD=pgsecret psql -h db", "PGPASSWORD=[REDACTED:password] psql -h db"),
        ("API_KEY=\"my secret value\" ./x", "API_KEY=[REDACTED:key] ./x"),
        ("ANTHROPIC_API_KEY='sk-ant-api03-short value' claude", "ANTHROPIC_API_KEY=[REDACTED:key] claude"),
        ("export TOKEN=\\\nabcdef123456secret", "export TOKEN=[REDACTED:token]"),
        ("AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCY aws s3 ls", "AWS_SECRET_ACCESS_KEY=[REDACTED:key] aws s3 ls"),
        ("DATABASE_URL=postgres://u:p@h/db rails s", "DATABASE_URL=[REDACTED:secret] rails s"),
        ("java -Ddb.password=s3cret -jar app.jar", "java -Ddb.password=[REDACTED:password] -jar app.jar"),
        ("curl 'https://api.test/v1?user=bob&api_key=abc123&page=2'", "curl 'https://api.test/v1?user=bob&api_key=[REDACTED:key]&page=2'"),
        ("kubectl create secret generic db --from-literal=password=pw1", "kubectl create secret generic db --from-literal=password=[REDACTED:password]"),
        ("aws configure set aws_secret_access_key wJalrXUtnFEMI", "aws configure set aws_secret_access_key [REDACTED:key]"),
        ("curl -H 'Authorization: Bearer abc123' https://x.test", "curl -H 'Authorization: Bearer [REDACTED:token]' https://x.test"),
        ("curl -H \"Authorization: Basic dXNlcjpwYXNz\" x.test", "curl -H \"Authorization: Basic [REDACTED:token]\" x.test"),
        ("curl -H Authorization:Bearer xyz9 x.test", "curl -H Authorization:Bearer [REDACTED:token] x.test"),
        ("curl -H 'X-Api-Key: k1' x.test", "curl -H 'X-Api-Key: [REDACTED:key]' x.test"),
        ("http :8000 'Authorization:token s3cr3t'", "http :8000 'Authorization:token [REDACTED:token]'"),
        ("curl -H \"Authorization: Bearer $TOKEN\" x.test", "curl -H \"Authorization: Bearer $TOKEN\" x.test"),
        ("curl -H 'Content-Type: application/json' x.test", "curl -H 'Content-Type: application/json' x.test"),
        ("echo AKIAIOSFODNN7EXAMPLE", "echo [REDACTED:token]"),
        ("git clone https://ghp_A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6Q7r8@github.com/o/r", "git clone https://[REDACTED:token]@github.com/o/r"),
        ("gh auth login --with-token github_pat_11ABCDEFG0123456789_abcdefXYZ", "gh auth login --with-token [REDACTED:token]"),
        ("slack-cli send xoxb-1234567890-abcdefghij", "slack-cli send [REDACTED:token]"),
        ("jwt decode eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.dBjftJeZ4CVPmB92K27uhbUJU1p1r_wW1gFWFOEjXk", "jwt decode [REDACTED:token]"),
        ("echo '-----BEGIN RSA PRIVATE KEY-----\nMIIEow\n-----END RSA PRIVATE KEY-----' > k", "echo '-----BEGIN RSA PRIVATE KEY-----[REDACTED:key]-----END RSA PRIVATE KEY-----' > k"),
        ("curl -d k=1 aB3dE6gH9jK2mN5pQ8sT1vX4yZ7bC0eF3hI6k", "curl -d k=1 [REDACTED:secret]"),
        ("git checkout 3f2a9c1d7e4b5a6f8091a2b3c4d5e6f708192a3b", "git checkout 3f2a9c1d7e4b5a6f8091a2b3c4d5e6f708192a3b"),
        ("docker pull alpine@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855", "docker pull alpine@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"),
        ("cd /usr/local/lib/python3.12/site-packages/numpy2/core", "cd /usr/local/lib/python3.12/site-packages/numpy2/core"),
        ("vim /Users/yuan/Developer/relic/src/ui/model.rs", "vim /Users/yuan/Developer/relic/src/ui/model.rs"),
        ("cd ~/Developer/MyProject2/Sources/Components", "cd ~/Developer/MyProject2/Sources/Components"),
        ("open https://github.com/anthropics/claude-code/pull/12345", "open https://github.com/anthropics/claude-code/pull/12345"),
        ("npm install @typescript-eslint/eslint-plugin-Recommended", "npm install @typescript-eslint/eslint-plugin-Recommended"),
        ("go test -run TestParseRFC3339WithTimezone ./...", "go test -run TestParseRFC3339WithTimezone ./..."),
        ("TOKENIZERS_PARALLELISM=false python train.py", "TOKENIZERS_PARALLELISM=false python train.py"),
        ("MAX_TOKEN=4096 ./run", "MAX_TOKEN=4096 ./run"),
        ("API_KEY_FILE=/run/secrets/key ./svc", "API_KEY_FILE=/run/secrets/key ./svc"),
        ("export GITHUB_TOKEN=$(gh auth token)", "export GITHUB_TOKEN=$(gh auth token)"),
        ("HOME=/Users/me PATH=/usr/bin cargo build", "HOME=/Users/me PATH=/usr/bin cargo build"),
        ("ssh -i ~/.ssh/id_ed25519 git@github.com", "ssh -i ~/.ssh/id_ed25519 git@github.com"),
        ("kubectl logs my-app-7d9f8b6c5-x2j4k -n prod", "kubectl logs my-app-7d9f8b6c5-x2j4k -n prod"),
        ("git commit -m 'send the bearer token header'", "git commit -m 'send the bearer token header'"),
        ("rustup target add aarch64-unknown-linux-gnu", "rustup target add aarch64-unknown-linux-gnu"),
        ("uuidgen # 550e8400-e29b-41d4-a716-446655440000", "uuidgen # 550e8400-e29b-41d4-a716-446655440000"),
        ("echo 部署 café ✓", "echo 部署 café ✓"),
        ("git commit -m 'fix bug' --amend", "git commit -m 'fix bug' --amend"),
        ("echo abcdefghijklmnopqrstuvwxyzabcdefghijklmn", "echo abcdefghijklmnopqrstuvwxyzabcdefghijklmn"),
        ("git config --global credential.helper osxkeychain", "git config --global credential.helper osxkeychain"),
        ("echo é=1 ü:2 café_token=x", "echo é=1 ü:2 café_token=[REDACTED:token]"),
        ("", ""),
        ("echo literal [REDACTED] markertest", "echo literal [REDACTED] markertest"),
        ("PGPASSWORD=x psql -c 'select \"[REDACTED]\"'", "PGPASSWORD=[REDACTED:password] psql -c 'select \"[REDACTED]\"'"),
        // Rules ADR 0003 added over the first corpus, with their must-keep twins.
        ("echo hunter2 | sudo -S apt update", "echo [REDACTED:password] | sudo -S apt update"),
        ("printf '%s\\n' 'my pass' | sudo -S -k systemctl restart nginx", "printf [REDACTED:password] | sudo -S -k systemctl restart nginx"),
        ("sudo -S whoami <<< hunter2", "sudo -S whoami <<< [REDACTED:password]"),
        ("echo hello | sudo tee /etc/motd", "echo hello | sudo tee /etc/motd"),
        ("echo done | grep d", "echo done | grep d"),
        ("htpasswd -b /etc/nginx/.htpasswd alice S3cretPw", "htpasswd -b /etc/nginx/.htpasswd alice [REDACTED:password]"),
        ("htpasswd -cbB .htpasswd bob 'pass word'", "htpasswd -cbB .htpasswd bob [REDACTED:password]"),
        ("htpasswd -nb carol pw123", "htpasswd -nb carol [REDACTED:password]"),
        ("htpasswd .htpasswd dave", "htpasswd .htpasswd dave"),
        ("openssl enc -aes-256-cbc -k MyPassphrase -in a -out b", "openssl enc -aes-256-cbc -k [REDACTED:password] -in a -out b"),
        ("openssl enc -aes-256-cbc -pass pass:hunter2 -in a", "openssl enc -aes-256-cbc -pass pass:[REDACTED:password] -in a"),
        ("openssl rsa -in k.pem -passin 'pass:old pw' -passout pass:newpw", "openssl rsa -in k.pem -passin 'pass:[REDACTED:password]' -passout pass:[REDACTED:password]"),
        ("openssl enc -pass env:PW -in a", "openssl enc -pass env:PW -in a"),
        ("openssl req -new -key server.key -out server.csr", "openssl req -new -key server.key -out server.csr"),
        ("gh secret set NPM_TOKEN --body abc123", "gh secret set NPM_TOKEN --body [REDACTED:secret]"),
        ("gh secret set NPM_TOKEN -b 'abc 123' --repo o/r", "gh secret set NPM_TOKEN -b [REDACTED:secret] --repo o/r"),
        ("gh secret set NPM_TOKEN --body=abc123", "gh secret set NPM_TOKEN --body=[REDACTED:secret]"),
        ("gh variable set REGION --body us-east-1", "gh variable set REGION --body us-east-1"),
        ("tool --signing-key wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY", "tool --signing-key [REDACTED:key]"),
        ("tool --hmac-secret=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY=", "tool --hmac-secret=[REDACTED:secret]"),
        ("tool --sort-key name --partition-key userId", "tool --sort-key name --partition-key userId"),
        ("ssh-keygen -f ~/.ssh/My-Deploy-Key2", "ssh-keygen -f ~/.ssh/My-Deploy-Key2"),
        ("echo typed [REDACTED:password] here", "echo typed [REDACTED:literal:password] here"),
        ("PGPASSWORD=x psql -c 'select \"[REDACTED:token]\"'", "PGPASSWORD=[REDACTED:password] psql -c 'select \"[REDACTED:literal:token]\"'"),
        ("cat k | grep -----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\n-----END OPENSSH PRIVATE KEY-----", "cat k | grep -----BEGIN OPENSSH PRIVATE KEY-----[REDACTED:key]-----END OPENSSH PRIVATE KEY-----"),
        // Found in review: carriers the first rules missed.
        ("curl -d '{\"username\":\"bob\",\"password\":\"hunter2\"}' https://api/login", "curl -d '{\"username\":\"bob\",\"password\":\"[REDACTED:password]\"}' https://api/login"),
        ("curl -d \"{\\\"api_key\\\": \\\"abc123def\\\"}\" x.test", "curl -d \"{\\\"api_key\\\": \\\"[REDACTED:key]\\\"}\" x.test"),
        ("curl -d '{\"max_token\": 4096, \"token\": 12345678}' x.test", "curl -d '{\"max_token\": 4096, \"token\": 12345678}' x.test"),
        ("htpasswd -nb admin hunter2 >> /etc/nginx/.htpasswd", "htpasswd -nb admin [REDACTED:password] >> /etc/nginx/.htpasswd"),
        ("htpasswd -b .htpasswd admin hunter2 > out.log", "htpasswd -b .htpasswd admin [REDACTED:password] > out.log"),
        ("gh auth login --with-token < mytoken.txt", "gh auth login --with-token < mytoken.txt"),
        ("mysql -u root --password < dump.sql", "mysql -u root --password < dump.sql"),
        ("echo hunter2 | sudo -u root -S ls", "echo [REDACTED:password] | sudo -u root -S ls"),
        ("echo big | sudo ls -S /var", "echo big | sudo ls -S /var"),
        ("AWS_SECRET_ACCESS_KEY=/wJalrXUtnFEMI/K7MDENG/bPxRfiCY aws s3 ls", "AWS_SECRET_ACCESS_KEY=[REDACTED:key] aws s3 ls"),
        ("GOOGLE_APPLICATION_CREDENTIALS=/Users/me/Keys2/Prod.json gcloud auth list", "GOOGLE_APPLICATION_CREDENTIALS=/Users/me/Keys2/Prod.json gcloud auth list"),
        // Found in review: ordinary commands the first rules damaged.
        ("cargo test auth::login", "cargo test auth::login"),
        ("cargo test token::tests::parses", "cargo test token::tests::parses"),
        ("docker pull myorg/auth:v2", "docker pull myorg/auth:v2"),
        ("jq '{token: .x}' f.json", "jq '{token: .x}' f.json"),
        ("grep -rn \"password:\" config/", "grep -rn \"password:\" config/"),
        ("grep -n token: values.yaml", "grep -n token: values.yaml"),
        ("grep -r \"Bearer \" src/", "grep -r \"Bearer \" src/"),
        ("psql \"host=db password=hunter2 dbname=app\"", "psql \"host=db password=[REDACTED:password] dbname=app\""),
        ("ssh host \"DB_PASSWORD=x ./deploy.sh --all\"", "ssh host \"DB_PASSWORD=[REDACTED:password] ./deploy.sh --all\""),
        ("git commit -m \"set password=foo and fix the bug\"", "git commit -m \"set password=[REDACTED:password] and fix the bug\""),
    ];

    #[test]
    fn corpus() {
        let r = Redactor::new();
        let failures: Vec<String> = CASES
            .iter()
            .filter_map(|&(input, want)| {
                let got = r.import(input);
                (got != want).then(|| format!("\n  in:   {input:?}\n  want: {want:?}\n  got:  {got:?}"))
            })
            .collect();
        assert!(failures.is_empty(), "{} of {} cases:{}", failures.len(), CASES.len(), failures.concat());
    }

    /// Safe text is redacted again on a rescrub and on every model egress.
    #[test]
    fn redacting_safe_text_again_changes_nothing() {
        let r = Redactor::new();
        for &(input, _) in CASES {
            let safe = r.import(input);
            assert_eq!(r.redact(&safe), safe, "second pass over {input:?}");
        }
        // A marker inside a value that is itself redacted is kept, not covered.
        let nested = "password=ab[REDACTED:token]cd";
        assert_eq!(r.redact(nested), "password=[REDACTED:password][REDACTED:token][REDACTED:password]");
        assert_eq!(r.redact(&r.redact(nested)), r.redact(nested));
    }

    #[test]
    fn a_marker_the_user_typed_is_not_one_the_redactor_wrote() {
        let r = Redactor::new();
        let typed = r.import("echo [REDACTED:password] and PGPASSWORD=x");
        assert_eq!(typed, "echo [REDACTED:literal:password] and PGPASSWORD=[REDACTED:password]");
        // Only the real one is a marker, to the UI and to a later pass.
        let found: Vec<&str> = markers(&typed).map(|m| &typed[m]).collect();
        assert_eq!(found, ["[REDACTED:password]"]);
        assert_eq!(r.redact(&typed), typed);

        // And the search index can tell them apart: the real marker is the
        // phrase "redacted password", the typed one is not.
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(x, tokenize='unicode61 remove_diacritics 2');
             INSERT INTO t VALUES ('echo [REDACTED:literal:password] typed');
             INSERT INTO t VALUES ('PGPASSWORD=[REDACTED:password] real');",
        )
        .unwrap();
        let hits: Vec<String> = conn
            .prepare("SELECT x FROM t WHERE t MATCH '\"redacted password\"'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(hits, ["PGPASSWORD=[REDACTED:password] real"]);
    }

    #[test]
    fn only_a_known_kind_makes_a_marker() {
        let text = "[REDACTED] [REDACTED:] [REDACTED:nonsense] [REDACTED:key] [REDACTED:keys]";
        assert_eq!(markers(text).map(|m| &text[m]).collect::<Vec<_>>(), ["[REDACTED:key]"]);
    }

    /// Detectors slice by byte offsets; no input may make one land inside a
    /// UTF-8 character, or the whole import panics.
    #[test]
    fn arbitrary_text_never_panics_and_settles_in_one_pass() {
        let alphabet: Vec<char> = "aZ9_-=:@/'\"\\ $()`\n.&?é部✓".chars().collect();
        let r = Redactor::new();
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..20_000 {
            let len = (seed >> 59) as usize;
            let text: String = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    alphabet[(seed % alphabet.len() as u64) as usize]
                })
                .collect();
            for input in [
                format!("password={text}"),
                format!("curl -H 'Authorization: {text}' https://u:{text}@h"),
                format!("git config {text} x [REDACTED:key]{text}"),
                text,
            ] {
                let safe = r.import(&input);
                assert_eq!(r.redact(&safe), safe, "second pass over {input:?}");
            }
        }
    }
}
