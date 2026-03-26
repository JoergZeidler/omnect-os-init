//! Overlayfs setup for etc and home directories
//!
//! This module handles:
//! - Setting up overlayfs for /etc (factory defaults + persistent upper)
//! - Setting up overlayfs for /home (factory defaults + data upper)
//! - Bind mounts for /var/lib and /usr/local
//! - Initial copy of factory etc to upper layer

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use nix::mount::MsFlags;

use crate::error::FilesystemError;
use crate::filesystem::{MountManager, MountOptions, MountPoint, Result};

/// Overlay filesystem type
const OVERLAY_FSTYPE: &str = "overlay";
/// cp command for copying directory contents (preserves attributes via -a)
const CP_CMD: &str = "/bin/cp";

/// Directory names for overlay layers
mod overlay_dirs {
    pub const UPPER: &str = "upper";
    pub const WORK: &str = "work";
}

/// Standard paths relative to rootfs
mod paths {
    pub const ETC: &str = "etc";
    pub const HOME: &str = "home";
    pub const VAR_LIB: &str = "var/lib";
    pub const USR_LOCAL: &str = "usr/local";
    pub const VAR_LOG: &str = "var/log";
}

/// Mount point paths for partitions
mod mount_points {
    pub const ETC_PARTITION: &str = "mnt/etc";
    pub const DATA_PARTITION: &str = "mnt/data";
    pub const FACTORY_PARTITION: &str = "mnt/factory";
}

/// Configuration for overlay setup
#[derive(Debug, Clone)]
pub struct OverlayConfig {
    /// Root filesystem directory (e.g., /rootfs)
    pub rootfs_dir: PathBuf,
    /// Whether to enable persistent /var/log
    pub persistent_var_log: bool,
    /// Additional mount options for data partition
    pub data_mount_options: Option<String>,
}

impl OverlayConfig {
    /// Create a new overlay configuration
    pub fn new(rootfs_dir: impl Into<PathBuf>) -> Self {
        Self {
            rootfs_dir: rootfs_dir.into(),
            persistent_var_log: false,
            data_mount_options: None,
        }
    }

    /// Enable persistent /var/log
    pub fn with_persistent_var_log(mut self, enabled: bool) -> Self {
        self.persistent_var_log = enabled;
        self
    }

    /// Set additional data mount options
    pub fn with_data_mount_options(mut self, options: Option<String>) -> Self {
        self.data_mount_options = options;
        self
    }
}

/// Setup the etc partition with overlayfs
///
/// Creates an overlay where:
/// - Lower layer: rootfs/etc (read-only from current OS)
/// - Upper layer: mnt/etc/upper (persistent changes)
/// - Work dir: mnt/etc/work
/// - Target: rootfs/etc
pub fn setup_etc_overlay(mm: &mut MountManager, config: &OverlayConfig) -> Result<()> {
    let rootfs = &config.rootfs_dir;
    let etc_mount = rootfs.join(mount_points::ETC_PARTITION);
    let factory_mount = rootfs.join(mount_points::FACTORY_PARTITION);

    // Overlay directories
    let upper_dir = etc_mount.join(overlay_dirs::UPPER);
    let work_dir = etc_mount.join(overlay_dirs::WORK);
    let lower_dir = rootfs.join(paths::ETC);
    let target = rootfs.join(paths::ETC);

    // Factory etc is only used for first-boot initialization
    let factory_etc = factory_mount.join(paths::ETC);

    // Ensure directories exist
    ensure_overlay_dirs(&upper_dir, &work_dir)?;

    // Check if this is first boot (upper is empty)
    let is_first_boot = is_directory_empty(&upper_dir)?;

    if is_first_boot {
        log::info!("First boot detected - copying factory etc to upper layer");
        copy_directory_contents(&factory_etc, &upper_dir)?;
    }

    // Mount the overlay
    mount_overlay(mm, &lower_dir, &upper_dir, &work_dir, &target)?;

    log::info!(
        "Setup etc overlay: lower={}, upper={} -> {}",
        lower_dir.display(),
        upper_dir.display(),
        target.display()
    );

    Ok(())
}

/// Setup the data partition with home overlayfs and bind mounts
///
/// Creates:
/// - Overlay for /home (rootfs/home lower, data/home/upper upper)
/// - Bind mount: data/var/lib -> rootfs/var/lib
/// - Bind mount: data/local -> rootfs/usr/local
/// - Optional: data/var/log -> rootfs/var/log (if persistent_var_log enabled)
pub fn setup_data_overlay(mm: &mut MountManager, config: &OverlayConfig) -> Result<()> {
    let rootfs = &config.rootfs_dir;
    let data_mount = rootfs.join(mount_points::DATA_PARTITION);

    // Setup home overlay (no factory_mount parameter needed)
    setup_home_overlay(mm, rootfs, &data_mount)?;

    // Setup bind mounts
    setup_var_lib_bind(mm, rootfs, &data_mount)?;
    setup_usr_local_bind(mm, rootfs, &data_mount)?;

    // Optional: persistent /var/log
    if config.persistent_var_log {
        setup_var_log_bind(mm, rootfs, &data_mount)?;
    }

    Ok(())
}

