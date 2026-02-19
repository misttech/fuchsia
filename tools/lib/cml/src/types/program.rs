// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::types::common::*;
use crate::{ContextSpanned, Error, Origin, byte_index_to_location};
pub use cm_types::{
    Availability, BorrowedName, BoundedName, DeliveryType, DependencyType, HandleType, Name,
    OnTerminate, ParseError, Path, RelativePath, StartupMode, StorageId, Url,
};
use json_spanned_value::Spanned;
use serde::{Deserialize, Serialize, de};
use serde_json::Value;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use indexmap::IndexMap;

#[derive(Debug, PartialEq, Default, Serialize)]
pub struct Program {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner: Option<Name>,
    #[serde(flatten)]
    pub info: IndexMap<String, Value>,
}

impl<'de> de::Deserialize<'de> for Program {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        struct Visitor;

        const EXPECTED_PROGRAM: &'static str =
            "a JSON object that includes a `runner` string property";
        const EXPECTED_RUNNER: &'static str = "a non-empty `runner` string property no more than 255 characters in length \
            that consists of [A-Za-z0-9_.-] and starts with [A-Za-z0-9_]";

        impl<'de> de::Visitor<'de> for Visitor {
            type Value = Program;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(EXPECTED_PROGRAM)
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: de::MapAccess<'de>,
            {
                let mut info = IndexMap::new();
                let mut runner = None;
                while let Some(e) = map.next_entry::<String, Value>()? {
                    let (k, v) = e;
                    if &k == "runner" {
                        if let Value::String(s) = v {
                            runner = Some(s);
                        } else {
                            return Err(de::Error::invalid_value(
                                de::Unexpected::Map,
                                &EXPECTED_RUNNER,
                            ));
                        }
                    } else {
                        info.insert(k, v);
                    }
                }
                let runner = runner
                    .map(|r| {
                        Name::new(r.clone()).map_err(|e| match e {
                            ParseError::InvalidValue => de::Error::invalid_value(
                                serde::de::Unexpected::Str(&r),
                                &EXPECTED_RUNNER,
                            ),
                            ParseError::TooLong | ParseError::Empty => {
                                de::Error::invalid_length(r.len(), &EXPECTED_RUNNER)
                            }
                            _ => {
                                panic!("unexpected parse error: {:?}", e);
                            }
                        })
                    })
                    .transpose()?;
                Ok(Program { runner, info })
            }
        }

        deserializer.deserialize_map(Visitor)
    }
}

#[derive(Debug, Deserialize, PartialEq)]
pub struct ParsedProgram {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner: Option<Spanned<String>>,
    #[serde(flatten)]
    pub info: IndexMap<String, serde_json::Value>,
}

impl Hydrate for ParsedProgram {
    type Output = ContextProgram;

    fn hydrate(self, file: &Arc<PathBuf>, buffer: &String) -> Result<Self::Output, Error> {
        let runner = self.runner.map(|spanned_string| {

            let span = spanned_string.span();
            let raw_name = spanned_string.into_inner();
            let location = byte_index_to_location(buffer, span.0);
            let validated_name = Name::new(raw_name.clone()).map_err(|e| {
                    let msg = match e {
                    ParseError::InvalidValue => format!(
                        "Runner name '{}' contains invalid characters. Expected [A-Za-z0-9_.-] starting with [A-Za-z0-9_].",
                        raw_name
                    ),
                    ParseError::TooLong | ParseError::Empty => {
                        format!("Runner name must be between 1 and 255 characters long.")
                    }
                    _ => {
                        panic!("unexpected parse error: {:?}", e);
                    }
                };

                Error::merge(msg, Some(Origin { file: file.clone(), location: location.clone() }))
            })?;
            Ok::<ContextSpanned<BoundedName<255>>, Error>(ContextSpanned {
                value: validated_name,
                origin: Origin { file: file.clone(), location },
            })
        }).transpose()?;

        Ok(ContextProgram { runner, info: self.info })
    }
}

#[derive(Debug, PartialEq, Serialize, Default, Clone)]
pub struct ContextProgram {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner: Option<ContextSpanned<Name>>,
    #[serde(flatten)]
    pub info: IndexMap<String, serde_json::Value>,
}
