// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Register abstractions for MMIO.
//!
//! This module provides the [`Register`] trait which simplifies interacting with
//! bit-level registers at a fixed offset within an MMIO region.

use crate::{Mmio, MmioExt, MmioOperand};

/// A trait for types representing a register at a fixed offset.
///
/// Types implementing this trait are typically wrappers around fundamental
/// types (u8, u16, u32, u64) that provide bit-level access to fields within
/// the register.
pub trait Register: Sized {
    /// The underlying integer type (e.g., u32) that holds the register bits.
    type Value: MmioOperand;

    /// The byte offset of this register within the MMIO region.
    const OFFSET: usize;

    /// Initializes the register type with a raw value, typically after reading from MMIO.
    fn from_raw(value: Self::Value) -> Self;

    /// Converts the register type back to its raw bits, typically before writing to MMIO.
    fn to_raw(&self) -> Self::Value;
}

/// A trait for registers that can be read.
pub trait ReadableRegister: Register {
    /// Loads the register's value from the MMIO region at its defined `OFFSET`.
    fn read<M: Mmio + ?Sized>(mmio: &M) -> Self {
        Self::from_raw(mmio.load::<Self::Value>(Self::OFFSET))
    }
}

/// A trait for registers that can be written.
pub trait WritableRegister: Register {
    /// Stores the register's current state into the MMIO region at its defined `OFFSET`.
    fn write<M: Mmio + ?Sized>(&self, mmio: &mut M) {
        mmio.store::<Self::Value>(Self::OFFSET, self.to_raw())
    }
}

/// A trait for types representing a register with a variable offset.
///
/// Types implementing this trait are typically wrappers around fundamental
/// types (u8, u16, u32, u64) that provide bit-level access to fields within
/// the register.
pub trait IndexedRegister: Sized {
    /// The underlying integer type (e.g., u32) that holds the register bits.
    type Value: MmioOperand;

    /// The byte offset of the first element in the register array.
    const BASE_OFFSET: usize;

    /// The byte distance between successive elements in the register array.
    const STRIDE: usize;

    /// The maximum number of valid elements in the register array.
    const COUNT: usize;

    /// Initializes the register type with a raw value, typically after reading from MMIO.
    fn from_raw(value: Self::Value) -> Self;

    /// Converts the register type back to its raw bits, typically before writing to MMIO.
    fn to_raw(&self) -> Self::Value;
}

/// A trait for indexed registers that can be read.
pub trait ReadableIndexedRegister: IndexedRegister {
    /// Loads the register value at `index` from the MMIO region.
    ///
    /// The offset is calculated as `BASE_OFFSET + (index * STRIDE)`.
    ///
    /// # Panics
    ///
    /// This method will panic if `index` is greater than or equal to `COUNT`.
    fn read_index<M: Mmio + ?Sized>(mmio: &M, index: usize) -> Self {
        assert!(index < Self::COUNT, "Register index out of bounds");
        let offset = Self::BASE_OFFSET + (index * Self::STRIDE);
        Self::from_raw(mmio.load::<Self::Value>(offset))
    }
}

/// A trait for indexed registers that can be written.
pub trait WritableIndexedRegister: IndexedRegister {
    /// Stores the register's state at `index` into the MMIO region.
    ///
    /// The offset is calculated as `BASE_OFFSET + (index * STRIDE)`.
    ///
    /// # Panics
    ///
    /// This method will panic if `index` is greater than or equal to `COUNT`.
    fn write_index<M: Mmio + ?Sized>(&self, mmio: &mut M, index: usize) {
        assert!(index < Self::COUNT, "Register index out of bounds");
        let offset = Self::BASE_OFFSET + (index * Self::STRIDE);
        mmio.store::<Self::Value>(offset, self.to_raw())
    }
}

/// A macro for defining a [`Register`] and its bitfields.
///
/// Access modes can be RO (Read-Only), WO (Write-Only), or RW (Read-Write).
///
/// # Examples
///
/// ```rust
/// register! {
///     StatusReg, u32, 0x10, RW, {
///         pub enabled, set_enabled: 0;
///         pub error, _: 1;
///         pub value, set_value: 15, 8;
///     }
/// }
/// ```
#[macro_export]
macro_rules! register {
    ($name:ident, $val_type:ty, $offset:expr, RO, { $($field_spec:tt)* }) => {
        $crate::bitfield::bitfield! {
            #[derive(Copy, Clone, PartialEq, Eq, Default)]
            pub struct $name($val_type);
            impl Debug;
            $($field_spec)*
        }

        impl $crate::Register for $name {
            type Value = $val_type;
            const OFFSET: usize = $offset;

            fn from_raw(value: Self::Value) -> Self {
                $name(value)
            }

            fn to_raw(&self) -> Self::Value {
                self.0
            }
        }

        impl $crate::ReadableRegister for $name {}
    };
    ($name:ident, $val_type:ty, $offset:expr, WO, { $($field_spec:tt)* }) => {
        $crate::bitfield::bitfield! {
            #[derive(Copy, Clone, PartialEq, Eq, Default)]
            pub struct $name($val_type);
            impl Debug;
            $($field_spec)*
        }

        impl $crate::Register for $name {
            type Value = $val_type;
            const OFFSET: usize = $offset;

            fn from_raw(value: Self::Value) -> Self {
                $name(value)
            }

            fn to_raw(&self) -> Self::Value {
                self.0
            }
        }

        impl $crate::WritableRegister for $name {}
    };
    ($name:ident, $val_type:ty, $offset:expr, RW, { $($field_spec:tt)* }) => {
        $crate::bitfield::bitfield! {
            #[derive(Copy, Clone, PartialEq, Eq, Default)]
            pub struct $name($val_type);
            impl Debug;
            $($field_spec)*
        }

        impl $crate::Register for $name {
            type Value = $val_type;
            const OFFSET: usize = $offset;

            fn from_raw(value: Self::Value) -> Self {
                $name(value)
            }

            fn to_raw(&self) -> Self::Value {
                self.0
            }
        }

        impl $crate::ReadableRegister for $name {}
        impl $crate::WritableRegister for $name {}
    };
}

