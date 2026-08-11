// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use log::error;

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
pub(crate) struct BatteryManagerConfig {
    #[serde(default)]
    pub shutdown_offset_percent: f32,
    #[serde(default)]
    pub ttf_tier_thresholds: Option<Vec<f32>>,
    #[serde(default)]
    pub ttf_charge_temp_limits: Option<Vec<i32>>,
    #[serde(default)]
    pub chg_cc_limits_ua: Option<Vec<Vec<i32>>>,
}

impl BatteryManagerConfig {
    #[cfg(test)]
    pub(crate) fn default_for_test() -> Self {
        BatteryManagerConfig {
            shutdown_offset_percent: 3.0,
            ttf_tier_thresholds: Some(vec![0.0, 84.0, 90.0]),
            ttf_charge_temp_limits: Some(vec![0, 10_000, 20_000, 42_000, 46_000]),
            chg_cc_limits_ua: Some(vec![
                vec![200_000, 100_000, 100_000],
                vec![275_000, 100_000, 100_000],
                vec![500_000, 500_000, 200_000],
                vec![400_000, 400_000, 200_000],
            ]),
        }
    }
}

pub(crate) fn read_battery_manager_config(path: &str) -> Result<BatteryManagerConfig, Error> {
    let contents = std::fs::read_to_string(path).map_err(|e| {
        let err = anyhow::format_err!(
            "Failed to read battery manager config at '{path}': {e}. \
            Please verify the configuration file path and permissions."
        );
        error!("{err}");
        err
    })?;
    parse_battery_manager_config(&contents, path)
}

