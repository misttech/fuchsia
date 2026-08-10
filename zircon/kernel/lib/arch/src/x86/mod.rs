// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod cpuid;
mod extension;
mod feature;

pub use extension::*;
pub use feature::*;

/// Enumeration of vendors.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Vendor {
    Intel,
    Amd,
    Unknown,
}
