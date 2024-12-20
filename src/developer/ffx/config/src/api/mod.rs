// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use serde_json::Value;
use thiserror::Error;

pub mod query;
pub mod value;

pub type ConfigResult = Result<ConfigValue>;
pub use query::ConfigQuery;
pub use value::ConfigValue;

#[derive(Debug, Error)]
#[error("Configuration error")]
pub enum ConfigError {
    #[error("{}", .0)]
    Error(#[from] anyhow::Error),
    #[error("Config key not found")]
    KeyNotFound,
    #[error("Can't remove empty key")]
    EmptyKey,
}

impl ConfigError {
    pub fn new(e: anyhow::Error) -> Self {
        Self::Error(e)
    }
}

pub(crate) fn validate_type<T>(value: Value) -> Option<Value>
where
    T: TryFrom<ConfigValue>,
    <T as std::convert::TryFrom<ConfigValue>>::Error: std::convert::From<ConfigError>,
{
    let result: std::result::Result<T, T::Error> = ConfigValue(Some(value.clone())).try_into();
    match result {
        Ok(_) => Some(value),
        Err(_) => None,
    }
}

impl From<ConfigError> for std::convert::Infallible {
    fn from(value: ConfigError) -> Self {
        panic!("cannot convert value into `Infallible`: {value:#}")
    }
}
