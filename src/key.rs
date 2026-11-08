//! The installation key: 32 random bytes in a 0600 file beside the database.
//! It keys only the import boundary guard, so a leaked database alone cannot
//! be used to brute-force the raw bytes a guard covers. Losing it is harmless:
//! the next import cannot verify its guard and anchor-aligns instead
//! (ADR 0002).

use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

pub type InstallationKey = [u8; 32];

/// The key at `path`, and whether it had to be written fresh because the file
/// was missing, unreadable or not a key.
pub fn load_or_create(path: &Path) -> io::Result<(InstallationKey, bool)> {
    match fs::read(path) {
        Ok(bytes) => {
            if let Ok(key) = InstallationKey::try_from(bytes) {
                return Ok((key, false));
            }
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        // Anything else may be a good key behind a passing fault: replacing
        // it would orphan every stored guard for nothing.
        Err(e) => return Err(e),
    }
    let mut key = InstallationKey::default();
    getrandom::fill(&mut key).map_err(io::Error::other)?;
    // Removed first: `mode` applies only to a file this open creates, and an
    // unreadable leftover could not be overwritten in place anyway.
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
        _ => {}
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&key)?;
    Ok((key, true))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_key_is_created_owner_only_and_then_reloaded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relic.db.key");

        let (key, created) = load_or_create(&path).unwrap();
        assert!(created);
        assert_ne!(key, InstallationKey::default());
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(load_or_create(&path).unwrap(), (key, false));
    }

    #[test]
    fn a_file_that_is_not_a_key_is_replaced_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relic.db.key");
        fs::write(&path, b"short").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let (key, created) = load_or_create(&path).unwrap();
        assert!(created);
        assert_eq!(fs::read(&path).unwrap(), key);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn a_key_that_cannot_be_read_is_an_error_and_is_left_in_place() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("relic.db.key");
        let (key, _) = load_or_create(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();

        assert_eq!(
            load_or_create(&path).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load_or_create(&path).unwrap(), (key, false));
    }

    #[test]
    fn a_key_that_cannot_be_written_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load_or_create(&dir.path().join("missing_dir/relic.db.key")).is_err());
    }
}
