// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::ConfigValue;
use super::value::TryConvert;
use crate::api::ConfigResult;
use crate::nested::RecursiveMap;
use crate::{ConfigError, ConfigLevel, EnvironmentContext, ValueStrategy};

use serde_json::Value;
use std::default::Default;
use thiserror::Error;

#[derive(Debug, Copy, Clone, Error)]
pub enum QueryError {
    #[error("No EnvironmentContext set for query")]
    ContextNotSet,
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum SelectMode {
    First,
    All,
}

impl Default for SelectMode {
    fn default() -> Self {
        SelectMode::First
    }
}

#[derive(Debug, Clone)]
pub struct ConfigQuery<'a> {
    pub name: Option<&'a str>,
    pub level: Option<ConfigLevel>,
    pub select: SelectMode,
}

#[derive(Debug, Default, Clone)]
pub struct ConfigQueryBuilder<'a> {
    pub name: Option<&'a str>,
    pub level: Option<ConfigLevel>,
    pub select: SelectMode,
}

impl<'a> ConfigQueryBuilder<'a> {
    pub fn new(name: Option<&'a str>, level: Option<ConfigLevel>, select: SelectMode) -> Self {
        Self { name, level, select }
    }

    /// Adds the given name to the query and returns a new composed query.
    pub fn name(self, name: Option<&'a str>) -> Self {
        Self { name, ..self }
    }
    /// Adds the given level to the query and returns a new composed query.
    pub fn level(self, level: Option<ConfigLevel>) -> Self {
        Self { level, ..self }
    }
    /// Adds the given select mode to the query and returns a new composed query.
    pub fn select(self, select: SelectMode) -> Self {
        Self { select, ..self }
    }

    pub fn build(self) -> ConfigQuery<'a> {
        ConfigQuery { name: self.name, level: self.level, select: self.select }
    }
}

impl<'a> ConfigQuery<'a> {
    fn get_config(&self, context: &EnvironmentContext) -> ConfigResult {
        let config = &context.config;
        let result = match self {
            Self { name: Some(name), level: None, select, .. } => config.get(*name, *select),
            Self { name: Some(name), level: Some(level), .. } => config.get_in_level(*name, *level),
            Self { name: None, level: Some(level), .. } => {
                config.get_level(*level).cloned().map(Value::Object)
            }
            _ => {
                let err_string = format!("Invalid query: {self}");
                log::debug!("{err_string}");
                return Err(ConfigError::InvalidQuery(err_string));
            }
        };
        log::debug!("`{self}` => `{result:?}`");
        Ok(result.into())
    }
    /// Get a value with as little processing as possible
    pub fn get_raw<T>(&self, context: &EnvironmentContext) -> Result<T, ConfigError>
    where
        T: TryConvert + ValueStrategy,
    {
        let ctx = context;
        T::validate_query(self)?;
        let cv = self.get_config(ctx)?;
        T::try_convert(cv)
    }

    /// Get an optional value, ignoring "BadKey" errors, which are only generated when in strict
    /// mode. Used to let callers choose not to report errors due to bad mappings.
    pub fn get_optional<T>(&self, context: &EnvironmentContext) -> Result<T, ConfigError>
    where
        T: TryConvert + ValueStrategy,
    {
        self.get(context).or_else(|e| {
            if matches!(e, ConfigError::BadValue { .. }) {
                T::try_convert(ConfigValue(None))
            } else {
                Err(e)
            }
        })
    }

    /// Get a value with the normal processing of substitution strings
    pub fn get<T>(&self, context: &EnvironmentContext) -> Result<T, ConfigError>
    where
        T: TryConvert + ValueStrategy,
    {
        use crate::mapping::*;

        let ctx = context;
        T::validate_query(self)?;

        // The use of `is_strict()` here is not ideal, because we'd like to have strict-specific
        // library inside the subtool boundary. But when we change to read-only config, this code
        // will all change: we'll build a single ConfigMap before invoking the subtool, rather than
        // doing substitutions and layers at query time.
        if ctx.is_strict() {
            let cv = self.get_config(ctx)?;
            let raw_val = cv.0.clone();
            let had_strict_ignored = std::cell::Cell::new(false);
            let cv = cv.try_recursive_map(&|val| match expand_macros_strict(ctx, val) {
                Ok(v) => Ok(v),
                Err(MappingError::StrictVariableIgnored(_)) => {
                    had_strict_ignored.set(true);
                    Ok(None)
                }
                Err(e) => Err(e.into()),
            })?;
            if had_strict_ignored.get() && cv.0.is_none() {
                return Err(ConfigError::BadValue {
                    value: raw_val.unwrap_or(Value::Null),
                    reason: format!(
                        "The value for {} contains a variable mapping, which is ignored in strict mode",
                        self.name.unwrap_or("<unnamed>"),
                    ),
                });
            }
            let cv = cv.recursive_map(&T::handle_arrays);
            T::try_convert(cv)
        } else {
            let cv = self
                .get_config(ctx)?
                .try_recursive_map(&|val| Ok(expand_macros(ctx, val)?))?
                .recursive_map(&T::handle_arrays);
            T::try_convert(cv)
        }
    }

