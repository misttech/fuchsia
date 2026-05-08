// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! # `spmi-hwreg`
//! Rust SPMI register access library matching the MMIO hwreg paradigm.

#![deny(missing_docs)]
// Re-export necessary types for the macros to use without requiring callers to add dependencies.
#[doc(hidden)]
pub mod __private {
    pub use fidl_fuchsia_hardware_spmi as fspmi;
}

/// Represents the endianness of a register.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Endianness {
    /// Little Endian.
    Little,
    /// Big Endian.
    Big,
}

/// Trait for types that can be read from or written to registers.
pub trait RegisterValue: Sized {
    /// The size of the value in bytes.
    const SIZE: usize;
    /// Creates a value from raw bytes.
    fn from_bytes(bytes: &[u8], is_big_endian: bool) -> Result<Self, Error>;
    /// Converts the value to raw bytes in-place.
    fn to_bytes(&self, is_big_endian: bool, out: &mut [u8]);
}

// --- Typestate Access Modes ---
/// Marker for Read-Only access.
pub struct ReadOnly;
/// Marker for Write-Only access.
pub struct WriteOnly;
/// Marker for Read-Write access.
pub struct ReadWrite;

/// Marker trait for modes that allow reading.
pub trait Readable {}
impl Readable for ReadOnly {}
impl Readable for ReadWrite {}

/// Marker trait for modes that allow writing.
pub trait Writable {}
impl Writable for WriteOnly {}
impl Writable for ReadWrite {}

// --- Generic Register ---
/// A generic register accessor parameterized by value type, mode, address, and endianness.
pub struct Register<'a, T, M, const ADDR: u16, const IS_BIG_ENDIAN: bool> {
    /// The underlying SPMI device proxy.
    pub spmi: &'a fidl_fuchsia_hardware_spmi::DeviceProxy,
    _marker: std::marker::PhantomData<(T, M)>,
}

impl<'a, T, M, const ADDR: u16, const IS_BIG_ENDIAN: bool> Register<'a, T, M, ADDR, IS_BIG_ENDIAN> {
    /// Creates a new register accessor.
    pub fn new(spmi: &'a fidl_fuchsia_hardware_spmi::DeviceProxy) -> Self {
        Self { spmi, _marker: std::marker::PhantomData }
    }
}

// Inherent read method available ONLY if M is Readable
impl<'a, T, M, const ADDR: u16, const IS_BIG_ENDIAN: bool> Register<'a, T, M, ADDR, IS_BIG_ENDIAN>
where
    M: Readable,
    T: RegisterValue,
{
    /// Reads the register value.
    pub async fn read(&self) -> Result<T, Error> {
        let size = T::SIZE;
        let bytes = self
            .spmi
            .register_read(ADDR, size as u32)
            .await
            .map_err(|e| Error::Fidl(e.into()))?
            .map_err(|e| Error::Spmi(e))?;
        T::from_bytes(&bytes, IS_BIG_ENDIAN)
    }
}

// Inherent write method available ONLY if M is Writable
impl<'a, T, M, const ADDR: u16, const IS_BIG_ENDIAN: bool> Register<'a, T, M, ADDR, IS_BIG_ENDIAN>
where
    M: Writable,
    T: RegisterValue,
{
    /// Writes the register value.
    pub async fn write(&self, val: T) -> Result<(), Error> {
        assert!(T::SIZE <= 8, "Register size exceeds stack buffer");
        let mut bytes = [0u8; 8];
        val.to_bytes(IS_BIG_ENDIAN, &mut bytes[0..T::SIZE]);
        self.spmi
            .register_write(ADDR, &bytes[0..T::SIZE])
            .await
            .map_err(|e| Error::Fidl(e.into()))?
            .map_err(|e| Error::Spmi(e))?;
        Ok(())
    }
}

/// Error type for the `spmi-hwreg` crate.
#[derive(Debug)]
pub enum Error {
    /// A FIDL error occurred.
    Fidl(fidl::Error),
    /// An SPMI protocol error occurred.
    Spmi(fidl_fuchsia_hardware_spmi::DriverError),
    /// The read operation returned an unexpected number of bytes.
    SizeMismatch,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Fidl(e) => write!(f, "FIDL error: {}", e),
            Error::Spmi(e) => write!(f, "SPMI error: {:?}", e),
            Error::SizeMismatch => write!(f, "Size mismatch in read operation"),
        }
    }
}

impl std::error::Error for Error {}

