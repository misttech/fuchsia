// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM CoreSight ROM table registers and tree walker, mirroring
//! `<dev/coresight/rom_table.h>`.

use core::ffi::{c_char, c_void};
use core::mem::{align_of, size_of};
use zr::static_assert;

use crate::component::{
    ClassId, ComponentIdRegister, DeviceArchRegister, MINIMUM_COMPONENT_SIZE, RegisterIo, arm,
};

/// [CS] D6.4.4
/// Class `0x1` ROM table entry register.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Class0x1RomEntry(pub u32);

impl Class0x1RomEntry {
    /// Returns the byte offset of entry `n` within a Class `0x1` ROM table.
    #[inline]
    pub const fn offset_of(n: u32) -> u32 {
        n * (size_of::<u32>() as u32)
    }

    /// Creates a `Class0x1RomEntry` from its raw 32-bit value.
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        Self(val)
    }

    /// Reads entry `n` from `io`.
    #[inline]
    pub fn read_at<Io: RegisterIo>(mut io: Io, n: u32) -> Self {
        Self(io.read32(Self::offset_of(n)))
    }

    /// Returns the raw 32-bit register value.
    #[inline]
    pub const fn reg_value(self) -> u32 {
        self.0
    }

    /// Returns the `offset` field (`[31:12]`).
    #[inline]
    pub const fn offset(self) -> u32 {
        self.0 >> 12
    }

    /// Sets the `offset` field (`[31:12]`).
    #[inline]
    pub const fn set_offset(&mut self, offset: u32) -> &mut Self {
        self.0 = (self.0 & 0xfff) | (offset << 12);
        self
    }

    /// Returns the `powerid` field (`[8:4]`).
    #[inline]
    pub const fn powerid(self) -> u8 {
        ((self.0 >> 4) & 0x1f) as u8
    }

    /// Sets the `powerid` field (`[8:4]`).
    #[inline]
    pub const fn set_powerid(&mut self, powerid: u8) -> &mut Self {
        self.0 = (self.0 & !(0x1f << 4)) | (((powerid as u32) & 0x1f) << 4);
        self
    }

    /// Returns the `powerid_valid` bit (`[2]`).
    #[inline]
    pub const fn powerid_valid(self) -> bool {
        ((self.0 >> 2) & 0x1) != 0
    }

    /// Sets the `powerid_valid` bit (`[2]`).
    #[inline]
    pub const fn set_powerid_valid(&mut self, valid: bool) -> &mut Self {
        if valid {
            self.0 |= 1 << 2;
        } else {
            self.0 &= !(1 << 2);
        }
        self
    }

    /// Returns the `format` bit (`[1]`).
    #[inline]
    pub const fn format(self) -> bool {
        ((self.0 >> 1) & 0x1) != 0
    }

    /// Sets the `format` bit (`[1]`).
    #[inline]
    pub const fn set_format(&mut self, format: bool) -> &mut Self {
        if format {
            self.0 |= 1 << 1;
        } else {
            self.0 &= !(1 << 1);
        }
        self
    }

    /// Returns the `present` bit (`[0]`).
    #[inline]
    pub const fn present(self) -> bool {
        (self.0 & 0x1) != 0
    }

    /// Sets the `present` bit (`[0]`).
    #[inline]
    pub const fn set_present(&mut self, present: bool) -> &mut Self {
        if present {
            self.0 |= 1;
        } else {
            self.0 &= !1;
        }
        self
    }
}

/// [CS] D7.5.17
/// Class `0x9` 32-bit ROM table entry register.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Class0x9Rom32BitEntry(pub u32);

impl Class0x9Rom32BitEntry {
    /// Returns the byte offset of entry `n` within a Class `0x9` 32-bit ROM table.
    #[inline]
    pub const fn offset_of(n: u32) -> u32 {
        n * (size_of::<u32>() as u32)
    }

    /// Creates a `Class0x9Rom32BitEntry` from its raw 32-bit value.
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        Self(val)
    }

    /// Reads entry `n` from `io`.
    #[inline]
    pub fn read_at<Io: RegisterIo>(mut io: Io, n: u32) -> Self {
        Self(io.read32(Self::offset_of(n)))
    }

    /// Returns the raw 32-bit register value.
    #[inline]
    pub const fn reg_value(self) -> u32 {
        self.0
    }

    /// Returns the `offset` field (`[31:12]`).
    #[inline]
    pub const fn offset(self) -> u32 {
        self.0 >> 12
    }

    /// Sets the `offset` field (`[31:12]`).
    #[inline]
    pub const fn set_offset(&mut self, offset: u32) -> &mut Self {
        self.0 = (self.0 & 0xfff) | (offset << 12);
        self
    }

    /// Returns the `powerid` field (`[8:4]`).
    #[inline]
    pub const fn powerid(self) -> u8 {
        ((self.0 >> 4) & 0x1f) as u8
    }

    /// Sets the `powerid` field (`[8:4]`).
    #[inline]
    pub const fn set_powerid(&mut self, powerid: u8) -> &mut Self {
        self.0 = (self.0 & !(0x1f << 4)) | (((powerid as u32) & 0x1f) << 4);
        self
    }

    /// Returns the `powerid_valid` bit (`[2]`).
    #[inline]
    pub const fn powerid_valid(self) -> bool {
        ((self.0 >> 2) & 0x1) != 0
    }

    /// Sets the `powerid_valid` bit (`[2]`).
    #[inline]
    pub const fn set_powerid_valid(&mut self, valid: bool) -> &mut Self {
        if valid {
            self.0 |= 1 << 2;
        } else {
            self.0 &= !(1 << 2);
        }
        self
    }

    /// Returns the 2-bit `present` field (`[1:0]`).
    #[inline]
    pub const fn present(self) -> u8 {
        (self.0 & 0x3) as u8
    }

    /// Sets the 2-bit `present` field (`[1:0]`).
    #[inline]
    pub const fn set_present(&mut self, present: u8) -> &mut Self {
        self.0 = (self.0 & !0x3) | ((present as u32) & 0x3);
        self
    }
}

