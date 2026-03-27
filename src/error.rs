//! Error types for the initramfs
//!
//! This module defines a hierarchy of error types for different subsystems.

use std::path::PathBuf;

use thiserror::Error;

/// Result type alias for the initramfs
pub type Result<T> = std::result::Result<T, InitramfsError>;

/// Top-level error type for the initramfs
#[derive(Error, Debug)]
pub enum InitramfsError {
    #[error("Bootloader error: {0}")]
    Bootloader(#[from] BootloaderError),

    #[error("Early init error: {0}")]
    EarlyInit(#[from] EarlyInitError),

    #[error("Config error: {0}")]
    Config(#[from] ConfigError),

    #[error("Partition error: {0}")]
    Partition(#[from] PartitionError),

    #[error("Filesystem error: {0}")]
    Filesystem(#[from] FilesystemError),

    #[error("Factory reset error: {0}")]
    FactoryReset(#[from] FactoryResetError),

    #[error("Flash mode error: {0}")]
    FlashMode(#[from] FlashModeError),

    #[error("Resize data error: {0}")]
    ResizeData(#[from] ResizeDataError),

    #[error("Logging error: {0}")]
    Logging(#[from] LoggingError),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors during early initialization (before logging is available)
#[derive(Error, Debug)]
pub enum EarlyInitError {
    #[error("Failed to mount {target}: {reason}")]
    MountFailed { target: String, reason: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors related to bootloader environment access
#[derive(Error, Debug)]
pub enum BootloaderError {
    #[error("Bootloader environment file not found: {}", path.display())]
    EnvFileNotFound { path: PathBuf },

    #[error("Command '{command}' failed: {reason}")]
    CommandFailed { command: String, reason: String },

    #[error("Command '{command}' exited with code {code:?}: {stderr}")]
    CommandExitCode {
        command: String,
        code: Option<i32>,
        stderr: String,
    },

    #[error("Compression failed: {0}")]
    CompressionFailed(String),

    #[error("Decompression failed: {0}")]
    DecompressionFailed(String),

    #[error("Invalid environment value for '{key}': {reason}")]
    InvalidValue { key: String, reason: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors related to configuration parsing
#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("Failed to read {path}: {reason}")]
    ReadFailed { path: String, reason: String },

    #[error("Missing required kernel parameter: {0}")]
    MissingParameter(String),

    #[error("Invalid parameter value for '{key}': {value}")]
    InvalidParameter { key: String, value: String },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors related to partition detection and management
#[derive(Error, Debug)]
pub enum PartitionError {
    #[error("device detection failed: {0}")]
    DeviceDetection(String),

    #[error("invalid partition table on {}: {reason}", device.display())]
    InvalidPartitionTable { device: PathBuf, reason: String },

    #[error("symlink creation failed for {} -> {}: {reason}", link.display(), target.display())]
    SymlinkFailed {
        link: PathBuf,
        target: PathBuf,
        reason: String,
    },

    #[error("symlink removal failed for {}: {reason}", path.display())]
    SymlinkRemoveFailed { path: PathBuf, reason: String },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors related to filesystem operations
#[derive(Error, Debug)]
pub enum FilesystemError {
    #[error("Failed to mount {} on {}: {reason}", src_path.display(), target.display())]
    MountFailed {
        src_path: PathBuf,
        target: PathBuf,
        reason: String,
    },

    #[error("Failed to unmount {}: {reason}", target.display())]
    UnmountFailed { target: PathBuf, reason: String },

    #[error("Filesystem check failed for {} with code {code}: {output}", device.display())]
    FsckFailed {
        device: PathBuf,
        code: i32,
        output: String,
    },

    #[error("Filesystem check for {} requires reboot (fsck exit code {code})", device.display())]
    FsckRequiresReboot {
        device: PathBuf,
        code: i32,
        output: String,
    },

    #[error("Overlayfs setup failed for {}: {reason}", target.display())]
    OverlayFailed { target: PathBuf, reason: String },

    #[error("Failed to format {} as {fstype}: {reason}", device.display())]
    FormatFailed {
        device: PathBuf,
        fstype: String,
        reason: String,
    },

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors related to factory reset operations
#[derive(Error, Debug)]
pub enum FactoryResetError {
    #[error("Invalid factory reset configuration: {0}")]
    InvalidConfig(String),

    #[error("Missing required field in factory reset config: {0}")]
    MissingField(String),

    #[error("Backup failed for path '{path}': {reason}")]
    BackupFailed { path: String, reason: String },

    #[error("Restore failed for path '{path}': {reason}")]
    RestoreFailed { path: String, reason: String },

    #[error("Wipe failed for partition '{partition}': {reason}")]
    WipeFailed { partition: String, reason: String },

    #[error("Reformat failed for '{device}': {reason}")]
    ReformatFailed { device: String, reason: String },

    #[error("Mount error during factory reset: {0}")]
    MountError(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors related to flash mode operations
#[derive(Error, Debug)]
pub enum FlashModeError {
    #[error("Invalid flash mode: {0}")]
    InvalidMode(String),

    #[error("Destination device not found: {}", .0.display())]
    DestinationNotFound(PathBuf),

    #[error("Clone failed: {0}")]
    CloneFailed(String),

    #[error("Network setup failed: {0}")]
    NetworkFailed(String),

    #[error("Download failed: {0}")]
    DownloadFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors related to data partition resize
#[derive(Error, Debug)]
pub enum ResizeDataError {
    #[error("Command '{command}' failed with code {code}: {output}")]
    CommandFailed {
        command: String,
        code: i32,
        output: String,
    },

    #[error("Could not determine partition number from device path: {}", .0.display())]
    InvalidDevicePath(PathBuf),

    #[error("Could not find extended partition on {}", .0.display())]
    ExtendedPartitionNotFound(PathBuf),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Errors related to logging
#[derive(Error, Debug)]
pub enum LoggingError {
    #[error("Failed to open kmsg: {0}")]
    KmsgOpenFailed(String),

    #[error("Failed to initialize logger: {0}")]
    InitFailed(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}
