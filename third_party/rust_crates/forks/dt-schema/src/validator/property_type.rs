// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use serde::Serialize;

use super::error::ValidatorError;
use std::str::FromStr;

#[derive(Debug, PartialEq, Eq, Hash, Copy, Clone, PartialOrd, Ord)]
/// See types.yaml in the dt-schema repo.
pub enum PropertyType {
    Flag,
    NonUniqueStringArray,
    StringArray,
    String,

    Uint8Item,
    Uint8Matrix,
    Uint8Array,
    Uint8,
    Int8Item,
    Int8Matrix,
    Int8Array,
    Int8,

    Uint16Item,
    Uint16Matrix,
    Uint16Array,
    Uint16,
    Int16Item,
    Int16Matrix,
    Int16Array,
    Int16,

    Cell,
    Uint32Matrix,
    Uint32Array,
    Uint32,
    Int32Item,
    Int32Matrix,
    Int32Array,
    Int32,

    Uint64Item,
    Uint64Matrix,
    Uint64Array,
    Uint64,
    Int64Item,
    Int64Matrix,
    Int64Array,
    Int64,

    Phandle,
    PhandleArray,

    Node,
}

impl PropertyType {
    pub fn is_matrix(&self) -> bool {
        matches!(
            self,
            PropertyType::PhandleArray
                | PropertyType::Int8Matrix
                | PropertyType::Int16Matrix
                | PropertyType::Int32Matrix
                | PropertyType::Int64Matrix
                | PropertyType::Uint8Matrix
                | PropertyType::Uint16Matrix
                | PropertyType::Uint32Matrix
                | PropertyType::Uint64Matrix
        )
    }

    pub fn is_looser(&self, other: &PropertyType) -> bool {
        match self {
            PropertyType::Int8 => {
                other == &PropertyType::Int8
                    || other == &PropertyType::Int8Item
                    || other == &PropertyType::Int8Array
                    || other == &PropertyType::Int8Matrix
            }

            PropertyType::Int16 => {
                other == &PropertyType::Int16
                    || other == &PropertyType::Int16Item
                    || other == &PropertyType::Int16Array
                    || other == &PropertyType::Int16Matrix
            }
            PropertyType::Int32 => {
                other == &PropertyType::Int32
                    || other == &PropertyType::Int32Item
                    || other == &PropertyType::Int32Array
                    || other == &PropertyType::Int32Matrix
            }
            PropertyType::Int64 => {
                other == &PropertyType::Int64
                    || other == &PropertyType::Int64Item
                    || other == &PropertyType::Int64Array
                    || other == &PropertyType::Int64Matrix
            }
            PropertyType::Uint8 => {
                other == &PropertyType::Uint8
                    || other == &PropertyType::Uint8Item
                    || other == &PropertyType::Uint8Array
                    || other == &PropertyType::Uint8Matrix
            }

            PropertyType::Uint16 => {
                other == &PropertyType::Uint16
                    || other == &PropertyType::Uint16Item
                    || other == &PropertyType::Uint16Array
                    || other == &PropertyType::Uint16Matrix
            }
            PropertyType::Uint32 => {
                other == &PropertyType::Uint32
                    || other == &PropertyType::Cell
                    || other == &PropertyType::Uint32Array
                    || other == &PropertyType::Uint32Matrix
            }
            PropertyType::Uint64 => {
                other == &PropertyType::Uint64
                    || other == &PropertyType::Uint64Item
                    || other == &PropertyType::Uint64Array
                    || other == &PropertyType::Uint64Matrix
            }
            PropertyType::Phandle => {
                other == &PropertyType::Phandle || other == &PropertyType::PhandleArray
            }
            _ => false,
        }
    }

    pub fn max_size(&self) -> Option<usize> {
        match self {
            PropertyType::Uint8 | PropertyType::Int8 => Some(std::mem::size_of::<u8>()),
            PropertyType::Uint16 | PropertyType::Int16 => Some(std::mem::size_of::<u16>()),
            PropertyType::Uint32 | PropertyType::Int32 => Some(std::mem::size_of::<u32>()),
            PropertyType::Uint64 | PropertyType::Int64 => Some(std::mem::size_of::<u64>()),
            PropertyType::Flag => Some(0),
            _ => None,
        }
    }

    pub fn bytes_per_element(&self) -> Option<usize> {
        match self {
            PropertyType::Uint8
            | PropertyType::Uint8Array
            | PropertyType::Uint8Matrix
            | PropertyType::Int8
            | PropertyType::Int8Array
            | PropertyType::Int8Matrix => Some(std::mem::size_of::<u8>()),
            PropertyType::Uint16
            | PropertyType::Uint16Array
            | PropertyType::Uint16Matrix
            | PropertyType::Int16
            | PropertyType::Int16Array
            | PropertyType::Int16Matrix => Some(std::mem::size_of::<u16>()),
            PropertyType::Uint32
            | PropertyType::Uint32Array
            | PropertyType::Uint32Matrix
            | PropertyType::PhandleArray
            | PropertyType::Phandle
            | PropertyType::Int32
            | PropertyType::Int32Array
            | PropertyType::Int32Matrix => Some(std::mem::size_of::<u32>()),
            PropertyType::Uint64
            | PropertyType::Uint64Array
            | PropertyType::Uint64Matrix
            | PropertyType::Int64
            | PropertyType::Int64Array
            | PropertyType::Int64Matrix => Some(std::mem::size_of::<u64>()),
            _ => None,
        }
    }