/// Setup home directory overlay
fn setup_home_overlay(mm: &mut MountManager, rootfs: &Path, data_mount: &Path) -> Result<()> {
    let home_data = data_mount.join(paths::HOME);
    let upper_dir = home_data.join(overlay_dirs::UPPER);
    let work_dir = home_data.join(overlay_dirs::WORK);
    let lower_dir = rootfs.join(paths::HOME);
    let target = rootfs.join(paths::HOME);

    // Ensure directories exist
    ensure_dir(&home_data)?;
    ensure_overlay_dirs(&upper_dir, &work_dir)?;

    // Mount the overlay with rootfs/home as lower layer
    mount_overlay(mm, &lower_dir, &upper_dir, &work_dir, &target)?;

    log::info!(
        "Setup home overlay: lower={}, upper={} -> {}",
        lower_dir.display(),
        upper_dir.display(),
        target.display()
    );

    Ok(())
}

/// Setup bind mount for /var/lib
fn setup_var_lib_bind(mm: &mut MountManager, rootfs: &Path, data_mount: &Path) -> Result<()> {
    let source = data_mount.join(paths::VAR_LIB);
    let target = rootfs.join(paths::VAR_LIB);

    ensure_dir(&source)?;
    ensure_dir(&target)?;

    mm.mount_bind(&source, &target)?;

    log::info!("Bind mounted {} -> {}", source.display(), target.display());

    Ok(())
}

/// Setup bind mount for /usr/local
fn setup_usr_local_bind(mm: &mut MountManager, rootfs: &Path, data_mount: &Path) -> Result<()> {
    // Data partition uses "local" instead of "usr/local"
    let source = data_mount.join("local");
    let target = rootfs.join(paths::USR_LOCAL);

    ensure_dir(&source)?;
    ensure_dir(&target)?;

    mm.mount_bind(&source, &target)?;

    log::info!("Bind mounted {} -> {}", source.display(), target.display());

    Ok(())
}

/// Setup bind mount for persistent /var/log
fn setup_var_log_bind(mm: &mut MountManager, rootfs: &Path, data_mount: &Path) -> Result<()> {
    let source = data_mount.join(paths::VAR_LOG);
    let target = rootfs.join(paths::VAR_LOG);

    ensure_dir(&source)?;
    ensure_dir(&target)?;

    mm.mount_bind(&source, &target)?;

    log::info!(
        "Bind mounted persistent var/log: {} -> {}",
        source.display(),
        target.display()
    );

    Ok(())
}

/// Mount an overlayfs
fn mount_overlay(
    mm: &mut MountManager,
    lower: &Path,
    upper: &Path,
    work: &Path,
    target: &Path,
) -> Result<()> {
    let options = format!(
        // uuid=null: lower and upper are on different partitions, so uuid=off
        // is rejected by the kernel. uuid=null disables UUID-based file handle
        // encoding without requiring co-located layers.
        "lowerdir={},upperdir={},workdir={},index=off,uuid=null",
        lower.display(),
        upper.display(),
        work.display()
    );

    let mount_opts = MountOptions {
        fstype: Some(OVERLAY_FSTYPE.to_string()),
        flags: MsFlags::MS_NOATIME | MsFlags::MS_NODIRATIME,
        data: Some(options.clone()),
    };

    mm.mount(MountPoint::new(OVERLAY_FSTYPE, target, mount_opts))
        .map_err(|e| FilesystemError::OverlayFailed {
            target: target.to_path_buf(),
            reason: format!("{}: options={}", e, options),
        })?;

    Ok(())
}

/// Ensure overlay directories (upper and work) exist
fn ensure_overlay_dirs(upper: &Path, work: &Path) -> Result<()> {
    ensure_dir(upper)?;
    ensure_dir(work)?;
    Ok(())
}

/// Ensure a directory exists, creating it if necessary
fn ensure_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::create_dir_all(path).map_err(|e| FilesystemError::OverlayFailed {
            target: path.to_path_buf(),
            reason: format!("Failed to create directory: {}", e),
        })?;
    }
    Ok(())
}

/// Check if a directory is empty
fn is_directory_empty(path: &Path) -> Result<bool> {
    if !path.exists() {
        return Ok(true);
    }

    let entries = fs::read_dir(path).map_err(|e| FilesystemError::OverlayFailed {
        target: path.to_path_buf(),
        reason: format!("Failed to read directory: {}", e),
    })?;

    Ok(entries.count() == 0)
}

