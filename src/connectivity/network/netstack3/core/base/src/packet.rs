// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Contexts for packet parsing and serialization in netstack3.

use packet::{DynamicSerializer, NoOpSerializationContext, PartialSerializer, Serializer};

/// The specific serialization context type used within netstack3.
// TODO(https://fxbug.dev/485599557): Replace this alias with the concrete
// implementation.
pub type NetworkSerializationContext = NoOpSerializationContext;

/// The specific packet `Serializer` type used within netstack3.
pub trait NetworkSerializer: Serializer<NetworkSerializationContext> {}
impl<S: Serializer<NetworkSerializationContext>> NetworkSerializer for S {}

/// The specific packet `PartialSerializer` type used within netstack3.
pub trait NetworkPartialSerializer: PartialSerializer<NetworkSerializationContext> {}
impl<S: PartialSerializer<NetworkSerializationContext>> NetworkPartialSerializer for S {}

/// The specific dynamic packet `Serializer` type used within netstack3.
pub trait DynamicNetworkSerializer: DynamicSerializer<NetworkSerializationContext> {}
impl<S: DynamicSerializer<NetworkSerializationContext>> DynamicNetworkSerializer for S {}