/// [CS] D7.5.17
/// Class `0x9` 64-bit ROM table entry register.
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Class0x9Rom64BitEntry(pub u64);

impl Class0x9Rom64BitEntry {
    /// Returns the byte offset of entry `n` within a Class `0x9` 64-bit ROM table.
    #[inline]
    pub const fn offset_of(n: u32) -> u32 {
        n * (size_of::<u64>() as u32)
    }

    /// Creates a `Class0x9Rom64BitEntry` from its raw 64-bit value.
    #[inline]
    pub const fn from_raw(val: u64) -> Self {
        Self(val)
    }

    /// Reads entry `n` from `io`.
    #[inline]
    pub fn read_at<Io: RegisterIo>(mut io: Io, n: u32) -> Self {
        Self(io.read64(Self::offset_of(n)))
    }

    /// Returns the raw 64-bit register value.
    #[inline]
    pub const fn reg_value(self) -> u64 {
        self.0
    }

    /// Returns the `offset` field (`[63:12]`).
    #[inline]
    pub const fn offset(self) -> u64 {
        self.0 >> 12
    }

    /// Sets the `offset` field (`[63:12]`).
    #[inline]
    pub const fn set_offset(&mut self, offset: u64) -> &mut Self {
        self.0 = (self.0 & 0xfff) | (offset << 12);
        self
    }

    /// Returns the `powerid` field (`[8:4]`).
    #[inline]
    pub const fn powerid(self) -> u8 {
        ((self.0 >> 4) & 0x1f) as u8
    }

    /// Sets the `powerid` field (`[8:4]`).
    #[inline]
    pub const fn set_powerid(&mut self, powerid: u8) -> &mut Self {
        self.0 = (self.0 & !(0x1f << 4)) | (((powerid as u64) & 0x1f) << 4);
        self
    }

    /// Returns the `powerid_valid` bit (`[2]`).
    #[inline]
    pub const fn powerid_valid(self) -> bool {
        ((self.0 >> 2) & 0x1) != 0
    }

    /// Sets the `powerid_valid` bit (`[2]`).
    #[inline]
    pub const fn set_powerid_valid(&mut self, valid: bool) -> &mut Self {
        if valid {
            self.0 |= 1 << 2;
        } else {
            self.0 &= !(1 << 2);
        }
        self
    }

    /// Returns the 2-bit `present` field (`[1:0]`).
    #[inline]
    pub const fn present(self) -> u8 {
        (self.0 & 0x3) as u8
    }

    /// Sets the 2-bit `present` field (`[1:0]`).
    #[inline]
    pub const fn set_present(&mut self, present: u8) -> &mut Self {
        self.0 = (self.0 & !0x3) | ((present as u64) & 0x3);
        self
    }
}

/// Entry format encoded in `DEVID.FORMAT` (`Class0x9RomDeviceIdRegister::Format`).
#[repr(u8)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Class0x9RomEntryFormat {
    /// 32-bit ROM table entries (`0`).
    Bits32 = 0,
    /// 64-bit ROM table entries (`1`).
    Bits64 = 1,
}

static_assert!(size_of::<Class0x9RomEntryFormat>() == size_of::<u8>());
static_assert!(align_of::<Class0x9RomEntryFormat>() == align_of::<u8>());

impl TryFrom<u8> for Class0x9RomEntryFormat {
    type Error = u8;

    #[inline]
    fn try_from(val: u8) -> Result<Self, Self::Error> {
        match val {
            0 => Ok(Self::Bits32),
            1 => Ok(Self::Bits64),
            other => Err(other),
        }
    }
}

/// [CS] D7.5.10
/// Class `0x9` ROM table device ID register (`DEVID`).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
pub struct Class0x9RomDeviceIdRegister(pub u32);

impl Class0x9RomDeviceIdRegister {
    /// Byte offset of `DEVID` within a component page.
    pub const OFFSET: u32 = 0xfc8;

