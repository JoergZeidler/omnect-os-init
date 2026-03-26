//! Filesystem check (fsck) operations
//!
//! Runs fsck on partitions before mounting and handles exit codes appropriately.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::FilesystemError;
use crate::filesystem::Result;

/// fsck command name
const FSCK_CMD: &str = "/sbin/fsck";

/// fsck exit codes
mod exit_code {
    /// No errors
    pub const OK: i32 = 0;
    /// Filesystem errors corrected
    pub const CORRECTED: i32 = 1;
    /// System should be rebooted
    pub const REBOOT_REQUIRED: i32 = 2;
    /// Filesystem errors left uncorrected
    pub const ERRORS_UNCORRECTED: i32 = 4;
    /// Operational error
    pub const OPERATIONAL_ERROR: i32 = 8;
    /// Usage or syntax error
    pub const USAGE_ERROR: i32 = 16;
    /// Cancelled by user
    pub const CANCELLED: i32 = 32;
    /// Shared library error
    pub const LIBRARY_ERROR: i32 = 128;
}

/// Result of a filesystem check
#[derive(Debug, Clone)]
pub struct FsckResult {
    /// Device that was checked
    pub device: PathBuf,
    /// Exit code from fsck
    pub exit_code: i32,
    /// Output from fsck (stdout + stderr)
    pub output: String,
    /// Whether the check was successful (code 0: clean, or code 1: errors corrected by -y)
    pub success: bool,
    /// Whether a reboot is required (code 2 only: fsck explicitly requests reboot)
    pub reboot_required: bool,
}

impl FsckResult {
    /// Check if there were uncorrected errors
    pub fn has_uncorrected_errors(&self) -> bool {
        self.exit_code & exit_code::ERRORS_UNCORRECTED != 0
    }

    /// Check if there was an operational error
    pub fn has_operational_error(&self) -> bool {
        self.exit_code & exit_code::OPERATIONAL_ERROR != 0
    }
}

/// Run fsck on a device
///
/// # Arguments
/// * `device` - Path to the block device to check
/// * `fstype` - Filesystem type (e.g. "ext4", "vfat")
///
/// # Returns
/// * `Ok(FsckResult)` - Result of the check (including exit code 1: errors corrected, safe to mount)
/// * `Err(FilesystemError::FsckRequiresReboot)` - If fsck requests a reboot (exit code 2 only)
/// * `Err(FilesystemError::FsckFailed)` - If check failed with uncorrectable errors
pub fn check_filesystem(device: &Path, fstype: &str) -> Result<FsckResult> {
    log::info!("Running fsck on {}", device.display());

    // Disable kernel message rate limiting during fsck — RAII guard restores on all exit paths.
    disable_kmsg_ratelimit();
    let _ratelimit_guard = KmsgRatelimitGuard;

    let mut cmd = Command::new(FSCK_CMD);

    // Always repair automatically; this is an unattended initramfs boot.
    cmd.arg("-y");

    // Explicitly specify the filesystem type. Without -t, fsck falls back to
    // /etc/fstab probing which does not exist in the initramfs, causing code 8
    // (operational error) on valid filesystems.
    cmd.args(["-t", fstype]);

    cmd.arg(device);

    let output = cmd.output().map_err(|e| FilesystemError::FsckFailed {
        device: device.to_path_buf(),
        code: -1,
        output: format!("Failed to execute fsck: {}", e),
    })?;

    let exit_code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined_output = format!("{}{}", stdout, stderr);

    let result = FsckResult {
        device: device.to_path_buf(),
        exit_code,
        output: combined_output.clone(),
        // Exit code 1: errors were corrected by -y. The filesystem is now clean
        // and safe to mount — no reboot needed. Matches legacy bash behaviour.
        // Exit code 2: fsck explicitly requests a reboot (e.g. kernel needs to
        // replay journal). Only this code triggers a reboot.
        success: exit_code == exit_code::OK || exit_code == exit_code::CORRECTED,
        reboot_required: exit_code & exit_code::REBOOT_REQUIRED != 0,
    };

    // Log the result
    if exit_code == exit_code::OK {
        log::debug!("fsck: {} is clean", device.display());
    } else if exit_code == exit_code::CORRECTED {
        log::info!(
            "fsck corrected errors on {} (code 1) — filesystem is clean, continuing",
            device.display()
        );
    } else if result.reboot_required {
        log::warn!(
            "fsck on {} requires reboot (code {})",
            device.display(),
            exit_code
        );
    } else {
        log::error!(
            "fsck failed on {} with code {}: {}",
            device.display(),
            exit_code,
            combined_output.lines().next().unwrap_or("(no output)")
        );
    }

    // Exit code 2: fsck requests a reboot before mounting
    if result.reboot_required {
        return Err(FilesystemError::FsckRequiresReboot {
            device: device.to_path_buf(),
            code: exit_code,
            output: combined_output,
        });
    }

    // Exit codes ≥4: uncorrectable errors
    if !result.success {
        return Err(FilesystemError::FsckFailed {
            device: device.to_path_buf(),
            code: exit_code,
            output: combined_output,
        });
    }

    Ok(result)
}

