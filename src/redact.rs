//! Redaction pipeline: detects and replaces sensitive patterns in command text.
//!
//! For the prototype spike, uses hand-written structural detectors.
//! Production will add aho-corasick/regex-automata after binary-size measurement.

const REDACTED_TOKEN: &str = "[REDACTED]";

/// Known secret-bearing environment variable prefixes and exact names.
const SECRET_VAR_PATTERNS: &[&str] = &[
    "AWS_SECRET",
    "AWS_ACCESS_KEY",
    "API_KEY",
    "SECRET_KEY",
    "PRIVATE_KEY",
    "DATABASE_URL",
    "DB_PASSWORD",
    "TOKEN",
    "GITHUB_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "AUTH_TOKEN",
    "PASSWORD",
];

/// A compiled redaction pipeline. Compile once per process, apply to each record.
pub struct Redactor {
    _private: (), // No config for now; the struct exists for future expansion.
}

impl Redactor {
    pub fn new() -> Self {
        Self { _private: () }
    }

    /// Redact sensitive patterns from command text. Returns the safe version.
    pub fn redact(&self, input: &str) -> String {
        let mut result = input.to_string();

        // 1. Inline env var assignments: FOO=<value> -> FOO=[REDACTED]
        result = redact_env_assignments(&result);

        // 2. Common token/key patterns in arguments.
        result = redact_long_hex_or_base64(&result);

        result
    }
}

impl Default for Redactor {
    fn default() -> Self {
        Self::new()
    }
}

/// Redact `VAR=value` patterns where VAR matches a secret name.
fn redact_env_assignments(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.char_indices().peekable();

    while let Some((i, ch)) = chars.next() {
        // Look for word boundary + UPPERCASE_NAME= pattern.
        if ch.is_ascii_uppercase() || ch == '_' {
            let var_start = i;
            let mut var_end = i + ch.len_utf8();

            // Consume the variable name (uppercase + digits + underscore).
            while let Some(&(j, c)) = chars.peek() {
                if c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' {
                    var_end = j + c.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }

            let var_name = &input[var_start..var_end];

            // Check for '=' immediately after.
            if let Some(&(_, '=')) = chars.peek() {
                chars.next(); // consume '='

                if is_secret_var(var_name) {
                    result.push_str(var_name);
                    result.push('=');
                    result.push_str(REDACTED_TOKEN);

                    // Skip the value (until whitespace or end).
                    while let Some(&(_, c)) = chars.peek() {
                        if c.is_ascii_whitespace() {
                            break;
                        }
                        chars.next();
                    }
                    continue;
                }
                // Not a secret var — emit normally.
                result.push_str(var_name);
                result.push('=');
            } else {
                result.push_str(var_name);
            }
        } else {
            result.push(ch);
        }
    }

    result
}

fn is_secret_var(name: &str) -> bool {
    SECRET_VAR_PATTERNS.iter().any(|&pattern| {
        name == pattern || name.ends_with(pattern) || name.starts_with(pattern)
    })
}

/// Redact long hex/base64-like strings that look like API keys or tokens.
/// Heuristic: 32+ contiguous alphanumeric/base64 chars not in a known-safe context.
fn redact_long_hex_or_base64(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;

    while i < bytes.len() {
        if is_token_char(bytes[i]) {
            let start = i;
            while i < bytes.len() && is_token_char(bytes[i]) {
                i += 1;
            }
            let token = &input[start..i];

            if looks_like_secret_token(token) {
                result.push_str(REDACTED_TOKEN);
            } else {
                result.push_str(token);
            }
        } else {
            result.push(bytes[i] as char);
            i += 1;
        }
    }

    result
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'+' || b == b'/'
}

fn looks_like_secret_token(s: &str) -> bool {
    // Must be at least 32 chars to be suspicious.
    if s.len() < 32 {
        return false;
    }

    // Must have mixed case or digits to look like a key (not a long word/path).
    let has_upper = s.bytes().any(|b| b.is_ascii_uppercase());
    let has_lower = s.bytes().any(|b| b.is_ascii_lowercase());
    let has_digit = s.bytes().any(|b| b.is_ascii_digit());

    // High entropy indicator: mixed char classes.
    let mixed_classes = (has_upper as u8) + (has_lower as u8) + (has_digit as u8);
    mixed_classes >= 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redact_env_var_assignment() {
        let r = Redactor::new();
        let input = "AWS_SECRET_ACCESS_KEY=AKIAIOSFODNN7EXAMPLE cargo deploy";
        let output = r.redact(input);
        assert!(output.contains("AWS_SECRET_ACCESS_KEY=[REDACTED]"));
        assert!(output.contains("cargo deploy"));
        assert!(!output.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn redact_api_key_assignment() {
        let r = Redactor::new();
        let input = "OPENAI_API_KEY=sk-1234567890abcdef cargo run";
        let output = r.redact(input);
        assert!(output.contains("OPENAI_API_KEY=[REDACTED]"));
        assert!(!output.contains("sk-1234567890abcdef"));
    }

    #[test]
    fn redact_preserves_safe_vars() {
        let r = Redactor::new();
        let input = "HOME=/Users/me PATH=/usr/bin cargo build";
        let output = r.redact(input);
        assert!(output.contains("HOME=/Users/me"));
        assert!(output.contains("PATH=/usr/bin"));
    }

    #[test]
    fn redact_long_token_in_arg() {
        let r = Redactor::new();
        // 40-char mixed-case alphanumeric string looks like an API key.
        let input = "curl -H Authorization:Bearer aB3dE6gH9jK2mN5pQ8sT1vX4yZ7bC0eF3hI6k";
        let output = r.redact(input);
        assert!(output.contains("[REDACTED]"));
        assert!(!output.contains("aB3dE6gH9jK2mN5pQ8sT1vX4yZ7bC0eF3hI6k"));
    }

    #[test]
    fn redact_preserves_short_args() {
        let r = Redactor::new();
        let input = "git commit -m 'fix bug' --amend";
        let output = r.redact(input);
        assert_eq!(output, input);
    }

    #[test]
    fn redact_preserves_normal_paths() {
        let r = Redactor::new();
        let input = "cat /usr/local/etc/config.toml";
        let output = r.redact(input);
        assert_eq!(output, input);
    }

    #[test]
    fn redact_long_lowercase_word_not_secret() {
        let r = Redactor::new();
        // All lowercase, 40 chars — not a key, just a long word.
        let input = "echo abcdefghijklmnopqrstuvwxyzabcdefghijklmn";
        let output = r.redact(input);
        assert_eq!(output, input);
    }

    #[test]
    fn redact_empty_input() {
        let r = Redactor::new();
        assert_eq!(r.redact(""), "");
    }

    #[test]
    fn redact_multiple_secrets_in_one_line() {
        let r = Redactor::new();
        let input = "API_KEY=secret123 DATABASE_URL=postgres://user:pass@host/db";
        let output = r.redact(input);
        assert!(output.contains("API_KEY=[REDACTED]"));
        assert!(output.contains("DATABASE_URL=[REDACTED]"));
    }
}
