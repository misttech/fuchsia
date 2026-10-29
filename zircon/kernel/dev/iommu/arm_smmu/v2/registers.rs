// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM SMMUv2 register layouts, typed bitfield accessors, and MMIO bank wrappers (`gr0`, `gr1`,
//! `s1cbr`, `s2cbr`).
//!
//! Defines [`SmmuMmio`], [`FieldDesc`], and the declarative [`define_smmu_reg!`] macro that stamps
//! out zero-cost `#[repr(transparent)]` register types backed by [`MmioBank`] and [`Offset`] in
//! kernel builds and by the `FakeSmmuMmioRouter` FFI bridge in host unit tests.

use core::mem::align_of;
#[cfg(not(is_kernel))]
use core::mem::size_of;
#[cfg(is_kernel)]
use regio::{MmioBank, MmioPtr};
use regio::{Offset, RwSafe};

#[cfg(not(is_kernel))]
use crate::arm_smmu::common::SmmuEnv;

/// Default size in bytes of an SMMUv2 MMIO register bank (64 KiB, the maximum SMMUv2 page size).
pub const SMMU_MMIO_DEFAULT_BANK_SIZE: usize = 0x10000;

/// MMIO register bank handle for an SMMUv2 register page (`GR0`, `GR1`, or `CBn`).
///
/// Wraps a mapped virtual base address (`0` when uninitialized or disabled) and bank size in
/// bytes. Kernel builds (`cfg(is_kernel)`) delegate directly to [`MmioBank`] (`MmioLe`) and
/// [`Offset`] volatile loads and stores, while host test builds (`cfg(not(is_kernel))`)
/// route accesses through `SmmuEnv::host_mmio_*` so the simulated hardware model can intercept
/// register reads, writes, and TLB synchronization operations.
///
/// # Invariants
/// Either `base == 0 && size == 0` (the null/disabled handle), or `[base, base + size)` is a
/// valid, 8-byte-aligned mapped SMMU MMIO region (or a valid host test MMIO region / buffer with
/// exposed provenance) that remains valid for volatile 32-bit and 64-bit register reads and writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SmmuMmio {
    base: usize,
    size: usize,
}

impl SmmuMmio {
    /// Returns a null (uninitialized/disabled) [`SmmuMmio`] handle (`base == 0`).
    #[inline]
    pub const fn null() -> Self {
        Self { base: 0, size: 0 }
    }

    /// Constructs an [`SmmuMmio`] handle for `base` with [`SMMU_MMIO_DEFAULT_BANK_SIZE`] bytes
    /// (or `0` bytes if `base == 0`).
    ///
    /// # Safety
    /// If `base != 0`, `[base, base + SMMU_MMIO_DEFAULT_BANK_SIZE)` must be a valid,
    /// 8-byte-aligned mapped MMIO region (or valid host test MMIO region / buffer with exposed
    /// provenance) for as long as the returned handle or any sub-bank derived from it is used.
    #[inline]
    pub const unsafe fn new(base: usize) -> Self {
        assert!(base.is_multiple_of(align_of::<u64>()), "SmmuMmio base must be 8-byte aligned");
        let size = if base == 0 { 0 } else { SMMU_MMIO_DEFAULT_BANK_SIZE };
        Self { base, size }
    }

    /// Constructs an [`SmmuMmio`] handle for `base` with an explicit `size` in bytes (or `0` bytes
    /// if `base == 0`).
    ///
    /// # Safety
    /// If `base != 0`, `[base, base + size)` must be a valid, 8-byte-aligned mapped MMIO region
    /// (or valid host test MMIO region / buffer with exposed provenance) for as long as the
    /// returned handle or any sub-bank derived from it is used.
    #[inline]
    pub const unsafe fn with_size(base: usize, size: usize) -> Self {
        assert!(base.is_multiple_of(align_of::<u64>()), "SmmuMmio base must be 8-byte aligned");
        let size = if base == 0 { 0 } else { size };
        Self { base, size }
    }

    /// Derives a bounds-checked sub-bank at `[offset, offset + size)` within `self`, or returns
    /// [`Self::null()`] if `self` is null.
    ///
    /// Panics if `self` is non-null and `[offset, offset + size)` exceeds `self.size` or is not
    /// 8-byte aligned.
    #[inline]
    pub const fn sub_bank(self, offset: usize, size: usize) -> Self {
        if self.base == 0 {
            Self::null()
        } else {
            assert!(offset <= self.size && size <= self.size - offset);
            let sub_base = self.base + offset;
            assert!(
                sub_base.is_multiple_of(align_of::<u64>()),
                "SmmuMmio sub-bank base must be 8-byte aligned"
            );
            Self { base: sub_base, size }
        }
    }

    /// Returns the virtual base address of this MMIO register bank (`0` if null).
    #[inline]
    pub const fn base(&self) -> usize {
        self.base
    }

    /// Returns the size in bytes of this MMIO register bank.
    #[inline]
    pub const fn size(&self) -> usize {
        self.size
    }

    /// Returns `true` if this MMIO handle is null (`base == 0`).
    #[inline]
    pub const fn is_null(&self) -> bool {
        self.base == 0
    }

    /// Reads a 32-bit register at `offset` within this MMIO bank.
    #[inline]
    pub fn read32(&self, offset: Offset<u32, RwSafe>) -> u32 {
        assert!(
            self.base != 0 && self.base.is_multiple_of(align_of::<u32>()),
            "MMIO read32 on null or unaligned SmmuMmio"
        );
        #[cfg(is_kernel)]
        {
            // SAFETY: By `SmmuMmio`'s type invariant and the assertion above, `self.base` is a
            // non-null, aligned pointer to a mapped SMMU MMIO region of `self.size` bytes, and
            // `MmioBank::at` verifies alignment and bounds of `offset`.
            unsafe {
                let ptr = MmioPtr::<u32, RwSafe>::new(
                    core::ptr::with_exposed_provenance_mut::<u32>(self.base),
                );
                let bank = MmioBank::new(ptr, self.size);
                bank.at(offset).read()
            }
        }
        #[cfg(not(is_kernel))]
        {
            assert!(offset.value.is_multiple_of(align_of::<u32>()));
            assert!(size_of::<u32>() <= self.size);
            assert!(offset.value <= self.size - size_of::<u32>());
            SmmuEnv::host_mmio_read32(self.base, offset.value as u32)
        }
    }

    /// Writes a 32-bit register value `val` at `offset` within this MMIO bank.
    #[inline]
    pub fn write32(&self, offset: Offset<u32, RwSafe>, val: u32) {
        assert!(
            self.base != 0 && self.base.is_multiple_of(align_of::<u32>()),
            "MMIO write32 on null or unaligned SmmuMmio"
        );
        #[cfg(is_kernel)]
        {
            // SAFETY: By `SmmuMmio`'s type invariant and the assertion above, `self.base` is a
            // non-null, aligned pointer to a mapped SMMU MMIO region of `self.size` bytes, and
            // `MmioBank::at` verifies alignment and bounds of `offset`.
            unsafe {
                let ptr = MmioPtr::<u32, RwSafe>::new(
                    core::ptr::with_exposed_provenance_mut::<u32>(self.base),
                );
                let bank = MmioBank::new(ptr, self.size);
                bank.at(offset).write(val);
            }
        }
        #[cfg(not(is_kernel))]
        {
            assert!(offset.value.is_multiple_of(align_of::<u32>()));
            assert!(size_of::<u32>() <= self.size);
            assert!(offset.value <= self.size - size_of::<u32>());
            SmmuEnv::host_mmio_write32(self.base, offset.value as u32, val);
        }
    }

    /// Reads a 64-bit register at `offset` within this MMIO bank.
    #[inline]
    pub fn read64(&self, offset: Offset<u64, RwSafe>) -> u64 {
        assert!(
            self.base != 0 && self.base.is_multiple_of(align_of::<u64>()),
            "MMIO read64 on null or unaligned SmmuMmio"
        );
        #[cfg(is_kernel)]
        {
            // SAFETY: By `SmmuMmio`'s type invariant and the assertion above, `self.base` is a
            // non-null, aligned pointer to a mapped SMMU MMIO region of `self.size` bytes, and
            // `MmioBank::at` verifies alignment and bounds of `offset`.
            unsafe {
                let ptr = MmioPtr::<u64, RwSafe>::new(
                    core::ptr::with_exposed_provenance_mut::<u64>(self.base),
                );
                let bank = MmioBank::new(ptr, self.size);
                bank.at(offset).read()
            }
        }
        #[cfg(not(is_kernel))]
        {
            assert!(offset.value.is_multiple_of(align_of::<u64>()));
            assert!(size_of::<u64>() <= self.size);
            assert!(offset.value <= self.size - size_of::<u64>());
            SmmuEnv::host_mmio_read64(self.base, offset.value as u32)
        }
    }

    /// Writes a 64-bit register value `val` at `offset` within this MMIO bank.
    #[inline]
    pub fn write64(&self, offset: Offset<u64, RwSafe>, val: u64) {
        assert!(
            self.base != 0 && self.base.is_multiple_of(align_of::<u64>()),
            "MMIO write64 on null or unaligned SmmuMmio"
        );
        #[cfg(is_kernel)]
        {
            // SAFETY: By `SmmuMmio`'s type invariant and the assertion above, `self.base` is a
            // non-null, aligned pointer to a mapped SMMU MMIO region of `self.size` bytes, and
            // `MmioBank::at` verifies alignment and bounds of `offset`.
            unsafe {
                let ptr = MmioPtr::<u64, RwSafe>::new(
                    core::ptr::with_exposed_provenance_mut::<u64>(self.base),
                );
                let bank = MmioBank::new(ptr, self.size);
                bank.at(offset).write(val);
            }
        }
        #[cfg(not(is_kernel))]
        {
            assert!(offset.value.is_multiple_of(align_of::<u64>()));
            assert!(size_of::<u64>() <= self.size);
            assert!(offset.value <= self.size - size_of::<u64>());
            SmmuEnv::host_mmio_write64(self.base, offset.value as u32, val);
        }
    }
}

/// Trait implemented by underlying register integer widths (`u32` and `u64`) for [`SmmuMmio`] I/O.
pub trait SmmuRegValue: Copy {
    /// Reads a raw register value at byte `offset` from `mmio`.
    fn read_mmio(mmio: &SmmuMmio, offset: u32) -> Self;

    /// Writes a raw register value `val` at byte `offset` to `mmio`.
    fn write_mmio(mmio: &SmmuMmio, offset: u32, val: Self);
}

impl SmmuRegValue for u32 {
    #[inline]
    fn read_mmio(mmio: &SmmuMmio, offset: u32) -> Self {
        mmio.read32(Offset::new(offset as usize))
    }

    #[inline]
    fn write_mmio(mmio: &SmmuMmio, offset: u32, val: Self) {
        mmio.write32(Offset::new(offset as usize), val);
    }
}

impl SmmuRegValue for u64 {
    #[inline]
    fn read_mmio(mmio: &SmmuMmio, offset: u32) -> Self {
        mmio.read64(Offset::new(offset as usize))
    }

    #[inline]
    fn write_mmio(mmio: &SmmuMmio, offset: u32, val: Self) {
        mmio.write64(Offset::new(offset as usize), val);
    }
}

/// Metadata describing a single register bitfield for debug console introspection (`DumpReg`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldDesc {
    /// Display name of the bitfield (empty `""` for `rsvdz` fields, `"__res..."` for non-zeroed
    /// reserved fields).
    pub name: &'static str,
    /// Most significant bit index (inclusive).
    pub msb: u32,
    /// Least significant bit index (inclusive).
    pub lsb: u32,
}

impl FieldDesc {
    /// Extracts this field's shifted unsigned value from `raw_reg_val`.
    #[inline]
    pub const fn extract(&self, raw_reg_val: u64) -> u64 {
        let width = (self.msb - self.lsb) + 1;
        let mask = if width >= 64 { u64::MAX } else { (1u64 << width) - 1 };
        (raw_reg_val >> self.lsb) & mask
    }
}

/// Client Port Disable (`CR0.CLIENTPD`) enumeration.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClientPd {
    /// Client transactions are subject to SMMU translation, access control, and attribute
    /// generation (`0`).
    #[default]
    Enabled = 0,
    /// Client transactions bypass the SMMU translation process (`1`).
    Disabled = 1,
}

impl ClientPd {
    /// Converts a raw 1-bit field value into a [`ClientPd`].
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        if (val & 1) == 0 { Self::Enabled } else { Self::Disabled }
    }
}

/// Stall Disable (`CR0.STALLD`) enumeration.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stalld {
    /// Per-context fault stalling is enabled (`0`).
    #[default]
    Enabled = 0,
    /// Per-context fault stalling is disabled (`1`).
    Disabled = 1,
}

impl Stalld {
    /// Converts a raw 1-bit field value into a [`Stalld`].
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        if (val & 1) == 0 { Self::Enabled } else { Self::Disabled }
    }
}

/// Extended Stream ID Enable (`CR0.EXIDENABLE`) enumeration.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExidEnable {
    /// Standard 15-bit Stream ID matching is used (`0`).
    #[default]
    Disabled = 0,
    /// Extended 16-bit Stream ID matching is enabled (`1`).
    Enabled = 1,
}

impl ExidEnable {
    /// Converts a raw 1-bit field value into an [`ExidEnable`].
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        if (val & 1) == 0 { Self::Disabled } else { Self::Enabled }
    }
}

/// Stream-to-Context Register (`S2CRn.TYPE`) operating mode enumeration.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum S2crType {
    /// Translation context: transactions matched by this group are routed to `S2CRn.CBNDX` (`0`).
    #[default]
    Translation = 0,
    /// Bypass context: transactions matched by this group bypass translation (`1`).
    Bypass = 1,
    /// Fault context: transactions matched by this group generate an Invalid Context Fault (`2`).
    Fault = 2,
    /// Reserved / invalid encoding (`3`).
    Invalid = 3,
}

