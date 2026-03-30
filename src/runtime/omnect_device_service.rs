//! omnect-device-service integration
//!
//! Creates runtime files that omnect-device-service reads at startup.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use nix::unistd::{Gid, Uid, chown};
use serde::Serialize;

use crate::bootloader::{Bootloader, vars};
use crate::error::{InitramfsError, Result};

/// Directory for ODS runtime files.
/// Written to the initramfs /run tmpfs; switch_root moves /run into the new
/// root via MS_MOVE, so these files appear at the same path after boot.
const ODS_RUNTIME_DIR: &str = "/run/omnect-device-service";

/// Main status file name
const ODS_STATUS_FILE: &str = "omnect-os-initramfs.json";

/// Update validation trigger file
const UPDATE_VALIDATE_FILE: &str = "omnect_validate_update";

/// Failed update validation marker
const UPDATE_VALIDATE_FAILED_FILE: &str = "omnect_validate_update_failed";

/// Bootloader updated marker
const BOOTLOADER_UPDATED_FILE: &str = "omnect_bootloader_updated";

/// Name of the omnect-device-service user and group in the rootfs
const ODS_USER: &str = "omnect_device_service";
const ODS_GROUP: &str = "omnect_device_service";

/// Permissions for the ODS runtime directory (rwxrwxr-x)
const DIR_MODE: u32 = 0o775;

/// Permissions for sensitive files readable only by ODS (rw-------)
const FILE_MODE_RESTRICTED: u32 = 0o600;

/// Permissions for trigger files readable by ODS and group (rw-r--r--)
const FILE_MODE_READABLE: u32 = 0o644;

/// Status information for omnect-device-service
#[derive(Debug, Clone, Default, Serialize)]
pub struct OdsStatus {
    /// Fsck results for each partition
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub fsck: HashMap<String, FsckStatus>,
}

/// Fsck status for a single partition
#[derive(Debug, Clone, Serialize)]
pub struct FsckStatus {
    /// Exit code from fsck
    pub code: i32,
    /// Output from fsck (may be compressed in bootloader)
    pub output: String,
}

impl OdsStatus {
    /// Create a new empty status
    pub fn new() -> Self {
        Self::default()
    }

    /// Add fsck result for a partition
    pub fn add_fsck_result(&mut self, partition: &str, code: i32, output: String) {
        self.fsck
            .insert(partition.to_string(), FsckStatus { code, output });
    }
}

/// Create all runtime files for omnect-device-service
///
/// Files are written directly to the initramfs `/run` tmpfs. `switch_root`
/// moves that mount into the new root via `MS_MOVE`, so they remain visible
/// to ODS at the same path after the root pivot.
///
/// Ownership and permissions are set to match legacy bash:
/// - dir: omnect_device_service:omnect_device_service, 775
/// - status JSON: 600
/// - trigger files: 644
/// - bootloader_updated: 600
pub fn create_ods_runtime_files(
    status: &OdsStatus,
    bootloader: Option<&mut Box<dyn Bootloader>>,
    rootfs_dir: &Path,
) -> Result<()> {
    let uid = lookup_uid(rootfs_dir, ODS_USER)?;
    let gid = lookup_gid(rootfs_dir, ODS_GROUP)?;

    let ods_dir = Path::new(ODS_RUNTIME_DIR);

    fs::create_dir_all(ods_dir).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to create ODS runtime dir: {}",
            e
        )))
    })?;
    set_ownership(ods_dir, uid, gid)?;
    set_mode(ods_dir, DIR_MODE)?;

    write_status_file(ods_dir, status)?;
    set_ownership(&ods_dir.join(ODS_STATUS_FILE), uid, gid)?;
    set_mode(&ods_dir.join(ODS_STATUS_FILE), FILE_MODE_RESTRICTED)?;

    // Handle update validation — requires a functional bootloader.
    // Skipped if bootloader is unavailable (e.g. missing grubenv on first boot).
    if let Some(bl) = bootloader {
        handle_update_validation(ods_dir, bl.as_mut(), uid, gid)?;
    }

    // Merge factory-reset status into the main ODS JSON under "factory-reset"
    // key and delete /tmp/factory-reset.json — matches legacy behaviour in
    // omnect-device-service-setup: factory_reset_status_handling().
    #[cfg(feature = "factory-reset")]
    merge_factory_reset_into_ods(ods_dir)?;

    log::info!("Created ODS runtime files in {}", ods_dir.display());

    Ok(())
}