    /// Get a value with normal processing, but verifying that it's a file that exists.
    pub fn get_file<T>(&self, ctx: &EnvironmentContext) -> Result<T, ConfigError>
    where
        T: TryConvert + ValueStrategy,
    {
        use crate::mapping::*;

        T::validate_query(self)?;
        // See comments re strict checking in get() above
        if ctx.is_strict() {
            let cv = self.get_config(ctx)?;
            let raw_val = cv.0.clone();
            let had_strict_ignored = std::cell::Cell::new(false);
            let cv = cv.try_recursive_map(&|val| match expand_macros_strict(ctx, val) {
                Ok(v) => Ok(v),
                Err(MappingError::StrictVariableIgnored(_)) => {
                    had_strict_ignored.set(true);
                    Ok(None)
                }
                Err(e) => Err(e.into()),
            })?;
            if had_strict_ignored.get() && cv.0.is_none() {
                return Err(ConfigError::BadValue {
                    value: raw_val.unwrap_or(Value::Null),
                    reason: format!(
                        "The value for {} contains a variable mapping, which is ignored in strict mode",
                        self.name.unwrap_or("<unnamed>"),
                    ),
                });
            }
            let cv = cv.recursive_map(&T::handle_arrays).recursive_map(&file_check);
            T::try_convert(cv)
        } else {
            let cv = self
                .get_config(ctx)?
                .try_recursive_map(&|val| Ok(expand_macros(ctx, val)?))?
                .recursive_map(&T::handle_arrays)
                .recursive_map(&file_check);
            T::try_convert(cv)
        }
    }

    pub fn validate_write_query(&self) -> std::result::Result<(&str, ConfigLevel), ConfigError> {
        match self {
            ConfigQuery { name: None, .. } => {
                return Err(ConfigError::ValidationError(super::ValidationError::NameRequired));
            }
            ConfigQuery { level: None, .. } => {
                return Err(ConfigError::ValidationError(super::ValidationError::LevelRequired));
            }
            ConfigQuery { level: Some(level), .. } if level == &ConfigLevel::Default => {
                return Err(ConfigError::ValidationError(
                    super::ValidationError::CannotOverrideDefaults,
                ));
            }
            ConfigQuery { name: Some(key), level: Some(level), .. } => Ok((*key, *level)),
        }
    }
}

impl<'a> std::fmt::Display for ConfigQuery<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self { name, level, select, .. } = self;
        let mut sep = "";
        if let Some(name) = name {
            write!(f, "{sep}key='{name}'")?;
            sep = ", ";
        }
        if let Some(level) = level {
            write!(f, "{sep}level={level:?}")?;
            sep = ", ";
        }
        write!(f, "{sep}select={select:?}")
    }
}

impl<'a> From<&'a str> for ConfigQueryBuilder<'a> {
    fn from(value: &'a str) -> Self {
        let name = Some(value);
        ConfigQueryBuilder { name, ..Default::default() }
    }
}

impl<'a> From<&'a String> for ConfigQueryBuilder<'a> {
    fn from(value: &'a String) -> Self {
        let name = Some(value.as_str());
        ConfigQueryBuilder { name, ..Default::default() }
    }
}