/// Defines a module for accessing a single SPMI hardware register.
///
/// This macro generates a public module named `$name` containing:
/// -   `ADDRESS`: A `u16` constant for the register address.
/// -   `Value`: A struct wrapping the raw register value (`$value_type`) and providing
///     const-fn accessors for defined fields.
/// -   `Register<'a>`: A struct with an asynchronous `read` and potentially `write` method
///     to interact with the SPMI device.
///
/// # Arguments
///
/// 1.  `$name`: The identifier for the generated module.
/// 2.  `$value_type:ty`: The Rust integer type representing the register's width (e.g., `u8`, `u16`, `u32`).
/// 3.  `$addr:expr`: The base address of the register as a `u16`.
/// 4.  `$mode:ident`: The access mode, one of `RO` (Read Only), `WO` (Write Only), or `RW` (Read/Write).
/// 5.  `$endianness:ident`: The endianness of the register. Required for `u16` and larger. Can be `LE` (Little Endian) or `BE` (Big Endian). For `u8` registers, this argument is omitted because endianness is irrelevant for `u8` registers.
/// 6.  `{ ... }`: A block defining the fields within the register. Each field definition ends with a semicolon.
///     The following formats are supported within the block:
///     *   **Single-bit field:** `$vis $field_name $(, $setter_name)? : $bit_index;`
///         -   `$vis`: Visibility (e.g., `pub`).
///         -   `$field_name`: The name of the getter method (returns `bool`).
///         -   `$setter_name`: (Optional) The name of the setter method (takes `bool`, returns `Self`).
///         -   `$bit_index`: The index of the single bit (e.g., `7`).
///         Example: `pub enable, set_enable: 7;`
///
///     *   **Multi-bit field:** `$vis $field_name $(, $setter_name)? : $msb, $lsb;`
///         -   `$vis`: Visibility.
///         -   `$field_name`: The name of the getter method (returns `$value_type`).
///         -   `$setter_name`: (Optional) The name of the setter method (takes `$value_type`, returns `Self`).
///         -   `$msb`: The Most Significant Bit index.
///         -   `$lsb`: The Least Significant Bit index.
///         Example: `pub field_val, set_field_val: 5, 2;`
///
///     *   **Enum field (external type):** `$vis enum $enum_type, $field_name $(, $setter_name)? : $msb, $lsb;`
///         -   `$vis`: Visibility.
///         -   `$enum_type`: The path to an existing enum type (e.g., `super::PowerMode`). This enum
///             must have a `pub const fn from_val(val: $value_type) -> Self` associated function.
///         -   `$field_name`: The name of the getter method (returns `$enum_type`).
///         -   `$setter_name`: (Optional) The name of the setter method (takes `$enum_type`, returns `Self`).
///         -   `$msb`, `$lsb`: The bit range.
///         Example: `pub enum super::PowerMode, mode, set_mode: 3, 2;`
///
///     *   **In-line Enum field:** `$vis enum $enum_name { ... }, $field_name $(, $setter_name)? : $msb, $lsb;`
///         -   `$vis`: Visibility.
///         -   `$enum_name`: The name for the enum type, defined within the generated module.
///         -   `{ ... }`: The enum variants and their `$value_type` values.
///         -   `$field_name`: The name of the getter method (returns `Result<$enum_name, $value_type>`).
///         -   `$setter_name`: (Optional) The name of the setter method (takes `$enum_name`, returns `Self`).
///         -   `$msb`, `$lsb`: The bit range.
///         Example: `pub enum InlineMode { A = 0, B = 1 }, mode, set_mode: 1, 0;`
///
///     *   **Custom constant:** `pub const $const_name : $type = $val;`
///         -   Allows defining constants within the generated register module.
///         Example: `pub const MAX_VALUE: u8 = 0xFF;`
///
/// # Examples
///
/// ```
/// // Define a register at address 0x10, 8-bit, Read/Write, Little Endian (default)
/// spmi_register! {
///     my_reg, u8, 0x10, RW, {
///         // Single bit flag
///         pub enable, set_enable: 7;
///         // 4-bit field
///         pub value, set_value: 3, 0;
///         // Inline enum field
///         pub enum Status {
///             Idle = 0,
///             Active = 1,
///             Error = 2,
///         }, status, set_status: 6, 5;
///     }
/// }
///
/// // Define a register at address 0x20, 16-bit, Read Only, Big Endian
/// spmi_register! {
///     status_be_reg, u16, 0x20, RO, BE, {
///         pub error_code: 15, 8;
///         pub ready: 0;
///     }
/// }
/// ```
#[macro_export]
macro_rules! spmi_register {
    // Helper to map mode and endianness to register type
    (@map_reg RO, $val:ty, $addr:expr, BE) => {
        $crate::Register<'a, $val, $crate::ReadOnly, $addr, true>
    };
    (@map_reg RO, $val:ty, $addr:expr, LE) => {
        $crate::Register<'a, $val, $crate::ReadOnly, $addr, false>
    };
    (@map_reg WO, $val:ty, $addr:expr, BE) => {
        $crate::Register<'a, $val, $crate::WriteOnly, $addr, true>
    };
    (@map_reg WO, $val:ty, $addr:expr, LE) => {
        $crate::Register<'a, $val, $crate::WriteOnly, $addr, false>
    };
    (@map_reg RW, $val:ty, $addr:expr, BE) => {
        $crate::Register<'a, $val, $crate::ReadWrite, $addr, true>
    };
    (@map_reg RW, $val:ty, $addr:expr, LE) => {
        $crate::Register<'a, $val, $crate::ReadWrite, $addr, false>
    };
    // Explicit endianness provided via single arm
    (
        $name:ident, $value_type:ty, $addr:expr, $mode:ident, $endianness:ident, {
            $($tail:tt)*
        }
    ) => {
        #[allow(unused_imports)]
        #[allow(dead_code)]
        pub mod $name {
            use super::*;
            /// The address of the register.
            pub const ADDRESS: u16 = $addr;

            $crate::spmi_register_extract_enums!($value_type, $($tail)*);

            /// Represents the value of the register.
            #[derive(Copy, Clone, Debug, PartialEq, Eq)]
            pub struct Value(pub $value_type);

            impl Value {
                pub const fn new(val: $value_type) -> Self { Self(val) }
                pub const fn reg_value(&self) -> $value_type { self.0 }

                /// Reconstructs a typed `Value` from a slice of raw bytes using the correct endianness.
                pub fn from_bytes(bytes: &[u8]) -> Result<Self, $crate::Error> {
                    let __is_big = spmi_register!(@is_big $endianness);
                    <Self as $crate::RegisterValue>::from_bytes(bytes, __is_big)
                }

                /// Converts the `Value` into its raw byte representation using the correct endianness.
                pub fn to_bytes(&self) -> [u8; std::mem::size_of::<$value_type>()] {
                    let __is_big = spmi_register!(@is_big $endianness);
                    let mut arr = [0u8; std::mem::size_of::<$value_type>()];
                    <Self as $crate::RegisterValue>::to_bytes(self, __is_big, &mut arr);
                    arr
                }

                $crate::spmi_register_fields!($value_type, $($tail)*);
            }

            impl $crate::RegisterValue for Value {
                const SIZE: usize = std::mem::size_of::<$value_type>();

                fn from_bytes(bytes: &[u8], is_big_endian: bool) -> Result<Self, $crate::Error> {
                    if bytes.len() != Self::SIZE {
                        return Err($crate::Error::SizeMismatch);
                    }
                    let mut arr = [0u8; std::mem::size_of::<$value_type>()];
                    arr.copy_from_slice(bytes);
                    let raw = if is_big_endian {
                        <$value_type>::from_be_bytes(arr)
                    } else {
                        <$value_type>::from_le_bytes(arr)
                    };
                    Ok(Self(raw))
                }

                fn to_bytes(&self, is_big_endian: bool, out: &mut [u8]) {
                    assert_eq!(out.len(), Self::SIZE);
                    let bytes = if is_big_endian {
                        self.0.to_be_bytes()
                    } else {
                        self.0.to_le_bytes()
                    };
                    out.copy_from_slice(&bytes);
                }
            }

            impl Default for Value {
                fn default() -> Self {
                    Self(0 as $value_type)
                }
            }

            /// The register accessor type.
            pub type Register<'a> = spmi_register!(@map_reg $mode, Value, $addr, $endianness);
        }
    };
    // Specialized arm for u8 where endianness is irrelevant
    (
        $name:ident, u8, $addr:expr, $mode:ident, {
            $($tail:tt)*
        }
    ) => {
        spmi_register!($name, u8, $addr, $mode, LE, { $($tail)* });
    };
    (@is_big BE) => { true };
    (@is_big LE) => { false };
}

