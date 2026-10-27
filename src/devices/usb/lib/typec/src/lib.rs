// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Hardware-agnostic USB Type-C Port Manager (TCPM) CC register decoding.

mod cc;

pub use cc::{
    Accessory, COMMAND_LOOK4CONNECTION, CcPinState, CcStatus, CcTermination, PowerRole,
    RoleControl, RpValue,
};

#[cfg(test)]
mod tests;