impl<'a> From<ConfigLevel> for ConfigQueryBuilder<'a> {
    fn from(value: ConfigLevel) -> Self {
        let level = Some(value);
        ConfigQueryBuilder { level, ..Default::default() }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::ConfigMap;
    use std::path::PathBuf;

    #[test]
    fn test_strict_query_env_var_failure_message() {
        let mut default_map = ConfigMap::new();
        default_map.insert(
            "test".to_string(),
            serde_json::json!({
                "disallowed_env": "$FOO_ENV/path",
                "disallowed_home": "$HOME/path",
                "allowed_build": "$BUILD_DIR/output",
            }),
        );
        let mut ctx = EnvironmentContext::default();
        ctx.kind = crate::environment::EnvironmentKind::StrictContext;
        ctx.config = crate::storage::Config::new(None, None, None, ConfigMap::new(), default_map);

        // 1. Disallowed env var in strict mode produces a descriptive BadValue error
        let q_env = ConfigQueryBuilder::from("test.disallowed_env").build();
        let res: Result<String, ConfigError> = q_env.get(&ctx);
        match res {
            Err(ConfigError::BadValue { value, reason }) => {
                assert_eq!(value, serde_json::json!("$FOO_ENV/path"));
                assert_eq!(
                    reason,
                    "The value for test.disallowed_env contains a variable mapping, which is ignored in strict mode"
                );
            }
            other => panic!("expected ConfigError::BadValue, got {:?}", other),
        }

        // 2. Disallowed $HOME in strict mode produces the same descriptive BadValue error
        let q_home = ConfigQueryBuilder::from("test.disallowed_home").build();
        let res_home: Result<String, ConfigError> = q_home.get(&ctx);
        match res_home {
            Err(ConfigError::BadValue { value, reason }) => {
                assert_eq!(value, serde_json::json!("$HOME/path"));
                assert_eq!(
                    reason,
                    "The value for test.disallowed_home contains a variable mapping, which is ignored in strict mode"
                );
            }
            other => panic!("expected ConfigError::BadValue, got {:?}", other),
        }

        // 3. Allowed variable $BUILD_DIR expands properly in strict mode (None here since no build dir set)
        let q_build = ConfigQueryBuilder::from("test.allowed_build").build();
        let res_build: Result<Option<String>, ConfigError> = q_build.get(&ctx);
        // build dir is None in this strict context, so $BUILD_DIR/output returns None
        assert_eq!(res_build.unwrap(), None);

        // 4. get_file with disallowed env var in strict mode
        let res_file: Result<PathBuf, ConfigError> = q_env.get_file(&ctx);
        match res_file {
            Err(ConfigError::BadValue { value, reason }) => {
                assert_eq!(value, serde_json::json!("$FOO_ENV/path"));
                assert_eq!(
                    reason,
                    "The value for test.disallowed_env contains a variable mapping, which is ignored in strict mode"
                );
            }
            other => panic!("expected ConfigError::BadValue, got {:?}", other),
        }
    }

    #[test]
    fn test_strict_query_fallback_array() {
        let mut default_map = ConfigMap::new();
        default_map.insert(
            "ssh".to_string(),
            serde_json::json!({
                "controlmaster": {
                    "dir": ["$XDG_RUNTIME_DIR/ffx", "/tmp/ffx-ssh"]
                },
                "all_disallowed": ["$XDG_RUNTIME_DIR/ffx", "$HOME/.ssh/id"],
            }),
        );
        let mut ctx = EnvironmentContext::default();
        ctx.kind = crate::environment::EnvironmentKind::StrictContext;
        ctx.config = crate::storage::Config::new(None, None, None, ConfigMap::new(), default_map);

        // 1. Fallback array with a disallowed element drops the disallowed element and succeeds with the literal fallback
        let q_ctrl = ConfigQueryBuilder::from("ssh.controlmaster.dir").build();
        let res_ctrl: Result<String, ConfigError> = q_ctrl.get(&ctx);
        assert_eq!(res_ctrl.unwrap(), "/tmp/ffx-ssh".to_string());

        // 2. Fallback array where all elements are disallowed in strict mode fails with BadValue
        let q_all = ConfigQueryBuilder::from("ssh.all_disallowed").build();
        let res_all: Result<String, ConfigError> = q_all.get(&ctx);
        match res_all {
            Err(ConfigError::BadValue { value, reason }) => {
                assert_eq!(value, serde_json::json!(["$XDG_RUNTIME_DIR/ffx", "$HOME/.ssh/id"]));
                assert_eq!(
                    reason,
                    "The value for ssh.all_disallowed contains a variable mapping, which is ignored in strict mode"
                );
            }
            other => panic!("expected ConfigError::BadValue, got {:?}", other),
        }
    }
}