/// Write the main status JSON file
fn write_status_file(ods_dir: &Path, status: &OdsStatus) -> Result<()> {
    let status_path = ods_dir.join(ODS_STATUS_FILE);
    let json = serde_json::to_string_pretty(status).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to serialize ODS status: {}",
            e
        )))
    })?;

    fs::write(&status_path, json).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to write ODS status to {}: {}",
            status_path.display(),
            e
        )))
    })?;
    log::debug!("Wrote ODS status to {}", status_path.display());

    Ok(())
}

/// Merge factory-reset status into the main ODS JSON under the `"factory-reset"` key.
///
/// Matches legacy `factory_reset_status_handling()`:
/// ```sh
/// cat omnect-os-initramfs.json \
///   | jq --argjson factory_reset "$(cat /tmp/factory-reset.json)" \
///         '."factory-reset"=$factory_reset' > /tmp/omnect-os-initramfs.json
/// mv /tmp/omnect-os-initramfs.json omnect-os-initramfs.json
/// rm /tmp/factory-reset.json
/// ```
#[cfg(feature = "factory-reset")]
fn merge_factory_reset_into_ods(ods_dir: &Path) -> Result<()> {
    use crate::runtime::factory_reset::status::FACTORY_RESET_STATUS_TMP;

    let tmp_path = std::path::Path::new(FACTORY_RESET_STATUS_TMP);
    if !tmp_path.exists() {
        return Ok(());
    }

    let ods_path = ods_dir.join(ODS_STATUS_FILE);

    // Read both JSON files as generic Values so we don't need to model every field.
    let ods_raw = fs::read_to_string(&ods_path).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to read {}: {}",
            ods_path.display(),
            e
        )))
    })?;
    let fr_raw = fs::read_to_string(tmp_path).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to read {}: {}",
            tmp_path.display(),
            e
        )))
    })?;

    let mut ods_json: serde_json::Value = serde_json::from_str(&ods_raw).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to parse ODS status JSON: {}",
            e
        )))
    })?;
    let fr_json: serde_json::Value = serde_json::from_str(&fr_raw).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to parse factory-reset status JSON: {}",
            e
        )))
    })?;

    ods_json["factory-reset"] = fr_json;

    let merged = serde_json::to_string_pretty(&ods_json).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to serialize merged ODS status: {}",
            e
        )))
    })?;
    fs::write(&ods_path, merged).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to write merged ODS status to {}: {}",
            ods_path.display(),
            e
        )))
    })?;

    // Remove the tmp file — it has served its purpose.
    fs::remove_file(tmp_path).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to remove {}: {}",
            tmp_path.display(),
            e
        )))
    })?;

    log::debug!("Merged factory-reset status into ODS JSON");
    Ok(())
}

/// Handle update validation workflow; applies ownership and permissions to any
/// trigger files it creates.
fn handle_update_validation(
    ods_dir: &Path,
    bootloader: &mut dyn Bootloader,
    uid: u32,
    gid: u32,
) -> Result<()> {
    let validate_update = match bootloader.get_env(vars::OMNECT_VALIDATE_UPDATE) {
        Ok(val) => val,
        Err(e) => {
            log::warn!(
                "failed to read omnect_validate_update from bootloader: {}",
                e
            );
            None
        }
    };

    if let Some(value) = validate_update {
        if value == "1" || value.to_lowercase() == "true" {
            let trigger_path = ods_dir.join(UPDATE_VALIDATE_FILE);
            fs::write(&trigger_path, "1").map_err(|e| {
                InitramfsError::Io(std::io::Error::other(format!(
                    "Failed to write {}: {}",
                    trigger_path.display(),
                    e
                )))
            })?;
            set_ownership(&trigger_path, uid, gid)?;
            set_mode(&trigger_path, FILE_MODE_READABLE)?;
            log::info!("Update validation requested - created trigger file");
        } else if value == "failed" {
            let failed_path = ods_dir.join(UPDATE_VALIDATE_FAILED_FILE);
            fs::write(&failed_path, "1").map_err(|e| {
                InitramfsError::Io(std::io::Error::other(format!(
                    "Failed to write {}: {}",
                    failed_path.display(),
                    e
                )))
            })?;
            set_ownership(&failed_path, uid, gid)?;
            set_mode(&failed_path, FILE_MODE_READABLE)?;
            log::warn!("Update validation failed marker created");
        }
    }

    let bootloader_updated = match bootloader.get_env(vars::OMNECT_BOOTLOADER_UPDATED) {
        Ok(val) => val,
        Err(e) => {
            log::warn!(
                "failed to read omnect_bootloader_updated from bootloader: {}",
                e
            );
            None
        }
    };

    if bootloader_updated.is_some_and(|v| !v.is_empty()) {
        let marker_path = ods_dir.join(BOOTLOADER_UPDATED_FILE);
        fs::write(&marker_path, "1").map_err(|e| {
            InitramfsError::Io(std::io::Error::other(format!(
                "Failed to write {}: {}",
                marker_path.display(),
                e
            )))
        })?;
        set_ownership(&marker_path, uid, gid)?;
        set_mode(&marker_path, FILE_MODE_RESTRICTED)?;
        bootloader
            .set_env(vars::OMNECT_BOOTLOADER_UPDATED, None)
            .map_err(|e| {
                InitramfsError::Io(std::io::Error::other(format!(
                    "Failed to clear omnect_bootloader_updated: {}",
                    e
                )))
            })?;
        log::info!("Bootloader update marker created");
    }

    Ok(())
}

