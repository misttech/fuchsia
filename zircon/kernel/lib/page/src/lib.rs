// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

// TODO(https://fxbug.dev/534566390): Extend this to support more generic page definitions. For now
// since the C++ side only supports 4k pages we can hard code the 4k constants here.

/// The selected page size.
pub const SIZE: usize = 4096;

/// The shift of the first level of virtual address mask used for page table walking.
pub const SHIFT: usize = 12;

/// Mask that will extract the offset into a page of an address.
pub const MASK: usize = 4095;
