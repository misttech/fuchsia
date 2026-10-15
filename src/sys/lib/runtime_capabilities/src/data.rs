// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{CapabilityBound, RemoteError};
use fidl_fuchsia_component_sandbox as fsandbox;
use flyweights::{FlyByteStr, FlyStr};
use std::fmt::Debug;
use std::sync::Arc;

/// A capability that holds immutable data.
///
/// `Bytes` and `String` use pointer-sized flyweight types (`8 B` on 64-bit, with inline SSO for
/// <= 7 bytes) rather than 16-byte fat pointers (`Arc<[u8]>` / `Arc<str>`) so that `Data` and
/// `Capability` stay 16 bytes while `Int64` and `Uint64` remain inline without heap allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Data {
    Bytes(FlyByteStr),
    String(FlyStr),
    Int64(i64),
    Uint64(u64),
}

impl CapabilityBound for Data {
    fn debug_typename() -> &'static str {
        "Data"
    }
}

impl TryFrom<fsandbox::Data> for Data {
    type Error = RemoteError;

    fn try_from(data_capability: fsandbox::Data) -> Result<Self, Self::Error> {
        match data_capability {
            fsandbox::Data::Bytes(bytes) => Ok(Self::Bytes(bytes.into())),
            fsandbox::Data::String(string) => Ok(Self::String(string.into())),
            fsandbox::Data::Int64(num) => Ok(Self::Int64(num)),
            fsandbox::Data::Uint64(num) => Ok(Self::Uint64(num)),
            fsandbox::DataUnknown!() => Err(RemoteError::UnknownVariant),
        }
    }
}

impl From<Data> for fsandbox::Data {
    fn from(data: Data) -> Self {
        match data {
            Data::Bytes(bytes) => fsandbox::Data::Bytes(bytes.into()),
            Data::String(string) => fsandbox::Data::String(string.into()),
            Data::Int64(num) => fsandbox::Data::Int64(num),
            Data::Uint64(num) => fsandbox::Data::Uint64(num),
        }
    }
}

impl From<Data> for fsandbox::Capability {
    fn from(data: Data) -> Self {
        Self::Data(data.into())
    }
}

impl From<Arc<Data>> for crate::Capability {
    fn from(data: Arc<Data>) -> Self {
        crate::Capability::Data((*data).clone())
    }
}

impl TryFrom<crate::Capability> for Arc<Data> {
    type Error = <Data as TryFrom<crate::Capability>>::Error;

    fn try_from(capability: crate::Capability) -> Result<Self, Self::Error> {
        let data: Data = capability.try_into()?;
        Ok(Arc::new(data))
    }
}
