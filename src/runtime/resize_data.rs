//! Data partition auto-resize
//!
//! Expands the data partition and its ext4 filesystem to fill available disk
//! space on first boot. Guarded by the `resized-data` bootloader variable so
//! it runs exactly once.

use std::path::Path;
use std::process::Command;

use crate::bootloader::{Bootloader, vars};
use crate::error::{ResizeDataError, Result};
use crate::partition::PartitionTableType;

const SGDISK_CMD: &str = "/usr/sbin/sgdisk";
const PARTED_CMD: &str = "/usr/sbin/parted";
const E2FSCK_CMD: &str = "/sbin/e2fsck";
const RESIZE2FS_CMD: &str = "/sbin/resize2fs";
const SYNC_CMD: &str = "/bin/sync";

const MTAB_PATH: &str = "/etc/mtab";
const PROC_MOUNTS_PATH: &str = "/proc/self/mounts";

type ResizeResult<T> = std::result::Result<T, ResizeDataError>;

/// Resize the data partition and filesystem to fill the disk, unless already done.
///
/// Must be called BEFORE the data partition is mounted.
pub fn resize_data_if_needed(
    data_dev: &Path,
    rootblk: &Path,
    table_type: PartitionTableType,
    bootloader: &mut dyn Bootloader,
) -> Result<()> {
    // Skip if already resized on a previous boot
    if bootloader.get_env(vars::RESIZED_DATA)?.is_some() {
        log::info!("Data partition already resized, skipping");
        return Ok(());
    }

    let part_nr = partition_number(data_dev)
        .ok_or_else(|| ResizeDataError::InvalidDevicePath(data_dev.to_path_buf()))?;

    log::info!(
        "Resizing data partition: {} partition {}",
        rootblk.display(),
        part_nr
    );

    match table_type {
        PartitionTableType::Gpt => {
            // Move backup GPT header to end of disk before resizing
            run_cmd(SGDISK_CMD, &[rootblk.to_str().unwrap_or(""), "-e"])?;
        }
        PartitionTableType::Dos => {
            // Resize extended partition to 100% before resizing the logical partition inside it
            let ext_nr = find_extended_partition(rootblk)?;
            run_cmd(
                PARTED_CMD,
                &[
                    rootblk.to_str().unwrap_or(""),
                    "resizepart",
                    &ext_nr.to_string(),
                    "100%",
                ],
            )?;
        }
    }

    run_cmd(
        PARTED_CMD,
        &[
            rootblk.to_str().unwrap_or(""),
            "resizepart",
            &part_nr.to_string(),
            "100%",
        ],
    )?;

    // resize2fs requires /etc/mtab to exist; point it at the live mount table
    ensure_mtab()?;

    // Fsck before resize2fs — exit code 1 (errors corrected) is acceptable
    run_e2fsck(data_dev)?;

    // Expand the filesystem to fill the enlarged partition
    run_cmd(RESIZE2FS_CMD, &["-f", data_dev.to_str().unwrap_or("")])?;

    run_cmd(SYNC_CMD, &[])?;

    bootloader.set_env(vars::RESIZED_DATA, Some("1"))?;

    log::info!("Data partition resize complete");
    Ok(())
}

/// Extract the trailing partition number from a device path.
/// e.g. /dev/sda8 → 8, /dev/mmcblk0p8 → 8, /dev/nvme0n1p7 → 7
fn partition_number(dev: &Path) -> Option<u32> {
    let name = dev.file_name()?.to_str()?;
    let digits: String = name
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    let number: String = digits.chars().rev().collect();
    number.parse().ok()
}

/// Parse `parted <disk> print` and return the number of the extended partition.
fn find_extended_partition(rootblk: &Path) -> ResizeResult<u32> {
    let out = Command::new(PARTED_CMD)
        .args([rootblk.to_str().unwrap_or(""), "print"])
        .output()
        .map_err(ResizeDataError::Io)?;

    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let lower = line.to_lowercase();
        if lower.contains("extended") {
            // parted print format: " N  start  end  size  type  ..."
            if let Some(nr_str) = line.split_whitespace().next()
                && let Ok(nr) = nr_str.parse::<u32>()
            {
                return Ok(nr);
            }
        }
    }

    Err(ResizeDataError::ExtendedPartitionNotFound(
        rootblk.to_path_buf(),
    ))
}

