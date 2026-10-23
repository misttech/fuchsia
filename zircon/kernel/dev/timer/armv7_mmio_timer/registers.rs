// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ptr::with_exposed_provenance_mut;
use regio::{LayoutOver, MmioBank, MmioPtr, Offset, RwSafe};

/// Memory-mapped register region wrapper backed by [`MmioBank`].
#[derive(Clone, Copy, Default)]
pub struct RegisterMmio {
    base: usize,
    size: usize,
}

impl RegisterMmio {
    /// Creates an empty, unmapped `RegisterMmio`.
    pub const fn empty() -> Self {
        Self { base: 0, size: 0 }
    }

    /// Creates a new `RegisterMmio` covering `size` bytes at `base`.
    ///
    /// # Safety
    ///
    /// `base` must be non-zero, 8-byte aligned, and point to a valid mapped MMIO region of at
    /// least `size` bytes that remains valid for the lifetime of the `RegisterMmio` and any
    /// subviews.
    pub const unsafe fn new(base: usize, size: usize) -> Self {
        assert!(base != 0 && base.is_multiple_of(core::mem::align_of::<u64>()));
        Self { base, size }
    }

    /// Returns the base virtual address of this MMIO region, or `0` if unmapped.
    pub const fn base(&self) -> usize {
        self.base
    }

    /// Creates a subview at `offset` with `size` bytes.
    pub const fn subview(&self, offset: u32, size: usize) -> Self {
        let offset = offset as usize;
        assert!(self.base != 0 && offset.is_multiple_of(core::mem::align_of::<u64>()));
        assert!(offset <= self.size && size <= self.size - offset);
        Self { base: self.base + offset, size }
    }

    fn bank32(&self) -> MmioBank<u32, RwSafe> {
        assert!(self.base != 0);
        // SAFETY: `self.base` was attested to be a valid mapped MMIO region of `self.size` bytes
        // when `RegisterMmio::new` was called.
        let ptr = unsafe { MmioPtr::<u32, RwSafe>::new(with_exposed_provenance_mut(self.base)) };
        MmioBank::new(ptr, self.size)
    }

    fn bank64(&self) -> MmioBank<u64, RwSafe> {
        assert!(self.base != 0);
        // SAFETY: `self.base` was attested to be a valid, 8-byte aligned mapped MMIO region of
        // `self.size` bytes when `RegisterMmio::new` was called.
        let ptr = unsafe { MmioPtr::<u64, RwSafe>::new(with_exposed_provenance_mut(self.base)) };
        MmioBank::new(ptr, self.size)
    }

    /// Reads a 32-bit register at `reg`.
    pub fn read32<L: LayoutOver<u32>>(&self, reg: Offset<L, RwSafe>) -> L {
        // SAFETY: `RegisterMmio::new` and `subview` guarantee that `[self.base, self.base +
        // self.size)` is a valid, mapped MMIO region, and `MmioBank::at` verifies alignment and
        // bounds.
        unsafe { self.bank32().at(reg) }.read()
    }

    /// Writes a 32-bit register at `reg`.
    pub fn write32<L: LayoutOver<u32>>(&self, reg: Offset<L, RwSafe>, val: L) {
        // SAFETY: `RegisterMmio::new` and `subview` guarantee that `[self.base, self.base +
        // self.size)` is a valid, mapped MMIO region, and `MmioBank::at` verifies alignment and
        // bounds.
        unsafe { self.bank32().at(reg) }.write(val);
    }

    /// Reads a 64-bit register at `reg`.
    pub fn read64<L: LayoutOver<u64>>(&self, reg: Offset<L, RwSafe>) -> L {
        // SAFETY: `RegisterMmio::new` and `subview` guarantee that `[self.base, self.base +
        // self.size)` is a valid, 8-byte aligned mapped MMIO region, and `MmioBank::at` verifies
        // alignment and bounds.
        unsafe { self.bank64().at(reg) }.read()
    }

    /// Writes a 64-bit register at `reg`.
    pub fn write64<L: LayoutOver<u64>>(&self, reg: Offset<L, RwSafe>, val: L) {
        // SAFETY: `RegisterMmio::new` and `subview` guarantee that `[self.base, self.base +
        // self.size)` is a valid, 8-byte aligned mapped MMIO region, and `MmioBank::at` verifies
        // alignment and bounds.
        unsafe { self.bank64().at(reg) }.write(val);
    }
}

