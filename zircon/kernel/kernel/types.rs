// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PAddr(pub usize);

impl From<PAddr> for u64 {
    #[inline]
    fn from(paddr: PAddr) -> u64 {
        paddr.0 as u64
    }
}

impl From<u64> for PAddr {
    #[inline]
    fn from(addr: u64) -> PAddr {
        PAddr(addr as usize)
    }
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VAddr(pub usize);

impl From<VAddr> for u64 {
    #[inline]
    fn from(vaddr: VAddr) -> u64 {
        vaddr.0 as u64
    }
}

impl From<u64> for VAddr {
    #[inline]
    fn from(addr: u64) -> VAddr {
        VAddr(addr as usize)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Koid(pub u64);

#[allow(non_camel_case_types)]
pub type cpu_mask_t = u32;

#[allow(non_camel_case_types)]
pub type cpu_num_t = u32;

pub const INVALID_CPU: cpu_num_t = u32::MAX;
pub const CPU_MASK_ALL: cpu_mask_t = u32::MAX;