fn parse_battery_manager_config(contents: &str, path: &str) -> Result<BatteryManagerConfig, Error> {
    let config: BatteryManagerConfig = serde_json::from_str(contents).map_err(|e| {
        let err = anyhow::format_err!(
            "Failed to parse battery manager config at '{path}': {e}. \
            Ensure the configuration file contains valid JSON matching the \
            BatteryManagerConfig schema."
        );
        error!("{err}");
        err
    })?;

    if config.shutdown_offset_percent.is_nan()
        || config.shutdown_offset_percent < 0.0
        || config.shutdown_offset_percent >= 100.0
    {
        let err = anyhow::format_err!(
            "Invalid battery manager config at '{path}': shutdown_offset_percent ({}) \
            must be in range [0.0, 100.0).",
            config.shutdown_offset_percent
        );
        error!("{err}");
        return Err(err);
    }

    let has_temp_limits = config.ttf_charge_temp_limits.is_some();
    let has_tier_thresholds = config.ttf_tier_thresholds.is_some();
    let has_cc_limits = config.chg_cc_limits_ua.is_some();

    if has_temp_limits || has_tier_thresholds || has_cc_limits {
        if !(has_temp_limits && has_tier_thresholds && has_cc_limits) {
            let err = anyhow::format_err!(
                "Invalid battery manager config at '{path}': \
                ttf_charge_temp_limits, ttf_tier_thresholds, and \
                chg_cc_limits_ua must all be provided together.",
            );
            error!("{err}");
            return Err(err);
        }

        let temp_limits = config.ttf_charge_temp_limits.as_ref().unwrap();
        let tier_thresholds = config.ttf_tier_thresholds.as_ref().unwrap();
        let cc_matrix = config.chg_cc_limits_ua.as_ref().unwrap();

        let expected_rows = temp_limits.len().saturating_sub(1);
        if cc_matrix.len() != expected_rows {
            let err = anyhow::format_err!(
                "Invalid battery manager config at '{path}': \
                chg_cc_limits_ua row count ({}) must match \
                ttf_charge_temp_limits count minus 1 ({}).",
                cc_matrix.len(),
                expected_rows
            );
            error!("{err}");
            return Err(err);
        }

        // TODO(https://fxbug.dev/540034557): When chg_cv_limits_uv is added, validate that
        // chg_cc_limits_ua column count exactly matches chg_cv_limits_uv.len().
        // For now, TTF estimation only requires num_cols >= tier_thresholds.len().
        let expected_min_cols = tier_thresholds.len();
        let num_cols = cc_matrix.first().map(|r| r.len()).unwrap_or(0);
        if num_cols < expected_min_cols {
            let err = anyhow::format_err!(
                "Invalid battery manager config at '{path}': \
                chg_cc_limits_ua column count ({num_cols}) must be at least \
                ttf_tier_thresholds count ({expected_min_cols}).",
            );
            error!("{err}");
            return Err(err);
        }

        for (i, row) in cc_matrix.iter().enumerate() {
            if row.len() != num_cols {
                let err = anyhow::format_err!(
                    "Invalid battery manager config at '{path}': \
                    chg_cc_limits_ua row {i} length ({}) must match \
                    row 0 length ({num_cols}).",
                    row.len()
                );
                error!("{err}");
                return Err(err);
            }
        }
    }

    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_battery_manager_config_valid() {
        let json = r#"{
            "shutdown_offset_percent": 3.0
        }"#;
        let config = parse_battery_manager_config(json, "test_path").unwrap();
        assert_eq!(config.shutdown_offset_percent, 3.0);
    }

    #[test]
    fn test_parse_battery_manager_config_default() {
        let json = r#"{}"#;
        let config = parse_battery_manager_config(json, "test_path").unwrap();
        assert_eq!(config.shutdown_offset_percent, 0.0);
    }

    #[test]
    fn test_parse_battery_manager_config_invalid_json() {
        let json = r#"{"shutdown_offset_percent": invalid_value}"#;
        let err = parse_battery_manager_config(json, "test_path").unwrap_err();
        assert!(err.to_string().contains("Failed to parse battery manager config"));
    }

    #[test]
    fn test_parse_battery_manager_config_out_of_range() {
        let json = r#"{"shutdown_offset_percent": -5.0}"#;
        let err = parse_battery_manager_config(json, "test_path").unwrap_err();
        assert!(err.to_string().contains("must be in range"));

        let json = r#"{"shutdown_offset_percent": 100.0}"#;
        let err = parse_battery_manager_config(json, "test_path").unwrap_err();
        assert!(err.to_string().contains("must be in range"));
    }

    #[test]
    fn test_parse_battery_manager_config_partial_cc_limits() {
        let json = r#"{
            "ttf_tier_thresholds": [0.0, 84.0, 90.0]
        }"#;
        let err = parse_battery_manager_config(json, "test_path").unwrap_err();
        assert!(err.to_string().contains("must all be provided together"));
    }

    #[test]
    fn test_parse_battery_manager_config_cc_limits_with_extra_columns() {
        // Valid matrix with more columns (4) than ttf_tier_thresholds (2)
        let json = r#"{
            "ttf_charge_temp_limits": [0, 10000, 20000],
            "ttf_tier_thresholds": [0.0, 84.0],
            "chg_cc_limits_ua": [
                [200000, 100000, 50000, 25000],
                [200000, 100000, 50000, 25000]
            ]
        }"#;
        let config = parse_battery_manager_config(json, "test_path").unwrap();
        let cc_limits = config.chg_cc_limits_ua.unwrap();
        assert_eq!(cc_limits.len(), 2);
        assert_eq!(cc_limits[0].len(), 4);
        assert_eq!(cc_limits[1].len(), 4);
    }

    #[test]
    fn test_parse_battery_manager_config_mismatched_cc_limits() {
        // Mismatched row count (temp_limits has 3 entries expecting 2 rows, but chg_cc_limits_ua
        // has 1 row)
        let json_rows = r#"{
            "ttf_charge_temp_limits": [0, 10000, 20000],
            "ttf_tier_thresholds": [0.0, 84.0],
            "chg_cc_limits_ua": [
                [200000, 100000]
            ]
        }"#;
        let err = parse_battery_manager_config(json_rows, "test_path").unwrap_err();
        assert!(err.to_string().contains("row count"));

        // Column count less than ttf_tier_thresholds (1 col < 2 tier thresholds)
        let json_fewer_cols = r#"{
            "ttf_charge_temp_limits": [0, 10000, 20000],
            "ttf_tier_thresholds": [0.0, 84.0],
            "chg_cc_limits_ua": [
                [200000],
                [200000]
            ]
        }"#;
        let err = parse_battery_manager_config(json_fewer_cols, "test_path").unwrap_err();
        assert!(err.to_string().contains("column count"));

        // Non-uniform row lengths (row 1 has 2 cols, row 0 has 3 cols)
        let json_ragged_cols = r#"{
            "ttf_charge_temp_limits": [0, 10000, 20000],
            "ttf_tier_thresholds": [0.0, 84.0],
            "chg_cc_limits_ua": [
                [200000, 100000, 50000],
                [200000, 100000]
            ]
        }"#;
        let err = parse_battery_manager_config(json_ragged_cols, "test_path").unwrap_err();
        assert!(err.to_string().contains("row 1 length"));
    }
}