/// Run fsck on a device, ignoring non-critical errors
///
/// This variant returns Ok even if fsck reports errors, unless a reboot is required.
/// Useful for partitions where we want to log errors but continue booting.
pub fn check_filesystem_lenient(device: &Path, fstype: &str) -> Result<FsckResult> {
    match check_filesystem(device, fstype) {
        Ok(result) => Ok(result),
        Err(FilesystemError::FsckRequiresReboot {
            device,
            code,
            output,
        }) => Err(FilesystemError::FsckRequiresReboot {
            device,
            code,
            output,
        }),
        Err(FilesystemError::FsckFailed {
            device,
            code,
            output,
        }) => {
            log::warn!(
                "fsck on {} had errors (code {}), continuing anyway",
                device.display(),
                code
            );
            Ok(FsckResult {
                device,
                exit_code: code,
                output,
                success: false,
                reboot_required: false,
            })
        }
        Err(e) => Err(e),
    }
}

/// RAII guard that re-enables kmsg rate limiting when dropped.
/// Guarantees restoration on all exit paths including early error returns.
struct KmsgRatelimitGuard;

impl Drop for KmsgRatelimitGuard {
    fn drop(&mut self) {
        enable_kmsg_ratelimit();
    }
}

const PRINTK_RATELIMIT_PATH: &str = "/proc/sys/kernel/printk_ratelimit";
const PRINTK_RATELIMIT_BURST_PATH: &str = "/proc/sys/kernel/printk_ratelimit_burst";

use std::sync::Mutex;

/// Saved rate limit values for restoration
static SAVED_RATELIMIT: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Disable kernel message rate limiting
///
/// This ensures fsck output isn't throttled in dmesg.
fn disable_kmsg_ratelimit() {
    let ratelimit = match std::fs::read_to_string(PRINTK_RATELIMIT_PATH) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            log::warn!("Failed to read {PRINTK_RATELIMIT_PATH}: {e}; skipping ratelimit save");
            return;
        }
    };
    let burst = match std::fs::read_to_string(PRINTK_RATELIMIT_BURST_PATH) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            log::warn!(
                "Failed to read {PRINTK_RATELIMIT_BURST_PATH}: {e}; skipping ratelimit save"
            );
            return;
        }
    };

    if let Ok(mut saved) = SAVED_RATELIMIT.lock() {
        *saved = Some((ratelimit, burst));
    }

    let _ = std::fs::write(PRINTK_RATELIMIT_PATH, "0");
    let _ = std::fs::write(PRINTK_RATELIMIT_BURST_PATH, "0");
}

/// Re-enable kernel message rate limiting
fn enable_kmsg_ratelimit() {
    if let Ok(mut saved) = SAVED_RATELIMIT.lock()
        && let Some((ratelimit, burst)) = saved.take()
    {
        let _ = std::fs::write(PRINTK_RATELIMIT_PATH, ratelimit);
        let _ = std::fs::write(PRINTK_RATELIMIT_BURST_PATH, burst);
    }
}

/// Parse fsck exit code into human-readable description
pub fn describe_fsck_exit_code(code: i32) -> String {
    let mut descriptions = Vec::new();

    if code == exit_code::OK {
        return "No errors".to_string();
    }

    if code & exit_code::CORRECTED != 0 {
        descriptions.push("errors corrected");
    }
    if code & exit_code::REBOOT_REQUIRED != 0 {
        descriptions.push("reboot required");
    }
    if code & exit_code::ERRORS_UNCORRECTED != 0 {
        descriptions.push("uncorrected errors");
    }
    if code & exit_code::OPERATIONAL_ERROR != 0 {
        descriptions.push("operational error");
    }
    if code & exit_code::USAGE_ERROR != 0 {
        descriptions.push("usage error");
    }
    if code & exit_code::CANCELLED != 0 {
        descriptions.push("cancelled");
    }
    if code & exit_code::LIBRARY_ERROR != 0 {
        descriptions.push("library error");
    }

    if descriptions.is_empty() {
        format!("unknown error (code {})", code)
    } else {
        descriptions.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_describe_fsck_exit_code_ok() {
        assert_eq!(describe_fsck_exit_code(0), "No errors");
    }

    #[test]
    fn test_describe_fsck_exit_code_corrected() {
        assert_eq!(describe_fsck_exit_code(1), "errors corrected");
    }

    #[test]
    fn test_describe_fsck_exit_code_reboot() {
        assert_eq!(describe_fsck_exit_code(2), "reboot required");
    }

    #[test]
    fn test_describe_fsck_exit_code_combined() {
        // Code 3 = CORRECTED | REBOOT_REQUIRED
        assert_eq!(
            describe_fsck_exit_code(3),
            "errors corrected, reboot required"
        );
    }

    #[test]
    fn test_describe_fsck_exit_code_errors() {
        assert_eq!(describe_fsck_exit_code(4), "uncorrected errors");
    }

    #[test]
    fn test_fsck_result_has_uncorrected_errors() {
        let result = FsckResult {
            device: PathBuf::from("/dev/sda1"),
            exit_code: 4,
            output: String::new(),
            success: false,
            reboot_required: false,
        };
        assert!(result.has_uncorrected_errors());

        let clean = FsckResult {
            device: PathBuf::from("/dev/sda1"),
            exit_code: 0,
            output: String::new(),
            success: true,
            reboot_required: false,
        };
        assert!(!clean.has_uncorrected_errors());
    }

    #[test]
    fn test_fsck_result_has_operational_error() {
        let result = FsckResult {
            device: PathBuf::from("/dev/sda1"),
            exit_code: 8,
            output: String::new(),
            success: false,
            reboot_required: false,
        };
        assert!(result.has_operational_error());
    }
}