/// Size of each 4 KiB MMIO timer register frame.
pub const MMIO_FRAME_SIZE: usize = 0x1000;

/// Size of the counter sub-region (`CT`).
pub const COUNTER_SUBVIEW_SIZE: usize = 0x08;

/// Size of the timer IRQ sub-region (`CVAL`, `TVAL`, `CTL`).
pub const TIMER_SUBVIEW_SIZE: usize = 0x10;

/// Trait for register values that can be formatted by `debug::dump_reg`.
pub trait PrintableReg: Copy {
    const HEX_WIDTH: usize;
    const DIGITS: u32;
    const PRINTER_ENABLED: bool;

    fn read_from(mmio: &RegisterMmio, reg: Offset<Self, RwSafe>) -> Self;
    fn reg_value(&self) -> u64;
    fn for_each_field(&self, cb: impl FnMut(&str, u64, u32, u32));
}

impl PrintableReg for u32 {
    const HEX_WIDTH: usize = (u32::BITS / 4) as usize;
    const DIGITS: u32 = u32::BITS;
    const PRINTER_ENABLED: bool = false;

    fn read_from(mmio: &RegisterMmio, reg: Offset<Self, RwSafe>) -> Self {
        mmio.read32(reg)
    }

    fn reg_value(&self) -> u64 {
        u64::from(*self)
    }

    fn for_each_field(&self, _cb: impl FnMut(&str, u64, u32, u32)) {}
}

impl PrintableReg for u64 {
    const HEX_WIDTH: usize = (u64::BITS / 4) as usize;
    const DIGITS: u32 = u64::BITS;
    const PRINTER_ENABLED: bool = false;

    fn read_from(mmio: &RegisterMmio, reg: Offset<Self, RwSafe>) -> Self {
        mmio.read64(reg)
    }

    fn reg_value(&self) -> u64 {
        *self
    }

    fn for_each_field(&self, _cb: impl FnMut(&str, u64, u32, u32)) {}
}

macro_rules! impl_u32_reg_newtype {
    ($ty:ident, [ $( ($name:literal, $hi:expr, $lo:expr) ),* $(,)? ]) => {
        impl From<u32> for $ty {
            fn from(val: u32) -> Self {
                Self(val)
            }
        }

        impl From<$ty> for u32 {
            fn from(val: $ty) -> Self {
                val.0
            }
        }

        impl PrintableReg for $ty {
            const HEX_WIDTH: usize = (u32::BITS / 4) as usize;
            const DIGITS: u32 = u32::BITS;
            const PRINTER_ENABLED: bool = true;

            fn read_from(mmio: &RegisterMmio, reg: Offset<Self, RwSafe>) -> Self {
                mmio.read32(reg)
            }

            fn reg_value(&self) -> u64 {
                u64::from(self.0)
            }

            fn for_each_field(&self, mut cb: impl FnMut(&str, u64, u32, u32)) {
                $(
                    {
                        let width = $hi - $lo + 1;
                        let mask = if width == 32 { u32::MAX } else { (1u32 << width) - 1 };
                        let val = (self.0 >> $lo) & mask;
                        cb($name, u64::from(val), $hi, $lo);
                    }
                )*
            }
        }
    };
}

// Section I2.3.1 of the Arm Architecture Reference Manual ARM DDI 0487K.a
pub mod cnt_ctl_base {
    use super::*;

    // Section I6.7.7
    pub const CNTFRQ_ADDR: u32 = 0x00;
    pub const CNTFRQ: Offset<u32, RwSafe> = Offset::new(CNTFRQ_ADDR as usize);

    // Section I6.7.9
    pub const CNTNSAR_ADDR: u32 = 0x04;
    pub const CNTNSAR: Offset<CntNsar, RwSafe> = Offset::new(CNTNSAR_ADDR as usize);

    #[repr(transparent)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct CntNsar(pub u32);

    impl_u32_reg_newtype!(
        CntNsar,
        [
            ("NS0", 0, 0),
            ("NS1", 1, 1),
            ("NS2", 2, 2),
            ("NS3", 3, 3),
            ("NS4", 4, 4),
            ("NS5", 5, 5),
            ("NS6", 6, 6),
            ("NS7", 7, 7),
        ]
    );

