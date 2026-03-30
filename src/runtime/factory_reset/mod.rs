//! Factory reset orchestration.
//!
//! Entry point: `run_factory_reset()`. Mirrors the legacy `factory_reset_run()`
//! bash function step-for-step.
//!
//! Called from main before `mount_late_partitions` when the `factory-reset`
//! bootloader env var is set. Normal partition mounts, overlays and ODS setup
//! are skipped when factory reset runs.

pub mod backup_restore;
pub mod config;
pub mod reformat;
pub mod status;

use std::path::{Path, PathBuf};

use crate::bootloader::{Bootloader, vars};
use crate::error::{FactoryResetError, InitramfsError, Result};
use crate::filesystem::MountManager;
use crate::runtime::factory_reset::{
    backup_restore::{RestoreResult, backup_all, restore_all},
    config::{FactoryResetConfig, build_preserve_list},
    reformat::reformat_ext4,
    status::{FactoryResetStatus, status_code, write_null_status, write_status},
};

const FACTORY_RESET_BACKUP_DIR: &str = "/tmp/factory_reset/backup";

/// Omnect partition device symlinks
mod omnect_dev {
    pub const FACTORY: &str = "/dev/omnect/factory";
    pub const ETC: &str = "/dev/omnect/etc";
    pub const DATA: &str = "/dev/omnect/data";
}

/// Check whether the `factory-reset` bootloader env var is set.
/// Returns `Some(json_string)` if factory reset should run, `None` otherwise.
pub fn factory_reset_requested(bootloader: &mut dyn Bootloader) -> Option<String> {
    match bootloader.get_env(vars::FACTORY_RESET) {
        Ok(Some(val)) if !val.is_empty() => Some(val),
        _ => None,
    }
}

/// Run the full factory reset sequence.
///
/// Step sequence (mirrors legacy factory_reset_run):
/// 1.  Write `null` sentinel to `/tmp/factory-reset.json`
/// 2.  Clear `factory-reset` bootloader env var
/// 3.  If GRUB: sync + unmount `/rootfs/boot`
/// 4.  Mount: rootCurrent (ro), factory (ro), etc (rw), data (rw)
/// 5.  Parse JSON config: extract mode and preserve array
/// 6.  Build preserve_list (mandatory + applications + custom keys)
/// 7.  Backup preserve_list → `/tmp/factory_reset/backup/`
/// 8.  Unmount all
/// 9.  Mode 1 = no wipe (modes 2-4 are PR8)
/// 10. Reformat `/dev/omnect/data` (label=data) + `/dev/omnect/etc` (label=etc)
/// 11. Remount: rootCurrent (ro), factory (ro), etc (rw), data (rw)
/// 12. Restore preserve_list with nested-mount detection
/// 13. Unmount all
/// 14. Write final status JSON to `/tmp/factory-reset.json`
pub fn run_factory_reset(
    factory_reset_json: &str,
    bootloader: &mut dyn Bootloader,
    rootfs: &Path,
    is_grub: bool,
    persistent_var_log: bool,
) -> Result<()> {
    // Step 1: sentinel so a crash mid-way leaves a detectable marker
    write_null_status()?;

    // Step 2: clear bootloader var so factory reset doesn't re-trigger on next boot
    bootloader.set_env(vars::FACTORY_RESET, None).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to clear factory-reset bootloader var: {}",
            e
        )))
    })?;

    // Step 3: GRUB already mounted /boot to access grubenv; unmount it now so
    // factory_reset_mount/umount can manage it without conflicts.
    if is_grub {
        sync_system();
        unmount_boot(rootfs)?;
    }

    // Step 4: mount partitions for backup
    let mut mm = MountManager::new();
    factory_reset_mount(&mut mm, rootfs, persistent_var_log)?;

    // Step 5-6: parse config and build preserve list
    let cfg = match FactoryResetConfig::parse(factory_reset_json) {
        Ok(c) => c,
        Err(e) => {
            let _ = write_status(&FactoryResetStatus::error(
                status_code::INVALID_MODE,
                "-",
                Some(format!("mode not set: {}", e)),
                vec![],
            ));
            return Err(e);
        }
    };

    log::info!(
        "Running factory-reset [mode={}, preserve={:?}]",
        cfg.mode,
        cfg.preserve
    );

    let preserve_list = match build_preserve_list(&cfg, rootfs) {
        Ok(l) => l,
        Err(e) => {
            let _ = write_status(&FactoryResetStatus::error(
                status_code::CONFIG_ERROR,
                "-",
                Some(format!("preserve object not set: {}", e)),
                vec![],
            ));
            return Err(e);
        }
    };

    log::info!("factory-reset preserve_list={:?}", preserve_list);

    // Step 7: backup
    let backup_dir = PathBuf::from(FACTORY_RESET_BACKUP_DIR);
    backup_all(rootfs, &preserve_list, &backup_dir).inspect_err(|e| {
        let _ = write_status(&FactoryResetStatus::error(
            status_code::BACKUP_RESTORE_ERROR,
            e.to_string(),
            Some(format!("{:?}:backup", preserve_list)),
            preserve_list.clone(),
        ));
    })?;

    // Step 8: unmount before wipe/reformat
    factory_reset_umount(&mut mm)?;

    // Step 9: wipe (mode 1 = none; modes 2-4 are PR8)
    let mut warnings: Vec<String> = Vec::new();
    match cfg.mode {
        1 => {} // no wipe
        _ => {
            let msg = format!(
                "factory reset mode \"{}\" not supported; skipping wipe",
                cfg.mode
            );
            log::warn!("{}", msg);
            warnings.push(format!("warning: {}", msg));
        }
    }

    // Step 10: reformat
    reformat_ext4(Path::new(omnect_dev::DATA), "data")?;
    reformat_ext4(Path::new(omnect_dev::ETC), "etc")?;

    // Step 11: remount for restore
    let mut mm2 = MountManager::new();
    factory_reset_mount(&mut mm2, rootfs, persistent_var_log)?;

    // Step 12: restore
    let restore_result = restore_all(rootfs, &preserve_list, &backup_dir)?;

    // Step 13: unmount
    factory_reset_umount(&mut mm2)?;

    // Step 14: write final status
    let final_status = match restore_result {
        RestoreResult::Success => {
            let context = build_warnings_context(None, &warnings);
            FactoryResetStatus {
                status: status_code::SUCCESS,
                error: Some("0".to_string()),
                context,
                paths: preserve_list,
            }
        }
        RestoreResult::PartialFailure { context, error } => {
            let combined = build_warnings_context(Some(context), &warnings);
            FactoryResetStatus::error(
                status_code::BACKUP_RESTORE_ERROR,
                error,
                combined,
                preserve_list,
            )
        }
    };

    write_status(&final_status)?;
    log::info!("factory-reset done");

    Ok(())
}