    /// Creates a `Class0x9RomDeviceIdRegister` from its raw 32-bit value.
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        Self(val)
    }

    /// Reads `DEVID` from `io`.
    #[inline]
    pub fn read_from<Io: RegisterIo>(mut io: Io) -> Self {
        Self(io.read32(Self::OFFSET))
    }

    /// Returns the raw 32-bit register value.
    #[inline]
    pub const fn reg_value(self) -> u32 {
        self.0
    }

    /// Returns the `prr` bit (`[5]`).
    #[inline]
    pub const fn prr(self) -> bool {
        ((self.0 >> 5) & 0x1) != 0
    }

    /// Sets the `prr` bit (`[5]`).
    #[inline]
    pub const fn set_prr(&mut self, prr: bool) -> &mut Self {
        if prr {
            self.0 |= 1 << 5;
        } else {
            self.0 &= !(1 << 5);
        }
        self
    }

    /// Returns the `sysmem` bit (`[4]`).
    #[inline]
    pub const fn sysmem(self) -> bool {
        ((self.0 >> 4) & 0x1) != 0
    }

    /// Sets the `sysmem` bit (`[4]`).
    #[inline]
    pub const fn set_sysmem(&mut self, sysmem: bool) -> &mut Self {
        if sysmem {
            self.0 |= 1 << 4;
        } else {
            self.0 &= !(1 << 4);
        }
        self
    }

    /// Returns the raw 4-bit `format` field (`[3:0]`).
    #[inline]
    pub const fn format_raw(self) -> u8 {
        (self.0 & 0xf) as u8
    }

    /// Returns the decoded [`Class0x9RomEntryFormat`] from `[3:0]`, or `Err(raw)` if
    /// unrecognized.
    #[inline]
    pub fn format(self) -> Result<Class0x9RomEntryFormat, u8> {
        Class0x9RomEntryFormat::try_from(self.format_raw())
    }

    /// Sets the `format` field (`[3:0]`).
    #[inline]
    pub const fn set_format(&mut self, format: Class0x9RomEntryFormat) -> &mut Self {
        self.0 = (self.0 & !0xf) | ((format as u32) & 0xf);
        self
    }
}

/// Represents an error that occurred while walking a ROM table.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct WalkError {
    /// Human-readable description of the walk error.
    pub reason: &'static str,
    /// Byte offset from the base ROM table at which the error occurred.
    pub offset: u32,
}

// Relativizes access with a fixed offset.
struct RelativizedIo<'a, Io: ?Sized> {
    io: &'a mut Io,
    offset: u32,
}

impl<'a, Io: RegisterIo + ?Sized> RelativizedIo<'a, Io> {
    #[inline]
    fn new(io: &'a mut Io, offset: u32) -> Self {
        Self { io, offset }
    }
}

impl<'a, Io: RegisterIo + ?Sized> RegisterIo for RelativizedIo<'a, Io> {
    #[inline]
    fn read32(&mut self, offset: u32) -> u32 {
        self.io.read32(offset.wrapping_add(self.offset))
    }

    #[inline]
    fn read64(&mut self, offset: u32) -> u64 {
        self.io.read64(offset.wrapping_add(self.offset))
    }

    #[inline]
    fn write32(&mut self, val: u32, offset: u32) {
        self.io.write32(val, offset.wrapping_add(self.offset));
    }

    #[inline]
    fn write64(&mut self, val: u64, offset: u32) {
        self.io.write64(val, offset.wrapping_add(self.offset));
    }
}

// There are several types of ROM table entry registers; this struct serves
// as unified front-end for accessing their contents.
struct EntryContents {
    value: u64,
    offset: u32,
    present: bool,
}

/// [CS] D5
/// A ROM table is a basic CoreSight component that provides pointers to other
/// components (including other ROM tables) in its lower registers via offsets
/// from its base address. It is an organizational structure that can be used to
/// find all CoreSight components - possibly as well as legacy or
/// vendor-specific ones - on an SoC. Thought of as a tree, the leaves are the
/// system's CoreSight components and the root is typically referred to as the
/// "base ROM table" (or, more plainly, "the ROM table").
pub struct RomTable;

impl RomTable {
    // [CS] D6.2.1, D7.2.1
    // The maximum number of ROM table entries, for various types.
    /// Maximum number of entries in a Class `0x1` ROM table.
    const MAX_0X1_ROM_ENTRIES: u32 = 960;
    /// Maximum number of entries in a Class `0x9` 32-bit ROM table.
    const MAX_0X9_ROM_32BIT_ENTRIES: u32 = 512;
    /// Maximum number of entries in a Class `0x9` 64-bit ROM table.
    const MAX_0X9_ROM_64BIT_ENTRIES: u32 = 256;

    /// Walks the underlying tree of components with no dynamic allocation,
    /// calling `callback` on the offset from the table's base address (implicitly
    /// encoded in `io`) of each component found. The (`io`, `max_offset`)
    /// together implicitly give the aperture to walk.
    ///
    /// The walk will visit and access the first page of memory of each found
    /// component. Unfortunately, however, there is no canonical means to
    /// determine how large a region of memory this entails. The determination of
    /// the maximum visited offset - or at least something deemed large enough -
    /// is left to the caller. The offset must at least be
    /// `MINIMUM_COMPONENT_SIZE`, which is the size of the base table proper.
    pub fn walk<Io, F>(mut io: Io, max_offset: u32, mut callback: F) -> Result<(), WalkError>
    where
        Io: RegisterIo,
        F: FnMut(u32),
    {
        static_assert!(size_of::<u32>() == 4);
        assert!((max_offset as usize) >= MINIMUM_COMPONENT_SIZE);
        Self::walk_from(&mut io, max_offset, &mut callback, 0)
    }

