// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_io as fio;

pub use cm_types::{
    Availability, BorrowedName, BoundedName, DeliveryType, DependencyType, HandleType, Name,
    OnTerminate, ParseError, Path, RelativePath, StartupMode, StorageId, Url,
};
use cml_macro::CheckedVec;
use serde::{Deserialize, Serialize};

use std::fmt;

/// A right or bundle of rights to apply to a directory.
#[derive(Deserialize, Clone, Debug, Eq, PartialEq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Right {
    // Individual
    Connect,
    Enumerate,
    Execute,
    GetAttributes,
    ModifyDirectory,
    ReadBytes,
    Traverse,
    UpdateAttributes,
    WriteBytes,

    // Aliass
    #[serde(rename = "r*")]
    ReadAlias,
    #[serde(rename = "w*")]
    WriteAlias,
    #[serde(rename = "x*")]
    ExecuteAlias,
    #[serde(rename = "rw*")]
    ReadWriteAlias,
    #[serde(rename = "rx*")]
    ReadExecuteAlias,
}

impl fmt::Display for Right {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Connect => "connect",
            Self::Enumerate => "enumerate",
            Self::Execute => "execute",
            Self::GetAttributes => "get_attributes",
            Self::ModifyDirectory => "modify_directory",
            Self::ReadBytes => "read_bytes",
            Self::Traverse => "traverse",
            Self::UpdateAttributes => "update_attributes",
            Self::WriteBytes => "write_bytes",
            Self::ReadAlias => "r*",
            Self::WriteAlias => "w*",
            Self::ExecuteAlias => "x*",
            Self::ReadWriteAlias => "rw*",
            Self::ReadExecuteAlias => "rx*",
        };
        write!(f, "{}", s)
    }
}

impl Right {
    /// Expands this right or bundle or rights into a list of `fio::Operations`.
    pub fn expand(&self) -> Vec<fio::Operations> {
        match self {
            Self::Connect => vec![fio::Operations::CONNECT],
            Self::Enumerate => vec![fio::Operations::ENUMERATE],
            Self::Execute => vec![fio::Operations::EXECUTE],
            Self::GetAttributes => vec![fio::Operations::GET_ATTRIBUTES],
            Self::ModifyDirectory => vec![fio::Operations::MODIFY_DIRECTORY],
            Self::ReadBytes => vec![fio::Operations::READ_BYTES],
            Self::Traverse => vec![fio::Operations::TRAVERSE],
            Self::UpdateAttributes => vec![fio::Operations::UPDATE_ATTRIBUTES],
            Self::WriteBytes => vec![fio::Operations::WRITE_BYTES],
            Self::ReadAlias => vec![
                fio::Operations::CONNECT,
                fio::Operations::ENUMERATE,
                fio::Operations::TRAVERSE,
                fio::Operations::READ_BYTES,
                fio::Operations::GET_ATTRIBUTES,
            ],
            Self::WriteAlias => vec![
                fio::Operations::CONNECT,
                fio::Operations::ENUMERATE,
                fio::Operations::TRAVERSE,
                fio::Operations::WRITE_BYTES,
                fio::Operations::MODIFY_DIRECTORY,
                fio::Operations::UPDATE_ATTRIBUTES,
            ],
            Self::ExecuteAlias => vec![
                fio::Operations::CONNECT,
                fio::Operations::ENUMERATE,
                fio::Operations::TRAVERSE,
                fio::Operations::EXECUTE,
            ],
            Self::ReadWriteAlias => vec![
                fio::Operations::CONNECT,
                fio::Operations::ENUMERATE,
                fio::Operations::TRAVERSE,
                fio::Operations::READ_BYTES,
                fio::Operations::WRITE_BYTES,
                fio::Operations::MODIFY_DIRECTORY,
                fio::Operations::GET_ATTRIBUTES,
                fio::Operations::UPDATE_ATTRIBUTES,
            ],
            Self::ReadExecuteAlias => vec![
                fio::Operations::CONNECT,
                fio::Operations::ENUMERATE,
                fio::Operations::TRAVERSE,
                fio::Operations::READ_BYTES,
                fio::Operations::GET_ATTRIBUTES,
                fio::Operations::EXECUTE,
            ],
        }
    }
}

/// A list of rights.
#[derive(CheckedVec, Debug, PartialEq, Clone)]
#[checked_vec(
    expected = "a nonempty array of rights, with unique elements",
    min_length = 1,
    unique_items = true
)]
pub struct Rights(pub Vec<Right>);

pub trait RightsClause {
    fn rights(&self) -> Option<&Rights>;
}
