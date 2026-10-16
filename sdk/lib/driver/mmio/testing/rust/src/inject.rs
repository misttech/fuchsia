// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! MMIO operation injection support for intercepting MMIO access.
//!
//! This module provides a framework for injecting side-effects on VMO access.
//!
//! # Usage
//!
//! [`VmoMemory`] is the main entry type for injection in regular usages of the
//! [`mmio`] crate. It replaces [`mmio::vmo::VmoMemory`] as the main implementer
//! of [`UnsafeMmio`] backing the operations for an [`MmioRegion`].
//!
//! See the documentation of [`VmoMemory`] for more details.
//!
//! ## Conditional Compilation
//!
//! For conditional compilation, the pattern to use is:
//!
//! ```
//! #[cfg(test)]
//! mod memory {
//!     // Wrapper is a type implementing `VmoMemoryWrapper`.
//!     pub type VmoMemory = fake_mmio::inject::VmoMemory<Wrapper>;
//! }
//!
//! #[cfg(not(test))]
//! mod memory {
//!     pub type VmoMemory = mmio::vmo::VmoMemory;
//! }
//! ```
//!
//! ## Type parameters
//!
//! [`VmoMemory`] implements [`VmoMapper`] like [`mmio::vmo::VmoMemory`] so it
//! can also be used in type parameters instead.
//!
//! ```
//! pub fn create_driver<M: mmio::vmo::VmoMapper>() -> MyDriver<M> { ... };
//!
//! #[cfg(test)]
//! fn test_fn() {
//!     let driver = create_driver::<fake_mmio::injec::VmoMemory<Wrapper>>();
//! }
//!
//! fn load_driver() {
//!     let driver = create_driver::<mmio::vmo::VmoMemory>();
//! }
//! ```
//!
//!

use std::marker::PhantomData;
use std::ops::Range;

use mmio::MmioExt;
use mmio::region::{MmioRegion, UnsafeMmio};
use mmio::vmo::VmoMapper;

use crate::atomic::{AtomicMmioPtr, AtomicOperand};
use crate::cached_vmo::CachedVmoMemory;
use crate::operand::MmioOperand;

mod after_store;
mod dispatch;
mod offset;
mod passthrough;
mod range_override;
mod registry;
mod w1c;

pub use after_store::AfterStore;
pub use dispatch::{
    DispatchBuilder, DispatchHandler, DispatchNode, DispatchNodeHandler, OpDispatcher,
    RegisterRange,
};
pub use offset::Offset;
pub use passthrough::Passthrough;
pub use range_override::{MaybeVmoMemoryHandler, RangeOverride};
pub use registry::{BaseRegistry, Registry, RegistryHandler, ScopedRegistry, StrictRegistry};
pub use w1c::WriteOneToClear;

#[cfg(test)]
use range_override::MockMaybeVmoMemoryHandler;

/// A factory trait for constructing a handler associated with a given VMO.
pub trait VmoMemoryWrapper: VmoMemoryHandler {
    /// Creates a new handler instance for `vmo`.
    fn new(vmo: &zx::Vmo) -> Self;
}

