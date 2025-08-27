// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::EnvironmentContext;
use crate::mapping::replace;
use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

pub(crate) fn config(ctx: &EnvironmentContext, value: Value) -> Option<Value> {
    static REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\$(CONFIG)").unwrap());

    replace(&*REGEX, || ctx.get_config_path(), value)
}

////////////////////////////////////////////////////////////////////////////////
// tests
#[cfg(test)]
mod test {
    use super::*;
    use crate::ConfigMap;
    use crate::environment::ExecutableKind;

    #[test]
    fn test_mapper() {
        let ctx = EnvironmentContext::isolated(
            ExecutableKind::Test,
            "/tmp".into(),
            Default::default(),
            ConfigMap::default(),
            None,
            None,
            false,
        )
        .unwrap();
        let value =
            ctx.get_config_path().expect("Getting config dir").to_string_lossy().to_string();
        let test = Value::String("$CONFIG".to_string());
        assert_eq!(config(&ctx, test), Some(Value::String(value)));
    }

    #[test]
    fn test_mapper_multiple() {
        let ctx = EnvironmentContext::isolated(
            ExecutableKind::Test,
            "/tmp".into(),
            Default::default(),
            ConfigMap::default(),
            None,
            None,
            false,
        )
        .unwrap();
        let value =
            ctx.get_config_path().expect("Getting config dir").to_string_lossy().to_string();
        let test = Value::String("$CONFIG/$CONFIG".to_string());
        assert_eq!(config(&ctx, test), Some(Value::String(format!("{}/{}", value, value))));
    }

    #[test]
    fn test_mapper_returns_pass_through() {
        let ctx = EnvironmentContext::isolated(
            ExecutableKind::Test,
            "/tmp".into(),
            Default::default(),
            ConfigMap::default(),
            None,
            None,
            false,
        )
        .unwrap();
        let test = Value::String("$WHATEVER".to_string());
        assert_eq!(config(&ctx, test), Some(Value::String("$WHATEVER".to_string())));
    }
}
