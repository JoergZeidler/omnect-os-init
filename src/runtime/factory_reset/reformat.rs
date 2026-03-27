//! Reformat ext4 partitions after factory reset wipe.
//!
//! Matches legacy `reformat_ext4_fs()` in common-sh:
//!   mkfs.ext4 -F -q <device>
//!   tune2fs <device> -c -1 -i 0 -L <label>

use std::path::Path;
use std::process::Command;

use crate::error::{FactoryResetError, Result};

const MKFS_EXT4_CMD: &str = "/sbin/mkfs.ext4";
const TUNE2FS_CMD: &str = "/sbin/tune2fs";

/// Reformat a partition as ext4 and apply the omnect label/tunables.
///
/// Equivalent to legacy:
/// ```sh
/// mkfs.ext4 -F -q <device>
/// tune2fs <device> -c -1 -i 0 -L <label>
/// ```
pub fn reformat_ext4(device: &Path, label: &str) -> Result<()> {
    log::info!("Reformatting {} with label={}", device.display(), label);

    let mkfs = Command::new(MKFS_EXT4_CMD)
        .args(["-F", "-q"])
        .arg(device)
        .output()
        .map_err(|e| FactoryResetError::ReformatFailed {
            device: device.display().to_string(),
            reason: format!("Failed to run mkfs.ext4: {}", e),
        })?;

    if !mkfs.status.success() {
        return Err(FactoryResetError::ReformatFailed {
            device: device.display().to_string(),
            reason: format!(
                "mkfs.ext4 failed ({}): {}",
                mkfs.status,
                String::from_utf8_lossy(&mkfs.stderr)
            ),
        }
        .into());
    }

    let tune = Command::new(TUNE2FS_CMD)
        .arg(device)
        .args(["-c", "-1", "-i", "0", "-L", label])
        .output()
        .map_err(|e| FactoryResetError::ReformatFailed {
            device: device.display().to_string(),
            reason: format!("Failed to run tune2fs: {}", e),
        })?;

    if !tune.status.success() {
        return Err(FactoryResetError::ReformatFailed {
            device: device.display().to_string(),
            reason: format!(
                "tune2fs failed ({}): {}",
                tune.status,
                String::from_utf8_lossy(&tune.stderr)
            ),
        }
        .into());
    }

    log::info!("Reformatted {} successfully", device.display());
    Ok(())
}