/// Copy contents of one directory to another
///
/// Uses `cp -a` for proper attribute preservation.
fn copy_directory_contents(src: &Path, dst: &Path) -> Result<()> {
    if !src.exists() {
        log::warn!("Source directory does not exist: {}", src.display());
        return Ok(());
    }

    // Use cp -a to preserve all attributes
    let output = Command::new(CP_CMD)
        .arg("-a")
        .arg(format!("{}/.", src.display()))
        .arg(dst)
        .output()
        .map_err(|e| FilesystemError::OverlayFailed {
            target: dst.to_path_buf(),
            reason: format!("Failed to execute cp: {}", e),
        })?;

    if !output.status.success() {
        return Err(FilesystemError::OverlayFailed {
            target: dst.to_path_buf(),
            reason: format!("cp failed: {}", String::from_utf8_lossy(&output.stderr)),
        });
    }

    log::debug!("Copied {} -> {}", src.display(), dst.display());

    Ok(())
}

/// Setup raw rootfs bind mount (must be called BEFORE overlays)
///
/// Creates a private bind mount at /mnt/rootCurrentPrivate that provides
/// access to the raw rootfs without overlay modifications.
pub fn setup_raw_rootfs_mount(mm: &mut MountManager, rootfs_dir: &Path) -> Result<()> {
    let raw_mount = rootfs_dir.join("mnt/rootCurrentPrivate");

    ensure_dir(&raw_mount)?;

    // Create private bind mount
    mm.mount_bind_private(rootfs_dir, &raw_mount)?;

    log::info!(
        "Created raw rootfs mount: {} -> {}",
        rootfs_dir.display(),
        raw_mount.display()
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_overlay_config_new() {
        let config = OverlayConfig::new("/rootfs");
        assert_eq!(config.rootfs_dir, PathBuf::from("/rootfs"));
        assert!(!config.persistent_var_log);
        assert!(config.data_mount_options.is_none());
    }

    #[test]
    fn test_overlay_config_builder() {
        let config = OverlayConfig::new("/rootfs")
            .with_persistent_var_log(true)
            .with_data_mount_options(Some("discard".to_string()));

        assert!(config.persistent_var_log);
        assert_eq!(config.data_mount_options, Some("discard".to_string()));
    }

    #[test]
    fn test_ensure_dir_creates_directory() {
        let temp = TempDir::new().unwrap();
        let new_dir = temp.path().join("test/nested/dir");

        assert!(!new_dir.exists());
        ensure_dir(&new_dir).unwrap();
        assert!(new_dir.exists());
    }

    #[test]
    fn test_ensure_dir_existing() {
        let temp = TempDir::new().unwrap();
        let existing = temp.path();

        assert!(existing.exists());
        ensure_dir(existing).unwrap();
        assert!(existing.exists());
    }

    #[test]
    fn test_is_directory_empty_true() {
        let temp = TempDir::new().unwrap();
        assert!(is_directory_empty(temp.path()).unwrap());
    }

    #[test]
    fn test_is_directory_empty_false() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join("file.txt"), "content").unwrap();
        assert!(!is_directory_empty(temp.path()).unwrap());
    }

    #[test]
    fn test_is_directory_empty_nonexistent() {
        let path = PathBuf::from("/nonexistent/path");
        assert!(is_directory_empty(&path).unwrap());
    }

    #[test]
    fn test_ensure_overlay_dirs() {
        let temp = TempDir::new().unwrap();
        let upper = temp.path().join("upper");
        let work = temp.path().join("work");

        ensure_overlay_dirs(&upper, &work).unwrap();

        assert!(upper.exists());
        assert!(work.exists());
    }

    #[test]
    fn test_copy_directory_contents() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("src");
        let dst = temp.path().join("dst");

        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("file1.txt"), "content1").unwrap();
        fs::create_dir_all(src.join("subdir")).unwrap();
        fs::write(src.join("subdir/file2.txt"), "content2").unwrap();

        copy_directory_contents(&src, &dst).unwrap();

        assert!(dst.join("file1.txt").exists());
        assert!(dst.join("subdir/file2.txt").exists());
        assert_eq!(
            fs::read_to_string(dst.join("file1.txt")).unwrap(),
            "content1"
        );
    }

    #[test]
    fn test_copy_directory_nonexistent_source() {
        let temp = TempDir::new().unwrap();
        let src = temp.path().join("nonexistent");
        let dst = temp.path().join("dst");

        fs::create_dir_all(&dst).unwrap();

        // Should not error on nonexistent source
        let result = copy_directory_contents(&src, &dst);
        assert!(result.is_ok());
    }
}
