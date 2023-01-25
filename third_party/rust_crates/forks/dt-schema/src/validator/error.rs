// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use valico::json_schema::SchemaError;

use crate::path::JsonPath;

#[derive(thiserror::Error, Debug)]
pub enum ValidatorError {
    #[error("i/o error")]
    IoError(#[from] std::io::Error),
    #[error("serde yaml error")]
    YamlError(#[from] serde_yaml::Error),
    #[error("serde json error")]
    JsonError(#[from] serde_json::Error),
    #[error("invalid regular expression")]
    RegexError(#[from] fancy_regex::Error),

    #[error("invalid reference: {0}")]
    InvalidReference(String),
    #[error("unknown property type: {0}")]
    UnknownPropType(String),

    #[error("json schema error: {0}")]
    SchemaError(#[from] SchemaError),

    #[error("schema is missing key '{0}' at {1}")]
    ExpectedKey(String, JsonPath),
}

/*
impl<'a> From<jsonschema::error::ValidationError<'a>> for ValidatorError {
    fn from(value: jsonschema::error::ValidationError<'a>) -> Self {
        ValidatorError::SchemaError {
            value: value.instance.into_owned(),
            kind: value.kind,
            instance_path: value.instance_path,
            schema_path: value.schema_path,
        }
    }
}
*/