/// Helper macro for `spmi_register!` to extract inline enums.
#[macro_export]
#[doc(hidden)]
macro_rules! spmi_register_extract_enums {
    // Case 4: In-line Enum
    ($value_type:ty, $(#[$attr:meta])* $vis:vis enum $enum_name:ident { $( $variant_name:ident = $variant_val:expr ),* $(,)? }, $field:ident $(, $setter:ident)? : $msb:expr, $lsb:expr; $($tail:tt)*) => {
        #[derive(Debug, PartialEq, Eq, Copy, Clone)]
        #[repr($value_type)]
        $vis enum $enum_name {
            $( $variant_name = $variant_val ),*
        }
        $crate::spmi_register_extract_enums!($value_type, $($tail)*);
    };

    // Forwarding other cases
    ($value_type:ty, $(#[$attr:meta])* $vis:vis $field:ident $(, $setter:ident)? : $bit:expr; $($tail:tt)*) => {
        $crate::spmi_register_extract_enums!($value_type, $($tail)*);
    };
    ($value_type:ty, $(#[$attr:meta])* $vis:vis $field:ident $(, $setter:ident)? : $msb:expr, $lsb:expr; $($tail:tt)*) => {
        $crate::spmi_register_extract_enums!($value_type, $($tail)*);
    };
    ($value_type:ty, $(#[$attr:meta])* $vis:vis enum $enum_type:ty, $field:ident $(, $setter:ident)? : $msb:expr, $lsb:expr; $($tail:tt)*) => {
        $crate::spmi_register_extract_enums!($value_type, $($tail)*);
    };
    ($value_type:ty, pub const $name:ident : $type:ty = $val:expr; $($tail:tt)*) => {
        $crate::spmi_register_extract_enums!($value_type, $($tail)*);
    };
    ($value_type:ty) => {};
    ($value_type:ty,) => {};
}

/// Helper macro for `spmi_register!` to generate field accessors.
#[macro_export]
#[doc(hidden)]
macro_rules! spmi_register_fields {
    // Terminating cases
    ($value_type:ty) => {};
    ($value_type:ty,) => {};

    // 1. Single-bit field
    ($value_type:ty, $(#[$attr:meta])* $vis:vis $field:ident $(, $setter:ident)? : $bit:expr; $($tail:tt)*) => {
        $(#[$attr])*
        #[allow(non_snake_case)]
        #[allow(dead_code)]
        $vis const fn $field(&self) -> bool {
            const _: () = assert!($bit < <$value_type>::BITS as u8, "Bit index out of bounds");
            let bit_mask = (1 as $value_type) << $bit;
            (self.0 & bit_mask) != 0
        }
        $(
            #[allow(non_snake_case)]
            #[allow(dead_code)]
            $vis const fn $setter(mut self, val: bool) -> Self {
                const _: () = assert!($bit < <$value_type>::BITS as u8, "Bit index out of bounds");
                let bit_mask = (1 as $value_type) << $bit;
                self.0 = (self.0 & !bit_mask) | ((val as $value_type) << $bit);
                self
            }
        )?
        $crate::spmi_register_fields!($value_type, $($tail)*);
    };

    // 2. Multi-bit field
    ($value_type:ty, $(#[$attr:meta])* $vis:vis $field:ident $(, $setter:ident)? : $msb:expr, $lsb:expr; $($tail:tt)*) => {
        $(#[$attr])*
        #[allow(non_snake_case)]
        #[allow(dead_code)]
        $vis const fn $field(&self) -> $value_type {
            const _: () = assert!($msb < <$value_type>::BITS as u8, "MSB index out of bounds");
            const _: () = assert!($lsb < $msb, "LSB must be strictly less than MSB. Use single-bit syntax (e.g., 'field: bit;') for 1-bit fields.");
            let bit_count = $msb - $lsb + 1;
            let bit_mask = ((!0 as $value_type) >> (<$value_type>::BITS as u8 - bit_count)) << $lsb;
            (self.0 & bit_mask) >> $lsb
        }
        $(
            #[allow(non_snake_case)]
            #[allow(dead_code)]
            $vis const fn $setter(mut self, val: $value_type) -> Self {
                const _: () = assert!($msb < <$value_type>::BITS as u8, "MSB index out of bounds");
                const _: () = assert!($lsb < $msb, "LSB must be strictly less than MSB. Use single-bit syntax (e.g., 'field: bit;') for 1-bit fields.");
                let bit_count = $msb - $lsb + 1;
                let bit_mask = ((!0 as $value_type) >> (<$value_type>::BITS as u8 - bit_count)) << $lsb;
                self.0 = (self.0 & !bit_mask) | ((val << $lsb) & bit_mask);
                self
            }
        )?
        $crate::spmi_register_fields!($value_type, $($tail)*);
    };

    // 3. Enum field
    ($value_type:ty, $(#[$attr:meta])* $vis:vis enum $enum_type:ty, $field:ident $(, $setter:ident)? : $msb:expr, $lsb:expr; $($tail:tt)*) => {
        $(#[$attr])*
        #[allow(non_snake_case)]
        #[allow(dead_code)]
        $vis const fn $field(&self) -> $enum_type {
            const _: () = assert!($msb < <$value_type>::BITS as u8, "MSB index out of bounds");
            const _: () = assert!($lsb <= $msb, "LSB must be less than or equal to MSB");
            let bit_count = $msb - $lsb + 1;
            let bit_mask = ((!0 as $value_type) >> (<$value_type>::BITS as u8 - bit_count)) << $lsb;
            <$enum_type>::from_val((self.0 & bit_mask) >> $lsb)
        }
        $(
            #[allow(non_snake_case)]
            #[allow(dead_code)]
            $vis const fn $setter(mut self, val: $enum_type) -> Self {
                const _: () = assert!($msb < <$value_type>::BITS as u8, "MSB index out of bounds");
                const _: () = assert!($lsb <= $msb, "LSB must be less than or equal to MSB");
                let bit_count = $msb - $lsb + 1;
                let bit_mask = ((!0 as $value_type) >> (<$value_type>::BITS as u8 - bit_count)) << $lsb;
                self.0 = (self.0 & !bit_mask) | (((val as $value_type) << $lsb) & bit_mask);
                self
            }
        )?
        $crate::spmi_register_fields!($value_type, $($tail)*);
    };

    // 4. In-line Enum field
    ($value_type:ty, $(#[$attr:meta])* $vis:vis enum $enum_name:ident { $( $variant_name:ident = $variant_val:expr ),* $(,)? }, $field:ident $(, $setter:ident)? : $msb:expr, $lsb:expr; $($tail:tt)*) => {
        $(#[$attr])*
        #[allow(non_snake_case)]
        #[allow(dead_code)]
        $vis const fn $field(&self) -> Result<$enum_name, $value_type> {
            const _: () = assert!($msb < <$value_type>::BITS as u8, "MSB index out of bounds");
            const _: () = assert!($lsb <= $msb, "LSB must be less than or equal to MSB");
            let bit_count = $msb - $lsb + 1;
            let bit_mask = ((!0 as $value_type) >> (<$value_type>::BITS as u8 - bit_count)) << $lsb;
            let val = (self.0 & bit_mask) >> $lsb;
            $(
                if val == $enum_name::$variant_name as $value_type {
                    return Ok($enum_name::$variant_name);
                }
            )*
            Err(val)
        }
        $(
            #[allow(non_snake_case)]
            #[allow(dead_code)]
            $vis const fn $setter(mut self, val: $enum_name) -> Self {
                const _: () = assert!($msb < <$value_type>::BITS as u8, "MSB index out of bounds");
                const _: () = assert!($lsb <= $msb, "LSB must be less than or equal to MSB");
                let bit_count = $msb - $lsb + 1;
                let bit_mask = ((!0 as $value_type) >> (<$value_type>::BITS as u8 - bit_count)) << $lsb;
                self.0 = (self.0 & !bit_mask) | (((val as $value_type) << $lsb) & bit_mask);
                self
            }
        )?
        $crate::spmi_register_fields!($value_type, $($tail)*);
    };

    // 5. Custom constant
    ($value_type:ty, pub const $name:ident : $type:ty = $val:expr; $($tail:tt)*) => {
        pub const $name: $type = $val;
        $crate::spmi_register_fields!($value_type, $($tail)*);
    };
}

/// Verifies at compile-time that a list of registers is contiguous.
#[macro_export]
#[doc(hidden)]
macro_rules! assert_contiguous {
    ($prev:ident, $curr:ident $(, $rest:ident)*) => {
        const _: () = assert!(
            $curr::ADDRESS == $prev::ADDRESS + $prev::Value::SIZE as u16,
            concat!("Registers ", stringify!($prev), " and ", stringify!($curr), " are not contiguous")
        );
        $crate::assert_contiguous!($curr $(, $rest)*);
    };
    ($last:ident) => {};
}

/// Defines a struct to group multiple SPMI registers.
///
/// This macro generates a public struct named `$name` that holds a `DeviceProxy`
/// and provides methods to access individual registers defined by `spmi_register!`,
/// as well as `read_bulk` and `write_bulk` methods for contiguous accesses.
///
/// # Arguments
///
/// 1.  `pub struct $name:ident`: The definition of the struct.
/// 2.  `{ ... }`: A block containing mappings from field names to register modules.
///     Each mapping has the format: `$vis $field:ident => $reg_mod:ident`, where:
///     -   `$vis`: Visibility (e.g., `pub`).
///     -   `$field:ident`: The name of the method to be generated in `$name`. This method
///         will return an instance of the register's `Register` type.
///     -   `$reg_mod:ident`: The name of the module generated by `spmi_register!`
///         (e.g., `my_reg`).
///
/// # Examples
///
/// ```
/// // Assume 'my_reg' and 'status_be_reg' are defined using spmi_register!
/// spmi_register_block! {
///     pub struct MySpmiRegisters {
///         pub general => my_reg,
///         pub status => status_be_reg,
///     }
/// }
///
/// // Usage:
/// // let spmi_proxy = ...;
/// // let regs = MySpmiRegisters::new(spmi_proxy);
/// //
/// // // Read individual registers:
/// // let my_val = regs.general().read().await?;
/// // let status_val = regs.status().read().await?;
/// ```
#[macro_export]
macro_rules! spmi_register_block {
    (
        pub struct $name:ident {
            $($tail:tt)*
        }
    ) => {
        pub struct $name {
            pub spmi: $crate::__private::fspmi::DeviceProxy,
        }

        #[allow(dead_code)]
        impl $name {
            pub fn new(spmi: $crate::__private::fspmi::DeviceProxy) -> Self {
                Self { spmi }
            }

            /// Reads a raw byte slice from the contiguous register range.
            ///
            /// # Note
            /// This method is public but hidden because it is required by the `spmi_read_contiguous!`
            /// macro. Direct use is discouraged.
            #[doc(hidden)]
            #[allow(dead_code)]
            pub async fn read_bulk(&self, address: u16, size: u32) -> Result<Vec<u8>, $crate::Error> {
                let data = self.spmi
                    .register_read(address, size)
                    .await
                    .map_err(|e| $crate::Error::Fidl(e.into()))?
                    .map_err(|e| $crate::Error::Spmi(e))?;
                if data.len() == size as usize {
                    Ok(data)
                } else {
                    Err($crate::Error::SizeMismatch)
                }
            }

            /// Reads a raw byte slice from the contiguous register range into a mutable buffer.
            ///
            /// # Note
            /// This method is public but hidden to discourage direct use, keeping the bulk API
            /// consistent with `read_bulk`.
            #[doc(hidden)]
            #[allow(dead_code)]
            pub async fn read_bulk_into(&self, address: u16, out: &mut [u8]) -> Result<(), $crate::Error> {
                let data = self.spmi
                    .register_read(address, out.len() as u32)
                    .await
                    .map_err(|e| $crate::Error::Fidl(e.into()))?
                    .map_err(|e| $crate::Error::Spmi(e))?;
                if data.len() == out.len() {
                    out.copy_from_slice(&data);
                    Ok(())
                } else {
                    Err($crate::Error::SizeMismatch)
                }
            }

            /// Writes the specified data to the contiguous register range.
            ///
            /// # Note
            /// This method is public but hidden because it is required by the `spmi_write_contiguous!`
            /// macro. Direct use is discouraged.
            #[doc(hidden)]
            #[allow(dead_code)]
            pub async fn write_bulk(&self, address: u16, data: &[u8]) -> Result<(), $crate::Error> {
                self.spmi
                    .register_write(address, data)
                    .await
                    .map_err(|e| $crate::Error::Fidl(e.into()))?
                    .map_err(|e| $crate::Error::Spmi(e))?;
                Ok(())
            }

            spmi_register_block!(@fields $($tail)*);
        }
    };

    (@fields) => {};

    // Case 1: Individual register with trailing fields
    (@fields $vis:vis $field:ident => $reg_mod:ident, $($tail:tt)*) => {
        #[allow(dead_code)]
        $vis fn $field(&self) -> $reg_mod::Register<'_> {
            $reg_mod::Register::new(&self.spmi)
        }
        spmi_register_block!(@fields $($tail)*);
    };

    // Case 2: Individual register at end of token stream
    (@fields $vis:vis $field:ident => $reg_mod:ident) => {
        #[allow(dead_code)]
        $vis fn $field(&self) -> $reg_mod::Register<'_> {
            $reg_mod::Register::new(&self.spmi)
        }
    };
}

/// Reads multiple contiguous registers in a single async call to the hardware.
///
/// This macro accepts an SPMI device proxy and a list of register modules, calculates the
/// base address and combined size, and performs a single async contiguous read.
///
/// # Arguments
///
/// 1.  `$spmi:expr`: The SPMI device proxy.
/// 2.  `$( $reg:ident ),*`: A comma-separated list of already-declared register modules.
///
/// # Examples
///
/// ```
/// // Read both 'general' and 'status' registers together, update, and write back:
/// // let regs = MySpmiRegisters::new(spmi_proxy);
/// let (mut general_val, status_val) = spmi_read_contiguous!(
///     &regs,
///     my_reg,
///     status_be_reg
/// ).await?;
///
/// general_val = general_val.set_field1(true);
///
/// spmi_write_contiguous!(
///     &regs,
///     my_reg => general_val,
///     status_be_reg => status_val
/// ).await?;
/// ```

#[macro_export]
macro_rules! spmi_read_contiguous {
    (
        $regs:expr,
        $head:ident $(, $tail:ident )* $(,)?
    ) => {
        async {
            $crate::assert_contiguous!($head $(, $tail)*);
            let res: Result<($head::Value, $( $tail::Value ),*), $crate::Error> = async {
                let base_addr = $head::ADDRESS;

                let total_size = std::mem::size_of::<$head::Value>() $( + std::mem::size_of::<$tail::Value>() )*;

                let data = $regs.read_bulk(base_addr, total_size as u32).await?;

                let mut cursor = 0;
                let $head = $head::Value::from_bytes(&data[cursor..std::mem::size_of::<$head::Value>()])?;
                cursor = std::mem::size_of::<$head::Value>();
                $(
                    let end = cursor + std::mem::size_of::<$tail::Value>();
                    let $tail = $tail::Value::from_bytes(&data[cursor..end])?;
                    cursor = end;
                )*
                let _ = cursor;

                Ok(($head, $( $tail ),*))
            }.await;
            res
        }
    };
}

/// Writes multiple contiguous registers in a single async call to the hardware.
///
/// This macro accepts an SPMI device proxy and a mapping of register modules to their values,
/// serializes the values using their correct endianness, and performs a single async contiguous write.
///
/// # Arguments
///
/// 1.  `$regs:expr`: The block struct instance.
/// 2.  `$( $reg:ident => $val:expr ),*`: A comma-separated mapping from register modules to their values.
///
/// # Examples
///
/// ```
/// // Write both 'general' and 'status' registers together:
/// // let regs = MySpmiRegisters::new(spmi_proxy);
/// spmi_write_contiguous!(
///     &regs,
///     my_reg => val_a,
///     status_be_reg => val_b
/// ).await?;
/// ```
#[macro_export]
macro_rules! spmi_write_contiguous {
    (
        $regs:expr,
        $head:ident => $head_val:expr $(, $tail:ident => $tail_val:expr )* $(,)?
    ) => {
        async {
            $crate::assert_contiguous!($head $(, $tail)*);
            let res: Result<(), $crate::Error> = async move {
                let base_addr = $head::ADDRESS;

                let mut bytes = Vec::new();
                bytes.extend_from_slice(&$head_val.to_bytes());
                $(
                    bytes.extend_from_slice(&$tail_val.to_bytes());
                )*

                $regs.write_bulk(base_addr, &bytes).await?;

                Ok(())
            }.await;
            res
        }
    };
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;
    use fidl_fuchsia_hardware_spmi as fspmi;
    use futures::StreamExt;

    // Note: For single-byte width registers (u8), explicit endianness is not required.
    spmi_register! {
        test_u8_reg, u8, 0xCD, RW, {
            pub flag, set_flag: 4;
            pub field, set_field: 3, 0;
        }
    }

    spmi_register_block! {
        pub struct DummyU8Regs {
            pub test => test_u8_reg,
        }
    }

    spmi_register! {
        test_u16_from_bytes_reg, u16, 0x99, RW, LE, {
            pub field, set_field: 15, 0;
        }
    }

    spmi_register_block! {
        pub struct DummyU16FromBytesRegs {
            pub test => test_u16_from_bytes_reg,
        }
    }

    spmi_register! {
        test_u16_contig_reg, u16, 0xCE, RW, LE, {
            pub field, set_field: 15, 0;
        }
    }

    #[fuchsia::test]
    async fn test_u16_from_bytes() {
        let (proxy, mut stream) =
            ::fidl::endpoints::create_proxy_and_stream::<fspmi::DeviceMarker>();

        fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fspmi::DeviceRequest::RegisterRead { address, size_bytes, responder } => {
                        assert_eq!(address, 0x99);
                        assert_eq!(size_bytes, 2);
                        responder.send(Ok(&[0x34, 0x12])).unwrap();
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        })
        .detach();

        let regs = DummyU16FromBytesRegs::new(proxy);
        let val = regs.test().read().await.unwrap();
        assert_eq!(val.reg_value(), 0x1234);
    }

    #[fuchsia::test]
    async fn test_u8_register() {
        let (proxy, mut stream) =
            ::fidl::endpoints::create_proxy_and_stream::<fspmi::DeviceMarker>();

        fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fspmi::DeviceRequest::RegisterRead { address, size_bytes, responder } => {
                        assert_eq!(address, 0xCD);
                        assert_eq!(size_bytes, 1);
                        responder.send(Ok(&[0x1A])).unwrap();
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        })
        .detach();

        let regs = DummyU8Regs::new(proxy);
        let val = regs.test().read().await.unwrap();
        assert_eq!(val.reg_value(), 0x1A);
        assert_eq!(val.flag(), true);
        assert_eq!(val.field(), 0x0A);
    }

    spmi_register! {
        test_reg, u16, 0xAB, RW, LE, {
            pub test_bit, set_test_bit: 7;
            pub test_field, set_test_field: 3, 0;
        }
    }

    spmi_register_block! {
        pub struct DummyRegs {
            pub test => test_reg,
        }
    }

    #[fuchsia::test]
    async fn test_read() {
        let (proxy, mut stream) =
            ::fidl::endpoints::create_proxy_and_stream::<fspmi::DeviceMarker>();

        fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fspmi::DeviceRequest::RegisterRead { address, size_bytes, responder } => {
                        assert_eq!(address, 0xAB);
                        assert_eq!(size_bytes, 2);
                        responder.send(Ok(&[0x8A, 0x00])).unwrap();
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        })
        .detach();

        let regs = DummyRegs::new(proxy);
        let val = regs.test().read().await.unwrap();
        assert_eq!(val.reg_value(), 0x008A);
        assert_eq!(val.test_bit(), true);
        assert_eq!(val.test_field(), 0x0A);
    }

    #[fuchsia::test]
    async fn test_write() {
        let (proxy, mut stream) =
            ::fidl::endpoints::create_proxy_and_stream::<fspmi::DeviceMarker>();

        fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fspmi::DeviceRequest::RegisterWrite { address, data, responder } => {
                        assert_eq!(address, 0xAB);
                        assert_eq!(data, &[0x8A, 0x00]);
                        responder.send(Ok(())).unwrap();
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        })
        .detach();

        let regs = DummyRegs::new(proxy);
        let v = test_reg::Value::new(0x008A);
        regs.test().write(v).await.unwrap();
    }

    spmi_register! {
        test_be_reg, u16, 0xEF, RW, BE, {
            pub flag, set_flag: 4;
            pub field, set_field: 3, 0;
        }
    }

    spmi_register_block! {
        pub struct DummyBERegs {
            pub test => test_be_reg,
        }
    }

    #[fuchsia::test]
    async fn test_be_register() {
        let (proxy, mut stream) =
            ::fidl::endpoints::create_proxy_and_stream::<fspmi::DeviceMarker>();

        fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fspmi::DeviceRequest::RegisterRead { address, size_bytes, responder } => {
                        assert_eq!(address, 0xEF);
                        assert_eq!(size_bytes, 2);
                        responder.send(Ok(&[0x00, 0x8A])).unwrap();
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        })
        .detach();

        let regs = DummyBERegs::new(proxy);
        let val = regs.test().read().await.unwrap();
        assert_eq!(val.reg_value(), 0x8A);
    }

    #[derive(Debug, PartialEq, Eq, Copy, Clone)]
    #[repr(u16)]
    pub enum PowerMode {
        Normal = 0,
        Hibernate = 1,
        LowPower = 2,
        Unknown = 0xFFFF,
    }

    impl PowerMode {
        pub const fn from_val(val: u16) -> Self {
            match val {
                0 => PowerMode::Normal,
                1 => PowerMode::Hibernate,
                2 => PowerMode::LowPower,
                _ => PowerMode::Unknown,
            }
        }
    }

    spmi_register! {
        test_enum_reg, u16, 0x44, RW, LE, {
            pub enum PowerMode, mode, set_mode: 3, 2;
        }
    }

    spmi_register_block! {
        pub struct DummyEnumRegs {
            pub test => test_enum_reg,
        }
    }

    #[fuchsia::test]
    async fn test_enum_register() {
        let (proxy, mut stream) =
            ::fidl::endpoints::create_proxy_and_stream::<fspmi::DeviceMarker>();

        fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fspmi::DeviceRequest::RegisterWrite { address, data, responder } => {
                        assert_eq!(address, 0x44);
                        assert_eq!(data, &[0x04, 0x00]);
                        responder.send(Ok(())).unwrap();
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        })
        .detach();

        let regs = DummyEnumRegs::new(proxy);
        let v = test_enum_reg::Value::new(0).set_mode(PowerMode::Hibernate);
        regs.test().write(v).await.unwrap();
    }

    spmi_register! {
        test_inline_enum_reg, u16, 0x55, RW, LE, {
            pub enum InlineMode {
                A = 0,
                B = 1,
            }, mode, set_mode: 1, 0;
        }
    }

    spmi_register_block! {
        pub struct DummyInlineEnumRegs {
            pub test => test_inline_enum_reg,
        }
    }

    #[fuchsia::test]
    async fn test_inline_enum() {
        let (proxy, mut stream) =
            ::fidl::endpoints::create_proxy_and_stream::<fspmi::DeviceMarker>();

        fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fspmi::DeviceRequest::RegisterWrite { address, data, responder } => {
                        assert_eq!(address, 0x55);
                        assert_eq!(data, &[0x01, 0x00]);
                        responder.send(Ok(())).unwrap();
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        })
        .detach();

        let regs = DummyInlineEnumRegs::new(proxy);
        let v = test_inline_enum_reg::Value::new(1).set_mode(test_inline_enum_reg::InlineMode::B);
        assert_eq!(v.mode(), Ok(test_inline_enum_reg::InlineMode::B));
        regs.test().write(v).await.unwrap();
    }

    #[fuchsia::test]
    async fn test_contiguous_read_write() {
        let (proxy, mut stream) =
            ::fidl::endpoints::create_proxy_and_stream::<fspmi::DeviceMarker>();

        fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fspmi::DeviceRequest::RegisterRead { address, size_bytes, responder } => {
                        assert_eq!(address, 0xCD);
                        assert_eq!(size_bytes, 3);
                        responder.send(Ok(&[0x1A, 0x34, 0x12])).unwrap();
                    }
                    fspmi::DeviceRequest::RegisterWrite { address, data, responder } => {
                        assert_eq!(address, 0xCD);
                        assert_eq!(data, &[0x1A, 0x34, 0x12]);
                        responder.send(Ok(())).unwrap();
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        })
        .detach();

        spmi_register_block! {
            pub struct ContiguousRegs {
                pub r1 => test_u8_reg,
                pub r2 => test_u16_contig_reg,
            }
        }
        let regs = ContiguousRegs::new(proxy);

        let (val_1, val_2) =
            spmi_read_contiguous!(&regs, test_u8_reg, test_u16_contig_reg).await.unwrap();

        assert_eq!(val_1.reg_value(), 0x1A);
        assert_eq!(val_2.reg_value(), 0x1234);

        spmi_write_contiguous!(
            &regs,
            test_u8_reg => val_1,
            test_u16_contig_reg => val_2
        )
        .await
        .unwrap();
    }

    #[fuchsia::test]
    async fn test_read_write_bulk() {
        let (proxy, mut stream) =
            ::fidl::endpoints::create_proxy_and_stream::<fspmi::DeviceMarker>();

        fuchsia_async::Task::local(async move {
            while let Some(Ok(req)) = stream.next().await {
                match req {
                    fspmi::DeviceRequest::RegisterRead { address, size_bytes, responder } => {
                        assert_eq!(address, 0x88);
                        assert_eq!(size_bytes, 3);
                        responder.send(Ok(&[0x1A, 0x2B, 0x3C])).unwrap();
                    }
                    fspmi::DeviceRequest::RegisterWrite { address, data, responder } => {
                        assert_eq!(address, 0x88);
                        assert_eq!(data, &[0x1A, 0x2B, 0x3C]);
                        responder.send(Ok(())).unwrap();
                    }
                    _ => panic!("Unexpected request"),
                }
            }
        })
        .detach();

        let regs = DummyU8Regs::new(proxy);
        let bytes = regs.read_bulk(0x88, 3).await.unwrap();
        assert_eq!(bytes, vec![0x1A, 0x2B, 0x3C]);

        let mut buf = [0u8; 3];
        regs.read_bulk_into(0x88, &mut buf).await.unwrap();
        assert_eq!(buf, [0x1A, 0x2B, 0x3C]);

        regs.write_bulk(0x88, &[0x1A, 0x2B, 0x3C]).await.unwrap();
    }
}