    // Section I6.7.16
    pub const CNTTIDR_ADDR: u32 = 0x08;
    pub const CNTTIDR: Offset<CntTidr, RwSafe> = Offset::new(CNTTIDR_ADDR as usize);

    #[repr(transparent)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct CntTidr(pub u32);

    impl_u32_reg_newtype!(
        CntTidr,
        [
            ("Frame0", 3, 0),
            ("Frame1", 7, 4),
            ("Frame2", 11, 8),
            ("Frame3", 15, 12),
            ("Frame4", 19, 16),
            ("Frame5", 23, 20),
            ("Frame6", 27, 24),
            ("Frame7", 31, 28),
        ]
    );

    impl CntTidr {
        pub const fn bits(self) -> u32 {
            self.0
        }

        pub const fn frame_bits(self, ndx: u32) -> u32 {
            (self.0 >> (ndx * 4)) & 0xF
        }

        pub const fn implemented(self, ndx: u32) -> bool {
            (self.frame_bits(ndx) & 0x1) != 0
        }

        pub const fn virt_impl(self, ndx: u32) -> bool {
            (self.frame_bits(ndx) & 0x2) != 0
        }

        pub const fn el0_impl(self, ndx: u32) -> bool {
            (self.frame_bits(ndx) & 0x2) != 0
        }
    }

    // Section I6.7.1
    pub const CNTACR_REG_COUNT: u32 = 8;
    pub const CNTACR_ADDR: u32 = 0x40;

    pub const fn cnt_acr(ndx: u32) -> Offset<CntAcr, RwSafe> {
        Offset::new((CNTACR_ADDR + (ndx << 2)) as usize)
    }

    #[repr(transparent)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct CntAcr(pub u32);

    impl_u32_reg_newtype!(
        CntAcr,
        [
            ("RPCT", 0, 0),
            ("RVCT", 1, 1),
            ("RFRQ", 2, 2),
            ("RVOFF", 3, 3),
            ("RWVT", 4, 4),
            ("RWPT", 5, 5),
        ]
    );

    impl CntAcr {
        pub const fn rpct(self) -> bool {
            (self.0 & (1 << 0)) != 0
        }

        pub const fn rvct(self) -> bool {
            (self.0 & (1 << 1)) != 0
        }

        pub const fn rfrq(self) -> bool {
            (self.0 & (1 << 2)) != 0
        }

        pub const fn rvoff(self) -> bool {
            (self.0 & (1 << 3)) != 0
        }

        pub const fn rwvt(self) -> bool {
            (self.0 & (1 << 4)) != 0
        }

        pub const fn rwpt(self) -> bool {
            (self.0 & (1 << 5)) != 0
        }
    }

    // Section I6.7.22
    pub const CNTVOFF_REG_COUNT: u32 = 8;
    pub const CNTVOFF_ADDR: u32 = 0x80;

    pub const fn cnt_voff(ndx: u32) -> Offset<u64, RwSafe> {
        Offset::new((CNTVOFF_ADDR + (ndx << 3)) as usize)
    }

    // Section I6.7.23
    pub const COUNTER_ID_REG_COUNT: u32 = 12;
    pub const COUNTER_ID_ADDR: u32 = 0xFD0;

    pub const fn counter_id(ndx: u32) -> Offset<u32, RwSafe> {
        Offset::new((COUNTER_ID_ADDR + (ndx << 2)) as usize)
    }
}

// Section I6.7.10 (PCT) or Section I6.7.17 (VCT)
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CntCtl(pub u32);

impl_u32_reg_newtype!(CntCtl, [("ENABLE", 0, 0), ("IMASK", 1, 1), ("ISTATUS", 2, 2)]);

impl CntCtl {
    pub const fn enable(self) -> bool {
        (self.0 & (1 << 0)) != 0
    }

    pub const fn set_enable(mut self, val: bool) -> Self {
        if val {
            self.0 |= 1 << 0;
        } else {
            self.0 &= !(1 << 0);
        }
        self
    }

    pub const fn imask(self) -> bool {
        (self.0 & (1 << 1)) != 0
    }

    pub const fn set_imask(mut self, val: bool) -> Self {
        if val {
            self.0 |= 1 << 1;
        } else {
            self.0 &= !(1 << 1);
        }
        self
    }

