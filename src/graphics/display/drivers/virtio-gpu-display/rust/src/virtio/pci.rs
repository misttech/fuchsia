// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implements the virtio PCI transport.
//!
//! virtio14 4.1 "Virtio Over PCI Bus"

pub mod bar_map;
pub mod capabilities;
pub mod capability_type;
pub mod common_configuration;
pub mod device;
