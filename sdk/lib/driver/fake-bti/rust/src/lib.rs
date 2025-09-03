// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![warn(missing_docs)]

//! Fake BTI for testing without access to hardware resources

use std::ops::Deref;

use zx::AsHandleRef;
use zx::sys::{zx_handle_t, zx_paddr_t};

mod ffi;

/// A fake BTI object which can be created without having access to hardware resources.
#[derive(Debug)]
pub struct FakeBti(zx::Bti);

impl Deref for FakeBti {
    type Target = zx::Bti;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl FakeBti {
    /// Creates a new FakeBti.
    pub fn create() -> Result<Self, zx::Status> {
        let handle = {
            let mut raw_handle = zx_handle_t::default();
            // SAFETY: `raw_handle` is valid
            unsafe {
                zx::ok(ffi::fake_bti_create(&mut raw_handle))?;
            }
            // SAFETY: `fake_bti_create` returned a valid handle on success
            unsafe { zx::Handle::from_raw(raw_handle) }
        };
        Ok(Self(handle.into()))
    }

    /// Sets the paddrs used by the fake BTI.  Whenever [`zx::Bti::pin`] is called, these static
    /// paddrs will be written out into the resulting array.  If more physical addresses are needed
    /// than are available in paddrs, `pin` will return an error.
    pub fn set_paddrs(&self, paddrs: &[zx_paddr_t]) {
        // SAFETY: `paddrs` is valid
        unsafe {
            // OK to unwrap -- we know that `self.0` is a valid handle to a fake BTI
            zx::ok(ffi::fake_bti_set_paddrs(self.0.raw_handle(), paddrs.as_ptr(), paddrs.len()))
                .unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn create() {
        let bti = FakeBti::create().expect("create failed");
        let info = bti.info().expect("info failed");
        assert!(info.minimum_contiguity > 0);
    }

    #[fuchsia::test]
    fn pin_vmo() {
        let bti = FakeBti::create().expect("create failed");
        let vmo = zx::Vmo::create(16384).expect("create VMO failed");
        let mut paddrs = [zx_paddr_t::default(); 4];
        let _pmt = bti
            .pin(zx::BtiOptions::PERM_READ, &vmo, 0, 16384, &mut paddrs[..])
            .expect("pin failed");
        assert_eq!(paddrs, [4096, 4096, 4096, 4096]);
    }

    #[fuchsia::test]
    fn create_and_pin_contiguous_vmo() {
        let bti = FakeBti::create().expect("create failed");
        let vmo = zx::Vmo::create_contiguous(&bti, 16384, 0).expect("create contiguous VMO failed");
        let mut paddr = [zx_paddr_t::default()];
        let _pmt = bti
            .pin(
                zx::BtiOptions::PERM_READ | zx::BtiOptions::CONTIGUOUS,
                &vmo,
                0,
                16384,
                &mut paddr[..],
            )
            .expect("pin failed");
        assert_eq!(paddr, [4096]);
    }

    #[fuchsia::test]
    fn pin_with_paddrs() {
        let bti = FakeBti::create().expect("create failed");
        bti.set_paddrs(&[4096, 16384, 65536]);
        let vmo = zx::Vmo::create(3 * 4096).expect("create VMO failed");
        {
            let mut paddrs = [zx_paddr_t::default(); 3];
            let _pmt = bti
                .pin(zx::BtiOptions::PERM_READ, &vmo, 0, 3 * 4096, &mut paddrs[..])
                .expect("pin failed");
            assert_eq!(paddrs, [4096, 16384, 65536]);
        }
        bti.set_paddrs(&[4096, 8192]);
        {
            let mut paddrs = [zx_paddr_t::default(); 2];
            let _pmt = bti
                .pin(zx::BtiOptions::PERM_READ, &vmo, 0, 2 * 4096, &mut paddrs[..])
                .expect("pin failed");
            assert_eq!(paddrs, [4096, 8192]);
        }
    }
}