    #[inline]
    const fn is_table(classid: u8, architect: u16, archid: u16) -> bool {
        if classid == ClassId::RomTable0x1 as u8 {
            return true;
        }
        if classid == ClassId::CoreSight as u8
            && architect == arm::ARCHITECT
            && archid == arm::archid::ROM_TABLE
        {
            return true;
        }
        false
    }

    fn walk_from<Io, F>(
        root_io: &mut Io,
        max_offset: u32,
        callback: &mut F,
        offset: u32,
    ) -> Result<(), WalkError>
    where
        Io: RegisterIo + ?Sized,
        F: FnMut(u32),
    {
        if max_offset - (MINIMUM_COMPONENT_SIZE as u32) < offset {
            return Err(WalkError { reason: "component exceeds aperture", offset });
        }

        // Treats I/O as rooted at the current offset (instead of rooted at the base ROM table,
        // which would complicate register access).
        let mut io = RelativizedIo::new(root_io, offset);

        let classid = ComponentIdRegister::read_from(&mut io).classid_raw();
        let arch_reg = DeviceArchRegister::read_from(&mut io);
        let architect = arch_reg.architect();
        let archid = arch_reg.archid();
        if Self::is_table(classid, architect, archid) {
            let max_entries: u32;
            let mut format: Option<u8> = None;
            if classid == ClassId::RomTable0x1 as u8 {
                max_entries = Self::MAX_0X1_ROM_ENTRIES;
            } else {
                // If not a class 0x1 table, then a class 0x9.
                debug_assert_eq!(classid, ClassId::CoreSight as u8);
                format = Some(Class0x9RomDeviceIdRegister::read_from(&mut io).format_raw());
                match format.unwrap() {
                    x if x == Class0x9RomEntryFormat::Bits32 as u8 => {
                        max_entries = Self::MAX_0X9_ROM_32BIT_ENTRIES;
                    }
                    x if x == Class0x9RomEntryFormat::Bits64 as u8 => {
                        max_entries = Self::MAX_0X9_ROM_64BIT_ENTRIES;
                    }
                    _ => {
                        return Err(WalkError { reason: "bad format value", offset });
                    }
                }
            }

            for i in 0..max_entries {
                let contents = Self::read_entry(&mut io, i, classid, format)
                    .map_err(|reason| WalkError { reason, offset })?;
                if contents.value == 0 {
                    break; // Signals that the walk is over if identically zero.
                }
                if !contents.present {
                    continue;
                }
                // [CS] D5.4
                // the offset provided by the ROM table entry requires a shift of 12 bits.
                let new_offset = offset.wrapping_add(contents.offset.wrapping_shl(12));
                Self::walk_from(io.io, max_offset, callback, new_offset)?;
            }
            return Ok(());
        }

        // There should be a ROM table at offset zero.
        if offset == 0 {
            return Err(WalkError { reason: "not a ROM table", offset: 0 });
        }

        callback(offset);
        Ok(())
    }

    fn read_entry<Io: RegisterIo>(
        mut io: Io,
        n: u32,
        classid: u8,
        format: Option<u8>,
    ) -> Result<EntryContents, &'static str> {
        if classid == ClassId::RomTable0x1 as u8 {
            let entry = Class0x1RomEntry::read_at(&mut io, n);
            return Ok(EntryContents {
                value: u64::from(entry.reg_value()),
                offset: entry.offset(),
                present: entry.present(),
            });
        }

        // If not a class 0x1 table, then a class 0x9.
        debug_assert_eq!(classid, ClassId::CoreSight as u8);
        debug_assert!(format.is_some());

        match format.unwrap() {
            x if x == Class0x9RomEntryFormat::Bits32 as u8 => {
                let entry = Class0x9Rom32BitEntry::read_at(&mut io, n);
                Ok(EntryContents {
                    value: u64::from(entry.reg_value()),
                    offset: entry.offset(),
                    // [CS] D7.5.17: only a value of 0b11 for present() signifies presence.
                    present: (entry.present() & 0b11) != 0,
                })
            }
            x if x == Class0x9RomEntryFormat::Bits64 as u8 => {
                let entry = Class0x9Rom64BitEntry::read_at(&mut io, n);
                let u32_offset = entry.offset() & 0xffff_ffff;
                if entry.offset() != u32_offset {
                    return Err(
                        "a simplifying assumption is made that a ROM table entry's offset only \
                         contains 32 bits of information. If this is no longer true, please file \
                         a bug.",
                    );
                }
                Ok(EntryContents {
                    value: entry.reg_value(),
                    offset: u32_offset as u32,
                    present: (entry.present() & 0b11) != 0,
                })
            }
            _ => Err("bad format value"),
        }
    }
}

