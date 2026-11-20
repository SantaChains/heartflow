//! Best-effort filesystem permission hardening for heartflow's own write
//! surface (security.dev.md §七 D9). On Unix the persisted surfaces (session
//! snapshots, SQLite mirror, checkpoint shadow git, jobs, cassettes) tighten
//! to owner-only; on Windows the user-profile ACL is already single-user by
//! default, so the calls degrade to no-ops rather than pulling an ACL crate.

use std::path::Path;

/// Owner-only file permissions (`0o600` on Unix). Best effort by contract:
/// a failed re-permission must never fail the save that produced the file —
/// the content writers already carry their own error paths.
pub fn harden_file(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mut perms = metadata.permissions();
            perms.set_mode(0o600);
            let _ = std::fs::set_permissions(path, perms);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

/// Owner-only directory permissions (`0o700` on Unix): traversal into the
/// directory is blocked entirely, which also covers files created inside it
/// later (SQLite WAL/SHM siblings, git objects). Best effort, same as
/// [`harden_file`].
pub fn harden_dir(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            if metadata.is_dir() {
                let mut perms = metadata.permissions();
                perms.set_mode(0o700);
                let _ = std::fs::set_permissions(path, perms);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{harden_dir, harden_file};
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hf-private-fs-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn file_tightens_to_owner_only() {
        let dir = scratch("file");
        let path = dir.join("data.json");
        fs::write(&path, "{}").expect("write");
        harden_file(&path);
        let mode = fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "file mode must be owner-only");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dir_tightens_to_owner_only_and_ignores_files() {
        let dir = scratch("dir");
        let path = dir.join("nested");
        fs::create_dir_all(&path).expect("nested dir");
        harden_dir(&path);
        let mode = fs::metadata(&path).expect("meta").permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "dir mode must be owner-only");
        // A plain file is not a directory: the hardening call must leave it.
        let file = dir.join("plain.txt");
        fs::write(&file, "x").expect("write");
        harden_dir(&file);
        let file_mode = fs::metadata(&file).expect("meta").permissions().mode();
        assert_ne!(file_mode & 0o777, 0o700, "file must not get dir mode");
        let _ = fs::remove_dir_all(&dir);
    }
}
