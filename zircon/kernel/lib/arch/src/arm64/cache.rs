// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bitrs::{bitfield_repr, layout};
use regio::arm64::{SysReg, spec};

/// [arm/v8]: D13.2.33  CTR_EL0, Cache Type Register.
pub const CTR_EL0: SysReg<spec::CTR_EL0, CacheTypeRegister> = SysReg::new();

/// L1 instruction cache policy.
#[bitfield_repr(u8)]
#[derive(Clone, Copy)]
pub enum L1ICachePolicy {
    Vpipt = 0b00,
    Aivivt = 0b01,
    Vipt = 0b10,
    Pipt = 0b11,
}

layout!({
    /// The layout of [`CTR_EL0`].
    pub struct CacheTypeRegister(u64);
    {
        let __ @ 63..38;
        let tmin_line @ 37..32;
        let __ @ 31 = 1;
        let __ @ 30;
        let dic @ 29;
        let idc @ 28;
        let cwg @ 27..24;
        let erg @ 23..20;

        /// log2 of the number of words in the smallest data cache line.
        let dmin_line @ 19..16;

        let l1_ip @ 15..14: L1ICachePolicy;
        let __ @ 13..4;

        /// log2 of the number of words in the smallest instruction cache line.
        let imin_line @ 3..0;
    }
});

impl CacheTypeRegister {
    /// Returns the smallest data cache line size in bytes.
    pub const fn dcache_line_size(&self) -> usize {
        (1 << self.dmin_line()) * size_of::<u32>()
    }

    /// Returns the smallest instruction cache line size in bytes.
    pub const fn icache_line_size(&self) -> usize {
        (1 << self.imin_line()) * size_of::<u32>()
    }
}

/// [arm/v8]: D13.2.36  DCZID_EL0, Data Cache Zero ID register
pub const DCZID_EL0: SysReg<spec::DCZID_EL0, DataCacheZeroIdRegister> = SysReg::new();

layout!({
    /// The layout of [`DCZID_EL0`].
    pub struct DataCacheZeroIdRegister(u64);
    {
        let __ @ 63..5;
        let dzp @ 4;
        let bz @ 3..0;
    }
});

impl DataCacheZeroIdRegister {
    /// Returns the block size for DC ZVA in bytes.
    pub fn zva_line_size(&self) -> usize {
        (1 << self.bz()) * size_of::<u32>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_type_el0() {
        let ctr = *CacheTypeRegister::new().set_dmin_line(4).set_imin_line(4);
        assert_eq!(ctr.dcache_line_size(), 64);
        assert_eq!(ctr.icache_line_size(), 64);
    }

    #[test]
    fn data_cache_zero_id_el0() {
        let dczid = *DataCacheZeroIdRegister::new().set_bz(4);
        assert_eq!(dczid.zva_line_size(), 64);
    }
}
