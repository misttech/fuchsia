// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::ConfigValue;
use super::value::TryConvert;
use crate::api::ConfigResult;
use crate::mapping::env_var::env_var_strict;
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
            // If we are going to fail to a reference to an env var, it's important that we
            // know which one. Threading the failure through the ConfigValue apparatus is quite
            // difficult, so for now, let's have an explicit check. Unfortunately, we need to
            // do all the other mappings first, since they _all_ look like env vars ("$BUILD_DIR", etc)
            let cv = self
                .get_config(ctx)?
                .try_recursive_map(&|val| Ok(shared_data(&ctx, val)?))?
                .recursive_map(&|val| build(&ctx, val))
                .recursive_map(&|val| workspace(&ctx, val));
            let cv = if let Some(ref v) = cv.0 {
                // We want recursive mapping here, so that arrays that contain
                // env variables get handled correctly.
                let ev_res = cv.clone().recursive_map(&|val| env_var_strict(val));
                if ev_res.0.is_none() {
                    // Conveniently, this message will make sense for config
                    // mappings that we are ignoring because they are based on
                    // home: $CACHE, etc. Since they all look like environment
                    // variables, they will cause the env_var_strict() check
                    // to fail
                    return Err(ConfigError::BadValue {
                        value: v.clone(),
                        reason: format!(
                            "The value for {} contains a variable mapping, which is ignored in strict mode",
                            self.name.unwrap(),
                        ),
                    });
                }
                ev_res
            } else {
                cv
            };
            // The problem is not with an env variable; keep going
            let cv = cv.recursive_map(&T::handle_arrays);
            T::try_convert(cv)
        } else {
            let cv = self
                .get_config(ctx)?
                .recursive_map(&|val| runtime(&ctx, val))
                .recursive_map(&|val| cache(&ctx, val))
                .recursive_map(&|val| data(&ctx, val))
                .try_recursive_map(&|val| Ok(shared_data(&ctx, val)?))?
                .recursive_map(&|val| config(&ctx, val))
                .recursive_map(&|val| home(&ctx, val))
                .recursive_map(&|val| build(&ctx, val))
                .recursive_map(&|val| workspace(&ctx, val))
                .recursive_map(&|val| env_var(&ctx, val))
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
            let cv = self
                .get_config(ctx)?
                .try_recursive_map(&|val| Ok(shared_data(&ctx, val)?))?
                .recursive_map(&|val| build(&ctx, val))
                .recursive_map(&|val| workspace(&ctx, val));
            let cv = if let Some(ref v) = cv.0 {
                let ev_res = cv.clone().recursive_map(&|val| env_var_strict(val));
                if ev_res.0.is_none() {
                    return Err(ConfigError::BadValue {
                        value: v.clone(),
                        reason: format!(
                            "The value for {} contains a variable mapping, which is ignored in strict mode",
                            self.name.unwrap(),
                        ),
                    });
                }
                ev_res
            } else {
                cv
            };
            // The problem is not with an env variable; keep going
            let cv = cv.recursive_map(&T::handle_arrays).recursive_map(&file_check);
            T::try_convert(cv)
        } else {
            let cv = self
                .get_config(ctx)?
                .recursive_map(&|val| runtime(&ctx, val))
                .recursive_map(&|val| cache(&ctx, val))
                .recursive_map(&|val| data(&ctx, val))
                .try_recursive_map(&|val| Ok(shared_data(&ctx, val)?))?
                .recursive_map(&|val| config(&ctx, val))
                .recursive_map(&|val| home(&ctx, val))
                .recursive_map(&|val| build(&ctx, val))
                .recursive_map(&|val| workspace(&ctx, val))
                .recursive_map(&|val| env_var(&ctx, val))
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