/// Look up the numeric UID for a user in the rootfs /etc/passwd.
fn lookup_uid(rootfs_dir: &Path, username: &str) -> Result<u32> {
    let passwd = rootfs_dir.join("etc/passwd");
    let content = fs::read_to_string(&passwd).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to read {}: {}",
            passwd.display(),
            e
        )))
    })?;
    for line in content.lines() {
        let mut fields = line.splitn(7, ':');
        let name = fields.next().unwrap_or("");
        if name != username {
            continue;
        }
        let _password = fields.next();
        if let Some(uid_str) = fields.next() {
            return uid_str.parse::<u32>().map_err(|e| {
                InitramfsError::Io(std::io::Error::other(format!(
                    "Invalid UID for {}: {}",
                    username, e
                )))
            });
        }
    }
    Err(InitramfsError::Io(std::io::Error::other(format!(
        "user {} not found in {}",
        username,
        passwd.display()
    ))))
}

/// Look up the numeric GID for a group in the rootfs /etc/group.
fn lookup_gid(rootfs_dir: &Path, groupname: &str) -> Result<u32> {
    let group = rootfs_dir.join("etc/group");
    let content = fs::read_to_string(&group).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to read {}: {}",
            group.display(),
            e
        )))
    })?;
    for line in content.lines() {
        let mut fields = line.splitn(4, ':');
        let name = fields.next().unwrap_or("");
        if name != groupname {
            continue;
        }
        let _password = fields.next();
        if let Some(gid_str) = fields.next() {
            return gid_str.parse::<u32>().map_err(|e| {
                InitramfsError::Io(std::io::Error::other(format!(
                    "Invalid GID for {}: {}",
                    groupname, e
                )))
            });
        }
    }
    Err(InitramfsError::Io(std::io::Error::other(format!(
        "group {} not found in {}",
        groupname,
        group.display()
    ))))
}

fn set_ownership(path: &Path, uid: u32, gid: u32) -> Result<()> {
    chown(path, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid))).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to chown {}: {}",
            path.display(),
            e
        )))
    })
}