impl S2crType {
    /// Converts a raw 2-bit `S2CRn.TYPE` value into an [`S2crType`].
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        match val & 0x3 {
            0 => Self::Translation,
            1 => Self::Bypass,
            2 => Self::Fault,
            _ => Self::Invalid,
        }
    }

    /// Returns the diagnostic string representation of this [`S2crType`].
    #[inline]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Translation => "Translation",
            Self::Bypass => "Bypass",
            Self::Fault => "Fault",
            Self::Invalid => "Invalid",
        }
    }
}

/// Stream-to-Context Register (`S2CRn.PRIVCFG`) privilege configuration enumeration.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum S2crPrivCfg {
    /// Use incoming transaction privilege attribute (`0`).
    #[default]
    Default = 0,
    /// Reserved encoding (`1`), mapped to Default (`1`).
    Reserved1 = 1,
    /// Force unprivileged access (`2`).
    Unprivileged = 2,
    /// Force privileged access (`3`).
    Privileged = 3,
}

impl S2crPrivCfg {
    /// Converts a raw 2-bit `S2CRn.PRIVCFG` value into an [`S2crPrivCfg`].
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        match val & 0x3 {
            0 => Self::Default,
            1 => Self::Reserved1,
            2 => Self::Unprivileged,
            _ => Self::Privileged,
        }
    }
}

/// Context Bank Attribute Register (`CBARn.TYPE`) context bank type enumeration.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CbarType {
    /// Stage 2 translation only (`0`).
    #[default]
    S2Translation = 0,
    /// Stage 1 translation with Stage 2 bypass (`1`).
    S1TS2Bypass = 1,
    /// Stage 1 translation with Stage 2 fault (`2`).
    S1TS2Fault = 2,
    /// Stage 1 translation followed by Stage 2 translation (`3`).
    S1TS2Translate = 3,
}

impl CbarType {
    /// Converts a raw 2-bit `CBARn.TYPE` value into a [`CbarType`].
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        match val & 0x3 {
            0 => Self::S2Translation,
            1 => Self::S1TS2Bypass,
            2 => Self::S1TS2Fault,
            _ => Self::S1TS2Translate,
        }
    }

    /// Returns the diagnostic string representation of this [`CbarType`].
    #[inline]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::S2Translation => "Translate: Stage 2 Context",
            Self::S1TS2Bypass => "Translate: Stage 1 Context with Stage 2 Bypass",
            Self::S1TS2Fault => "Translate: Stage 1 Context with Stage 2 Fault",
            Self::S1TS2Translate => "Translate: Stage 1 Context with Stage 2 Translate",
        }
    }
}

/// Context Bank Attribute Register 2 (`CBA2Rn.VA64`) virtual address width selector.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Cba2rVa64 {
    /// AArch32 translation scheme (`0`).
    #[default]
    Va32 = 0,
    /// AArch64 VMSA64 translation scheme (`1`).
    Va64 = 1,
}

impl Cba2rVa64 {
    /// Converts a raw 1-bit `CBA2Rn.VA64` value into a [`Cba2rVa64`].
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        if (val & 1) == 0 { Self::Va32 } else { Self::Va64 }
    }
}

/// Translation Control Register (`TCR_64Bit.TG0` / `TG1`) translation granule size selector.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TcrTg0 {
    /// 4 KiB translation granule (`0b00`).
    #[default]
    Granule4KB = 0,
    /// 64 KiB translation granule (`0b01`).
    Granule64KB = 1,
    /// 16 KiB translation granule (`0b10`).
    Granule16KB = 2,
    /// Reserved encoding (`0b11`).
    Reserved = 3,
}

impl TcrTg0 {
    /// Converts a raw 2-bit `TG0` value into a [`TcrTg0`].
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        match val & 0x3 {
            0 => Self::Granule4KB,
            1 => Self::Granule64KB,
            2 => Self::Granule16KB,
            _ => Self::Reserved,
        }
    }
}

/// Translation Control Register (`TCR_64Bit.TG1`) TTBR1 granule size selector.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TcrTg1 {
    /// Reserved encoding (`0b00`).
    #[default]
    Reserved0 = 0,
    /// 16 KiB translation granule (`0b01`).
    Granule16KB = 1,
    /// 4 KiB translation granule (`0b10`).
    Granule4KB = 2,
    /// 64 KiB translation granule (`0b11`).
    Granule64KB = 3,
}

impl TcrTg1 {
    /// Converts a raw 2-bit `TG1` value into a [`TcrTg1`].
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        match val & 0x3 {
            0 => Self::Reserved0,
            1 => Self::Granule16KB,
            2 => Self::Granule4KB,
            _ => Self::Granule64KB,
        }
    }
}

/// Translation Control Register (`TCR_64Bit.SH0` / `SH1`) shareability attribute.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TcrSh0 {
    /// Non-shareable (`0b00`).
    #[default]
    NonShareable = 0,
    /// Reserved encoding (`0b01`).
    Reserved = 1,
    /// Outer Shareable (`0b10`).
    OuterShareable = 2,
    /// Inner Shareable (`0b11`).
    InnerShareable = 3,
}

impl TcrSh0 {
    /// Converts a raw 2-bit `SH0`/`SH1` value into a [`TcrSh0`].
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        match val & 0x3 {
            0 => Self::NonShareable,
            1 => Self::Reserved,
            2 => Self::OuterShareable,
            _ => Self::InnerShareable,
        }
    }
}

/// Translation Control Register (`TCR_64Bit.ORGN0` / `ORGN1`) outer cacheability attribute.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TcrOrg0 {
    /// Normal memory, Outer Non-cacheable (`0b00`).
    #[default]
    NonCacheable = 0,
    /// Normal memory, Outer Write-Back Read-Allocate Write-Allocate Cacheable (`0b01`).
    WriteBackWriteAllocate = 1,
    /// Normal memory, Outer Write-Through Read-Allocate No Write-Allocate Cacheable (`0b10`).
    WriteThrough = 2,
    /// Normal memory, Outer Write-Back Read-Allocate No Write-Allocate Cacheable (`0b11`).
    WriteBackNoWriteAllocate = 3,
}

impl TcrOrg0 {
    /// Converts a raw 2-bit `ORGN0`/`ORGN1` value into a [`TcrOrg0`].
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        match val & 0x3 {
            0 => Self::NonCacheable,
            1 => Self::WriteBackWriteAllocate,
            2 => Self::WriteThrough,
            _ => Self::WriteBackNoWriteAllocate,
        }
    }
}

/// Translation Control Register (`TCR_64Bit.IRGN0` / `IRGN1`) inner cacheability attribute.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TcrIrg0 {
    /// Normal memory, Inner Non-cacheable (`0b00`).
    #[default]
    NonCacheable = 0,
    /// Normal memory, Inner Write-Back Read-Allocate Write-Allocate Cacheable (`0b01`).
    WriteBackWriteAllocate = 1,
    /// Normal memory, Inner Write-Through Read-Allocate No Write-Allocate Cacheable (`0b10`).
    WriteThrough = 2,
    /// Normal memory, Inner Write-Back Read-Allocate No Write-Allocate Cacheable (`0b11`).
    WriteBackNoWriteAllocate = 3,
}

impl TcrIrg0 {
    /// Converts a raw 2-bit `IRGN0`/`IRGN1` value into a [`TcrIrg0`].
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        match val & 0x3 {
            0 => Self::NonCacheable,
            1 => Self::WriteBackWriteAllocate,
            2 => Self::WriteThrough,
            _ => Self::WriteBackNoWriteAllocate,
        }
    }
}

/// Translation Control Register (`TCR2.PASize` / `TCR_64Bit.IPS`) intermediate physical address
/// size selector.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TcrIps {
    /// 32 bits, 4 GiB (`0b000`).
    #[default]
    Bits32 = 0,
    /// 36 bits, 64 GiB (`0b001`).
    Bits36 = 1,
    /// 40 bits, 1 TiB (`0b010`).
    Bits40 = 2,
    /// 42 bits, 4 TiB (`0b011`).
    Bits42 = 3,
    /// 44 bits, 16 TiB (`0b100`).
    Bits44 = 4,
    /// 48 bits, 256 TiB (`0b101`).
    Bits48 = 5,
    /// Reserved encoding (`0b110`).
    Reserved6 = 6,
    /// Reserved encoding (`0b111`).
    Reserved7 = 7,
}

impl TcrIps {
    /// Converts a raw 3-bit `IPS`/`PASize` value into a [`TcrIps`].
    #[inline]
    pub const fn from_raw(val: u32) -> Self {
        match val & 0x7 {
            0 => Self::Bits32,
            1 => Self::Bits36,
            2 => Self::Bits40,
            3 => Self::Bits42,
            4 => Self::Bits44,
            5 => Self::Bits48,
            6 => Self::Reserved6,
            _ => Self::Reserved7,
        }
    }
}