/// A macro for defining an [`IndexedRegister`] and its bitfields.
///
/// Access modes can be RO, WO, or RW.
///
/// # Examples
///
/// ```rust
/// indexed_register! {
///     DataReg, u32, 0x100, 4, 16, RO, {
///         pub value, _: 31, 0;
///     }
/// }
/// ```
#[macro_export]
macro_rules! indexed_register {
    ($name:ident, $val_type:ty, $base_offset:expr, $stride:expr, $count:expr, RO, { $($field_spec:tt)* }) => {
        $crate::bitfield::bitfield! {
            #[derive(Copy, Clone, PartialEq, Eq, Default)]
            pub struct $name($val_type);
            impl Debug;
            $($field_spec)*
        }

        impl $crate::IndexedRegister for $name {
            type Value = $val_type;
            const BASE_OFFSET: usize = $base_offset;
            const STRIDE: usize = $stride;
            const COUNT: usize = $count;

            fn from_raw(value: Self::Value) -> Self {
                $name(value)
            }

            fn to_raw(&self) -> Self::Value {
                self.0
            }
        }

        impl $crate::ReadableIndexedRegister for $name {}
    };
    ($name:ident, $val_type:ty, $base_offset:expr, $stride:expr, $count:expr, WO, { $($field_spec:tt)* }) => {
        $crate::bitfield::bitfield! {
            #[derive(Copy, Clone, PartialEq, Eq, Default)]
            pub struct $name($val_type);
            impl Debug;
            $($field_spec)*
        }

        impl $crate::IndexedRegister for $name {
            type Value = $val_type;
            const BASE_OFFSET: usize = $base_offset;
            const STRIDE: usize = $stride;
            const COUNT: usize = $count;

            fn from_raw(value: Self::Value) -> Self {
                $name(value)
            }

            fn to_raw(&self) -> Self::Value {
                self.0
            }
        }

        impl $crate::WritableIndexedRegister for $name {}
    };
    ($name:ident, $val_type:ty, $base_offset:expr, $stride:expr, $count:expr, RW, { $($field_spec:tt)* }) => {
        $crate::bitfield::bitfield! {
            #[derive(Copy, Clone, PartialEq, Eq, Default)]
            pub struct $name($val_type);
            impl Debug;
            $($field_spec)*
        }

        impl $crate::IndexedRegister for $name {
            type Value = $val_type;
            const BASE_OFFSET: usize = $base_offset;
            const STRIDE: usize = $stride;
            const COUNT: usize = $count;

            fn from_raw(value: Self::Value) -> Self {
                $name(value)
            }

            fn to_raw(&self) -> Self::Value {
                self.0
            }
        }

        impl $crate::ReadableIndexedRegister for $name {}
        impl $crate::WritableIndexedRegister for $name {}
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Memory;
    use core::mem::MaybeUninit;

    register! {
        TestReg, u32, 4, RW, {
            pub field1, set_field1: 7, 0;
            pub field2, set_field2: 15, 8;
        }
    }

    #[test]
    fn test_register_read_write() {
        let mut mem = MaybeUninit::<[u32; 4]>::zeroed();
        let mut mmio = Memory::borrow_uninit(&mut mem);

        let mut reg = TestReg::default();
        reg.set_field1(0x12);
        reg.set_field2(0x34);

        reg.write(&mut mmio);

        let reg2 = TestReg::read(&mmio);
        assert_eq!(reg, reg2);
        assert_eq!(reg2.field1(), 0x12);
        assert_eq!(reg2.field2(), 0x34);
    }

    #[test]
    fn test_update_reg() {
        let mut mem = MaybeUninit::<[u32; 4]>::zeroed();
        let mut mmio = Memory::borrow_uninit(&mut mem);

        mmio.update_reg::<TestReg, _>(|reg| {
            reg.set_field1(0xab);
        });

        let reg = mmio.read_reg::<TestReg>();
        assert_eq!(reg.field1(), 0xab);
        assert_eq!(reg.field2(), 0);
    }
}