    pub const fn istatus(self) -> bool {
        (self.0 & (1 << 2)) != 0
    }
}

// Section I2.3.2 of the Arm Architecture Reference Manual ARM DDI 0487K.a
pub mod cnt_base {
    use super::*;

    // Section I6.7.13
    pub const CNTPCT_ADDR: u32 = 0x00;
    pub const CNTPCT: Offset<u64, RwSafe> = Offset::new(CNTPCT_ADDR as usize);

    // Section I6.7.20
    pub const CNTVCT_ADDR: u32 = 0x08;
    pub const CNTVCT: Offset<u64, RwSafe> = Offset::new(CNTVCT_ADDR as usize);

    // Section I6.7.7
    pub const CNTFRQ_ADDR: u32 = 0x10;
    pub const CNTFRQ: Offset<u32, RwSafe> = Offset::new(CNTFRQ_ADDR as usize);

    // Section I6.7.4
    pub const CNTEL0ACR_ADDR: u32 = 0x14;
    pub const CNTEL0ACR: Offset<CntEl0Acr, RwSafe> = Offset::new(CNTEL0ACR_ADDR as usize);

    #[repr(transparent)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct CntEl0Acr(pub u32);

    impl_u32_reg_newtype!(
        CntEl0Acr,
        [("EL0PCTEN", 0, 0), ("EL0VCTEN", 1, 1), ("EL0VTEN", 8, 8), ("EL0PTEN", 9, 9)]
    );

    impl CntEl0Acr {
        pub const fn el0pcten(self) -> bool {
            (self.0 & (1 << 0)) != 0
        }

        pub const fn el0vcten(self) -> bool {
            (self.0 & (1 << 1)) != 0
        }

        pub const fn el0vten(self) -> bool {
            (self.0 & (1 << 8)) != 0
        }

        pub const fn el0pten(self) -> bool {
            (self.0 & (1 << 9)) != 0
        }
    }

    // Section I6.7.21
    pub const CNTVOFF_ADDR: u32 = 0x18;
    pub const CNTVOFF: Offset<u64, RwSafe> = Offset::new(CNTVOFF_ADDR as usize);

    // Section I6.7.11
    pub const CNTP_CVAL_ADDR: u32 = 0x20;
    pub const CNTP_CVAL: Offset<u64, RwSafe> = Offset::new(CNTP_CVAL_ADDR as usize);

    // Section I6.7.7
    pub const CNTP_TVAL_ADDR: u32 = 0x28;
    pub const CNTP_TVAL: Offset<u32, RwSafe> = Offset::new(CNTP_TVAL_ADDR as usize);

    // Section I6.7.10
    pub const CNTP_CTL_ADDR: u32 = 0x2c;
    pub const CNTP_CTL: Offset<CntCtl, RwSafe> = Offset::new(CNTP_CTL_ADDR as usize);

    // Section I6.7.18
    pub const CNTV_CVAL_ADDR: u32 = 0x30;
    pub const CNTV_CVAL: Offset<u64, RwSafe> = Offset::new(CNTV_CVAL_ADDR as usize);

    // Section I6.7.19
    pub const CNTV_TVAL_ADDR: u32 = 0x38;
    pub const CNTV_TVAL: Offset<u32, RwSafe> = Offset::new(CNTV_TVAL_ADDR as usize);

    // Section I6.7.17
    pub const CNTV_CTL_ADDR: u32 = 0x3c;
    pub const CNTV_CTL: Offset<CntCtl, RwSafe> = Offset::new(CNTV_CTL_ADDR as usize);

    // Section I6.7.23
    pub const COUNTER_ID_REG_COUNT: u32 = 12;
    pub const COUNTER_ID_ADDR: u32 = 0xFD0;

    pub const fn counter_id(ndx: u32) -> Offset<u32, RwSafe> {
        Offset::new((COUNTER_ID_ADDR + (ndx << 2)) as usize)
    }
}

// Section I2.3.2 of the Arm Architecture Reference Manual ARM DDI 0487K.a
pub mod cnt_el0_base {
    use super::*;

    // Section I6.7.13
    pub const CNTPCT_ADDR: u32 = 0x00;
    pub const CNTPCT: Offset<u64, RwSafe> = Offset::new(CNTPCT_ADDR as usize);

