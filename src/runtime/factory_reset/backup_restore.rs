//! Backup and restore operations for factory reset preserve paths.
//!
//! Matches legacy bash `factory_reset_backup()`, `factory_reset_restore()`,
//! and `iterate_restore_list()` from the factory-reset script.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::error::{FactoryResetError, Result};

const CP_CMD: &str = "/bin/cp";
const SYNC_CMD: &str = "/bin/sync";

/// Backup a single path from `rootfs/path` to `backup_dir/rootfs/path`.
///
/// Matches legacy:
/// ```sh
/// cp --parents -av "${ROOTFS_DIR}/${path}" "${factory_reset_backup_dir}"
/// sync
/// ```
pub fn backup_path(rootfs: &Path, path: &str, backup_dir: &Path) -> Result<()> {
    let src = rootfs.join(path.trim_start_matches('/'));

    if !src.exists() {
        log::info!("backup: {} does not exist; skipping", path);
        return Ok(());
    }

    log::info!("backup: {}", path);

    let output = Command::new(CP_CMD)
        .args(["--parents", "-a"])
        .arg(&src)
        .arg(backup_dir)
        .output()
        .map_err(|e| FactoryResetError::BackupFailed {
            path: path.to_string(),
            reason: format!("Failed to run cp: {}", e),
        })?;

    if !output.status.success() {
        return Err(FactoryResetError::BackupFailed {
            path: path.to_string(),
            reason: format!(
                "cp failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        }
        .into());
    }

    run_sync()?;
    Ok(())
}

/// Restore a single path from `backup_dir/rootfs/path` back into the mounted rootfs.
///
/// Nested mount detection: walk up the destination path hierarchy until a
/// mountpoint is found — this is the target for the cp. Matches legacy:
/// ```sh
/// while [[ "${destpath}" != "${ROOTFS_DIR}" ]]; do
///     mountpoint -q ${destpath} && nested_mount=... && break
///     destpath=$(dirname ${destpath})
/// done
/// ```
///
/// Returns error code 66 (like legacy) if no mountpoint is found.
pub fn restore_path(rootfs: &Path, path: &str, backup_dir: &Path) -> Result<()> {
    let path_stripped = path.trim_start_matches('/');
    let backup_src = backup_dir
        .join(rootfs.strip_prefix("/").unwrap_or(rootfs))
        .join(path_stripped);

    if !backup_src.exists() {
        log::info!("restore: {} does not exist in backup; skipping", path);
        return Ok(());
    }

    log::info!("restore: {}", path);

    // Find the deepest ancestor of rootfs/path that is a mountpoint.
    let dest_full = rootfs.join(path_stripped);
    let nested_mount = find_nested_mountpoint(rootfs, &dest_full)?;

    let nested_mount = nested_mount.ok_or_else(|| FactoryResetError::RestoreFailed {
        path: path.to_string(),
        reason: format!(
            "no mountpoint found in destination path (error 66): {}",
            rootfs.join(path_stripped).display()
        ),
    })?;

    // cd to backup_dir/rootfs/nested_mount, then cp --parents -a <relative_path> rootfs/nested_mount
    let backup_nested = backup_dir
        .join(rootfs.strip_prefix("/").unwrap_or(rootfs))
        .join(nested_mount.trim_start_matches('/'));

    // Path relative to nested_mount
    let relative_path = path_stripped
        .strip_prefix(nested_mount.trim_start_matches('/'))
        .unwrap_or(path_stripped)
        .trim_start_matches('/');

    let dest_mount = rootfs.join(nested_mount.trim_start_matches('/'));

    log::debug!(
        "restore nested: cd {} && cp --parents -a {} {}",
        backup_nested.display(),
        relative_path,
        dest_mount.display()
    );

    let output = Command::new(CP_CMD)
        .current_dir(&backup_nested)
        .args(["--parents", "-a"])
        .arg(relative_path)
        .arg(&dest_mount)
        .output()
        .map_err(|e| FactoryResetError::RestoreFailed {
            path: path.to_string(),
            reason: format!("Failed to run cp: {}", e),
        })?;

    if !output.status.success() {
        return Err(FactoryResetError::RestoreFailed {
            path: path.to_string(),
            reason: format!(
                "cp failed ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        }
        .into());
    }

    run_sync()?;
    Ok(())
}

/// Backup all paths in the preserve list. Aborts on first failure.
pub fn backup_all(rootfs: &Path, preserve_list: &[String], backup_dir: &Path) -> Result<()> {
    fs::create_dir_all(backup_dir).map_err(FactoryResetError::Io)?;
    for path in preserve_list {
        backup_path(rootfs, path, backup_dir)?;
    }
    Ok(())
}

/// Restore all paths. Accumulates errors (non-fatal per path) and returns
/// a combined context string if any restores failed — matching legacy behaviour.
pub fn restore_all(
    rootfs: &Path,
    preserve_list: &[String],
    backup_dir: &Path,
) -> Result<RestoreResult> {
    let mut failed_context: Vec<String> = Vec::new();
    let mut last_error: Option<String> = None;

    for path in preserve_list {
        if let Err(e) = restore_path(rootfs, path, backup_dir) {
            log::warn!("restore failed for {}: {}", path, e);
            failed_context.push(path.clone());
            last_error = Some(e.to_string());
        }
    }

    if failed_context.is_empty() {
        Ok(RestoreResult::Success)
    } else {
        let context = format!("{}:restore", failed_context.join(";"));
        Ok(RestoreResult::PartialFailure {
            context,
            error: last_error.unwrap_or_default(),
        })
    }
}

/// Result of a restore_all call.
pub enum RestoreResult {
    Success,
    PartialFailure { context: String, error: String },
}

/// Walk up from `dest` to `rootfs`, returning the relative path of the
/// deepest ancestor that is a mountpoint (as reported by `mountpoint -q`).
fn find_nested_mountpoint(rootfs: &Path, dest: &Path) -> Result<Option<String>> {
    let mut current = dest.parent().unwrap_or(dest).to_path_buf();

    loop {
        if current == rootfs {
            break;
        }

        if is_mountpoint(&current) {
            // Return the relative part (strip rootfs prefix)
            let relative = current
                .strip_prefix(rootfs)
                .map(|p| format!("/{}", p.display()))
                .unwrap_or_else(|_| current.display().to_string());
            return Ok(Some(relative));
        }

        match current.parent() {
            Some(p) => current = p.to_path_buf(),
            None => break,
        }
    }

    Ok(None)
}

/// Check if `path` is a mountpoint using the `mountpoint` utility.
fn is_mountpoint(path: &Path) -> bool {
    Command::new("mountpoint")
        .arg("-q")
        .arg(path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn run_sync() -> Result<()> {
    Command::new(SYNC_CMD)
        .status()
        .map_err(|e| FactoryResetError::Io(std::io::Error::other(format!("sync failed: {}", e))))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_backup_path_nonexistent_source() {
        let temp = TempDir::new().unwrap();
        let rootfs = temp.path().join("rootfs");
        let backup = temp.path().join("backup");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(&backup).unwrap();

        // Non-existent path should succeed silently
        backup_path(&rootfs, "/etc/hostname", &backup).unwrap();
    }

    #[test]
    fn test_restore_path_nonexistent_backup() {
        let temp = TempDir::new().unwrap();
        let rootfs = temp.path().join("rootfs");
        let backup = temp.path().join("backup");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(&backup).unwrap();

        // Non-existent backup path should succeed silently
        restore_path(&rootfs, "/etc/hostname", &backup).unwrap();
    }

    #[test]
    fn test_restore_all_returns_success_on_empty_list() {
        let temp = TempDir::new().unwrap();
        let rootfs = temp.path().join("rootfs");
        let backup = temp.path().join("backup");
        fs::create_dir_all(&rootfs).unwrap();
        fs::create_dir_all(&backup).unwrap();

        let result = restore_all(&rootfs, &[], &backup).unwrap();
        assert!(matches!(result, RestoreResult::Success));
    }

    #[test]
    fn test_find_nested_mountpoint_none() {
        let temp = TempDir::new().unwrap();
        let rootfs = temp.path().join("rootfs");
        let dest = rootfs.join("etc/hostname");
        fs::create_dir_all(rootfs.join("etc")).unwrap();

        // No actual mountpoints in tempdir — should return None
        let result = find_nested_mountpoint(&rootfs, &dest).unwrap();
        assert!(result.is_none());
    }
}