/// Handles MMIO read and write operations on a VMO.
///
/// Implementors define custom behavior when MMIO loads and stores of various
/// sizes occur.
///
/// The [`crate::inject`] module contain a number of implementations of
/// `VmoMemoryHandler` that can be used to alter the behavior of loads and
/// stores for rich MMIO fakes.
pub trait VmoMemoryHandler {
    /// Handles an 8-bit MMIO load operation.
    fn load8(&self, op: VmoOpHelper<'_, u8>) -> u8;

    /// Handles a 16-bit MMIO load operation.
    fn load16(&self, op: VmoOpHelper<'_, u16>) -> u16;

    /// Handles a 32-bit MMIO load operation.
    fn load32(&self, op: VmoOpHelper<'_, u32>) -> u32;

    /// Handles a 64-bit MMIO load operation.
    fn load64(&self, op: VmoOpHelper<'_, u64>) -> u64;

    /// Handles an 8-bit MMIO store operation.
    fn store8(&self, op: VmoOpHelper<'_, u8>, value: u8);

    /// Handles a 16-bit MMIO store operation.
    fn store16(&self, op: VmoOpHelper<'_, u16>, value: u16);

    /// Handles a 32-bit MMIO store operation.
    fn store32(&self, op: VmoOpHelper<'_, u32>, value: u32);

    /// Handles a 64-bit MMIO store operation.
    fn store64(&self, op: VmoOpHelper<'_, u64>, value: u64);
}

/// A generic handler for MMIO operations parameterized by operand type.
///
/// Implementing this trait automatically provides an implementation of
/// [`VmoMemoryHandler`].
pub trait GenericVmoMemoryHandler {
    /// Handles an MMIO load operation for type `T`.
    fn load<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>) -> T;

    /// Handles an MMIO store operation for type `T`.
    fn store<T: MmioOperand>(&self, op: VmoOpHelper<'_, T>, value: T);
}

impl<O> VmoMemoryHandler for O
where
    O: GenericVmoMemoryHandler,
{
    fn load8(&self, op: VmoOpHelper<'_, u8>) -> u8 {
        self.load(op)
    }

    fn load16(&self, op: VmoOpHelper<'_, u16>) -> u16 {
        self.load(op)
    }

    fn load32(&self, op: VmoOpHelper<'_, u32>) -> u32 {
        self.load(op)
    }

    fn load64(&self, op: VmoOpHelper<'_, u64>) -> u64 {
        self.load(op)
    }

    fn store8(&self, op: VmoOpHelper<'_, u8>, value: u8) {
        self.store(op, value)
    }

    fn store16(&self, op: VmoOpHelper<'_, u16>, value: u16) {
        self.store(op, value)
    }

    fn store32(&self, op: VmoOpHelper<'_, u32>, value: u32) {
        self.store(op, value)
    }

    fn store64(&self, op: VmoOpHelper<'_, u64>, value: u64) {
        self.store(op, value)
    }
}

/// A wrapper around an [`UnsafeMmio`] underlying storage that permits injecting
/// behavior on reads and writes to the underlying VMO.
///
/// The `H` handler type is an implementation of [`VmoMemoryHandler`], which is
/// tailored specifically for intersecting loads and stores to the underlying
/// VMO.
///
/// Note that [`VmoMemory`] _always_ operates on [`CachedVmoMemory`], since the
/// fakes are running in the same process as the test, and there's no
/// device/peripheral memory in play.
pub struct VmoMemory<H> {
    handler: H,
    vmo: CachedVmoMemory,
}

impl<H: VmoMemoryWrapper> VmoMemory<H> {
    /// Maps a `zx::Vmo` into memory and wraps the resulting `VmoMemory`
    /// inside `VmoMemory`.
    pub fn map(offset: usize, size: usize, vmo: zx::Vmo) -> Result<MmioRegion<Self>, zx::Status> {
        let handler = H::new(&vmo);
        let mmio = CachedVmoMemory::map(offset, size, vmo)?;

        Ok(mmio.map(move |vmo| Self { handler, vmo }))
    }
}

impl<H> VmoMemory<H> {
    /// Creates a new `VmoMemory` with the provided `handler` and backing `vmo`.
    pub fn new(handler: H, vmo: CachedVmoMemory) -> Self {
        Self { handler, vmo }
    }

    fn op_helper<T>(&self, offset: usize) -> VmoOpHelper<'_, T> {
        VmoOpHelper {
            vmo_offset: offset,
            relative_offset: offset,
            inner: &self.vmo,
            _marker: PhantomData,
        }
    }
}

impl<H: VmoMemoryHandler> UnsafeMmio for VmoMemory<H> {
    #[inline]
    fn len(&self) -> usize {
        self.vmo.len()
    }

    #[inline]
    fn align_offset(&self, align: usize) -> usize {
        self.vmo.align_offset(align)
    }

    #[inline]
    unsafe fn load8_unchecked(&self, offset: usize) -> u8 {
        self.handler.load8(self.op_helper(offset))
    }

    #[inline]
    unsafe fn load16_unchecked(&self, offset: usize) -> u16 {
        self.handler.load16(self.op_helper(offset))
    }

    #[inline]
    unsafe fn load32_unchecked(&self, offset: usize) -> u32 {
        self.handler.load32(self.op_helper(offset))
    }

    #[inline]
    unsafe fn load64_unchecked(&self, offset: usize) -> u64 {
        self.handler.load64(self.op_helper(offset))
    }

    #[inline]
    unsafe fn store8_unchecked(&self, offset: usize, v: u8) {
        self.handler.store8(self.op_helper(offset), v)
    }

    #[inline]
    unsafe fn store16_unchecked(&self, offset: usize, v: u16) {
        self.handler.store16(self.op_helper(offset), v)
    }

    #[inline]
    unsafe fn store32_unchecked(&self, offset: usize, v: u32) {
        self.handler.store32(self.op_helper(offset), v)
    }

    #[inline]
    unsafe fn store64_unchecked(&self, offset: usize, v: u64) {
        self.handler.store64(self.op_helper(offset), v)
    }

    #[inline]
    fn write_barrier(&self) {
        self.vmo.write_barrier();
    }

    #[inline]
    fn read_barrier(&self) {
        self.vmo.read_barrier();
    }
}

impl<H> AtomicMmioPtr for VmoMemory<H> {
    fn ptr<T: AtomicOperand>(&self, offset: usize) -> std::ptr::NonNull<T> {
        self.vmo.ptr(offset)
    }
}

impl<H: VmoMemoryWrapper> VmoMapper for VmoMemory<H> {
    fn map(offset: usize, size: usize, vmo: zx::Vmo) -> Result<MmioRegion<Self>, zx::Status> {
        Self::map(offset, size, vmo)
    }
}

/// Helper providing contextual information and operations for an MMIO access.
pub struct VmoOpHelper<'a, T> {
    vmo_offset: usize,
    relative_offset: usize,
    inner: &'a CachedVmoMemory,
    _marker: PhantomData<T>,
}

impl<'a, T> VmoOpHelper<'a, T> {
    /// Returns the _relative offset_ for this vmo operation.
    ///
    /// NOTE: A `VmoOpHelper` can be decorated by shifting an internally-kept
    /// relative offset to help build fakes with relative register bank
    /// addresses. See [`VmoOpHelper::apply_offset`].
    pub fn offset(&self) -> usize {
        self.relative_offset
    }

    /// Returns the _relative memory range_ for this vmo operation.
    ///
    /// See [`VmoOpHelper::offset`].
    pub fn range(&self) -> Range<usize> {
        self.relative_offset..(self.relative_offset + std::mem::size_of::<T>())
    }

    /// Returns the offset of this operation relative to the start of the
    /// wrapped VMO.
    ///
    /// Use [`VmoOpHelper::vmo_offset`] when using [`VmoOpHelper::unsafe_mmio`].
    pub fn vmo_offset(&self) -> usize {
        self.vmo_offset
    }

    /// Returns the memory for this vmo operation relative to the start of the
    /// wrapped VMO.
    ///
    /// See [`VmoOpHelper::vmo_offset`].
    pub fn vmo_range(&self) -> Range<usize> {
        self.vmo_offset..(self.vmo_offset + std::mem::size_of::<T>())
    }

    /// Provides access to the underlying unsafe mmio implementation.
    pub fn unsafe_mmio(&self) -> &'a impl UnsafeMmio {
        self.inner
    }

    /// Applies `offset` to this `VmoOpHelper`.
    ///
    /// This function is used so that any further receivers of this
    /// `VmoOpHelper` can work in an offset address space. For example, when
    /// register banks are defined with offsets or when reusing register
    /// definitions post an [`mmio::MmioSplit`] region.
    ///
    /// # Panics
    ///
    /// Panics if `offset` is larger than the current relative offset.
    pub fn apply_offset(self, offset: usize) -> Self {
        let Some(relative_offset) = self.relative_offset.checked_sub(offset) else {
            panic!("offset {offset} too big for current offset {}", self.relative_offset);
        };
        Self { relative_offset, ..self }
    }

    fn borrow_region(&self) -> MmioRegion<CachedVmoMemory, &'a CachedVmoMemory> {
        // SAFETY: We're piggybacking on the handy implementation of
        // `MmioRegion` here to be able to implement `unsafe` operations in the
        // context of a `VmoOpHelper`.
        //
        // We're guaranteeing this is ok by creation of a `VmoOpHelper`
        // exclusively in the context of `UnsafeMmio` calls.
        unsafe { MmioRegion::new_unchecked(self.inner, self.vmo_range()) }
    }
}

impl<'a, T: MmioOperand> VmoOpHelper<'a, T> {
    /// Performs the load from the underlying VMO memory.
    pub fn load(&self) -> T {
        self.borrow_region().load(0)
    }

    /// Performs the store to the underlying VMO memory.
    pub fn store(&self, value: T) {
        self.borrow_region().store(0, value)
    }
}

/// Provides a mock for `VmoMemoryHandler` via `mockall`.
///
/// NOTE: We can't use `automock` here because `mockall` gets confused with the
/// lifetimes.
#[cfg(test)]
mod mock {
    #![allow(clippy::extra_unused_lifetimes)]

    use super::*;
    mockall::mock! {
        pub VmoMemoryHandler {}

        impl VmoMemoryHandler for VmoMemoryHandler {
            fn load8<'a>(&self, op: VmoOpHelper<'a, u8>) -> u8;
            fn load16<'a>(&self, op: VmoOpHelper<'a, u16>) -> u16;
            fn load32<'a>(&self, op: VmoOpHelper<'a, u32>) -> u32;
            fn load64<'a>(&self, op: VmoOpHelper<'a, u64>) -> u64;
            fn store8<'a>(&self, op: VmoOpHelper<'a, u8>, value: u8);
            fn store16<'a>(&self, op: VmoOpHelper<'a, u16>, value: u16);
            fn store32<'a>(&self, op: VmoOpHelper<'a, u32>, value: u32);
            fn store64<'a>(&self, op: VmoOpHelper<'a, u64>, value: u64);
        }
    }
}
#[cfg(test)]
pub use mock::*;

