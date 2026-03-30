//! Factory reset status — written to /tmp/factory-reset.json and later
//! copied by omnect-device-service into its runtime directory.

use std::fs;

use serde::Serialize;

use crate::error::{FactoryResetError, InitramfsError, Result};

/// Path for the factory reset status file (in initramfs tmpfs).
pub const FACTORY_RESET_STATUS_TMP: &str = "/tmp/factory-reset.json";

/// Status codes matching the legacy bash script.
pub mod status_code {
    pub const SUCCESS: u32 = 0;
    pub const INVALID_MODE: u32 = 1;
    pub const BACKUP_RESTORE_ERROR: u32 = 2;
    pub const CONFIG_ERROR: u32 = 3;
}

/// Factory reset execution status, serialised to JSON for omnect-device-service.
#[derive(Debug, Clone, Serialize)]
pub struct FactoryResetStatus {
    pub status: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

impl FactoryResetStatus {
    pub fn success(paths: Vec<String>) -> Self {
        Self {
            status: status_code::SUCCESS,
            error: None,
            context: None,
            paths,
        }
    }

    pub fn error(
        code: u32,
        error: impl Into<String>,
        context: Option<String>,
        paths: Vec<String>,
    ) -> Self {
        Self {
            status: code,
            error: Some(error.into()),
            context,
            paths,
        }
    }
}

/// Write null sentinel to status file at start of factory reset so that a
/// crash mid-way leaves a detectable marker (matches legacy `echo "null"`).
pub fn write_null_status() -> Result<()> {
    fs::write(FACTORY_RESET_STATUS_TMP, "null").map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to write null status to {}: {}",
            FACTORY_RESET_STATUS_TMP, e
        )))
    })
}

/// Persist the final status struct as JSON.
pub fn write_status(status: &FactoryResetStatus) -> Result<()> {
    let json = serde_json::to_string(status).map_err(|e| {
        InitramfsError::FactoryReset(FactoryResetError::InvalidConfig(format!(
            "Failed to serialize status: {}",
            e
        )))
    })?;
    fs::write(FACTORY_RESET_STATUS_TMP, json).map_err(|e| {
        InitramfsError::Io(std::io::Error::other(format!(
            "Failed to write status to {}: {}",
            FACTORY_RESET_STATUS_TMP, e
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_success_status_serialization() {
        let s = FactoryResetStatus::success(vec!["/etc/omnect/factory-reset.d/".into()]);
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["status"], 0);
        assert!(json.get("error").is_none());
        assert!(json.get("context").is_none());
    }

    #[test]
    fn test_error_status_with_context() {
        let s = FactoryResetStatus::error(
            status_code::BACKUP_RESTORE_ERROR,
            "66",
            Some("etc/hostname:restore".into()),
            vec!["/etc/omnect/factory-reset.d/".into()],
        );
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["status"], 2);
        assert_eq!(json["error"], "66");
        assert_eq!(json["context"], "etc/hostname:restore");
    }
}
