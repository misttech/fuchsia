// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod memory_mapped_arc_vmo;
pub use memory_mapped_arc_vmo::MemoryMappedArcVmo;
pub type ReadOnlyMemoryMappedArcVmo<M = ()> = MemoryMappedArcVmo<false, M>;
pub type ReadWriteMemoryMappedArcVmo<M = ()> = MemoryMappedArcVmo<true, M>;

mod memory_mapped_vmo;
pub use memory_mapped_vmo::MemoryMappedVmo;
pub type ReadOnlyMemoryMappedVmo = MemoryMappedVmo<false>;
pub type ReadWriteMemoryMappedVmo = MemoryMappedVmo<true>;

#[derive(Debug)]
pub enum Error {
    InvalidInput,
}
