//! Parse the factory-reset JSON payload from the bootloader env var and build
//! the preserve list.
//!
//! Bootloader var format: `{"mode": N, "preserve": ["applications", ...]}`
//!
//! Preserve list construction (matches legacy bash):
//! 1. Always include `/etc/omnect/factory-reset.d/`
//! 2. If `"applications"` in preserve array: scan all `.json` files in
//!    `rootfs/etc/omnect/factory-reset.d/` and collect `.paths[]`
//! 3. For every other key: read `.key[]` from `rootfs/etc/omnect/factory-reset.json`

use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::error::{FactoryResetError, Result};

/// Paths relative to rootfs
const FACTORY_RESET_CONFIG_FILE: &str = "etc/omnect/factory-reset.json";
const FACTORY_RESET_CONFIG_DIR: &str = "etc/omnect/factory-reset.d";

/// Preserve list always contains this entry.
const PRESERVE_LIST_MANDATORY: &str = "/etc/omnect/factory-reset.d/";

/// Parsed factory reset configuration from the bootloader env var.
#[derive(Debug, Deserialize)]
pub struct FactoryResetConfig {
    pub mode: u32,
    #[serde(default)]
    pub preserve: Vec<String>,
}

impl FactoryResetConfig {
    /// Parse from the JSON string stored in the bootloader env var.
    pub fn parse(json: &str) -> Result<Self> {
        serde_json::from_str(json).map_err(|e| {
            FactoryResetError::InvalidConfig(format!("Failed to parse factory-reset JSON: {}", e))
                .into()
        })
    }
}

/// Build the preserve list from the config and rootfs filesystem.
///
/// Always includes `/etc/omnect/factory-reset.d/`. If `"applications"` is in
/// the preserve array, scans `*.json` files in that directory for `.paths[]`
/// entries. Other keys are resolved against `etc/omnect/factory-reset.json`.
pub fn build_preserve_list(config: &FactoryResetConfig, rootfs: &Path) -> Result<Vec<String>> {
    let mut list: Vec<String> = vec![PRESERVE_LIST_MANDATORY.to_string()];

    for key in &config.preserve {
        if key == "applications" {
            collect_application_paths(rootfs, &mut list)?;
        } else {
            collect_key_paths(key, rootfs, &mut list)?;
        }
    }

    Ok(list)
}

/// Scan `etc/omnect/factory-reset.d/*.json` and collect `.paths[]` entries.
fn collect_application_paths(rootfs: &Path, list: &mut Vec<String>) -> Result<()> {
    let dir = rootfs.join(FACTORY_RESET_CONFIG_DIR);

    if !dir.exists() {
        return Ok(());
    }

    let entries = std::fs::read_dir(&dir).map_err(|e| {
        FactoryResetError::InvalidConfig(format!("Failed to read {}: {}", dir.display(), e))
    })?;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }

        let content = std::fs::read_to_string(&path).map_err(|e| {
            FactoryResetError::InvalidConfig(format!("Failed to read {}: {}", path.display(), e))
        })?;

        let value: Value = serde_json::from_str(&content).map_err(|e| {
            FactoryResetError::MissingField(format!("{}: no path object ({})", path.display(), e))
        })?;

        let paths = value.get("paths").ok_or_else(|| {
            FactoryResetError::MissingField(format!("{}: no path object", path.display()))
        })?;

        if let Some(arr) = paths.as_array() {
            for p in arr {
                if let Some(s) = p.as_str() {
                    list.push(s.to_string());
                }
            }
        }
    }

    Ok(())
}

/// Read `.key[]` from `etc/omnect/factory-reset.json` and append to list.
fn collect_key_paths(key: &str, rootfs: &Path, list: &mut Vec<String>) -> Result<()> {
    let config_file = rootfs.join(FACTORY_RESET_CONFIG_FILE);

    let content = std::fs::read_to_string(&config_file).map_err(|e| {
        FactoryResetError::InvalidConfig(format!("Failed to read {}: {}", config_file.display(), e))
    })?;

    let value: Value = serde_json::from_str(&content).map_err(|e| {
        FactoryResetError::InvalidConfig(format!(
            "Failed to parse {}: {}",
            config_file.display(),
            e
        ))
    })?;

    let paths = value.get(key).ok_or_else(|| {
        FactoryResetError::MissingField(format!("{}: no {} object", config_file.display(), key))
    })?;

    if let Some(arr) = paths.as_array() {
        for p in arr {
            if let Some(s) = p.as_str() {
                list.push(s.to_string());
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_parse_mode_and_preserve() {
        let cfg = FactoryResetConfig::parse(r#"{"mode":1,"preserve":[]}"#).unwrap();
        assert_eq!(cfg.mode, 1);
        assert!(cfg.preserve.is_empty());
    }

    #[test]
    fn test_parse_with_preserve_keys() {
        let cfg = FactoryResetConfig::parse(r#"{"mode":2,"preserve":["applications","network"]}"#)
            .unwrap();
        assert_eq!(cfg.mode, 2);
        assert_eq!(cfg.preserve, vec!["applications", "network"]);
    }

    #[test]
    fn test_parse_invalid_json() {
        assert!(FactoryResetConfig::parse("not json").is_err());
    }

    #[test]
    fn test_parse_missing_mode() {
        assert!(FactoryResetConfig::parse(r#"{"preserve":[]}"#).is_err());
    }

    #[test]
    fn test_build_preserve_list_empty_preserve() {
        let temp = TempDir::new().unwrap();
        let cfg = FactoryResetConfig {
            mode: 1,
            preserve: vec![],
        };
        let list = build_preserve_list(&cfg, temp.path()).unwrap();
        assert_eq!(list, vec!["/etc/omnect/factory-reset.d/"]);
    }

    #[test]
    fn test_build_preserve_list_applications() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("etc/omnect/factory-reset.d");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("app.json"),
            r#"{"paths":["/home/user/.config","var/app"]}"#,
        )
        .unwrap();

        let cfg = FactoryResetConfig {
            mode: 1,
            preserve: vec!["applications".into()],
        };
        let list = build_preserve_list(&cfg, temp.path()).unwrap();
        assert_eq!(list[0], "/etc/omnect/factory-reset.d/");
        assert!(list.contains(&"/home/user/.config".to_string()));
        assert!(list.contains(&"var/app".to_string()));
    }

    #[test]
    fn test_build_preserve_list_custom_key() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("etc/omnect");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("factory-reset.json"),
            r#"{"network":["/etc/network/interfaces","/etc/wpa_supplicant.conf"]}"#,
        )
        .unwrap();

        let cfg = FactoryResetConfig {
            mode: 1,
            preserve: vec!["network".into()],
        };
        let list = build_preserve_list(&cfg, temp.path()).unwrap();
        assert!(list.contains(&"/etc/network/interfaces".to_string()));
        assert!(list.contains(&"/etc/wpa_supplicant.conf".to_string()));
    }
}
