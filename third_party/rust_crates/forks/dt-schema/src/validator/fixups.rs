// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use crate::path::JsonPath;

mod common_properties;
mod empty_items;
mod fixup_201909;
mod int_items_to_matrix;
mod int_minmax;
mod interrupts;
mod items_size;
pub mod property_fixups;
mod reg;
pub mod schema_fixups;
mod single_int;
mod single_string;
mod variable_matrix_fixup;

#[derive(thiserror::Error, Debug)]
pub enum FixupError {
    #[error("JSON error {0} at {1}")]
    JsonError(serde_json::Error, Box<std::backtrace::Backtrace>),

    #[error("Could not parse schema as it was in an unexpected format: {0}. at {1}, value: {2}")]
    UnexpectedSchemaError(String, JsonPath, serde_json::Value),
}

impl From<serde_json::Error> for FixupError {
    fn from(value: serde_json::Error) -> Self {
        FixupError::JsonError(value, Box::new(std::backtrace::Backtrace::force_capture()))
    }
}

pub trait Fixup: Sized {
    /// Make a new instance of this |fixup|. Returns None if the fixup is not applicable to this property.
    fn new(
        propname: &str,
        value: &serde_json::Value,
        path: JsonPath,
    ) -> Result<Option<Self>, FixupError>;

    /// Run the fixup and give the fixed |Value| back.
    fn fixup(self) -> Result<serde_json::Value, FixupError>;
}

/// Helper function that does a fixup (if it is applicable, i.e. T::new() returns Ok(Some(...))), or just returns the given |value|.
/// |path| should not include |propname|.
fn do_fixup<T: Fixup>(
    propname: &str,
    value: serde_json::Value,
    path: &JsonPath,
) -> Result<serde_json::Value, FixupError> {
    let extended = path.extend(propname);
    let result = T::new(propname, &value, extended.clone())?
        .map(|v| v.fixup())
        .unwrap_or(Ok(value.clone()));
    if Some(&value) != result.as_ref().ok() {
        let string = serde_json::to_string(result.as_ref().unwrap_or(&serde_json::json!(null)))
            .unwrap_or_else(|_| "N/A".to_owned());
        if string.len() <= 100 {
            tracing::trace!(
                "applied fixup {} to {}: {}",
                std::any::type_name::<T>(),
                extended,
                string,
            );
        } else {
            tracing::trace!(
                "applied fixup {} to {}: <omitted>",
                std::any::type_name::<T>(),
                extended,
            );
        }
    }
    result
}