    // Section I6.7.20
    pub const CNTVCT_ADDR: u32 = 0x08;
    pub const CNTVCT: Offset<u64, RwSafe> = Offset::new(CNTVCT_ADDR as usize);

    // Section I6.7.7
    pub const CNTFRQ_ADDR: u32 = 0x10;
    pub const CNTFRQ: Offset<u32, RwSafe> = Offset::new(CNTFRQ_ADDR as usize);

    // Section I6.7.11
    pub const CNTP_CVAL_ADDR: u32 = 0x20;
    pub const CNTP_CVAL: Offset<u64, RwSafe> = Offset::new(CNTP_CVAL_ADDR as usize);

    // Section I6.7.7
    pub const CNTP_TVAL_ADDR: u32 = 0x28;
    pub const CNTP_TVAL: Offset<u32, RwSafe> = Offset::new(CNTP_TVAL_ADDR as usize);

    // Section I6.7.10
    pub const CNTP_CTL_ADDR: u32 = 0x2c;
    pub const CNTP_CTL: Offset<CntCtl, RwSafe> = Offset::new(CNTP_CTL_ADDR as usize);

    // Section I6.7.18
    pub const CNTV_CVAL_ADDR: u32 = 0x30;
    pub const CNTV_CVAL: Offset<u64, RwSafe> = Offset::new(CNTV_CVAL_ADDR as usize);

    // Section I6.7.19
    pub const CNTV_TVAL_ADDR: u32 = 0x38;
    pub const CNTV_TVAL: Offset<u32, RwSafe> = Offset::new(CNTV_TVAL_ADDR as usize);

    // Section I6.7.17
    pub const CNTV_CTL_ADDR: u32 = 0x3c;
    pub const CNTV_CTL: Offset<CntCtl, RwSafe> = Offset::new(CNTV_CTL_ADDR as usize);

    // Section I6.7.23
    pub const COUNTER_ID_REG_COUNT: u32 = 12;
    pub const COUNTER_ID_ADDR: u32 = 0xFD0;

    pub const fn counter_id(ndx: u32) -> Offset<u32, RwSafe> {
        Offset::new((COUNTER_ID_ADDR + (ndx << 2)) as usize)
    }
}

// In each of the timer frames, there are two sets of identical registers at
// different offsets:
//
// 1) a compare register
// 2) a "timer" (countdown) register, and
// 3) a control register.
//
// One set of these registers operates against the PCT counter, while the other
// operates against the VCT counter.  Since they are at different offsets, they
// need to have different names and different offsets encoded in their register
// definition.
//
// This binding of two otherwise identical sets of registers to different types
// (differing only by offset) makes is a bit difficult to write a single generic
// "timer control" struct, forcing us to parameterize the type to account for
// the offset differences.
//
// Instead of doing this, we go ahead give a generic definition which can be
// used for either the PCT or VCT timer registers starting at offset zero,
// allowing us to handle the offset at runtime via the `RegisterMmio` struct,
// allowing us to have a single type for the timer hardware, regardless of
// whether it is the PCT or VCT hardware.
pub mod cnt_pv_regs {
    use super::*;

    // Section I6.7.11 (PCT) or Section I6.7.18 (VCT)
    pub const CVAL_ADDR: u32 = 0x00;
    pub const CVAL: Offset<u64, RwSafe> = Offset::new(CVAL_ADDR as usize);

    // Section I6.7.7 (PCT) or Section I6.7.19 (VCT)
    pub const TVAL_ADDR: u32 = 0x08;
    pub const TVAL: Offset<u32, RwSafe> = Offset::new(TVAL_ADDR as usize);

    // Section I6.7.10 (PCT) or Section I6.7.17 (VCT)
    pub const CTL_ADDR: u32 = 0x0c;
    pub const CTL: Offset<CntCtl, RwSafe> = Offset::new(CTL_ADDR as usize);

    // Likewise, the counter itself is at offset 0x0 for the PCT counter, and 0x8
    // for the VCT counter. Define a generic version so we can handle the difference
    // at runtime a bit more easily.
    //
    // Section I6.7.13 (PCT) or Section I6.7.20 (VCT)
    pub const CT_ADDR: u32 = 0x00;
    pub const CT: Offset<u64, RwSafe> = Offset::new(CT_ADDR as usize);
}
