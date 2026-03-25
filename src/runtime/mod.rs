//! Runtime setup and integration modules
//!
//! This module handles:
//! - omnect-device-service runtime file creation
//! - fs-link symbolic link creation
//! - switch_root to final rootfs

mod fs_link;
mod omnect_device_service;
#[cfg(feature = "resize-data")]
pub mod resize_data;
mod switch_root;

pub use self::fs_link::create_fs_links;
pub use self::omnect_device_service::{OdsStatus, create_ods_runtime_files};
pub use self::switch_root::switch_root;