/// Declarative macro for defining ARM SMMUv2 MMIO registers, bitfields, RSVDZ masks, and console
/// field metadata tables.
///
/// # Overview
///
/// `define_smmu_reg!` generates a `#[repr(transparent)]` wrapper struct over `u32` or `u64` that
/// implements `LayoutOver` (`Copy + From<Base> + Into<Base>`), typed bitfield getters and
/// chainable `&mut Self` setters, reserved-as-zero (`[rsvdz]`) write masking, and a static
/// `FIELDS: &'static [FieldDesc]` slice used by the `smmu dumpregs` / `smmu show -v` kernel
/// console commands.
///
/// Two addressing forms are supported:
/// 1. **Fixed-offset registers**: `pub struct RegName($val_ty) @ $offset_expr => { ... }`
/// 2. **Indexed register arrays**:
///    `pub struct RegName($val_ty) @ [$ndx:ident : u32] $offset_expr => { ... }`
///
/// Four field kinds are supported inside the `{ ... }` body:
/// 1. **Integer bit or multi-bit field**:
///    `msb:lsb => getter_name, set_name ["DISPLAY_NAME"]: u32;` (or `u64;`).
/// 2. **Boolean single-bit flag**:
///    `bit:bit => getter_name, set_name ["DISPLAY_NAME"]: bool;`.
/// 3. **Typed enum field**:
///    `msb:lsb => getter_name, set_name ["DISPLAY_NAME"]: enum EnumType;` (where `EnumType`
///    provides `const fn from_raw(u32) -> EnumType` and `#[repr(u32)]`).
/// 4. **Reserved-as-zero (`[rsvdz]`) field**:
///    `msb:lsb => [rsvdz];`. No getter or setter is emitted; the bit range is accumulated into
///    `Self::RSVDZ_MASK` and automatically cleared before every `write_to` / `write_at` MMIO
///    store, and its [`FieldDesc`] entry carries an empty `name: ""` so `DumpReg` omits it.
///
/// # Concrete Examples
///
/// ## Example 1: Fixed-offset register with integer, boolean, enum, and `[rsvdz]` fields
///
/// ```ignore
/// define_smmu_reg! {
///     /// Global Configuration Register 2 (`SMMU_sCR2`).
///     pub struct Cr2(u32) @ 0x008 => {
///         31:31 => exsmrgenable, set_exsmrgenable ["EXSMRGENABLE"]: u32;
///         30:30 => [rsvdz];
///         29:29 => compindexenable, set_compindexenable ["COMPINDEXENABLE"]: u32;
///         26:16 => [rsvdz];
///         7:0   => bpvmid, set_bpvmid ["BPVMID"]: u32;
///     }
/// }
/// ```
///
/// This expands to:
/// - `#[repr(transparent)] pub struct Cr2(u32);` implementing `Clone`, `Copy`, `Debug`, `Default`,
///   `PartialEq`, `Eq`, `From<u32>`, and `From<Cr2> for u32`.
/// - `pub const OFFSET: u32 = 0x008;` and
///   `pub const REGIO_OFFSET: Offset<Self, RwSafe> = Offset::new(0x008);`
/// - `pub const RSVDZ_MASK: u32 = (1 << 30) | (0x7ff << 16);`
/// - `pub const FIELDS: &'static [FieldDesc] = &[ ... ];`
/// - `pub const fn from_value(val: u32) -> Self` and `pub const fn reg_value(&self) -> u32`
/// - `pub fn read_from(mmio: &SmmuMmio) -> Self` and
///   `pub fn write_to(&mut self, mmio: &SmmuMmio) -> &mut Self` (clearing `RSVDZ_MASK` before
///   writing to `mmio`).
/// - `pub const fn exsmrgenable(&self) -> u32`,
///   `pub fn set_exsmrgenable(&mut self, val: u32) -> &mut Self`, etc.
///
/// ## Example 2: Indexed register array (`SMRn`, `S2CRn`, `CBARn`) with typed enum field
///
/// ```ignore
/// define_smmu_reg! {
///     /// Stream-to-Context Register (`SMMU_S2CRn`).
///     pub struct S2cr(u32) @ [ndx: u32] 0xc00 + (ndx << 2) => {
///         17:16 => s2cr_type, set_s2cr_type ["TYPE"]: enum S2crType;
///     }
/// }
/// ```
///
/// This expands to:
/// - `pub const fn offset(ndx: u32) -> u32 { 0xc00 + (ndx << 2) }` and
///   `pub const fn regio_offset(ndx: u32) -> Offset<Self, RwSafe>`
/// - `pub fn read_at(mmio: &SmmuMmio, ndx: u32) -> Self` and
///   `pub fn write_at(&mut self, mmio: &SmmuMmio, ndx: u32) -> &mut Self`
/// - `pub const fn s2cr_type(&self) -> S2crType` and
///   `pub fn set_s2cr_type(&mut self, val: S2crType) -> &mut Self`.
macro_rules! define_smmu_reg {
    // =========================================================================
    // Arm 1: Indexed register array definition (`@ [$ndx:ident : u32] $offset:expr`)
    // =========================================================================
    // Matches a register family indexed by `ndx: u32` (such as `SMRn`, `S2CRn`, `CBARn`,
    // `CBFRSYNRAn`, `CBA2Rn`). Generates `offset(ndx)`, `regio_offset(ndx)`, `read_at(mmio, ndx)`,
    // and `write_at(mmio, ndx)` in place of a fixed `OFFSET`.
    (
        $(#[$doc:meta])*
        $vis:vis struct $name:ident ($val_ty:ty) @ [$ndx:ident : u32] $offset:expr => {
            $(
                $msb:literal : $lsb:literal =>
                    $( [$rsvdz:ident] )?
                    $( $get:ident, $set:ident [$disp:literal] : $($field_ty:ident)+ )?
            );* $(;)?
        }
    ) => {
        $(#[$doc])*
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        $vis struct $name($val_ty);

        impl From<$val_ty> for $name {
            #[inline]
            fn from(val: $val_ty) -> Self {
                Self(val)
            }
        }

        impl From<$name> for $val_ty {
            #[inline]
            fn from(reg: $name) -> Self {
                reg.0
            }
        }

        impl $name {
            /// Computes the byte offset of element `$ndx` within its MMIO bank.
            #[inline]
            pub const fn offset($ndx: u32) -> u32 {
                $offset
            }

            /// Computes the typed [`Offset`] of element `ndx` within its MMIO bank.
            #[inline]
            pub const fn regio_offset(ndx: u32) -> Offset<Self, RwSafe> {
                Offset::new(Self::offset(ndx) as usize)
            }

            /// Bitmask of reserved-as-zero (`[rsvdz]`) bit positions that are automatically
            /// cleared before writing this register to MMIO.
            pub const RSVDZ_MASK: $val_ty = 0 $(
                | define_smmu_reg!(
                    @rsvdz_mask $val_ty, $msb, $lsb,
                    $( [$rsvdz] )?
                    $( $get, $set [$disp] : $($field_ty)+ )?
                )
            )*;

            /// Static bitfield descriptor table used by `DumpReg` in the kernel debug console.
            pub const FIELDS: &'static [FieldDesc] = &[
                $(
                    define_smmu_reg!(
                        @field_desc $msb, $lsb,
                        $( [$rsvdz] )?
                        $( $get, $set [$disp] : $($field_ty)+ )?
                    ),
                )*
            ];

            /// Constructs a register value wrapper from a raw integer `val`.
            #[inline]
            pub const fn from_value(val: $val_ty) -> Self {
                Self(val)
            }

            /// Returns the raw integer value stored in this register snapshot.
            #[inline]
            pub const fn reg_value(&self) -> $val_ty {
                self.0
            }

            /// Replaces the raw integer value stored in this register snapshot.
            #[inline]
            pub fn set_reg_value(&mut self, val: $val_ty) -> &mut Self {
                self.0 = val;
                self
            }

            /// Reads element `ndx` of this register array from `mmio`.
            #[inline]
            pub fn read_at(mmio: &SmmuMmio, ndx: u32) -> Self {
                Self(<$val_ty as SmmuRegValue>::read_mmio(mmio, Self::offset(ndx)))
            }

            /// Clears any [`Self::RSVDZ_MASK`] bits and writes element `ndx` of this register
            /// array to `mmio`.
            #[inline]
            pub fn write_at(&mut self, mmio: &SmmuMmio, ndx: u32) -> &mut Self {
                let masked = self.0 & !Self::RSVDZ_MASK;
                <$val_ty as SmmuRegValue>::write_mmio(mmio, Self::offset(ndx), masked);
                self
            }

            $(
                define_smmu_reg!(
                    @field_methods $val_ty, $msb, $lsb,
                    $( [$rsvdz] )?
                    $( $get, $set [$disp] : $($field_ty)+ )?
                );
            )*
        }
    };

    // =========================================================================
    // Arm 2: Fixed-offset register definition (`@ $offset:expr`)
    // =========================================================================
    // Generates the `#[repr(transparent)]` wrapper type, `From`/`Into` conversions required by
    // `LayoutOver`, `OFFSET` and `REGIO_OFFSET` constants, `RSVDZ_MASK`, `FIELDS` metadata
    // slice, `read_from` / `write_to` MMIO methods, and all bitfield getters/setters.
    (
        $(#[$doc:meta])*
        $vis:vis struct $name:ident ($val_ty:ty) @ $offset:expr => {
            $(
                $msb:literal : $lsb:literal =>
                    $( [$rsvdz:ident] )?
                    $( $get:ident, $set:ident [$disp:literal] : $($field_ty:ident)+ )?
            );* $(;)?
        }
    ) => {
        $(#[$doc])*
        #[repr(transparent)]
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        $vis struct $name($val_ty);

        impl From<$val_ty> for $name {
            #[inline]
            fn from(val: $val_ty) -> Self {
                Self(val)
            }
        }

        impl From<$name> for $val_ty {
            #[inline]
            fn from(reg: $name) -> Self {
                reg.0
            }
        }

        impl $name {
            /// Byte offset of this register within its MMIO bank.
            pub const OFFSET: u32 = $offset;

            /// Typed [`Offset`] of this register within its MMIO bank.
            pub const REGIO_OFFSET: Offset<Self, RwSafe> = Offset::new(Self::OFFSET as usize);

            /// Bitmask of reserved-as-zero (`[rsvdz]`) bit positions that are automatically
            /// cleared before writing this register to MMIO.
            pub const RSVDZ_MASK: $val_ty = 0 $(
                | define_smmu_reg!(
                    @rsvdz_mask $val_ty, $msb, $lsb,
                    $( [$rsvdz] )?
                    $( $get, $set [$disp] : $($field_ty)+ )?
                )
            )*;

            /// Static bitfield descriptor table used by `DumpReg` in the kernel debug console.
            pub const FIELDS: &'static [FieldDesc] = &[
                $(
                    define_smmu_reg!(
                        @field_desc $msb, $lsb,
                        $( [$rsvdz] )?
                        $( $get, $set [$disp] : $($field_ty)+ )?
                    ),
                )*
            ];

            /// Constructs a register value wrapper from a raw integer `val`.
            #[inline]
            pub const fn from_value(val: $val_ty) -> Self {
                Self(val)
            }

            /// Returns the raw integer value stored in this register snapshot.
            #[inline]
            pub const fn reg_value(&self) -> $val_ty {
                self.0
            }

            /// Replaces the raw integer value stored in this register snapshot.
            #[inline]
            pub fn set_reg_value(&mut self, val: $val_ty) -> &mut Self {
                self.0 = val;
                self
            }

            /// Returns the byte offset (`reg_addr`) of this register within its MMIO bank.
            #[inline]
            pub const fn reg_addr(&self) -> u32 {
                Self::OFFSET
            }

            /// Reads this register from `mmio` at [`Self::OFFSET`].
            #[inline]
            pub fn read_from(mmio: &SmmuMmio) -> Self {
                Self(<$val_ty as SmmuRegValue>::read_mmio(mmio, Self::OFFSET))
            }

            /// Clears any [`Self::RSVDZ_MASK`] bits and writes this register to `mmio` at
            /// [`Self::OFFSET`].
            #[inline]
            pub fn write_to(&mut self, mmio: &SmmuMmio) -> &mut Self {
                let masked = self.0 & !Self::RSVDZ_MASK;
                <$val_ty as SmmuRegValue>::write_mmio(mmio, Self::OFFSET, masked);
                self
            }

            $(
                define_smmu_reg!(
                    @field_methods $val_ty, $msb, $lsb,
                    $( [$rsvdz] )?
                    $( $get, $set [$disp] : $($field_ty)+ )?
                );
            )*
        }
    };

    // =========================================================================
    // Internal Helper Rules: `@bit_mask`
    // =========================================================================
    // Computes the unshifted bitmask `((1 << (msb - lsb + 1)) - 1)` for an inclusive bit range
    // `[msb:lsb]` in `$val_ty` (`u32` or `u64`) at compile time without overflow when the field
    // spans the full register width (`width == $val_ty::BITS`).
    (@bit_mask $val_ty:ty, $msb:literal, $lsb:literal) => {{
        assert!($msb >= $lsb, "Upper bit must be >= lower bit");
        assert!($msb < <$val_ty>::BITS, "Upper bit exceeds register width");
        let width: u32 = ($msb - $lsb) + 1;
        if width == <$val_ty>::BITS {
            <$val_ty>::MAX
        } else {
            ((1 as $val_ty) << width) - 1
        }
    }};

    // =========================================================================
    // Internal Helper Rules: `@rsvdz_mask`
    // =========================================================================
    // Evaluates to the shifted bitmask `MASK << lsb` for `[rsvdz]` fields, and `0` for all
    // normal readable/writable fields.
    (@rsvdz_mask $val_ty:ty, $msb:literal, $lsb:literal, [rsvdz]) => {
        define_smmu_reg!(@bit_mask $val_ty, $msb, $lsb) << $lsb
    };
    (@rsvdz_mask $val_ty:ty, $msb:literal, $lsb:literal, $($other:tt)+) => {
        0 as $val_ty
    };

    // =========================================================================
    // Internal Helper Rules: `@field_desc`
    // =========================================================================
    // Builds a `FieldDesc` entry for `Self::FIELDS`. Reserved-zero (`[rsvdz]`) fields receive an
    // empty `name: ""` so `DumpReg` skips them during both width calculation and field printing,
    // matching `hwreg`'s `nullptr` name behavior.
    (@field_desc $msb:literal, $lsb:literal, [rsvdz]) => {
        FieldDesc { name: "", msb: $msb, lsb: $lsb }
    };
    (
        @field_desc
        $msb:literal,
        $lsb:literal,
        $get:ident,
        $set:ident
        [$disp:literal] : $($rest:tt)+
    ) => {
        FieldDesc { name: $disp, msb: $msb, lsb: $lsb }
    };

    // =========================================================================
    // Internal Helper Rules: `@field_methods`
    // =========================================================================
    // Generates typed getter and setter methods for each field kind:
    // 1. `[rsvdz]`: no getter/setter (`RSVDZ_MASK` already validates the bit range).
    // 2. `: bool`: emits `pub const fn $get(&self) -> bool` and
    //    `pub fn $set(&mut self, val: bool) -> &mut Self`.
    // 3. `: enum $enum_ty`: emits `pub const fn $get(&self) -> $enum_ty` (via `$enum_ty::from_raw`)
    //    and `pub fn $set(&mut self, val: $enum_ty) -> &mut Self`.
    // 4. `: $field_ty` (`u32` or `u64`): emits `pub const fn $get(&self) -> $val_ty` and
    //    `pub fn $set(&mut self, val: $val_ty) -> &mut Self` with a `debug_assert!` verifying that
    //    `val` fits within the field width.
    (@field_methods $val_ty:ty, $msb:literal, $lsb:literal, [rsvdz]) => {};
    (
        @field_methods
        $val_ty:ty,
        $msb:literal,
        $lsb:literal,
        $get:ident,
        $set:ident
        [$disp:literal] : bool
    ) => {
        /// Extracts boolean bitfield `$disp`.
        #[inline]
        pub const fn $get(&self) -> bool {
            const _ASSERT_SINGLE_BIT: () =
                assert!($msb == $lsb, "Boolean register field must be a single bit");
            const _ASSERT_IN_BOUNDS: () =
                assert!($msb < <$val_ty>::BITS, "Bit index exceeds register width");
            ((self.0 >> $lsb) & 1) != 0
        }

        /// Updates boolean bitfield `$disp` in place.
        #[inline]
        pub fn $set(&mut self, val: bool) -> &mut Self {
            let bit: $val_ty = (1 as $val_ty) << $lsb;
            if val {
                self.0 |= bit;
            } else {
                self.0 &= !bit;
            }
            self
        }
    };
    (
        @field_methods
        $val_ty:ty,
        $msb:literal,
        $lsb:literal,
        $get:ident,
        $set:ident
        [$disp:literal] : enum $enum_ty:ty
    ) => {
        /// Extracts typed enum bitfield `$disp`.
        #[inline]
        pub const fn $get(&self) -> $enum_ty {
            const MASK: $val_ty = define_smmu_reg!(@bit_mask $val_ty, $msb, $lsb);
            let raw = (self.0 >> $lsb) & MASK;
            <$enum_ty>::from_raw(raw as u32)
        }

        /// Updates typed enum bitfield `$disp` in place.
        #[inline]
        pub fn $set(&mut self, val: $enum_ty) -> &mut Self {
            const MASK: $val_ty = define_smmu_reg!(@bit_mask $val_ty, $msb, $lsb);
            let raw = (val as u32) as $val_ty;
            // Temporary porting note: like hwreg, this debug-asserts that `val` fits in the
            // field. In builds without that assert hwreg ORs an oversized value in unmasked, so
            // it spills into the fields above; here it is masked to the width of the field.
            // The register value therefore differs from the C++ only when a caller passes a
            // value too wide for the field, and no caller does: the one place the C++ did so
            // (the SMR ID in `StreamMatchRegGroup::enable_for_context_bank`, see the note
            // there) now passes just the ID and writes the same register value. Were it to
            // happen, both would write a wrong value; here the damage is confined to the one
            // field instead of also corrupting its neighbors, which is at least as good.
            debug_assert!(raw.checked_shr(($msb - $lsb) + 1).unwrap_or(0) == 0);
            self.0 = (self.0 & !(MASK << $lsb)) | ((raw & MASK) << $lsb);
            self
        }
    };
    (
        @field_methods
        $val_ty:ty,
        $msb:literal,
        $lsb:literal,
        $get:ident,
        $set:ident
        [$disp:literal] : $field_ty:ty
    ) => {
        /// Extracts integer bitfield `$disp` (`bits [$msb:$lsb]`).
        #[inline]
        pub const fn $get(&self) -> $val_ty {
            const MASK: $val_ty = define_smmu_reg!(@bit_mask $val_ty, $msb, $lsb);
            (self.0 >> $lsb) & MASK
        }

        /// Updates integer bitfield `$disp` (`bits [$msb:$lsb]`) in place.
        #[inline]
        pub fn $set(&mut self, val: $val_ty) -> &mut Self {
            const MASK: $val_ty = define_smmu_reg!(@bit_mask $val_ty, $msb, $lsb);
            // Temporary porting note: like hwreg, this debug-asserts that `val` fits in the
            // field. In builds without that assert hwreg ORs an oversized value in unmasked, so
            // it spills into the fields above; here it is masked to the width of the field.
            // The register value therefore differs from the C++ only when a caller passes a
            // value too wide for the field, and no caller does: the one place the C++ did so
            // (the SMR ID in `StreamMatchRegGroup::enable_for_context_bank`, see the note
            // there) now passes just the ID and writes the same register value. Were it to
            // happen, both would write a wrong value; here the damage is confined to the one
            // field instead of also corrupting its neighbors, which is at least as good.
            debug_assert!(val.checked_shr(($msb - $lsb) + 1).unwrap_or(0) == 0);
            self.0 = (self.0 & !(MASK << $lsb)) | ((val & MASK) << $lsb);
            self
        }
    };
}

/// Global Register Space 0 (`SMMU_GR0_BASE`) register definitions.
pub mod gr0 {
    use super::*;
    pub use super::{ClientPd, ExidEnable, S2crPrivCfg, S2crType, Stalld};

    define_smmu_reg! {
        /// Configuration Register 0 (`SMMU_sCR0`, offset `0x00`).
        pub struct Cr0(u32) @ 0x00 => {
            31:31 => vmid16en, set_vmid16en ["VMID16EN"]: u32;
            30:30 => hypmode, set_hypmode ["HYPMODE"]: u32;
            29:28 => nscfg, set_nscfg ["NSCFG"]: u32;
            27:26 => wacfg, set_wacfg ["WACFG"]: u32;
            25:24 => racfg, set_racfg ["RACFG"]: u32;
            23:22 => shcfg, set_shcfg ["SHCFG"]: u32;
            21:21 => smcfcfg, set_smcfcfg ["SMCFCFG"]: u32;
            20:20 => mtcfg, set_mtcfg ["MTCFG"]: u32;
            19:16 => mem_attr, set_mem_attr ["MemAttr"]: u32;
            15:14 => bsu, set_bsu ["BSU"]: u32;
            13:13 => fb, set_fb ["FB"]: u32;
            11:11 => vmidpne, set_vmidpne ["VMIDPNE"]: u32;
            12:12 => ptm, set_ptm ["PTM"]: u32;
            10:10 => usfcfg, set_usfcfg ["USFCFG"]: u32;
            9:9   => gse, set_gse ["GSE"]: u32;
            8:8   => stalld, set_stalld ["STALLD"]: enum Stalld;
            7:6   => transientcfg, set_transientcfg ["TRANSIENTCFG"]: u32;
            5:5   => gcfgfie, set_gcfgfie ["GCFGFIE"]: u32;
            4:4   => gcfgfre, set_gcfgfre ["GCFGFRE"]: u32;
            3:3   => exidenable, set_exidenable ["EXIDENABLE"]: enum ExidEnable;
            2:2   => gfie, set_gfie ["GFIE"]: u32;
            1:1   => gfre, set_gfre ["GFRE"]: u32;
            0:0   => clientpd, set_clientpd ["CLIENTPD"]: enum ClientPd;
        }
    }

    define_smmu_reg! {
        /// Configuration Register 2 (`SMMU_sCR2`, offset `0x08`).
        pub struct Cr2(u32) @ 0x08 => {
            31:31 => exsmrgenable, set_exsmrgenable ["EXSMRGENABLE"]: u32;
            30:30 => [rsvdz];
            29:29 => compindexenable, set_compindexenable ["COMPINDEXENABLE"]: u32;
            26:16 => [rsvdz];
            7:0   => bpvmid, set_bpvmid ["BPVMID"]: u32;
        }
    }

    define_smmu_reg! {
        /// Auxiliary Configuration Register (`SMMU_sACR`, offset `0x10`).
        pub struct Acr(u32) @ 0x10 => {
            31:0 => impl_val, set_impl_val ["Impl"]: u32;
        }
    }

    define_smmu_reg! {
        /// Identification Register 0 (`SMMU_IDR0`, offset `0x20`).
        pub struct Idr0(u32) @ 0x20 => {
            31:31 => ses, set_ses ["SES"]: u32;
            30:30 => s1ts, set_s1ts ["S1TS"]: u32;
            29:29 => s2ts, set_s2ts ["S2TS"]: u32;
            28:28 => nts, set_nts ["NTS"]: u32;
            27:27 => sms, set_sms ["SMS"]: u32;
            26:26 => atosns, set_atosns ["ATOSNS"]: u32;
            25:24 => ptfs, set_ptfs ["PTFS"]: u32;
            23:16 => numirpt, set_numirpt ["NUMIRPT"]: u32;
            15:15 => exsmrgs, set_exsmrgs ["EXSMRGS"]: u32;
            14:14 => cttw, set_cttw ["CTTW"]: u32;
            13:13 => btm, set_btm ["BTM"]: u32;
            12:9  => numsidb, set_numsidb ["NUMSIDB"]: u32;
            8:8   => exids, set_exids ["EXIDS"]: u32;
            7:0   => numsmrg, set_numsmrg ["NUMSMRG"]: u32;
        }
    }

    define_smmu_reg! {
        /// Identification Register 1 (`SMMU_IDR1`, offset `0x24`).
        pub struct Idr1(u32) @ 0x24 => {
            31:31 => pagesize, set_pagesize ["PAGESIZE"]: u32;
            30:28 => numpagendxb, set_numpagendxb ["NUMPAGENDXB"]: u32;
            25:24 => hafdbs, set_hafdbs ["HAFDBS"]: u32;
            23:16 => nums2cb, set_nums2cb ["NUMS2CB"]: u32;
            15:15 => smcd, set_smcd ["SMCD"]: u32;
            13:12 => ssdtp, set_ssdtp ["SSDTP"]: u32;
            11:8  => numssdndxb, set_numssdndxb ["NUMSSDNDXB"]: u32;
            7:0   => numcb, set_numcb ["NUMCB"]: u32;
        }
    }

    define_smmu_reg! {
        /// Identification Register 2 (`SMMU_IDR2`, offset `0x28`).
        pub struct Idr2(u32) @ 0x28 => {
            30:30 => dipans, set_dipans ["DIPANS"]: u32;
            29:29 => compindexs, set_compindexs ["COMPINDEXS"]: u32;
            28:28 => hads, set_hads ["HADS"]: u32;
            27:27 => e2hs, set_e2hs ["E2HS"]: u32;
            26:16 => exnumsmgr, set_exnumsmgr ["EXNUMSMGR"]: u32;
            15:15 => vmid16s, set_vmid16s ["VMID16S"]: u32;
            14:14 => ptfsv8_64kb, set_ptfsv8_64kb ["PTFSv8_64kb"]: u32;
            13:13 => ptfsv8_16kb, set_ptfsv8_16kb ["PTFSv8_16kb"]: u32;
            12:12 => ptfsv8_4kb, set_ptfsv8_4kb ["PTFSv8_4kb"]: u32;
            11:8  => ubs, set_ubs ["UBS"]: u32;
            7:4   => oas, set_oas ["OAS"]: u32;
            3:0   => ias, set_ias ["IAS"]: u32;
        }
    }

    define_smmu_reg! {
        /// Identification Register 7 (`SMMU_IDR7`, offset `0x3c`).
        pub struct Idr7(u32) @ 0x3c => {
            7:4 => major, set_major ["major"]: u32;
            3:0 => minor, set_minor ["minor"]: u32;
        }
    }

    // Note: the address in the fault address register has N active bits, where N is determined by
    // the value of IDR2.UBS (upstream bus size).
    define_smmu_reg! {
        /// Global Fault Address Register (`SMMU_sGFAR`, offset `0x40`).
        pub struct Gfar(u64) @ 0x40 => {
            63:0 => faddr, set_faddr ["FADDR"]: u64;
        }
    }

    define_smmu_reg! {
        /// Global Fault Status Register (`SMMU_sGFSR`, offset `0x48`).
        pub struct Gfsr(u32) @ 0x48 => {
            31:31 => multi, set_multi ["MULTI"]: u32;
            8:8   => uut, set_uut ["UUT"]: u32;
            7:7   => pf, set_pf ["PF"]: u32;
            6:6   => ef, set_ef ["EF"]: u32;
            5:5   => caf, set_caf ["CAF"]: u32;
            4:4   => ucif, set_ucif ["UCIF"]: u32;
            3:3   => ucbf, set_ucbf ["UCBF"]: u32;
            2:2   => smcf, set_smcf ["SMCF"]: u32;
            1:1   => usf, set_usf ["USF"]: u32;
            0:0   => icf, set_icf ["ICF"]: u32;
        }
    }

    define_smmu_reg! {
        /// Global Fault Status Restore Register (`SMMU_sGFSRRESTORE`, offset `0x4c`).
        pub struct GfsrRestore(u32) @ 0x4c => {
            31:31 => multi, set_multi ["MULTI"]: u32;
            8:8   => uut, set_uut ["UUT"]: u32;
            7:7   => pf, set_pf ["PF"]: u32;
            6:6   => ef, set_ef ["EF"]: u32;
            5:5   => caf, set_caf ["CAF"]: u32;
            4:4   => ucif, set_ucif ["UCIF"]: u32;
            3:3   => ucbf, set_ucbf ["UCBF"]: u32;
            2:2   => smcf, set_smcf ["SMCF"]: u32;
            1:1   => usf, set_usf ["USF"]: u32;
            0:0   => icf, set_icf ["ICF"]: u32;
        }
    }

    define_smmu_reg! {
        /// Global Fault Syndrome Register 0 (`SMMU_sGFSYNR0`, offset `0x50`).
        pub struct Gfsynr0(u32) @ 0x50 => {
            15:8 => impl_val, set_impl_val ["Impl"]: u32;
            6:6  => ats, set_ats ["ATS"]: u32;
            5:5  => nsattr, set_nsattr ["NSATTR"]: u32;
            4:4  => nsstate, set_nsstate ["NSSTATE"]: u32;
            3:3  => ind, set_ind ["IND"]: u32;
            2:2  => pnu, set_pnu ["PNU"]: u32;
            1:1  => wnr, set_wnr ["WNR"]: u32;
            0:0  => nested, set_nested ["Nested"]: u32;
        }
    }

    define_smmu_reg! {
        /// Global Fault Syndrome Register 1 (`SMMU_sGFSYNR1`, offset `0x54`).
        pub struct Gfsynr1(u32) @ 0x54 => {
            31:16 => ssd_index, set_ssd_index ["SSD_Index"]: u32;
            15:0  => stream_id, set_stream_id ["StreamID"]: u32;
        }
    }

    define_smmu_reg! {
        /// Global Fault Syndrome Register 2 (`SMMU_sGFSYNR2`, offset `0x58`).
        pub struct Gfsynr2(u32) @ 0x58 => {
            31:0 => impl_val, set_impl_val ["Impl"]: u32;
        }
    }

    define_smmu_reg! {
        /// Secure TLB Invalidate All (`SMMU_STLBIALL`, offset `0x60`).
        pub struct Stlbiall(u32) @ 0x60 => {
            0:0 => trigger, set_trigger ["Trigger"]: u32;
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate by VMID (`SMMU_TLBIVMID`, offset `0x64`).
        pub struct Tlbivmid(u32) @ 0x64 => {
            0:0 => trigger, set_trigger ["Trigger"]: u32;
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate All Non-Secure Non-Hyp (`SMMU_TLBIALLNSNH`, offset `0x68`).
        pub struct Tlbiallnsnh(u32) @ 0x68 => {
            0:0 => trigger, set_trigger ["Trigger"]: u32;
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate All Hyp (`SMMU_TLBIALLH`, offset `0x6c`).
        pub struct Tlbiallh(u32) @ 0x6c => {
            0:0 => trigger, set_trigger ["Trigger"]: u32;
        }
    }

    define_smmu_reg! {
        /// Global Synchronize TLB Invalidate (`SMMU_sTLBGSYNC`, offset `0x70`).
        pub struct Tlbgsync(u32) @ 0x70 => {
            0:0 => trigger, set_trigger ["Trigger"]: u32;
        }
    }

    define_smmu_reg! {
        /// Global TLB Status Register (`SMMU_sTLBGSTATUS`, offset `0x74`).
        pub struct Tlbgstatus(u32) @ 0x74 => {
            0:0 => gsactive, set_gsactive ["GSACTIVE"]: u32;
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate by VA, Hyp (`SMMU_TLBIIVAH`, offset `0x78`).
        pub struct Tlbiivah(u32) @ 0x78 => {
            31:12 => address, set_address ["Address"]: u32;
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate by VA, Last Level, EL3 (`SMMU_TLBIIVALM`, offset `0xa0`).
        pub struct Tlbiivalm(u64) @ 0xa0 => {
            43:0 => address, set_address ["Address"]: u64;
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate by VA, EL3 (`SMMU_TLBIIVAM`, offset `0xa8`).
        pub struct Tlbiivam(u64) @ 0xa8 => {
            43:0 => address, set_address ["Address"]: u64;
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate by VA, Last Level, Hyp 64-bit (`SMMU_TLBIIVALH64`, offset `0xb0`).
        pub struct Tlbiivalh64(u64) @ 0xb0 => {
            43:0 => address, set_address ["Address"]: u64;
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate Stage 1 by VMID (`SMMU_TLBIIVMIDS1`, offset `0xb8`).
        pub struct Tlbiivmids1(u32) @ 0xb8 => {
            31:12 => address, set_address ["Address"]: u32;
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate All EL3 (`SMMU_TLBIIALLM`, offset `0xbc`).
        pub struct Tlbiiallm(u32) @ 0xbc => {
            0:0 => trigger, set_trigger ["Trigger"]: u32;
        }
    }

    /// Alias for [`Tlbiiallm`].
    pub type Tlbiallm = Tlbiiallm;

    define_smmu_reg! {
        /// TLB Invalidate by VA, Hyp 64-bit (`SMMU_TLBIIVAH64`, offset `0xc0`).
        pub struct Tlbiivah64(u64) @ 0xc0 => {
            43:0 => address, set_address ["Address"]: u64;
        }
    }

    define_smmu_reg! {
        /// Global Address Translation Stage 1 Unprivileged Read (`SMMU_GATS1UR`, offset `0x100`).
        pub struct Gats1ur(u64) @ 0x100 => {
            63:12 => addr, set_addr ["Addr"]: u64;
            7:0   => ndx, set_ndx ["NDX"]: u64;
        }
    }

    define_smmu_reg! {
        /// Global Address Translation Stage 1 Unprivileged Write (`SMMU_GATS1UW`, offset `0x108`).
        pub struct Gats1uw(u64) @ 0x108 => {
            63:12 => addr, set_addr ["Addr"]: u64;
            7:0   => ndx, set_ndx ["NDX"]: u64;
        }
    }

    define_smmu_reg! {
        /// Global Address Translation Stage 1 Privileged Read (`SMMU_GATS1PR`, offset `0x110`).
        pub struct Gats1pr(u64) @ 0x110 => {
            63:12 => addr, set_addr ["Addr"]: u64;
            7:0   => ndx, set_ndx ["NDX"]: u64;
        }
    }

    define_smmu_reg! {
        /// Global Address Translation Stage 1 Privileged Write (`SMMU_GATS1PW`, offset `0x118`).
        pub struct Gats1pw(u64) @ 0x118 => {
            63:12 => addr, set_addr ["Addr"]: u64;
            7:0   => ndx, set_ndx ["NDX"]: u64;
        }
    }

    define_smmu_reg! {
        /// Global Address Translation Stage 1+2 Unprivileged Read (`SMMU_GATS12UR`, offset
        /// `0x120`).
        pub struct Gats12ur(u64) @ 0x120 => {
            63:12 => addr, set_addr ["Addr"]: u64;
            7:0   => ndx, set_ndx ["NDX"]: u64;
        }
    }

    define_smmu_reg! {
        /// Global Address Translation Stage 1+2 Unprivileged Write (`SMMU_GATS12UW`, offset
        /// `0x128`).
        pub struct Gats12uw(u64) @ 0x128 => {
            63:12 => addr, set_addr ["Addr"]: u64;
            7:0   => ndx, set_ndx ["NDX"]: u64;
        }
    }

    define_smmu_reg! {
        /// Global Address Translation Stage 1+2 Privileged Read (`SMMU_GATS12PR`, offset `0x130`).
        pub struct Gats12pr(u64) @ 0x130 => {
            63:12 => addr, set_addr ["Addr"]: u64;
            7:0   => ndx, set_ndx ["NDX"]: u64;
        }
    }

    define_smmu_reg! {
        /// Global Address Translation Stage 1+2 Privileged Write (`SMMU_GATS12PW`, offset `0x138`).
        pub struct Gats12pw(u64) @ 0x138 => {
            63:12 => addr, set_addr ["Addr"]: u64;
            7:0   => ndx, set_ndx ["NDX"]: u64;
        }
    }

    define_smmu_reg! {
        /// Global Physical Address Register (`SMMU_sGPAR`, offset `0x180`).
        pub struct Gpar(u64) @ 0x180 => {
            63:56 => mattr, set_mattr ["MATTR"]: u64;
            47:12 => pa, set_pa ["PA"]: u64;
            10:10 => imp, set_imp ["IMP"]: u64;
            9:9   => ns, set_ns ["NS"]: u64;
            8:7   => sh, set_sh ["SH"]: u64;
            0:0   => f, set_f ["F"]: u64;
        }
    }

    define_smmu_reg! {
        /// Global Address Translation Status Register (`SMMU_sGATSR`, offset `0x188`).
        pub struct Gatsr(u32) @ 0x188 => {
            0:0 => active, set_active ["ACTIVE"]: u32;
        }
    }

    // There may be up to 128 stream match registers in the implementation, numbered as
    // [SMR0 .. SMR127]. SMR registers are a part of a stream match register group; read
    // IDR0.NUMSMRG to determine the total number of stream match register groups in the system.
    define_smmu_reg! {
        /// Stream Match Register (`SMMU_SMRn`, offset `0x800 + 4 * ndx`).
        pub struct Smr(u32) @ [ndx: u32] 0x800 + (ndx << 2) => {
            31:31 => valid, set_valid ["VALID"]: u32;
            30:16 => mask, set_mask ["MASK"]: u32;
            14:0  => id, set_id ["ID"]: u32;
        }
    }

    // There may be up to 128 stream-to-context registers in the implementation, numbered as
    // [S2CR0 .. S2CR127]. Use `read_at(mmio, ndx)` / `write_at(&mut self, mmio, ndx)` to access
    // the proper index at runtime.
    //
    // S2CR registers are a part of a stream match register group. Read `IDR0.NUMSMRG` to determine
    // the total number of stream match register groups in the system.
    //
    // Additionally, there are 3 different encodings for the fields of the S2CR registers,
    // depending on the value present in `TYPE`. They are:
    // + TYPE = 0 : This is a "Translation" context (`S2crType0` / `S2crTranslation`).
    // + TYPE = 1 : This entry is in "Bypass mode", no translation takes place (`S2crType1` /
    //              `S2crBypass`).
    // + TYPE = 2 : This is a "Fault" context (`S2crType2` / `S2crFault`). Any transaction that
    //              maps to this stream mapping group incurs an invalid context fault.
    //
    // When reading, callers may use the base `S2cr` version of the register to perform the read
    // and examine the `s2cr_type` field. Afterwards, they can instantiate an alternate definition
    // of the register based on the type in order to access type-specific fields.
    //
    // When writing, callers may directly use the properly typed version, but they are responsible
    // for correctly populating the `TYPE` field (`set_s2cr_type`). It will *not* be done
    // automatically.
    define_smmu_reg! {
        /// Stream-to-Context Register base layout (`SMMU_S2CRn`, offset `0xc00 + 4 * ndx`).
        pub struct S2cr(u32) @ [ndx: u32] 0xc00 + (ndx << 2) => {
            17:16 => s2cr_type, set_s2cr_type ["TYPE"]: enum S2crType;
        }
    }

    define_smmu_reg! {
        /// Stream-to-Context Register Translation layout (`SMMU_S2CRn` when `TYPE == Translation`).
        pub struct S2crType0(u32) @ [ndx: u32] 0xc00 + (ndx << 2) => {
            31:30 => impl_val, set_impl_val ["Impl"]: u32;
            29:28 => transientcfg, set_transientcfg ["TRANSIENTCFG"]: u32;
            27:26 => instcfg, set_instcfg ["INSTCFG"]: u32;
            25:24 => privcfg, set_privcfg ["PRIVCFG"]: enum S2crPrivCfg;
            23:22 => wacfg, set_wacfg ["WACFG"]: u32;
            21:20 => racfg, set_racfg ["RACFG"]: u32;
            19:18 => nscfg, set_nscfg ["NSCFG"]: u32;
            17:16 => s2cr_type, set_s2cr_type ["TYPE"]: enum S2crType;
            15:12 => mem_attr, set_mem_attr ["MemAttr"]: u32;
            11:11 => mtcfg, set_mtcfg ["MTCFG"]: u32;
            10:10 => exidvalid, set_exidvalid ["EXIDVALID"]: u32;
            9:8   => shcfg, set_shcfg ["SHCFG"]: u32;
            7:0   => cbndx, set_cbndx ["CBNDX"]: u32;
        }
    }

    define_smmu_reg! {
        /// Stream-to-Context Register Bypass layout (`SMMU_S2CRn` when `TYPE == Bypass`).
        pub struct S2crType1(u32) @ [ndx: u32] 0xc00 + (ndx << 2) => {
            31:30 => impl_val, set_impl_val ["Impl"]: u32;
            29:28 => transientcfg, set_transientcfg ["TRANSIENTCFG"]: u32;
            26:26 => fb, set_fb ["FB"]: u32;
            25:24 => bsu, set_bsu ["BSU"]: u32;
            23:22 => wacfg, set_wacfg ["WACFG"]: u32;
            21:20 => racfg, set_racfg ["RACFG"]: u32;
            19:18 => nscfg, set_nscfg ["NSCFG"]: u32;
            17:16 => s2cr_type, set_s2cr_type ["TYPE"]: enum S2crType;
            15:12 => mem_attr, set_mem_attr ["MemAttr"]: u32;
            11:11 => mtcfg, set_mtcfg ["MTCFG"]: u32;
            10:10 => exidvalid, set_exidvalid ["EXIDVALID"]: u32;
            9:8   => shcfg, set_shcfg ["SHCFG"]: u32;
            7:0   => vmid, set_vmid ["VMID"]: u32;
        }
    }

    define_smmu_reg! {
        /// Stream-to-Context Register Fault layout (`SMMU_S2CRn` when `TYPE == Fault`).
        pub struct S2crType2(u32) @ [ndx: u32] 0xc00 + (ndx << 2) => {
            31:28 => impl_val, set_impl_val ["Impl"]: u32;
            17:16 => s2cr_type, set_s2cr_type ["TYPE"]: enum S2crType;
            10:10 => exidvalid, set_exidvalid ["EXIDVALID"]: u32;
        }
    }

    /// Type alias for [`S2crType0`] (`TYPE == Translation`).
    pub type S2crTranslation = S2crType0;
    /// Type alias for [`S2crType1`] (`TYPE == Bypass`).
    pub type S2crBypass = S2crType1;
    /// Type alias for [`S2crType2`] (`TYPE == Fault`).
    pub type S2crFault = S2crType2;
}

/// Global Register Space 1 (`SMMU_GR1_BASE`) register definitions.
pub mod gr1 {
    use super::*;
    pub use super::{Cba2rVa64, CbarType};

    // Section 10.2.1: Context Bank Attribute Registers (`SMMU_CBARn`).
    // The field definitions in a CBAR depend on the value of the TYPE field in the register.
    define_smmu_reg! {
        /// Context Bank Attribute Register base layout (`SMMU_CBARn`, offset `4 * ndx`).
        pub struct Cbar(u32) @ [ndx: u32] ndx << 2 => {
            17:16 => cbar_type, set_cbar_type ["TYPE"]: enum CbarType;
        }
    }

    // Type 0 CBARs are "Stage 2 Context" registers.
    // (Note: Stage 2 Context CBARs only define `VMID`, `TYPE`, `rsvdz [19:18]`, and `IRPTNDX`.)
    define_smmu_reg! {
        /// Stage 2 Context Bank Attribute Register (`SMMU_CBARn` Type 0).
        pub struct CbarType0(u32) @ [ndx: u32] ndx << 2 => {
            7:0   => vmid, set_vmid ["VMID"]: u32;
            17:16 => cbar_type, set_cbar_type ["TYPE"]: enum CbarType;
            19:18 => [rsvdz];
            31:24 => irptndx, set_irptndx ["IRPTNDX"]: u32;
        }
    }

    // Type 1 CBARs are "Stage 1 Context with stage 2 bypass" registers.
    //
    // Bit 10 is either:
    // + The `HYPC` bit when:
    //   + This is a v1 SMMU, or
    //   + This is a v2 SMMU, and (`IDR2.E2HS` is 0 or (`IDR2.E2HS` is 1 and `CR0.HYPMODE` is 0))
    // + The `E2HC` bit when:
    //   + This is a v2 SMMU, and
    //   + `IDR2.E2HS` is 1, and
    //   + `CR0.HYPMODE` is 1
    //
    // Because register definitions do not permit overlapping field definitions in the same struct,
    // these two names are combined into one field (`hypc_e2hc` / `"HYPC_E2HC"`).
    define_smmu_reg! {
        /// Stage 1 Context with Stage 2 Bypass Attribute Register (`SMMU_CBARn` Type 1).
        pub struct CbarType1(u32) @ [ndx: u32] ndx << 2 => {
            7:0   => vmid, set_vmid ["VMID"]: u32;
            9:8   => bpshcfg, set_bpshcfg ["BPSHCFG"]: u32;
            10:10 => hypc_e2hc, set_hypc_e2hc ["HYPC_E2HC"]: u32;
            11:11 => fb, set_fb ["FB"]: u32;
            15:12 => mem_attr, set_mem_attr ["MemAttr"]: u32;
            17:16 => cbar_type, set_cbar_type ["TYPE"]: enum CbarType;
            19:18 => bsu, set_bsu ["BSU"]: u32;
            21:20 => racfg, set_racfg ["RACFG"]: u32;
            23:22 => wacfg, set_wacfg ["WACFG"]: u32;
            31:24 => irptndx, set_irptndx ["IRPTNDX"]: u32;
        }
    }

    // Type 2 CBARs are "Stage 1 Context with stage 2 fault" registers.
    // Note: bits `[19:18]` are `SBZ` ("should be zero").
    define_smmu_reg! {
        /// Stage 1 Context with Stage 2 Fault Attribute Register (`SMMU_CBARn` Type 2).
        pub struct CbarType2(u32) @ [ndx: u32] ndx << 2 => {
            17:16 => cbar_type, set_cbar_type ["TYPE"]: enum CbarType;
            19:18 => sbz, set_sbz ["SBZ"]: u32;
            31:24 => irptndx, set_irptndx ["IRPTNDX"]: u32;
        }
    }

    // Type 3 CBARs are "Stage 1 followed by stage 2 translation context" registers.
    // Note: bits `[19:18]` are `SBZ` ("should be zero").
    define_smmu_reg! {
        /// Stage 1 followed by Stage 2 Translation Context Attribute Register (`SMMU_CBARn`
        /// Type 3).
        pub struct CbarType3(u32) @ [ndx: u32] ndx << 2 => {
            7:0   => vmid, set_vmid ["VMID"]: u32;
            15:8  => cbndx, set_cbndx ["CBNDX"]: u32;
            17:16 => cbar_type, set_cbar_type ["TYPE"]: enum CbarType;
            19:18 => sbz, set_sbz ["SBZ"]: u32;
            31:24 => irptndx, set_irptndx ["IRPTNDX"]: u32;
        }
    }

    /// Type alias for [`CbarType0`] (`TYPE == S2Translation`).
    pub type CbarS2Translation = CbarType0;
    /// Type alias for [`CbarType1`] (`TYPE == S1TS2Bypass`).
    pub type CbarS1TS2Bypass = CbarType1;
    /// Type alias for [`CbarType2`] (`TYPE == S1TS2Fault`).
    pub type CbarS1TS2Fault = CbarType2;
    /// Type alias for [`CbarType3`] (`TYPE == S1TS2Translate`).
    pub type CbarS1TS2Translate = CbarType3;

    define_smmu_reg! {
        /// Context Bank Fault Restricted Syndrome Register A (`SMMU_CBFRSYNRAn`, offset
        /// `0x400 + 4 * ndx`).
        pub struct Cbfrsynra(u32) @ [ndx: u32] 0x400 + (ndx << 2) => {
            15:0  => stream_id, set_stream_id ["StreamID"]: u32;
            31:16 => ssd_index, set_ssd_index ["SSD_Index"]: u32;
        }
    }

    define_smmu_reg! {
        /// Context Bank Attribute Register 2 (`SMMU_CBA2Rn`, offset `0x800 + 4 * ndx`).
        pub struct Cba2r(u32) @ [ndx: u32] 0x800 + (ndx << 2) => {
            0:0   => va64, set_va64 ["VA64"]: enum Cba2rVa64;
            1:1   => monc, set_monc ["MONC"]: u32;
            31:16 => vmid16, set_vmid16 ["VMID16"]: u32;
        }
    }
}

/// Stage 1 Translation Context Bank (`SMMU_CBn_BASE`) register definitions.
pub mod s1cbr {
    use super::*;
    pub use super::{TcrIps, TcrIrg0, TcrOrg0, TcrSh0, TcrTg0, TcrTg1};

    define_smmu_reg! {
        /// Stage 1 System Control Register (`SMMU_CBn_SCTLR`, offset `0x00`).
        pub struct Sctlr(u32) @ 0x00 => {
            0:0   => m, set_m ["M"]: u32;
            1:1   => tre, set_tre ["TRE"]: u32;
            2:2   => afe, set_afe ["AFE"]: u32;
            3:3   => affd, set_affd ["AFFD"]: u32;
            4:4   => e, set_e ["E"]: u32;
            5:5   => cfre, set_cfre ["CFRE"]: u32;
            6:6   => cfie, set_cfie ["CFIE"]: u32;
            7:7   => cfcfg, set_cfcfg ["CFCFG"]: u32;
            8:8   => hupcf, set_hupcf ["HUPCF"]: u32;
            9:9   => wxn, set_wxn ["WXN"]: u32;
            10:10 => uwxn, set_uwxn ["UWXN"]: u32;
            11:11 => res1, set_res1 ["__res1"]: u32;
            12:12 => asidpne, set_asidpne ["ASIDPNE"]: u32;
            15:14 => transientcfg, set_transientcfg ["TRANSIENTCFG"]: u32;
            19:16 => mem_attr, set_mem_attr ["MemAttr"]: u32;
            20:20 => mtcfg, set_mtcfg ["MTCFG"]: u32;
            21:21 => res2, set_res2 ["__res2"]: u32;
            23:22 => shcfg, set_shcfg ["SHCFG"]: u32;
            25:24 => racfg, set_racfg ["RACFG"]: u32;
            27:26 => wacfg, set_wacfg ["WACFG"]: u32;
            29:28 => nscfg, set_nscfg ["NSCFG"]: u32;
            30:30 => uci, set_uci ["UCI"]: u32;
            31:31 => res3, set_res3 ["__res3"]: u32;
        }
    }

    // Section 16.5.1: Auxiliary Control Register.
    // The SMMU_CBn_ACTLR bit assignments are IMPLEMENTATION DEFINED.
    define_smmu_reg! {
        /// Auxiliary Control Register (`SMMU_CBn_ACTLR`, offset `0x04`).
        pub struct Actlr(u32) @ 0x04 => {
            31:0 => value, set_value ["value"]: u32;
        }
    }

    define_smmu_reg! {
        /// Transaction Resume Register (`SMMU_CBn_RESUME`, offset `0x08`).
        pub struct Resume(u32) @ 0x08 => {
            0:0 => tnr, set_tnr ["TnR"]: u32;
        }
    }

    // Section 16.5.39: Translation Control Register 2 (`SMMU_CBn_TCR2`, used when `TCR.EAE == 1`
    // or `CBA2R.VA64 == 1`).
    define_smmu_reg! {
        /// Translation Control Register 2 (`SMMU_CBn_TCR2`, offset `0x10`).
        pub struct Tcr2(u32) @ 0x10 => {
            2:0   => pa_size, set_pa_size ["PASize"]: enum TcrIps;
            3:3   => res1, set_res1 ["__res1"]: u32;
            4:4   => as_bit, set_as_bit ["AS"]: u32;
            5:5   => tbi0, set_tbi0 ["TBI0"]: u32;
            6:6   => tbi1, set_tbi1 ["TBI1"]: u32;
            7:7   => res2, set_res2 ["__res2"]: u32;
            8:8   => had0, set_had0 ["HAD0"]: u32;
            9:9   => had1, set_had1 ["HAD1"]: u32;
            10:10 => ha, set_ha ["HA"]: u32;
            11:11 => hd, set_hd ["HD"]: u32;
            13:12 => res3, set_res3 ["__res3"]: u32;
            14:14 => nscfg0, set_nscfg0 ["NSCFG0"]: u32;
            17:15 => sep, set_sep ["SEP"]: u32;
            29:18 => res4, set_res4 ["__res4"]: u32;
            30:30 => nscfg1, set_nscfg1 ["NSCFG1"]: u32;
            31:31 => res5, set_res5 ["__res5"]: u32;
        }
    }

    // Section 16.5.40: Translation Table Base Registers.
    //
    // TTBRs have a few different formats depending on which translation scheme (`CBA2R.VA64` and
    // `TCR.EAE`) this context bank is using. These schemes are:
    // + AArch32 Short-descriptor (`AddrMode::Mode32Bit`, `EAE == 0`)
    // + AArch32 Long-descriptor  (`AddrMode::Ext32Bit`, `EAE == 1`)
    // + AArch64                  (`AddrMode::Mode64Bit`, `VA64 == 1`)
    //
    // Note: in `Ttbr0_32Bit` and `Ttbr1_32Bit`, bit 0 is `IRGN[1]` and bit 6 is `IRGN[0]` per the
    // ARM SMMUv2 specification; the accessor at bit 6 is named `irgn0`/`set_irgn0` to disambiguate
    // it from `irgn1`/`set_irgn1` in the flat `define_smmu_reg!` expansion while preserving the
    // `"IRGN1"` display label for register dump output parity.
    define_smmu_reg! {
        /// Translation Table Base Register 0, AArch32 Short-descriptor (`SMMU_CBn_TTBR0`, offset
        /// `0x20`).
        pub struct Ttbr0_32Bit(u32) @ 0x20 => {
            0:0  => irgn1, set_irgn1 ["IRGN1"]: u32;
            1:1  => s, set_s ["S"]: u32;
            2:2  => imp, set_imp ["IMP"]: u32;
            4:3  => rgn, set_rgn ["RGN"]: u32;
            5:5  => nos, set_nos ["NOS"]: u32;
            6:6  => irgn0, set_irgn0 ["IRGN1"]: u32;
            31:7 => base_address, set_base_address ["BaseAddress"]: u32;
        }
    }

    impl Ttbr0_32Bit {
        /// Sets `BaseAddress` from an unshifted 32-bit physical base address (`base_addr >> 7`).
        #[inline]
        pub fn set_base_addr_from_value(&mut self, base_address_value: u32) -> &mut Self {
            self.set_base_address(base_address_value >> 7)
        }

        /// Returns the unshifted 32-bit physical base address (`BaseAddress << 7`).
        #[inline]
        pub const fn base_addr_value(&self) -> u32 {
            self.base_address() << 7
        }
    }

    define_smmu_reg! {
        /// Translation Table Base Register 0, AArch32 Long-descriptor (`SMMU_CBn_TTBR0`, offset
        /// `0x20`).
        pub struct Ttbr0Ext32Bit(u64) @ 0x20 => {
            2:0   => [rsvdz];
            47:3  => base_address, set_base_address ["BaseAddress"]: u64;
            55:48 => asid, set_asid ["ASID"]: u64;
            63:56 => res1, set_res1 ["__res1"]: u64;
        }
    }

    impl Ttbr0Ext32Bit {
        /// Sets `BaseAddress` from an unshifted 64-bit physical base address (`base_addr >> 3`).
        #[inline]
        pub fn set_base_addr_from_value(&mut self, base_address_value: u64) -> &mut Self {
            self.set_base_address(base_address_value >> 3)
        }

        /// Returns the unshifted 64-bit physical base address (`BaseAddress << 3`).
        #[inline]
        pub const fn base_addr_value(&self) -> u64 {
            self.base_address() << 3
        }
    }

    define_smmu_reg! {
        /// Translation Table Base Register 0, AArch64 (`SMMU_CBn_TTBR0`, offset `0x20`).
        pub struct Ttbr0_64Bit(u64) @ 0x20 => {
            2:0   => [rsvdz];
            47:3  => base_address, set_base_address ["BaseAddress"]: u64;
            63:48 => asid, set_asid ["ASID"]: u64;
        }
    }

    impl Ttbr0_64Bit {
        /// Sets `BaseAddress` from an unshifted 64-bit physical base address (`base_addr >> 3`).
        #[inline]
        pub fn set_base_addr_from_value(&mut self, base_address_value: u64) -> &mut Self {
            self.set_base_address(base_address_value >> 3)
        }

        /// Returns the unshifted 64-bit physical base address (`BaseAddress << 3`).
        #[inline]
        pub const fn base_addr_value(&self) -> u64 {
            self.base_address() << 3
        }
    }

    // Note: bit 6 (`IRGN[0]`) is named `irgn0`/`set_irgn0` to disambiguate it from bit 0 (`irgn1`)
    // in the flat `define_smmu_reg!` expansion while preserving `"IRGN1"` for dump output parity.
    define_smmu_reg! {
        /// Translation Table Base Register 1, AArch32 Short-descriptor (`SMMU_CBn_TTBR1`, offset
        /// `0x28`).
        pub struct Ttbr1_32Bit(u32) @ 0x28 => {
            0:0  => irgn1, set_irgn1 ["IRGN1"]: u32;
            1:1  => s, set_s ["S"]: u32;
            2:2  => imp, set_imp ["IMP"]: u32;
            4:3  => rgn, set_rgn ["RGN"]: u32;
            5:5  => nos, set_nos ["NOS"]: u32;
            6:6  => irgn0, set_irgn0 ["IRGN1"]: u32;
            31:7 => base_address, set_base_address ["BaseAddress"]: u32;
        }
    }

    impl Ttbr1_32Bit {
        /// Sets `BaseAddress` from an unshifted 32-bit physical base address (`base_addr >> 7`).
        #[inline]
        pub fn set_base_addr_from_value(&mut self, base_address_value: u32) -> &mut Self {
            self.set_base_address(base_address_value >> 7)
        }

        /// Returns the unshifted 32-bit physical base address (`BaseAddress << 7`).
        #[inline]
        pub const fn base_addr_value(&self) -> u32 {
            self.base_address() << 7
        }
    }

    define_smmu_reg! {
        /// Translation Table Base Register 1, AArch32 Long-descriptor (`SMMU_CBn_TTBR1`, offset
        /// `0x28`).
        pub struct Ttbr1Ext32Bit(u64) @ 0x28 => {
            2:0   => [rsvdz];
            47:3  => base_address, set_base_address ["BaseAddress"]: u64;
            55:48 => asid, set_asid ["ASID"]: u64;
            63:56 => res1, set_res1 ["__res1"]: u64;
        }
    }

    impl Ttbr1Ext32Bit {
        /// Sets `BaseAddress` from an unshifted 64-bit physical base address (`base_addr >> 3`).
        #[inline]
        pub fn set_base_addr_from_value(&mut self, base_address_value: u64) -> &mut Self {
            self.set_base_address(base_address_value >> 3)
        }

        /// Returns the unshifted 64-bit physical base address (`BaseAddress << 3`).
        #[inline]
        pub const fn base_addr_value(&self) -> u64 {
            self.base_address() << 3
        }
    }

    define_smmu_reg! {
        /// Translation Table Base Register 1, AArch64 (`SMMU_CBn_TTBR1`, offset `0x28`).
        pub struct Ttbr1_64Bit(u64) @ 0x28 => {
            2:0   => [rsvdz];
            47:3  => base_address, set_base_address ["BaseAddress"]: u64;
            63:48 => asid, set_asid ["ASID"]: u64;
        }
    }

    impl Ttbr1_64Bit {
        /// Sets `BaseAddress` from an unshifted 64-bit physical base address (`base_addr >> 3`).
        #[inline]
        pub fn set_base_addr_from_value(&mut self, base_address_value: u64) -> &mut Self {
            self.set_base_address(base_address_value >> 3)
        }

        /// Returns the unshifted 64-bit physical base address (`BaseAddress << 3`).
        #[inline]
        pub const fn base_addr_value(&self) -> u64 {
            self.base_address() << 3
        }
    }

    // Section 16.5.38: Translation Control Register.
    //
    // There are three possible formats for the TCR based on the values of `CBA2R.VA64` and
    // `TCR.EAE`, which determine the addressing mode for the context bank:
    //
    //        Type       | CBA2R.VA64 | TCR.EAE |
    // ------------------+------------+---------+
    //   32 Bit          |      0     |    0    |
    //   Extended 32 Bit |      0     |    1    |
    //   64 Bit          |      1     |    x    |
    define_smmu_reg! {
        /// Translation Control Register base layout (`SMMU_CBn_TCR`, offset `0x30`).
        pub struct Tcr(u32) @ 0x30 => {
            31:31 => eae, set_eae ["EAE"]: u32;
        }
    }

    define_smmu_reg! {
        /// Translation Control Register, AArch32 Short-descriptor (`SMMU_CBn_TCR`, offset `0x30`).
        pub struct Tcr32Bit(u32) @ 0x30 => {
            2:0   => t0sz, set_t0sz ["T0SZ"]: u32;
            3:3   => [rsvdz];
            4:4   => pd0, set_pd0 ["PD0"]: u32;
            5:5   => pd1, set_pd1 ["PD1"]: u32;
            14:14 => nscfg0, set_nscfg0 ["NSCFG0"]: u32;
            30:30 => nscfg1, set_nscfg1 ["NSCFG1"]: u32;
            31:31 => eae, set_eae ["EAE"]: u32;
        }
    }

    // Note: bit 30 (`NSCFG1`) is named `nscfg1`/`set_nscfg1` to disambiguate it from bit 14
    // (`nscfg0`) in the flat `define_smmu_reg!` expansion while preserving `"NSCFG0"` for dump
    // output parity.
    define_smmu_reg! {
        /// Translation Control Register, AArch32 Long-descriptor (`SMMU_CBn_TCR`, offset `0x30`).
        pub struct TcrExt32Bit(u32) @ 0x30 => {
            2:0   => t0sz, set_t0sz ["T0SZ"]: u32;
            7:7   => epd0, set_epd0 ["EPD0"]: u32;
            9:8   => irgn0, set_irgn0 ["IRGN0"]: enum TcrIrg0;
            11:10 => orgn0, set_orgn0 ["ORGN0"]: enum TcrOrg0;
            13:12 => sh0, set_sh0 ["SH0"]: enum TcrSh0;
            14:14 => nscfg0, set_nscfg0 ["NSCFG0"]: u32;
            18:16 => t1sz, set_t1sz ["T1SZ"]: u32;
            22:22 => a1, set_a1 ["A1"]: u32;
            23:23 => epd1, set_epd1 ["EPD1"]: u32;
            25:24 => irgn1, set_irgn1 ["IRGN1"]: enum TcrIrg0;
            27:26 => orgn1, set_orgn1 ["ORGN1"]: enum TcrOrg0;
            29:28 => sh1, set_sh1 ["SH1"]: enum TcrSh0;
            30:30 => nscfg1, set_nscfg1 ["NSCFG0"]: u32;
            31:31 => eae, set_eae ["EAE"]: u32;
        }
    }

    define_smmu_reg! {
        /// Translation Control Register, AArch64 (`SMMU_CBn_TCR`, offset `0x30`).
        pub struct Tcr64Bit(u32) @ 0x30 => {
            5:0   => t0sz, set_t0sz ["T0SZ"]: u32;
            7:7   => epd0, set_epd0 ["EPD0"]: u32;
            9:8   => irgn0, set_irgn0 ["IRGN0"]: enum TcrIrg0;
            11:10 => orgn0, set_orgn0 ["ORGN0"]: enum TcrOrg0;
            13:12 => sh0, set_sh0 ["SH0"]: enum TcrSh0;
            15:14 => tg0, set_tg0 ["TG0"]: enum TcrTg0;
            21:16 => t1sz, set_t1sz ["T1SZ"]: u32;
            22:22 => a1, set_a1 ["A1"]: u32;
            23:23 => epd1, set_epd1 ["EPD1"]: u32;
            25:24 => irgn1, set_irgn1 ["IRGN1"]: enum TcrIrg0;
            27:26 => orgn1, set_orgn1 ["ORGN1"]: enum TcrOrg0;
            29:28 => sh1, set_sh1 ["SH1"]: enum TcrSh0;
            31:30 => tg1, set_tg1 ["TG1"]: enum TcrTg1;
        }
    }

    // Section 16.5.7: Context Identification Register.
    //
    // The Context ID register has one of two forms depending on whether `TCR.EAE` is 0 or not. The
    // two forms are defined as `ContextidrEae0` and `ContextidrEae1` here.
    define_smmu_reg! {
        /// Context Identification Register when `TCR.EAE == 0` (`SMMU_CBn_CONTEXTIDR`, offset
        /// `0x34`).
        pub struct ContextidrEae0(u32) @ 0x34 => {
            7:0  => asid, set_asid ["ASID"]: u32;
            31:8 => procid, set_procid ["PROCID"]: u32;
        }
    }

    define_smmu_reg! {
        /// Context Identification Register when `TCR.EAE == 1` (`SMMU_CBn_CONTEXTIDR`, offset
        /// `0x34`).
        pub struct ContextidrEae1(u32) @ 0x34 => {
            31:0 => procid, set_procid ["PROCID"]: u32;
        }
    }

    // Section 16.5.27: Primary Region Remap Register.
    // Used when the AArch32 Short-descriptor translation scheme is being used.
    define_smmu_reg! {
        /// Primary Region Remap Register (`SMMU_CBn_PRRR`, offset `0x38`).
        pub struct Prrr(u32) @ 0x38 => {
            1:0   => tr0, set_tr0 ["TR0"]: u32;
            3:2   => tr1, set_tr1 ["TR1"]: u32;
            5:4   => tr2, set_tr2 ["TR2"]: u32;
            7:6   => tr3, set_tr3 ["TR3"]: u32;
            9:8   => tr4, set_tr4 ["TR4"]: u32;
            11:10 => tr5, set_tr5 ["TR5"]: u32;
            13:12 => tr6, set_tr6 ["TR6"]: u32;
            15:14 => tr7, set_tr7 ["TR7"]: u32;
            16:16 => ds0, set_ds0 ["DS0"]: u32;
            17:17 => ds1, set_ds1 ["DS1"]: u32;
            18:18 => ns0, set_ns0 ["NS0"]: u32;
            19:19 => ns1, set_ns1 ["NS1"]: u32;
            24:24 => nos0, set_nos0 ["NOS0"]: u32;
            25:25 => nos1, set_nos1 ["NOS1"]: u32;
            26:26 => nos2, set_nos2 ["NOS2"]: u32;
            27:27 => nos3, set_nos3 ["NOS3"]: u32;
            28:28 => nos4, set_nos4 ["NOS4"]: u32;
            29:29 => nos5, set_nos5 ["NOS5"]: u32;
            30:30 => nos6, set_nos6 ["NOS6"]: u32;
            31:31 => nos7, set_nos7 ["NOS7"]: u32;
        }
    }

    // Section 16.5.13: Normal Memory Remap Register.
    // Used when the AArch32 Short-descriptor translation scheme is being used.
    define_smmu_reg! {
        /// Normal Memory Remap Register (`SMMU_CBn_NMRR`, offset `0x3c`).
        pub struct Nmrr(u32) @ 0x3c => {
            1:0   => ir0, set_ir0 ["IR0"]: u32;
            3:2   => ir1, set_ir1 ["IR1"]: u32;
            5:4   => ir2, set_ir2 ["IR2"]: u32;
            7:6   => ir3, set_ir3 ["IR3"]: u32;
            9:8   => ir4, set_ir4 ["IR4"]: u32;
            11:10 => ir5, set_ir5 ["IR5"]: u32;
            13:12 => ir6, set_ir6 ["IR6"]: u32;
            15:14 => ir7, set_ir7 ["IR7"]: u32;
            17:16 => or0, set_or0 ["OR0"]: u32;
            19:18 => or1, set_or1 ["OR1"]: u32;
            21:20 => or2, set_or2 ["OR2"]: u32;
            23:22 => or3, set_or3 ["OR3"]: u32;
            25:24 => or4, set_or4 ["OR4"]: u32;
            27:26 => or5, set_or5 ["OR5"]: u32;
            29:28 => or6, set_or6 ["OR6"]: u32;
            31:30 => or7, set_or7 ["OR7"]: u32;
        }
    }

    // Section 16.5.12: Memory Attribute Indirection Registers.
    // Used when the AArch32 Long-descriptor, or the AArch64 translation scheme is being used.
    define_smmu_reg! {
        /// Memory Attribute Indirection Register 0 (`SMMU_CBn_MAIR0`, offset `0x38`).
        pub struct Mair0(u32) @ 0x38 => {
            7:0   => attr0, set_attr0 ["Attr0"]: u32;
            15:8  => attr1, set_attr1 ["Attr1"]: u32;
            23:16 => attr2, set_attr2 ["Attr2"]: u32;
            31:24 => attr3, set_attr3 ["Attr3"]: u32;
        }
    }

    define_smmu_reg! {
        /// Memory Attribute Indirection Register 1 (`SMMU_CBn_MAIR1`, offset `0x3c`).
        pub struct Mair1(u32) @ 0x3c => {
            7:0   => attr4, set_attr4 ["Attr4"]: u32;
            15:8  => attr5, set_attr5 ["Attr5"]: u32;
            23:16 => attr6, set_attr6 ["Attr6"]: u32;
            31:24 => attr7, set_attr7 ["Attr7"]: u32;
        }
    }

    // Section 16.5.14: Physical Address Register.
    //
    // The PAR has three different forms depending on:
    // 1) The specific address translation scheme we are using.
    // 2) Whether or not there is a fault reported in the register (`F == 1`).
    //
    // The names we use to distinguish between the forms are:
    // + `Par`        : Defines the `F` bit so that a specific type may be chosen.
    // + `ParType0F0` : AArch32 Short-descriptor, no fault (`F == 0`)
    // + `ParType1F0` : AArch32 Long-descriptor, or AArch64, no fault (`F == 0`)
    // + `ParF1`      : Yes fault (`F == 1`)
    define_smmu_reg! {
        /// Physical Address Register base layout (`SMMU_CBn_PAR`, offset `0x50`).
        pub struct Par(u64) @ 0x50 => {
            0:0 => f, set_f ["F"]: u64;
        }
    }

    define_smmu_reg! {
        /// Physical Address Register, AArch32 Short-descriptor, no fault (`SMMU_CBn_PAR`, offset
        /// `0x50`).
        pub struct ParType0F0(u64) @ 0x50 => {
            0:0   => f, set_f ["F"]: u64;
            1:1   => ss, set_ss ["SS"]: u64;
            3:2   => outer, set_outer ["Outer"]: u64;
            6:4   => inner, set_inner ["Inner"]: u64;
            7:7   => sh, set_sh ["SH"]: u64;
            8:8   => imp, set_imp ["IMP"]: u64;
            9:9   => ns, set_ns ["NS"]: u64;
            10:10 => nos, set_nos ["NOS"]: u64;
            11:11 => [rsvdz];
            31:12 => pa, set_pa ["PA"]: u64;
        }
    }

    define_smmu_reg! {
        /// Physical Address Register, AArch32 Long-descriptor or AArch64, no fault
        /// (`SMMU_CBn_PAR`, offset `0x50`).
        pub struct ParType1F0(u64) @ 0x50 => {
            0:0   => f, set_f ["F"]: u64;
            8:7   => sh, set_sh ["SH"]: u64;
            9:9   => ns, set_ns ["NS"]: u64;
            10:10 => imp, set_imp ["IMP"]: u64;
            47:12 => pa, set_pa ["PA"]: u64;
            63:56 => mattr, set_mattr ["MATTR"]: u64;
        }
    }

    define_smmu_reg! {
        /// Physical Address Register when a fault is reported (`F == 1`, `SMMU_CBn_PAR`, offset
        /// `0x50`).
        pub struct ParF1(u64) @ 0x50 => {
            0:0   => f, set_f ["F"]: u64;
            1:1   => tf, set_tf ["TF"]: u64;
            2:2   => aff, set_aff ["AFF"]: u64;
            3:3   => pf, set_pf ["PF"]: u64;
            4:4   => ef, set_ef ["EF"]: u64;
            5:5   => tlbmcf, set_tlbmcf ["TLBMCF"]: u64;
            6:6   => tlblkf, set_tlblkf ["TLBLKF"]: u64;
            7:7   => asf, set_asf ["ASF"]: u64;
            9:8   => format, set_format ["Format"]: u64;
            29:29 => icf, set_icf ["ICF"]: u64;
            31:31 => atot, set_atot ["ATOT"]: u64;
            33:32 => plvl, set_plvl ["PLVL"]: u64;
            35:35 => stage, set_stage ["STAGE"]: u64;
        }
    }

    define_smmu_reg! {
        /// Fault Status Register (`SMMU_CBn_FSR`, offset `0x58`).
        pub struct Fsr(u32) @ 0x58 => {
            1:1   => tf, set_tf ["TF"]: u32;
            2:2   => aff, set_aff ["AFF"]: u32;
            3:3   => pf, set_pf ["PF"]: u32;
            4:4   => ef, set_ef ["EF"]: u32;
            5:5   => tlbmcf, set_tlbmcf ["TLBMCF"]: u32;
            6:6   => tlblkf, set_tlblkf ["TLBLKF"]: u32;
            7:7   => asf, set_asf ["ASF"]: u32;
            8:8   => uut, set_uut ["UUT"]: u32;
            10:9  => format, set_format ["Format"]: u32;
            30:30 => ss, set_ss ["SS"]: u32;
            31:31 => multi, set_multi ["MULTI"]: u32;
        }
    }

    define_smmu_reg! {
        /// Fault Status Restore Register (`SMMU_CBn_FSRRESTORE`, offset `0x5c`).
        pub struct FsrRestore(u32) @ 0x5c => {
            1:1   => tf, set_tf ["TF"]: u32;
            2:2   => aff, set_aff ["AFF"]: u32;
            3:3   => pf, set_pf ["PF"]: u32;
            4:4   => ef, set_ef ["EF"]: u32;
            5:5   => tlbmcf, set_tlbmcf ["TLBMCF"]: u32;
            6:6   => tlblkf, set_tlblkf ["TLBLKF"]: u32;
            7:7   => asf, set_asf ["ASF"]: u32;
            8:8   => uut, set_uut ["UUT"]: u32;
            31:31 => multi, set_multi ["MULTI"]: u32;
        }
    }

    // Section 16.5.8: Fault Address Register.
    //
    // The `FADDR` field of the FAR is defined as bits `[N-1, 0]`, where `N` is implementation
    // defined, but must be `>= IDR2.UBS`. Unimplemented bits are required to be RAZ, so we define
    // the field as occupying all 64 bits.
    //
    // Additionally, the bottom `M` bits (`M` is determined by the translation granule size) *may*
    // contain the lowest `M` bits of the address which produced the fault, but they are not
    // *required* to do so as there may be significant implementation cost in providing the bits.
    // So, *technically* these bits are `UNKNOWN`.
    define_smmu_reg! {
        /// Fault Address Register (`SMMU_CBn_FAR`, offset `0x60`).
        pub struct Far(u64) @ 0x60 => {
            63:0 => faddr, set_faddr ["FADDR"]: u64;
        }
    }

    define_smmu_reg! {
        /// Fault Syndrome Register 0 (`SMMU_CBn_FSYNR0`, offset `0x68`).
        pub struct Fsynr0(u32) @ 0x68 => {
            1:0   => plvl, set_plvl ["PLVL"]: u32;
            4:4   => wnr, set_wnr ["WNR"]: u32;
            5:5   => pnu, set_pnu ["PNU"]: u32;
            6:6   => ind, set_ind ["IND"]: u32;
            8:8   => nsattr, set_nsattr ["NSATTR"]: u32;
            10:10 => ptwf, set_ptwf ["PTWF"]: u32;
            11:11 => afr, set_afr ["AFR"]: u32;
            23:16 => s1cbndx, set_s1cbndx ["S1CBNDX"]: u32;
        }
    }

    // FSYNR1 bits are all "implementation defined".
    define_smmu_reg! {
        /// Fault Syndrome Register 1 (`SMMU_CBn_FSYNR1`, offset `0x6c`).
        pub struct Fsynr1(u32) @ 0x6c => {}
    }

    define_smmu_reg! {
        /// TLB Invalidate by VA, AArch64 (`SMMU_CBn_TLBIVA`, offset `0x600`).
        pub struct TlbivaAArch64(u64) @ 0x600 => {
            43:0  => address_55_12, set_address_55_12 ["Address_55_12"]: u64;
            63:48 => asid, set_asid ["ASID"]: u64;
        }
    }

    impl TlbivaAArch64 {
        /// Sets `Address_55_12` from a 64-bit virtual address (`addr >> 12`).
        #[inline]
        pub fn set_address(&mut self, addr: u64) -> &mut Self {
            self.set_address_55_12(addr >> 12)
        }

        /// Returns the 64-bit virtual address (`Address_55_12 << 12`).
        #[inline]
        pub const fn address(&self) -> u64 {
            self.address_55_12() << 12
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate by VA, AArch32 (`SMMU_CBn_TLBIVA`, offset `0x600`).
        pub struct TlbivaAArch32(u32) @ 0x600 => {
            7:0   => asid, set_asid ["ASID"]: u32;
            31:12 => va, set_va ["VA"]: u32;
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate by VA All ASID, AArch64 (`SMMU_CBn_TLBIVAA`, offset `0x608`).
        pub struct TlbivaaAArch64(u64) @ 0x608 => {
            43:0 => address_55_12, set_address_55_12 ["Address_55_12"]: u64;
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate by VA All ASID, AArch32 (`SMMU_CBn_TLBIVAA`, offset `0x608`).
        pub struct TlbivaaAArch32(u32) @ 0x608 => {
            31:12 => va, set_va ["VA"]: u32;
        }
    }

    // Section 16.5.31: TLB Invalidate by ASID.
    // Note: for SMMUv1, and when using AArch32 translation schemes, the ASID is only 8 bits
    // (`bits [0, 7]`).
    define_smmu_reg! {
        /// TLB Invalidate by ASID (`SMMU_CBn_TLBIASID`, offset `0x610`).
        pub struct Tlbiasid(u32) @ 0x610 => {
            15:0 => asid, set_asid ["ASID"]: u32;
        }
    }

    // Section 16.5.30: TLB Invalidate All.
    // Note: no fields are formally defined for this register. The documentation says "This
    // operation requires no arguments". Simply writing to the register is all that is needed,
    // however we reserve-as-zero all of the bits so that we always write 0.
    define_smmu_reg! {
        /// TLB Invalidate All (`SMMU_CBn_TLBIALL`, offset `0x618`).
        pub struct Tlbiall(u32) @ 0x618 => {
            31:0 => [rsvdz];
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate by VA, Last Level, AArch64 (`SMMU_CBn_TLBIVAL`, offset `0x620`).
        pub struct TlbivalAArch64(u64) @ 0x620 => {
            43:0  => address_55_12, set_address_55_12 ["Address_55_12"]: u64;
            63:48 => asid, set_asid ["ASID"]: u64;
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate by VA, Last Level, AArch32 (`SMMU_CBn_TLBIVAL`, offset `0x620`).
        pub struct TlbivalAArch32(u32) @ 0x620 => {
            31:12 => va, set_va ["VA"]: u32;
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate by VA, All ASID, Last Level, AArch64 (`SMMU_CBn_TLBIVAAL`, offset
        /// `0x628`).
        pub struct TlbivaalAArch64(u64) @ 0x628 => {
            43:0 => address_55_12, set_address_55_12 ["Address_55_12"]: u64;
        }
    }

    define_smmu_reg! {
        /// TLB Invalidate by VA, All ASID, Last Level, AArch32 (`SMMU_CBn_TLBIVAAL`, offset
        /// `0x628`).
        pub struct TlbivaalAArch32(u32) @ 0x628 => {
            31:12 => va, set_va ["VA"]: u32;
        }
    }

    // Section 16.5.37: TLB Synchronize Invalidate.
    // Note: as with `Tlbiall`, no fields are formally defined for this register. A write operation
    // is all that is required; we reserve all of the bits as zeros.
    define_smmu_reg! {
        /// TLB Synchronize Invalidate (`SMMU_CBn_TLBSYNC`, offset `0x7f0`).
        pub struct Tlbsync(u32) @ 0x7f0 => {
            31:0 => [rsvdz];
        }
    }

    // Section 16.5.36: TLB Status.
    define_smmu_reg! {
        /// TLB Status Register (`SMMU_CBn_TLBSTATUS`, offset `0x7f4`).
        pub struct Tlbstatus(u32) @ 0x7f4 => {
            0:0 => sactive, set_sactive ["SACTIVE"]: u32;
        }
    }

    define_smmu_reg! {
        /// Address Translation Stage 1 Privileged Read (`SMMU_CBn_ATS1PR`, offset `0x800`).
        pub struct Ats1pr(u64) @ 0x800 => {
            63:12 => addr, set_addr ["Addr"]: u64;
        }
    }

    define_smmu_reg! {
        /// Address Translation Stage 1 Privileged Write (`SMMU_CBn_ATS1PW`, offset `0x808`).
        pub struct Ats1pw(u64) @ 0x808 => {
            63:12 => addr, set_addr ["Addr"]: u64;
        }
    }

    define_smmu_reg! {
        /// Address Translation Stage 1 Unprivileged Read (`SMMU_CBn_ATS1UR`, offset `0x810`).
        pub struct Ats1ur(u64) @ 0x810 => {
            63:12 => addr, set_addr ["Addr"]: u64;
        }
    }

    define_smmu_reg! {
        /// Address Translation Stage 1 Unprivileged Write (`SMMU_CBn_ATS1UW`, offset `0x818`).
        pub struct Ats1uw(u64) @ 0x818 => {
            63:12 => addr, set_addr ["Addr"]: u64;
        }
    }

    define_smmu_reg! {
        /// Address Translation Status Register (`SMMU_CBn_ATSR`, offset `0x8f0`).
        pub struct Atsr(u32) @ 0x8f0 => {
            0:0 => active, set_active ["ACTIVE"]: u32;
        }
    }

    // Sections 16.5.15 - 16.5.26: Performance Monitors.
    // Performance monitor registers live in the context bank at offsets `[0xE00, 0xFFF]`. They are
    // not defined here.
}

/// Stage 2 Translation Context Bank (`SMMU_CBn_BASE`) register definitions.
pub mod s2cbr {
    pub use super::s1cbr::{Actlr, Far, Fsr, FsrRestore, Fsynr1, Resume, Tlbstatus, Tlbsync};
    use super::*;

    define_smmu_reg! {
        /// Stage 2 System Control Register (`SMMU_CBn_SCTLR`, offset `0x00`).
        pub struct Sctlr(u32) @ 0x00 => {
            0:0   => m, set_m ["M"]: u32;
            1:1   => tre, set_tre ["TRE"]: u32;
            2:2   => afe, set_afe ["AFE"]: u32;
            3:3   => affd, set_affd ["AFFD"]: u32;
            4:4   => e, set_e ["E"]: u32;
            5:5   => cfre, set_cfre ["CFRE"]: u32;
            6:6   => cfie, set_cfie ["CFIE"]: u32;
            7:7   => cfcfg, set_cfcfg ["CFCFG"]: u32;
            8:8   => hupcf, set_hupcf ["HUPCF"]: u32;
            12:9  => res1, set_res1 ["__res1"]: u32;
            13:13 => ptw, set_ptw ["PTW"]: u32;
            15:14 => psu, set_psu ["PSU"]: u32;
            19:16 => mem_attr, set_mem_attr ["MemAttr"]: u32;
            20:20 => res2, set_res2 ["__res2"]: u32;
            23:22 => shcfg, set_shcfg ["SHCFG"]: u32;
            25:24 => racfg, set_racfg ["RACFG"]: u32;
            27:26 => wacfg, set_wacfg ["WACFG"]: u32;
            31:28 => res3, set_res3 ["__res3"]: u32;
        }
    }

    define_smmu_reg! {
        /// Stage 2 Translation Table Base Register 0 (`SMMU_CBn_TTBR0`, offset `0x20`).
        pub struct Ttbr0(u64) @ 0x20 => {
            2:0   => [rsvdz];
            47:3  => base_address, set_base_address ["BaseAddress"]: u64;
            63:48 => res1, set_res1 ["__res1"]: u64;
        }
    }

    impl Ttbr0 {
        /// Sets `BaseAddress` from an unshifted 64-bit physical base address (`base_addr >> 3`).
        #[inline]
        pub fn set_base_addr_from_value(&mut self, base_address_value: u64) -> &mut Self {
            self.set_base_address(base_address_value >> 3)
        }

        /// Returns the unshifted 64-bit physical base address (`BaseAddress << 3`).
        #[inline]
        pub const fn base_addr_value(&self) -> u64 {
            self.base_address() << 3
        }
    }

    // Section 17.3.8: Translation Control Register.
    //
    // There are two possible formats for the stage-2 TCR based on the value of `CBA2R.VA64`:
    // + `CBA2R.VA64 == 0` : Extended 32-bit addressing (`TcrExt32Bit`)
    // + `CBA2R.VA64 == 1` : 64-bit addressing (`Tcr64Bit`)
    define_smmu_reg! {
        /// Stage 2 Translation Control Register, AArch32 Long-descriptor (`SMMU_CBn_TCR`, offset
        /// `0x30`).
        pub struct TcrExt32Bit(u32) @ 0x30 => {
            3:0   => t0sz, set_t0sz ["T0SZ"]: u32;
            5:4   => res1, set_res1 ["__res1"]: u32;
            7:6   => sl0, set_sl0 ["SL0"]: u32;
            9:8   => irgn0, set_irgn0 ["IRGN0"]: enum TcrIrg0;
            11:10 => orgn0, set_orgn0 ["ORGN0"]: enum TcrOrg0;
            13:12 => sh0, set_sh0 ["SH0"]: enum TcrSh0;
            30:14 => res2, set_res2 ["__res2"]: u32;
            31:31 => eae, set_eae ["EAE"]: u32;
        }
    }

    define_smmu_reg! {
        /// Stage 2 Translation Control Register, AArch64 (`SMMU_CBn_TCR`, offset `0x30`).
        pub struct Tcr64Bit(u32) @ 0x30 => {
            5:0   => t0sz, set_t0sz ["T0SZ"]: u32;
            7:6   => sl0, set_sl0 ["SL0"]: u32;
            9:8   => irgn0, set_irgn0 ["IRGN0"]: enum TcrIrg0;
            11:10 => orgn0, set_orgn0 ["ORGN0"]: enum TcrOrg0;
            13:12 => sh0, set_sh0 ["SH0"]: enum TcrSh0;
            15:14 => tg0, set_tg0 ["TG0"]: enum TcrTg0;
            18:16 => pa_size, set_pa_size ["PASize"]: enum TcrIps;
            20:19 => [rsvdz];
            21:21 => ha, set_ha ["HA"]: u32;
            22:22 => hb, set_hb ["HB"]: u32;
            31:23 => [rsvdz];
        }
    }

    define_smmu_reg! {
        /// Stage 2 Fault Syndrome Register 0 (`SMMU_CBn_FSYNR0`, offset `0x68`).
        pub struct Fsynr0(u32) @ 0x68 => {
            1:0   => plvl, set_plvl ["PLVL"]: u32;
            2:2   => nested, set_nested ["NESTED"]: u32;
            3:3   => s1ptwf, set_s1ptwf ["S1PTWF"]: u32;
            4:4   => wnr, set_wnr ["WNR"]: u32;
            5:5   => pnu, set_pnu ["PNU"]: u32;
            6:6   => ind, set_ind ["IND"]: u32;
            7:7   => unknown, set_unknown ["UNKNOWN"]: u32;
            8:8   => nsattr, set_nsattr ["NSATTR"]: u32;
            9:9   => atof, set_atof ["ATOF"]: u32;
            10:10 => ptwf, set_ptwf ["PTWF"]: u32;
            11:11 => afr, set_afr ["AFR"]: u32;
            15:12 => res1, set_res1 ["__res1"]: u32;
            23:16 => s1cbndx, set_s1cbndx ["S1CBNDX"]: u32;
            31:24 => res2, set_res2 ["__res2"]: u32;
        }
    }

    // Section 17.3.2: IPA Fault Address Register.
    // See the notes for `s1cbr::Far`. The same bit definition limitations apply here as well.
    define_smmu_reg! {
        /// IPA Fault Address Register (`SMMU_CBn_IPAFAR`, offset `0x70`).
        pub struct Ipafar(u64) @ 0x70 => {
            63:0 => faddr, set_faddr ["FADDR"]: u64;
        }
    }

    define_smmu_reg! {
        /// Invalidate TLB by IPA (`SMMU_CBn_TLBIIPAS2`, offset `0x630`).
        pub struct Tlbiipas2(u64) @ 0x630 => {
            35:0  => address_47_12, set_address_47_12 ["Address_47_12"]: u64;
            63:36 => res1, set_res1 ["__res1"]: u64;
        }
    }

    define_smmu_reg! {
        /// Invalidate TLB by IPA, Last Level (`SMMU_CBn_TLBIIPAS2L`, offset `0x638`).
        pub struct Tlbiipas2l(u64) @ 0x638 => {
            35:0  => address_47_12, set_address_47_12 ["Address_47_12"]: u64;
            63:36 => res1, set_res1 ["__res1"]: u64;
        }
    }
}