/// Ensure /etc/mtab exists and points to /proc/self/mounts.
/// resize2fs requires /etc/mtab to determine mounted filesystems.
fn ensure_mtab() -> ResizeResult<()> {
    let mtab = Path::new(MTAB_PATH);
    let target = Path::new(PROC_MOUNTS_PATH);

    if mtab.exists() && !mtab.is_symlink() {
        // Real file: leave it alone, resize2fs can use it
        return Ok(());
    }

    // Remove stale symlink if present
    if mtab.is_symlink() {
        std::fs::remove_file(mtab)?;
    }

    std::os::unix::fs::symlink(target, mtab)?;
    Ok(())
}

/// Run e2fsck with -y; treat exit codes 0 and 1 as success.
/// Exit code 1 means "errors were corrected", which is expected on
/// a freshly resized partition.
fn run_e2fsck(dev: &Path) -> ResizeResult<()> {
    let dev_str = dev.to_str().unwrap_or("");
    log::info!("Running: {} -y {}", E2FSCK_CMD, dev_str);

    let out = Command::new(E2FSCK_CMD)
        .args(["-y", dev_str])
        .output()
        .map_err(ResizeDataError::Io)?;

    let code = out.status.code().unwrap_or(-1);
    // 0 = clean, 1 = errors corrected; both are acceptable
    if code > 1 {
        let output = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        return Err(ResizeDataError::CommandFailed {
            command: format!("{} -y {}", E2FSCK_CMD, dev_str),
            code,
            output,
        });
    }

    Ok(())
}

/// Run an external command and return an error if it exits non-zero.
fn run_cmd(program: &str, args: &[&str]) -> ResizeResult<()> {
    log::info!("Running: {} {}", program, args.join(" "));

    let out = Command::new(program)
        .args(args)
        .output()
        .map_err(ResizeDataError::Io)?;

    if !out.status.success() {
        let code = out.status.code().unwrap_or(-1);
        let output = String::from_utf8_lossy(&out.stdout).into_owned()
            + &String::from_utf8_lossy(&out.stderr);
        return Err(ResizeDataError::CommandFailed {
            command: format!("{} {}", program, args.join(" ")),
            code,
            output,
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_partition_number_sata() {
        assert_eq!(partition_number(Path::new("/dev/sda8")), Some(8));
    }

    #[test]
    fn test_partition_number_nvme() {
        assert_eq!(partition_number(Path::new("/dev/nvme0n1p7")), Some(7));
    }

    #[test]
    fn test_partition_number_mmc() {
        assert_eq!(partition_number(Path::new("/dev/mmcblk0p8")), Some(8));
    }

    #[test]
    fn test_partition_number_multi_digit() {
        assert_eq!(partition_number(Path::new("/dev/sda10")), Some(10));
    }

    #[test]
    fn test_partition_number_no_digits() {
        assert_eq!(partition_number(Path::new("/dev/sda")), None);
    }

    #[test]
    fn test_find_extended_partition_parses_output() {
        // Simulate `parted /dev/sda print` output for a DOS disk
        // with an extended partition at number 3
        let output = "\
Model: ATA VBOX HARDDISK (scsi)
Disk /dev/sda: 8590MB
Sector size (logical/physical): 512B/512B
Partition Table: msdos
Disk Flags:

Number  Start   End     Size    Type      File system  Flags
 1      1049kB  500MB   499MB   primary   fat32        boot, esp
 2      500MB   1500MB  1000MB  primary   ext4
 3      1500MB  8589MB  7089MB  extended
 8      1501MB  8589MB  7088MB  logical   ext4";

        // We can't call find_extended_partition directly since it runs `parted`,
        // so we test the parsing logic manually here.
        let mut found: Option<u32> = None;
        for line in output.lines() {
            let lower = line.to_lowercase();
            if lower.contains("extended") {
                if let Some(nr_str) = line.split_whitespace().next() {
                    if let Ok(nr) = nr_str.parse::<u32>() {
                        found = Some(nr);
                        break;
                    }
                }
            }
        }
        assert_eq!(found, Some(3));
    }

    #[test]
    fn test_find_extended_partition_not_found() {
        // GPT disk output has no "extended" line
        let output = "\
Number  Start   End     Size   File system  Name  Flags
 1      1049kB  500MB   499MB  fat32              boot, esp
 7      500MB   8589MB  8089MB ext4";

        let found = output
            .lines()
            .find(|l| l.to_lowercase().contains("extended"));
        assert!(found.is_none());
    }
}
