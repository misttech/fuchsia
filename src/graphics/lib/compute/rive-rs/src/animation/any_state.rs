// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::animation::LayerState;
use crate::core::{Core, ObjectRef, OnAdded};

#[derive(Debug, Default)]
pub struct AnyState {
    layer_state: LayerState,
}

impl Core for AnyState {
    parent_types![(layer_state, LayerState)];

    properties!(layer_state);
}

impl OnAdded for ObjectRef<'_, AnyState> {
    on_added!(LayerState);
}
