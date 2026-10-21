// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// Returns the hardcoded dictionary key used for `ProviderId` types.
///
/// TODO(https://fxbug.dev/536123161): Remove this workaround when schema-defined
/// keys are supported for provider IDs.
pub fn provider_id_key() -> &'static str {
    "provider"
}

/// Returns the C++ expression (as a string literal) for the provider ID key.
pub fn provider_id_key_cpp_expr() -> &'static str {
    "\"provider\""
}

/// Deduplicates resources for specific metadata types that require unique entries.
///
/// Currently, this only supports `fuchsia.hardware.pinimpl.Metadata` by deduplicating
/// entries by their `pin` field.
///
/// TODO(https://fxbug.dev/536122353): Make deduplication generic (e.g. via schema annotations).
pub fn deduplicate_metadata_resources(metadata_id: &str, resources: &mut Vec<Value>) {
    if metadata_id == "fuchsia.hardware.pinimpl.Metadata" {
        let mut seen_pins = HashSet::new();
        resources.retain(|val| {
            if let Some(obj) = val.as_object() {
                if let Some(pin_num) = obj.get("pin").and_then(|v| v.as_i64()) {
                    return seen_pins.insert(pin_num);
                }
            }
            true
        });
    }
}

/// Attempts to generate a special bind rule for init steps.
///
/// Returns `Some(bind_rule_string)` if the service is a known init step,
/// or `None` if it should use standard service binding.
pub fn try_generate_init_step_bind_rule(service_name: &str) -> Option<String> {
    if service_name.ends_with(".Init") {
        let namespace = service_name.trim_end_matches(".Init");
        if !namespace.is_empty()
            && let Some(short_name) = namespace.split('.').next_back()
            && !short_name.is_empty()
        {
            return Some(format!(
                "  fuchsia.BIND_INIT_STEP == {}.BIND_INIT_STEP.{};\n",
                namespace,
                short_name.to_uppercase()
            ));
        }
    }
    None
}

/// Manages auto-incrementing fields in resource constraints.
///
/// TODO(https://fxbug.dev/536123469): Refactor auto-increment feature to be generic.
pub struct AutoIncrementer {
    service_to_field: HashMap<String, String>,
    counters: HashMap<(String, String), u32>,
}

impl AutoIncrementer {
    pub fn new(mappings: &[crate::parser::MetadataMapping]) -> Self {
        let mut service_to_field = HashMap::new();
        for mapping in mappings {
            for agg in &mapping.aggregations {
                if let Some(field) = &agg.auto_increment {
                    service_to_field.insert(agg.service.clone(), field.clone());
                }
            }
        }
        Self { service_to_field, counters: HashMap::new() }
    }

    pub fn apply(
        &mut self,
        service_name: &str,
        provider: &str,
        constraint_val: &mut Value,
    ) -> Result<(), anyhow::Error> {
        if let Some(field_name) = self.service_to_field.get(service_name) {
            let counter =
                self.counters.entry((provider.to_string(), service_name.to_string())).or_insert(1);
            let c_obj = constraint_val.as_object_mut().ok_or_else(|| {
                anyhow::anyhow!(
                    "Constraints must be a JSON object when auto-incrementing field '{}' for service '{}'",
                    field_name,
                    service_name
                )
            })?;
            c_obj.insert(field_name.clone(), Value::Number((*counter).into()));
            *counter += 1;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_try_generate_init_step_bind_rule() {
        assert_eq!(
            try_generate_init_step_bind_rule("fuchsia.gpio.Init").as_deref(),
            Some("  fuchsia.BIND_INIT_STEP == fuchsia.gpio.BIND_INIT_STEP.GPIO;\n")
        );
        assert_eq!(
            try_generate_init_step_bind_rule("fuchsia.clock.Init").as_deref(),
            Some("  fuchsia.BIND_INIT_STEP == fuchsia.clock.BIND_INIT_STEP.CLOCK;\n")
        );
        assert_eq!(
            try_generate_init_step_bind_rule("fuchsia.pwm.Init").as_deref(),
            Some("  fuchsia.BIND_INIT_STEP == fuchsia.pwm.BIND_INIT_STEP.PWM;\n")
        );
        assert_eq!(
            try_generate_init_step_bind_rule("fuchsia.power.Init").as_deref(),
            Some("  fuchsia.BIND_INIT_STEP == fuchsia.power.BIND_INIT_STEP.POWER;\n")
        );

        // Non-init services should return None
        assert_eq!(try_generate_init_step_bind_rule("fuchsia.hardware.gpio.Service"), None);
        assert_eq!(try_generate_init_step_bind_rule("fuchsia.gpio"), None);
        assert_eq!(try_generate_init_step_bind_rule("Init"), None);
        assert_eq!(try_generate_init_step_bind_rule(".Init"), None);
    }
}