    pub fn is_string(&self) -> bool {
        matches!(
            self,
            PropertyType::NonUniqueStringArray | PropertyType::String | PropertyType::StringArray
        )
    }
}

impl FromStr for PropertyType {
    type Err = ValidatorError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "flag" => PropertyType::Flag,
            "non-unique-string-array" => PropertyType::NonUniqueStringArray,
            "string-array" => PropertyType::StringArray,
            "string" => PropertyType::String,

            "uint8-item" => PropertyType::Uint8Item,
            "uint8-matrix" => PropertyType::Uint8Matrix,
            "uint8-array" => PropertyType::Uint8Array,
            "uint8" => PropertyType::Uint8,
            "int8-item" => PropertyType::Int8Item,
            "int8-matrix" => PropertyType::Int8Matrix,
            "int8-array" => PropertyType::Int8Array,
            "int8" => PropertyType::Int8,

            "uint16-item" => PropertyType::Uint16Item,
            "uint16-matrix" => PropertyType::Uint16Matrix,
            "uint16-array" => PropertyType::Uint16Array,
            "uint16" => PropertyType::Uint16,
            "int16-item" => PropertyType::Int16Item,
            "int16-matrix" => PropertyType::Int16Matrix,
            "int16-array" => PropertyType::Int16Array,
            "int16" => PropertyType::Int16,

            "cell" => PropertyType::Cell,
            "uint32-matrix" => PropertyType::Uint32Matrix,
            "uint32-array" => PropertyType::Uint32Array,
            "uint32" => PropertyType::Uint32,
            "int32-item" => PropertyType::Int32Item,
            "int32-matrix" => PropertyType::Int32Matrix,
            "int32-array" => PropertyType::Int32Array,
            "int32" => PropertyType::Int32,

            "uint64-item" => PropertyType::Uint64Item,
            "uint64-matrix" => PropertyType::Uint64Matrix,
            "uint64-array" => PropertyType::Uint64Array,
            "uint64" => PropertyType::Uint64,
            "int64-item" => PropertyType::Int64Item,
            "int64-matrix" => PropertyType::Int64Matrix,
            "int64-array" => PropertyType::Int64Array,
            "int64" => PropertyType::Int64,

            "phandle" => PropertyType::Phandle,
            "phandle-array" => PropertyType::PhandleArray,

            "node" => PropertyType::Node,
            _ => return Err(ValidatorError::UnknownPropType(s.to_owned())),
        })
    }
}

impl ToString for PropertyType {
    fn to_string(&self) -> String {
        match self {
            PropertyType::Flag => "flag",
            PropertyType::NonUniqueStringArray => "non-unique-string-array",
            PropertyType::StringArray => "string-array",
            PropertyType::String => "string",

            PropertyType::Uint8Item => "uint8-item",
            PropertyType::Uint8Matrix => "uint8-matrix",
            PropertyType::Uint8Array => "uint8-array",
            PropertyType::Uint8 => "uint8",
            PropertyType::Int8Item => "int8-item",
            PropertyType::Int8Matrix => "int8-matrix",
            PropertyType::Int8Array => "int8-array",
            PropertyType::Int8 => "int8",

            PropertyType::Uint16Item => "uint16-item",
            PropertyType::Uint16Matrix => "uint16-matrix",
            PropertyType::Uint16Array => "uint16-array",
            PropertyType::Uint16 => "uint16",
            PropertyType::Int16Item => "int16-item",
            PropertyType::Int16Matrix => "int16-matrix",
            PropertyType::Int16Array => "int16-array",
            PropertyType::Int16 => "int16",

            PropertyType::Cell => "cell",
            PropertyType::Uint32Matrix => "uint32-matrix",
            PropertyType::Uint32Array => "uint32-array",
            PropertyType::Uint32 => "uint32",
            PropertyType::Int32Item => "int32-item",
            PropertyType::Int32Matrix => "int32-matrix",
            PropertyType::Int32Array => "int32-array",
            PropertyType::Int32 => "int32",

            PropertyType::Uint64Item => "uint64-item",
            PropertyType::Uint64Matrix => "uint64-matrix",
            PropertyType::Uint64Array => "uint64-array",
            PropertyType::Uint64 => "uint64",
            PropertyType::Int64Item => "int64-item",
            PropertyType::Int64Matrix => "int64-matrix",
            PropertyType::Int64Array => "int64-array",
            PropertyType::Int64 => "int64",

            PropertyType::Phandle => "phandle",
            PropertyType::PhandleArray => "phandle-array",

            PropertyType::Node => "node",
        }
        .to_owned()
    }
}

impl Serialize for PropertyType {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}