#[cfg(test)]
mod tests {
    use super::*;
    use mmio::Mmio as _;

    const VMO_SIZE: usize = 4096;

    #[test]
    fn test_load8() {
        let mut mock = MockVmoMemoryHandler::new();
        let expected_val = 0xab_u8;
        let _ = mock
            .expect_load8()
            .once()
            .withf(|op| op.offset() == 0 && op.range() == (0..1))
            .return_const(expected_val);

        let mmio = CachedVmoMemory::new_mapped(VMO_SIZE)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(mock, vmo));

        assert_eq!(mmio.load::<u8>(0), expected_val);
    }

    #[test]
    fn test_load16() {
        let mut mock = MockVmoMemoryHandler::new();
        let expected_val = 0x1234_u16;
        let _ = mock
            .expect_load16()
            .once()
            .withf(|op| op.offset() == 2 && op.range() == (2..4))
            .return_const(expected_val);

        let mmio = CachedVmoMemory::new_mapped(VMO_SIZE)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(mock, vmo));

        assert_eq!(mmio.load::<u16>(2), expected_val);
    }

    #[test]
    fn test_load32() {
        let mut mock = MockVmoMemoryHandler::new();
        let expected_val = 0x5678_9abc_u32;
        let _ = mock
            .expect_load32()
            .once()
            .withf(|op| op.offset() == 4 && op.range() == (4..8))
            .return_const(expected_val);

        let mmio = CachedVmoMemory::new_mapped(VMO_SIZE)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(mock, vmo));

        assert_eq!(mmio.load::<u32>(4), expected_val);
    }

    #[test]
    fn test_load64() {
        let mut mock = MockVmoMemoryHandler::new();
        let expected_val = 0xdef0_1234_5678_9abc_u64;
        let _ = mock
            .expect_load64()
            .once()
            .withf(|op| op.offset() == 8 && op.range() == (8..16))
            .return_const(expected_val);

        let mmio = CachedVmoMemory::new_mapped(VMO_SIZE)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(mock, vmo));

        assert_eq!(mmio.load::<u64>(8), expected_val);
    }

    #[test]
    fn test_store8() {
        let mut mock = MockVmoMemoryHandler::new();
        let val_to_store = 0xab_u8;
        let _ = mock
            .expect_store8()
            .once()
            .withf(move |op, val| op.offset() == 0 && op.range() == (0..1) && *val == val_to_store)
            .return_const(());

        let mut mmio = CachedVmoMemory::new_mapped(VMO_SIZE)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(mock, vmo));

        mmio.store::<u8>(0, val_to_store);
    }

    #[test]
    fn test_store16() {
        let mut mock = MockVmoMemoryHandler::new();
        let val_to_store = 0x1234_u16;
        let _ = mock
            .expect_store16()
            .once()
            .withf(move |op, val| op.offset() == 2 && op.range() == (2..4) && *val == val_to_store)
            .return_const(());

        let mut mmio = CachedVmoMemory::new_mapped(VMO_SIZE)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(mock, vmo));

        mmio.store::<u16>(2, val_to_store);
    }

    #[test]
    fn test_store32() {
        let mut mock = MockVmoMemoryHandler::new();
        let val_to_store = 0x5678_9abc_u32;
        let _ = mock
            .expect_store32()
            .once()
            .withf(move |op, val| op.offset() == 4 && op.range() == (4..8) && *val == val_to_store)
            .return_const(());

        let mut mmio = CachedVmoMemory::new_mapped(VMO_SIZE)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(mock, vmo));

        mmio.store::<u32>(4, val_to_store);
    }

    #[test]
    fn test_store64() {
        let mut mock = MockVmoMemoryHandler::new();
        let val_to_store = 0xdef0_1234_5678_9abc_u64;
        let _ = mock
            .expect_store64()
            .once()
            .withf(move |op, val| op.offset() == 8 && op.range() == (8..16) && *val == val_to_store)
            .return_const(());

        let mut mmio = CachedVmoMemory::new_mapped(VMO_SIZE)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(mock, vmo));

        mmio.store::<u64>(8, val_to_store);
    }

    #[test]
    fn test_vmo_memory_helpers() {
        let mock = MockVmoMemoryHandler::new();
        let mmio = CachedVmoMemory::new_mapped(VMO_SIZE)
            .expect("Failed to map VMO.")
            .map(|vmo| VmoMemory::new(mock, vmo));

        assert_eq!(mmio.len(), VMO_SIZE);
        assert_eq!(mmio.align_offset(8), 0);
        mmio.write_barrier();
        mmio.read_barrier();
    }
}