/// FFI trampoline for `coresight::RomTable::Walk`.
///
/// Returns `true` on success, or `false` on error after populating `out_error_reason_ptr`,
/// `out_error_reason_len`, and `out_error_offset`.
///
/// # Safety
///
/// - `read32` and `read64` must be valid function pointers safe to invoke with `io_ctx` for the
///   duration of the call.
/// - `callback` must be a valid function pointer safe to invoke with `cb_ctx` for the duration of
///   the call.
/// - `out_error_reason_ptr`, `out_error_reason_len`, and `out_error_offset` must be valid, aligned
///   pointers writable for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_coresight_rom_table_walk(
    io_ctx: *mut c_void,
    read32: unsafe extern "C" fn(*mut c_void, u32) -> u32,
    read64: unsafe extern "C" fn(*mut c_void, u32) -> u64,
    max_offset: u32,
    cb_ctx: *mut c_void,
    callback: unsafe extern "C" fn(*mut c_void, u32),
    out_error_reason_ptr: *mut *const c_char,
    out_error_reason_len: *mut usize,
    out_error_offset: *mut u32,
) -> bool {
    struct CppCallbackIo {
        ctx: *mut c_void,
        read32: unsafe extern "C" fn(*mut c_void, u32) -> u32,
        read64: unsafe extern "C" fn(*mut c_void, u32) -> u64,
    }

    impl RegisterIo for CppCallbackIo {
        #[inline]
        fn read32(&mut self, offset: u32) -> u32 {
            // SAFETY: The caller of `rust_coresight_rom_table_walk` guarantees that `read32` is a
            // valid function pointer safe to invoke with `self.ctx` (`io_ctx`).
            unsafe { (self.read32)(self.ctx, offset) }
        }

        #[inline]
        fn read64(&mut self, offset: u32) -> u64 {
            // SAFETY: The caller of `rust_coresight_rom_table_walk` guarantees that `read64` is a
            // valid function pointer safe to invoke with `self.ctx` (`io_ctx`).
            unsafe { (self.read64)(self.ctx, offset) }
        }

        #[inline]
        fn write32(&mut self, _val: u32, _offset: u32) {}

        #[inline]
        fn write64(&mut self, _val: u64, _offset: u32) {}
    }

    let io = CppCallbackIo { ctx: io_ctx, read32, read64 };
    let result = RomTable::walk(io, max_offset, |offset| {
        // SAFETY: The caller of `rust_coresight_rom_table_walk` guarantees that `callback` is a
        // valid function pointer safe to invoke with `cb_ctx`.
        unsafe { callback(cb_ctx, offset) };
    });

    match result {
        Ok(()) => true,
        Err(err) => {
            // SAFETY: Caller guarantees the output pointers are valid, aligned, and writable.
            unsafe {
                *out_error_reason_ptr = err.reason.as_ptr().cast::<c_char>();
                *out_error_reason_len = err.reason.len();
                *out_error_offset = err.offset;
            }
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLASS_0X1_ROM_COMPONENT_ID_REG: u32 = 0x0000_0010;
    const CLASS_0X9_ROM_COMPONENT_ID_REG: u32 = 0x0000_0090;

    const DEV_ID_REG: u32 = 0x0000_0000;

    const EMPTY_DEV_ARCH_REG: u32 = 0x0000_0000;
    const CLASS_0X9_ROM_TABLE_DEV_ARCH_REG: u32 = 0x4760_0af7;

    const EMPTY_ROM_ENTRY_REG: u32 = 0x0000_0000;
    const OFFSET_0X1000_CLASS_0X1_ROM_ENTRY_REG: u32 = 0x0000_1001;
    const OFFSET_0X2000_CLASS_0X9_ROM_ENTRY_REG: u32 = 0x0000_2011;
    const OFFSET_0X2000_NOT_PRESENT_CLASS_0X1_ROM_ENTRY_REG: u32 = 0x0000_2000;
    const OFFSET_0X3000_CLASS_0X1_ROM_ENTRY_REG: u32 = 0x0000_3001;
    const OFFSET_0X4000_CLASS_0X1_ROM_ENTRY_REG: u32 = 0x0000_4001;
    const OFFSET_0X5000_CLASS_0X1_ROM_ENTRY_REG: u32 = 0x0000_5001;
    const OFFSET_0XA000_CLASS_0X1_ROM_ENTRY_REG: u32 = 0x0000_a001;
    const OFFSET_0XFFFFF000_CLASS_0X1_ROM_ENTRY_REG: u32 = 0xffff_f001;

    #[derive(Copy, Clone, Debug, Eq, PartialEq)]
    enum ExpectedOp {
        None,
        Read32 { offset: u32, value: u32 },
        Read64 { offset: u32, value: u64 },
    }

    struct MockRegisterIo {
        expected: [ExpectedOp; 32],
        len: usize,
        cursor: usize,
    }

    impl MockRegisterIo {
        fn new() -> Self {
            Self { expected: [ExpectedOp::None; 32], len: 0, cursor: 0 }
        }

        fn expect_read(&mut self, value: u32, offset: u32) -> &mut Self {
            self.expected[self.len] = ExpectedOp::Read32 { offset, value };
            self.len += 1;
            self
        }

        fn expect_read64(&mut self, value: u64, offset: u32) -> &mut Self {
            self.expected[self.len] = ExpectedOp::Read64 { offset, value };
            self.len += 1;
            self
        }

        fn verify_and_clear(&mut self) {
            assert_eq!(self.cursor, self.len, "not all expected reads were consumed");
            self.len = 0;
            self.cursor = 0;
        }
    }

    impl RegisterIo for MockRegisterIo {
        fn read32(&mut self, offset: u32) -> u32 {
            assert!(self.cursor < self.len, "unexpected read32 at offset {offset:#x}");
            let op = self.expected[self.cursor];
            self.cursor += 1;
            match op {
                ExpectedOp::Read32 { offset: expected_offset, value } => {
                    assert_eq!(offset, expected_offset, "read32 offset mismatch");
                    value
                }
                ExpectedOp::Read64 { offset: expected_offset, .. } => {
                    panic!("expected read64 at {expected_offset:#x}, got read32 at {offset:#x}");
                }
                ExpectedOp::None => unreachable!(),
            }
        }

        fn read64(&mut self, offset: u32) -> u64 {
            assert!(self.cursor < self.len, "unexpected read64 at offset {offset:#x}");
            let op = self.expected[self.cursor];
            self.cursor += 1;
            match op {
                ExpectedOp::Read64 { offset: expected_offset, value } => {
                    assert_eq!(offset, expected_offset, "read64 offset mismatch");
                    value
                }
                ExpectedOp::Read32 { offset: expected_offset, .. } => {
                    panic!("expected read32 at {expected_offset:#x}, got read64 at {offset:#x}");
                }
                ExpectedOp::None => unreachable!(),
            }
        }

        fn write32(&mut self, _val: u32, _offset: u32) {}
        fn write64(&mut self, _val: u64, _offset: u32) {}
    }

    struct VisitedList {
        offsets: [u32; 8],
        len: usize,
    }

    impl VisitedList {
        const fn new() -> Self {
            Self { offsets: [0; 8], len: 0 }
        }

        fn push(&mut self, offset: u32) {
            self.offsets[self.len] = offset;
            self.len += 1;
        }

        fn as_slice(&self) -> &[u32] {
            &self.offsets[..self.len]
        }
    }

    #[test]
    fn empty_0x1_table() {
        let end_offset = 0x0000 + (MINIMUM_COMPONENT_SIZE as u32);
        let mut mock = MockRegisterIo::new();
        mock
            // Visit: Table (class 0x1)
            .expect_read(CLASS_0X1_ROM_COMPONENT_ID_REG, 0x0000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0x0000 + 0xfbc)
            // Read: Entry0 of Table (empty and last)
            .expect_read(EMPTY_ROM_ENTRY_REG, 0);

        let result = RomTable::walk(&mut mock, end_offset, |offset| {
            panic!("unexpected component found at offset {offset:#x}");
        });
        assert_eq!(result, Ok(()));
        mock.verify_and_clear();
    }

    #[test]
    fn empty_0x9_table() {
        let end_offset = 0x0000 + (MINIMUM_COMPONENT_SIZE as u32);
        let mut mock = MockRegisterIo::new();
        mock
            // Visit: Table (class 0x9)
            .expect_read(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x0000 + 0xff4)
            .expect_read(CLASS_0X9_ROM_TABLE_DEV_ARCH_REG, 0x0000 + 0xfbc)
            .expect_read(DEV_ID_REG, 0x0000 + 0xfc8)
            // Read: Entry0 of Table (empty and last)
            .expect_read(EMPTY_ROM_ENTRY_REG, 0);

        let result = RomTable::walk(&mut mock, end_offset, |offset| {
            panic!("unexpected component found at offset {offset:#x}");
        });
        assert_eq!(result, Ok(()));
        mock.verify_and_clear();
    }

    #[test]
    fn depth_one_references() {
        let mut mock = MockRegisterIo::new();
        mock
            // Visit: Table (class 0x1)
            .expect_read(CLASS_0X1_ROM_COMPONENT_ID_REG, 0x0000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0x0000 + 0xfbc)
            // Read: Entry0 of Table -> Component0
            .expect_read(OFFSET_0X1000_CLASS_0X1_ROM_ENTRY_REG, 0x0000)
            // Visit: Component0
            .expect_read(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x1000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0x1000 + 0xfbc)
            // Read: Entry1 of Table (not present)
            .expect_read(
                OFFSET_0X2000_NOT_PRESENT_CLASS_0X1_ROM_ENTRY_REG,
                0x0000 + (size_of::<u32>() as u32),
            )
            // Read: Entry2 of Table -> Component2
            .expect_read(
                OFFSET_0X3000_CLASS_0X1_ROM_ENTRY_REG,
                0x0000 + 2 * (size_of::<u32>() as u32),
            )
            // Visit: Component2
            .expect_read(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x3000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0x3000 + 0xfbc)
            // Read: Entry3 of Table (empty and last).
            .expect_read(EMPTY_ROM_ENTRY_REG, 0x0000 + 3 * (size_of::<u32>() as u32));

        let end_offset = 0x3000 + (MINIMUM_COMPONENT_SIZE as u32);
        let mut visited = VisitedList::new();
        let result = RomTable::walk(&mut mock, end_offset, |offset| visited.push(offset));
        assert_eq!(result, Ok(()));
        assert_eq!(visited.as_slice(), &[0x1000, 0x3000]);
        mock.verify_and_clear();
    }

    #[test]
    fn depth_two_references() {
        let u32_size = size_of::<u32>() as u32;
        let mut mock = MockRegisterIo::new();
        mock
            // Visit: Table (class 0x1)
            .expect_read(CLASS_0X1_ROM_COMPONENT_ID_REG, 0x0000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0x0000 + 0xfbc)
            // Read: Entry0 of Table -> Subtable0
            .expect_read(OFFSET_0X1000_CLASS_0X1_ROM_ENTRY_REG, 0x0000)
            // Visit: Subtable0 (class 0x1)
            .expect_read(CLASS_0X1_ROM_COMPONENT_ID_REG, 0x1000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0x1000 + 0xfbc)
            // Read: Entry0 of Subtable0 -> Component00
            .expect_read(OFFSET_0X1000_CLASS_0X1_ROM_ENTRY_REG, 0x1000)
            // Visit: Component00
            .expect_read(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x1000 + 0x1000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0x1000 + 0x1000 + 0xfbc)
            // Read: Entry1 of Subtable0 (not present)
            .expect_read(OFFSET_0X2000_NOT_PRESENT_CLASS_0X1_ROM_ENTRY_REG, 0x1000 + u32_size)
            // Read: Entry2 of Subtable0 -> Component02
            .expect_read(OFFSET_0X3000_CLASS_0X1_ROM_ENTRY_REG, 0x1000 + 2 * u32_size)
            // Visit: Component02
            .expect_read(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x1000 + 0x3000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0x1000 + 0x3000 + 0xfbc)
            // Read: Entry3 of Subtable0 (empty and last).
            .expect_read(EMPTY_ROM_ENTRY_REG, 0x1000 + 3 * u32_size)
            // Read: Entry1 of Table -> Component1
            .expect_read(OFFSET_0X5000_CLASS_0X1_ROM_ENTRY_REG, 0x0000 + u32_size)
            // Visit: Component1
            .expect_read(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x5000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0x5000 + 0xfbc)
            // Read: Entry2 of Table -> Subtable2
            .expect_read(OFFSET_0X4000_CLASS_0X1_ROM_ENTRY_REG, 0x0000 + 2 * u32_size)
            // Visit: Subtable2 (class 0x9)
            .expect_read(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x4000 + 0xff4)
            .expect_read(CLASS_0X9_ROM_TABLE_DEV_ARCH_REG, 0x4000 + 0xfbc)
            .expect_read(DEV_ID_REG, 0x4000 + 0xfc8)
            // Read: Entry0 of Subtable2 -> Component20
            .expect_read(OFFSET_0X2000_CLASS_0X9_ROM_ENTRY_REG, 0x4000)
            // Visit: Component2
            .expect_read(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x4000 + 0x2000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0x4000 + 0x2000 + 0xfbc)
            // Read: Entry1 of Subtable2 (empty and last)
            .expect_read(EMPTY_ROM_ENTRY_REG, 0x4000 + u32_size)
            // Read: Entry3 of Table (empty and last).
            .expect_read(EMPTY_ROM_ENTRY_REG, 3 * u32_size);

        let end_offset = 0x6000 + (MINIMUM_COMPONENT_SIZE as u32);
        let mut visited = VisitedList::new();
        let result = RomTable::walk(&mut mock, end_offset, |offset| visited.push(offset));
        assert_eq!(result, Ok(()));
        assert_eq!(visited.as_slice(), &[0x2000, 0x4000, 0x5000, 0x6000]);
        mock.verify_and_clear();
    }

    #[test]
    fn negative_offset() {
        // An encoded -4096 in two's complement form.
        // const COMP00_ENCODED_OFFSET: u32 = !1u32 + 1;

        let u32_size = size_of::<u32>() as u32;
        let mut mock = MockRegisterIo::new();
        mock
            // Visit: Table (class 0x1)
            .expect_read(CLASS_0X1_ROM_COMPONENT_ID_REG, 0x0000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0x0000 + 0xfbc)
            // Read: Entry0 of Table -> Subtable0
            .expect_read(OFFSET_0XA000_CLASS_0X1_ROM_ENTRY_REG, 0x0000)
            // Visit: Subtable0 (class 0x1)
            .expect_read(CLASS_0X1_ROM_COMPONENT_ID_REG, 0xa000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0xa000 + 0xfbc)
            // Read: Entry0 of Subtable0 -> Component00
            .expect_read(OFFSET_0XFFFFF000_CLASS_0X1_ROM_ENTRY_REG, 0xa000)
            // Visit: Component00
            .expect_read(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x9000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0x9000 + 0xfbc)
            // Read: Entry1 of Subtable0 (empty and last).
            .expect_read(EMPTY_ROM_ENTRY_REG, 0xa000 + u32_size)
            // Read: Entry1 of Table (empty and last).
            .expect_read(EMPTY_ROM_ENTRY_REG, u32_size);

        let end_offset = 0xa000 + (MINIMUM_COMPONENT_SIZE as u32);
        let mut visited = VisitedList::new();
        let result = RomTable::walk(&mut mock, end_offset, |offset| visited.push(offset));
        assert_eq!(result, Ok(()));
        assert_eq!(visited.as_slice(), &[0x9000]);
        mock.verify_and_clear();
    }

    #[test]
    fn class_0x9_64bit_entries_and_errors() {
        // 64-bit Class 0x9 table with one valid entry at offset 0x1000.
        let mut mock = MockRegisterIo::new();
        mock.expect_read(CLASS_0X9_ROM_COMPONENT_ID_REG, 0xff4)
            .expect_read(CLASS_0X9_ROM_TABLE_DEV_ARCH_REG, 0xfbc)
            .expect_read(0x0000_0001, 0xfc8) // Format::Bits64
            .expect_read64(0x0000_0000_0000_1003, 0)
            .expect_read(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x1000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0x1000 + 0xfbc)
            .expect_read64(0, 8);

        let mut visited = VisitedList::new();
        assert_eq!(
            RomTable::walk(&mut mock, 0x1000 + (MINIMUM_COMPONENT_SIZE as u32), |offset| {
                visited.push(offset)
            }),
            Ok(())
        );
        assert_eq!(visited.as_slice(), &[0x1000]);
        mock.verify_and_clear();

        // Not a ROM table at offset 0.
        mock.expect_read(CLASS_0X9_ROM_COMPONENT_ID_REG, 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0xfbc);
        assert_eq!(
            RomTable::walk(&mut mock, MINIMUM_COMPONENT_SIZE as u32, |_| {}),
            Err(WalkError { reason: "not a ROM table", offset: 0 })
        );
        mock.verify_and_clear();
    }

    #[test]
    fn ffi_trampolines_and_register_setters() {
        assert!(RomTable::is_table(ClassId::RomTable0x1 as u8, 0, 0));
        assert!(RomTable::is_table(
            ClassId::CoreSight as u8,
            arm::ARCHITECT,
            arm::archid::ROM_TABLE,
        ));
        assert!(!RomTable::is_table(ClassId::CoreSight as u8, arm::ARCHITECT, arm::archid::ETM4));
        assert!(!RomTable::is_table(0x5, arm::ARCHITECT, arm::archid::ROM_TABLE));

        let mut e0x1 = Class0x1RomEntry::default();
        e0x1.set_offset(1)
            .set_powerid(0)
            .set_powerid_valid(false)
            .set_format(false)
            .set_present(true);
        assert_eq!(e0x1.reg_value(), OFFSET_0X1000_CLASS_0X1_ROM_ENTRY_REG);

        let mut e0x9_32 = Class0x9Rom32BitEntry::default();
        e0x9_32.set_offset(2).set_powerid(1).set_powerid_valid(false).set_present(1);
        assert_eq!(e0x9_32.reg_value(), OFFSET_0X2000_CLASS_0X9_ROM_ENTRY_REG);

        let mut e0x9_64 = Class0x9Rom64BitEntry::default();
        e0x9_64.set_offset(1).set_powerid(0).set_powerid_valid(false).set_present(0b11);
        assert_eq!(e0x9_64.reg_value(), 0x0000_0000_0000_1003);

        let mut devid = Class0x9RomDeviceIdRegister::default();
        devid.set_prr(true).set_sysmem(true).set_format(Class0x9RomEntryFormat::Bits64);
        assert!(devid.prr());
        assert!(devid.sysmem());
        assert_eq!(devid.format(), Ok(Class0x9RomEntryFormat::Bits64));

        /// # Safety
        ///
        /// `ctx` must point to a valid `MockRegisterIo`.
        unsafe extern "C" fn mock_read32(ctx: *mut c_void, offset: u32) -> u32 {
            // SAFETY: `ctx` is a valid pointer to `MockRegisterIo`.
            unsafe { (*ctx.cast::<MockRegisterIo>()).read32(offset) }
        }
        /// # Safety
        ///
        /// `ctx` must point to a valid `MockRegisterIo`.
        unsafe extern "C" fn mock_read64(ctx: *mut c_void, offset: u32) -> u64 {
            // SAFETY: `ctx` is a valid pointer to `MockRegisterIo`.
            unsafe { (*ctx.cast::<MockRegisterIo>()).read64(offset) }
        }
        /// # Safety
        ///
        /// `ctx` must point to a valid `VisitedList`.
        unsafe extern "C" fn visit_cb(ctx: *mut c_void, offset: u32) {
            // SAFETY: `ctx` is a valid pointer to `VisitedList`.
            unsafe { (*ctx.cast::<VisitedList>()).push(offset) };
        }

        let mut mock = MockRegisterIo::new();
        mock.expect_read(CLASS_0X1_ROM_COMPONENT_ID_REG, 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0xfbc)
            .expect_read(OFFSET_0X1000_CLASS_0X1_ROM_ENTRY_REG, 0)
            .expect_read(CLASS_0X9_ROM_COMPONENT_ID_REG, 0x1000 + 0xff4)
            .expect_read(EMPTY_DEV_ARCH_REG, 0x1000 + 0xfbc)
            .expect_read(EMPTY_ROM_ENTRY_REG, 4);

        let mut visited = VisitedList::new();
        let mut err_ptr: *const c_char = core::ptr::null();
        let mut err_len: usize = 0;
        let mut err_offset: u32 = 0;
        // SAFETY: `mock` and `visited` outlive the call and match the callback casts above.
        let ok = unsafe {
            rust_coresight_rom_table_walk(
                (&raw mut mock).cast::<c_void>(),
                mock_read32,
                mock_read64,
                0x1000 + (MINIMUM_COMPONENT_SIZE as u32),
                (&raw mut visited).cast::<c_void>(),
                visit_cb,
                &mut err_ptr,
                &mut err_len,
                &mut err_offset,
            )
        };
        assert!(ok);
        assert_eq!(visited.as_slice(), &[0x1000]);
        mock.verify_and_clear();
    }
}
