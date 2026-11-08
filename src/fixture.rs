//! Deterministic fixture generation for benchmarks and tests.
//!
//! Generates reproducible zsh/bash history corpora with a fixed seed.

/// Generate a deterministic zsh extended history corpus with `count` records.
/// Returns the raw bytes suitable for writing to a file or parsing directly.
pub fn generate_zsh_history(count: usize, seed: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(count * 60);
    let mut rng = SimpleRng::new(seed);

    let commands = [
        "cargo build --release",
        "cargo test --lib",
        "git status",
        "git push origin main",
        "git log --oneline -10",
        "cd /tmp",
        "ls -la",
        "cat README.md",
        "npm install",
        "docker compose up -d",
        "kubectl get pods",
        "ssh user@server.example.com",
        "curl https://api.example.com/health",
        "python3 -m pytest tests/",
        "make clean && make",
        "grep -r TODO src/",
        "find . -name '*.rs' | wc -l",
        "echo hello world",
        "ps aux | grep relic",
        "tail -f /var/log/system.log",
    ];

    let base_ts: u64 = 1_700_000_000;

    for i in 0..count {
        let ts = base_ts + i as u64;
        let dur = rng.next_u64() % 30;
        let cmd = commands[(rng.next_u64() as usize) % commands.len()];

        // Occasionally add a multi-line command via backslash continuation.
        if rng.next_u64().is_multiple_of(20) {
            buf.extend_from_slice(
                format!(": {ts}:{dur};echo line1 \\\nline2 \\\nline3\n").as_bytes(),
            );
        } else {
            buf.extend_from_slice(format!(": {ts}:{dur};{cmd}\n").as_bytes());
        }
    }

    buf
}

/// Generate a deterministic bash history corpus with timestamps.
pub fn generate_bash_history(count: usize, seed: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(count * 40);
    let mut rng = SimpleRng::new(seed);

    let commands = [
        "ls -la",
        "cd /home/user",
        "git diff",
        "make test",
        "vim config.yaml",
        "systemctl status nginx",
        "journalctl -u app -f",
        "apt update && apt upgrade",
        "cargo clippy",
        "rg pattern src/",
    ];

    let base_ts: u64 = 1_700_000_000;

    for i in 0..count {
        let ts = base_ts + i as u64;
        let cmd = commands[(rng.next_u64() as usize) % commands.len()];
        buf.extend_from_slice(format!("#{ts}\n{cmd}\n").as_bytes());
    }

    buf
}

/// Generate a diverse zsh history with realistic command distributions.
/// Commands include randomized arguments (paths, branches, flags) to produce
/// realistic term frequencies — most terms appear in <1% of records.
pub fn generate_diverse_zsh_history(count: usize, seed: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(count * 80);
    let mut rng = SimpleRng::new(seed);

    let prefixes = [
        "git", "cargo", "docker", "kubectl", "npm", "pip", "ssh", "curl", "make", "cd", "ls",
        "cat", "grep", "find", "sed", "awk", "tar", "chmod", "chown", "mv", "cp", "rm", "mkdir",
        "python3", "node", "go", "rustc", "gcc", "vim", "code",
    ];

    let git_subcmds = [
        "commit",
        "push",
        "pull",
        "checkout",
        "branch",
        "merge",
        "rebase",
        "stash",
        "log",
        "diff",
        "status",
        "add",
        "reset",
        "cherry-pick",
        "tag",
        "fetch",
        "clone",
        "bisect",
    ];

    let paths = [
        "src/main.rs",
        "src/lib.rs",
        "tests/integration.rs",
        "config/prod.toml",
        "docs/README.md",
        "Cargo.toml",
        "/tmp/output.log",
        "/var/log/app.log",
        "~/.config/relic",
        "pkg/server/handler.go",
        "internal/db/migrations",
        "frontend/src/App.tsx",
        "backend/api/routes.py",
    ];

    let flags = [
        "--release",
        "--verbose",
        "-v",
        "--force",
        "-f",
        "--dry-run",
        "--all",
        "-a",
        "--recursive",
        "-r",
        "--quiet",
        "-q",
        "--no-cache",
        "--watch",
        "--json",
        "--format=table",
    ];

    let base_ts: u64 = 1_700_000_000;

    for i in 0..count {
        let ts = base_ts + i as u64;
        let dur = rng.next_u64() % 30;
        let prefix = prefixes[(rng.next_u64() as usize) % prefixes.len()];

        let cmd = match prefix {
            "git" => {
                let sub = git_subcmds[(rng.next_u64() as usize) % git_subcmds.len()];
                let path = paths[(rng.next_u64() as usize) % paths.len()];
                format!("git {sub} {path}")
            }
            "cd" => {
                let path = paths[(rng.next_u64() as usize) % paths.len()];
                format!("cd {path}")
            }
            _ => {
                let flag = flags[(rng.next_u64() as usize) % flags.len()];
                let path = paths[(rng.next_u64() as usize) % paths.len()];
                let id = rng.next_u64() % 10000;
                format!("{prefix} {flag} {path} # task-{id}")
            }
        };

        buf.extend_from_slice(format!(": {ts}:{dur};{cmd}\n").as_bytes());
    }

    buf
}

/// Minimal xorshift64 PRNG for deterministic fixture generation.
struct SimpleRng {
    state: u64,
}

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    fn next_u64(&mut self) -> u64 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_zsh_deterministic() {
        let a = generate_zsh_history(100, 42);
        let b = generate_zsh_history(100, 42);
        assert_eq!(a, b, "same seed must produce identical output");
    }

    #[test]
    fn generate_zsh_correct_count() {
        let data = generate_zsh_history(1000, 42);
        assert_eq!(crate::source::zsh::Parser::new(&data[..]).count(), 1000);
    }

    #[test]
    fn generate_bash_deterministic() {
        let a = generate_bash_history(100, 42);
        let b = generate_bash_history(100, 42);
        assert_eq!(a, b);
    }

    #[test]
    fn generate_bash_correct_count() {
        let data = generate_bash_history(1000, 42);
        assert_eq!(
            crate::source::bash::Parser::new(&data[..], true).count(),
            1000
        );
    }
}