fn set_mode(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to chmod {}: {}",
            path.display(),
            e
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn current_uid() -> u32 {
        nix::unistd::getuid().as_raw()
    }

    fn current_gid() -> u32 {
        nix::unistd::getgid().as_raw()
    }

    /// Create a minimal rootfs with /etc/passwd and /etc/group for ODS user,
    /// using the current process's uid/gid so chown succeeds without root.
    fn make_fake_rootfs(uid: u32, gid: u32) -> TempDir {
        let rootfs = TempDir::new().unwrap();
        let etc = rootfs.path().join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(
            etc.join("passwd"),
            format!(
                "root:x:0:0:root:/root:/bin/sh\nomnect_device_service:x:{uid}:{gid}::/:/bin/sh\n"
            ),
        )
        .unwrap();
        fs::write(
            etc.join("group"),
            format!("root:x:0:\nomnect_device_service:x:{gid}:\n"),
        )
        .unwrap();
        rootfs
    }

    #[test]
    fn test_ods_status_default() {
        let status = OdsStatus::default();
        assert!(status.fsck.is_empty());
    }

    #[test]
    fn test_ods_status_add_fsck() {
        let mut status = OdsStatus::new();
        status.add_fsck_result("boot", 0, "clean".to_string());
        status.add_fsck_result("data", 1, "errors corrected".to_string());

        assert_eq!(status.fsck.len(), 2);
        assert_eq!(status.fsck.get("boot").unwrap().code, 0);
        assert_eq!(status.fsck.get("data").unwrap().code, 1);
    }

    #[test]
    fn test_ods_status_serialization() {
        let mut status = OdsStatus::new();
        status.add_fsck_result("boot", 0, "clean".to_string());

        let json = serde_json::to_string(&status).unwrap();
        assert!(json.contains("\"boot\""));
        assert!(json.contains("\"code\":0"));
    }

    #[test]
    fn test_write_status_file() {
        let temp = TempDir::new().unwrap();
        let status = OdsStatus::new();

        write_status_file(temp.path(), &status).unwrap();

        let status_path = temp.path().join(ODS_STATUS_FILE);
        assert!(status_path.exists());

        let content = fs::read_to_string(status_path).unwrap();
        assert!(content.contains("{"));
    }

    #[test]
    fn test_handle_update_validation_value_1() {
        let temp = TempDir::new().unwrap();
        let mut bl =
            crate::bootloader::create_mock_bootloader().with_env(vars::OMNECT_VALIDATE_UPDATE, "1");

        handle_update_validation(temp.path(), &mut bl, current_uid(), current_gid()).unwrap();

        assert!(temp.path().join(UPDATE_VALIDATE_FILE).exists());
        assert!(!temp.path().join(UPDATE_VALIDATE_FAILED_FILE).exists());
        assert!(!temp.path().join(BOOTLOADER_UPDATED_FILE).exists());
    }

    #[test]
    fn test_handle_update_validation_value_true() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(vars::OMNECT_VALIDATE_UPDATE, "true");

        handle_update_validation(temp.path(), &mut bl, current_uid(), current_gid()).unwrap();

        assert!(temp.path().join(UPDATE_VALIDATE_FILE).exists());
    }

    #[test]
    fn test_handle_update_validation_failed() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(vars::OMNECT_VALIDATE_UPDATE, "failed");

        handle_update_validation(temp.path(), &mut bl, current_uid(), current_gid()).unwrap();

        assert!(!temp.path().join(UPDATE_VALIDATE_FILE).exists());
        assert!(temp.path().join(UPDATE_VALIDATE_FAILED_FILE).exists());
    }

    #[test]
    fn test_handle_update_validation_unexpected_value_creates_nothing() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(vars::OMNECT_VALIDATE_UPDATE, "unexpected");

        handle_update_validation(temp.path(), &mut bl, current_uid(), current_gid()).unwrap();

        assert!(!temp.path().join(UPDATE_VALIDATE_FILE).exists());
        assert!(!temp.path().join(UPDATE_VALIDATE_FAILED_FILE).exists());
    }

    #[test]
    fn test_handle_update_validation_bootloader_updated() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(vars::OMNECT_BOOTLOADER_UPDATED, "1");

        handle_update_validation(temp.path(), &mut bl, current_uid(), current_gid()).unwrap();

        assert!(temp.path().join(BOOTLOADER_UPDATED_FILE).exists());
        // Env var must be cleared after marker creation (matches legacy behaviour)
        assert_eq!(bl.get_env(vars::OMNECT_BOOTLOADER_UPDATED).unwrap(), None);
    }

    #[test]
    fn test_handle_update_validation_bootloader_updated_any_nonempty_value() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader()
            .with_env(vars::OMNECT_BOOTLOADER_UPDATED, "0");

        handle_update_validation(temp.path(), &mut bl, current_uid(), current_gid()).unwrap();

        // Any non-empty value triggers marker creation (matches legacy [ -n ... ] check)
        assert!(temp.path().join(BOOTLOADER_UPDATED_FILE).exists());
    }

    #[test]
    fn test_handle_update_validation_no_env_creates_nothing() {
        let temp = TempDir::new().unwrap();
        let mut bl = crate::bootloader::create_mock_bootloader();

        handle_update_validation(temp.path(), &mut bl, current_uid(), current_gid()).unwrap();

        assert!(!temp.path().join(UPDATE_VALIDATE_FILE).exists());
        assert!(!temp.path().join(UPDATE_VALIDATE_FAILED_FILE).exists());
        assert!(!temp.path().join(BOOTLOADER_UPDATED_FILE).exists());
    }

    #[test]
    fn test_lookup_uid_and_gid() {
        let uid = current_uid();
        let gid = current_gid();
        let rootfs = make_fake_rootfs(uid, gid);

        assert_eq!(lookup_uid(rootfs.path(), ODS_USER).unwrap(), uid);
        assert_eq!(lookup_gid(rootfs.path(), ODS_GROUP).unwrap(), gid);
    }

    #[test]
    fn test_lookup_uid_missing_user() {
        let rootfs = TempDir::new().unwrap();
        fs::create_dir_all(rootfs.path().join("etc")).unwrap();
        fs::write(
            rootfs.path().join("etc/passwd"),
            "root:x:0:0::/root:/bin/sh\n",
        )
        .unwrap();

        assert!(lookup_uid(rootfs.path(), ODS_USER).is_err());
    }

    #[test]
    fn test_lookup_gid_missing_group() {
        let rootfs = TempDir::new().unwrap();
        fs::create_dir_all(rootfs.path().join("etc")).unwrap();
        fs::write(rootfs.path().join("etc/group"), "root:x:0:\n").unwrap();

        assert!(lookup_gid(rootfs.path(), ODS_GROUP).is_err());
    }
}