/// Mount partitions needed during factory reset.
///
/// rootCurrent is already mounted at `rootfs` by `mount_early_partitions()` —
/// we must not remount it. Only factory, etc and data are mounted here.
/// Order matches legacy factory_reset_mount() minus the rootCurrent step.
fn factory_reset_mount(
    mm: &mut MountManager,
    rootfs: &Path,
    _persistent_var_log: bool,
) -> Result<()> {
    let factory_mount = rootfs.join("mnt/factory");
    std::fs::create_dir_all(&factory_mount)?;
    mm.mount_readonly(omnect_dev::FACTORY, &factory_mount, "ext4")
        .map_err(|e| FactoryResetError::MountError(format!("factory: {}", e)))?;

    let etc_mount = rootfs.join("mnt/etc");
    std::fs::create_dir_all(&etc_mount)?;
    mm.mount_readwrite(omnect_dev::ETC, &etc_mount, "ext4")
        .map_err(|e| FactoryResetError::MountError(format!("etc: {}", e)))?;

    let data_mount = rootfs.join("mnt/data");
    std::fs::create_dir_all(&data_mount)?;
    mm.mount_readwrite(omnect_dev::DATA, &data_mount, "ext4")
        .map_err(|e| FactoryResetError::MountError(format!("data: {}", e)))?;

    Ok(())
}

/// Unmount all factory reset partitions in reverse order.
fn factory_reset_umount(mm: &mut MountManager) -> Result<()> {
    mm.umount_all()
        .map_err(|e| FactoryResetError::MountError(format!("umount: {}", e)))?;
    Ok(())
}

/// Unmount the boot partition (GRUB only — it was mounted during early init).
fn unmount_boot(rootfs: &Path) -> Result<()> {
    let boot_mount = rootfs.join("boot");
    nix::mount::umount2(&boot_mount, nix::mount::MntFlags::empty())
        .map_err(|e| FactoryResetError::MountError(format!("umount boot: {}", e)))?;
    Ok(())
}

fn sync_system() {
    let _ = std::process::Command::new("/bin/sync").status();
}

/// Combine restore error context with accumulated wipe warnings.
/// Matches legacy: `context+="${warnings}"` separated by `;`.
fn build_warnings_context(context: Option<String>, warnings: &[String]) -> Option<String> {
    if warnings.is_empty() {
        return context;
    }
    let w = warnings.join(";");
    Some(match context {
        Some(c) if !c.is_empty() => format!("{};{}", c, w),
        _ => w,
    })
}
