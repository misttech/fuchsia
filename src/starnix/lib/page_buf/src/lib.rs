// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Buffers backed by VMOs mapped into the root VMAR.

use std::mem::MaybeUninit;
use std::ptr::NonNull;
use zx::AsHandleRef;

const DEFAULT_VMO_NAME: zx::Name = zx::Name::from_bytes_lossy(b"starnix_page_buf");

/// A buffer backed by a VMO mapped into the root VMAR.
#[derive(Debug)]
pub struct PageBuf<T> {
    vmo: zx::Vmo,
    base: NonNull<MaybeUninit<u8>>,
    mapped_len: usize,
    _ty: std::marker::PhantomData<T>,
}

impl<T> PageBuf<T> {
    /// Create a new `PageBuf` in the calling process' root VMAR.
    pub fn new(capacity: usize) -> Result<Self, zx::Status> {
        let capacity_bytes = capacity * std::mem::size_of::<T>();
        let vmo = zx::Vmo::create(capacity_bytes as u64)?;

        let mapped_len = capacity_bytes.next_multiple_of(zx::system_get_page_size() as usize);
        let addr = fuchsia_runtime::vmar_root_self().map(
            0,
            &vmo,
            0,
            mapped_len,
            zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE | zx::VmarFlags::ALLOW_FAULTS,
        )?;
        let base =
            NonNull::new(std::ptr::with_exposed_provenance_mut::<MaybeUninit<u8>>(addr)).unwrap();

        let this = Self { vmo, base, mapped_len, _ty: std::marker::PhantomData };
        this.set_name(&DEFAULT_VMO_NAME);
        Ok(this)
    }

    /// Set the name of the buffer's VMO.
    pub fn set_name(&self, name: &zx::Name) {
        self.vmo.set_name(name).expect("default vmo rights must include ability to set name");
    }

    pub fn len(&self) -> usize {
        self.mapped_len / std::mem::size_of::<T>()
    }

    /// Return a mutable reference to the underlying memory.
    pub fn as_mut(&mut self) -> &mut [MaybeUninit<T>] {
        assert!(
            std::mem::align_of::<T>() <= zx::system_get_page_size() as usize,
            "can't handle types with alignment greater than a page yet"
        );

        // SAFETY: The base address is always valid to access as MaybeUninit as long as self is
        // live. The mutable reference receiver of this method ensures that we have exclusive
        // access to the underlying mapping.
        let bytes = unsafe { std::slice::from_raw_parts_mut(self.base.as_ptr(), self.mapped_len) };
        let num_elems = bytes.len() / std::mem::size_of::<T>();
        let bytes_as_t = bytes.as_mut_ptr().cast::<MaybeUninit<T>>();

        // SAFETY: The whole MaybeUninit<T> doesn't have any requirements on the state of the
        // backing memory and `num_elems` will produce a slice within the bounds of `bytes`.
        unsafe { std::slice::from_raw_parts_mut(bytes_as_t, num_elems) }
    }
}

// SAFETY: PageBuf can be dropped from any thread if T is Send
unsafe impl<T: Send> Send for PageBuf<T> {}
// SAFETY: no interior mutability if T is Sync
unsafe impl<T: Sync> Sync for PageBuf<T> {}

impl<T> Drop for PageBuf<T> {
    fn drop(&mut self) {
        // SAFETY: the zircon mapping is "owned" by this PageBuf, no other references to it exist
        unsafe {
            fuchsia_runtime::vmar_root_self()
                .unmap(self.base.addr().into(), self.mapped_len)
                .unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_runtime::vmar_root_self;
    use zx::AsHandleRef;

    #[track_caller]
    fn fill_buf(buf: &mut PageBuf<[u8; 16]>) {
        for slot in buf.as_mut() {
            slot.write([1u8; 16]);
        }
    }

    #[track_caller]
    fn find_zx_mappings(
        infos: &[zx::MapInfo],
        addr_range: std::ops::Range<usize>,
    ) -> Vec<(zx::Name, std::ops::Range<usize>, zx::MappingDetails)> {
        let mut found = vec![];
        for info in infos {
            let info_range = info.base..info.base + info.size;
            if addr_range.contains(&info_range.start) || addr_range.contains(&info_range.end) {
                if let Some(mapping) = info.details().as_mapping() {
                    found.push((info.name, info_range, mapping.clone()));
                }
            }
        }

        found.sort_by_key(|m| m.1.start);
        found
    }

    #[fuchsia::test]
    fn basic() {
        let mut buf = PageBuf::<[u8; 16]>::new(100).unwrap();
        assert!(buf.len() >= 100);
        fill_buf(&mut buf);

        let buf_base = buf.base.addr().get();
        let buf_len = buf.mapped_len;
        let maps = vmar_root_self().info_maps_vec().unwrap();
        let desired_range = buf_base..buf_base + buf_len;
        let (name, range, details) = &find_zx_mappings(&maps, desired_range.clone())[0];

        assert_eq!(name, &DEFAULT_VMO_NAME);
        assert_eq!(range, &desired_range);
        assert_eq!(details.vmo_koid, buf.vmo.get_koid().unwrap());

        // Verify unmapping on drop
        drop(buf);
        let maps = vmar_root_self().info_maps_vec().unwrap();
        assert_eq!(find_zx_mappings(&maps, desired_range), vec![]);
    }

    #[fuchsia::test]
    fn setting_name_works() {
        let buf = PageBuf::<[u8; 16]>::new(100).unwrap();
        let vmo_name = zx::Name::from_bytes_lossy(b"setting_name_works");
        buf.set_name(&vmo_name);

        let buf_base = buf.base.addr().get();
        let buf_len = buf.mapped_len;
        let maps = vmar_root_self().info_maps_vec().unwrap();
        let desired_range = buf_base..buf_base + buf_len;
        let (name, _range, _details) = &find_zx_mappings(&maps, desired_range.clone())[0];

        assert_eq!(name, &vmo_name);
    }

    #[fuchsia::test]
    fn can_be_used_for_object_info() {
        let (_, _, avail) = vmar_root_self().info_maps(&mut []).unwrap();
        let mut buf = PageBuf::<zx::MapInfo>::new(avail * 2).unwrap();

        let (maps, _, avail) = vmar_root_self().info_maps(buf.as_mut()).unwrap();
        assert_eq!(maps.len(), avail, "should have consumed all of the mappings");
    }

    #[fuchsia::test]
    fn can_be_used_for_vmo_reads() {
        let len = 8 * zx::system_get_page_size() as usize;
        let mut buf = PageBuf::<u8>::new(len).unwrap();

        let source_contents = vec![42u8; len];
        let source_vmo = zx::Vmo::create(len as u64).unwrap();
        source_vmo.write(&source_contents, 0).unwrap();

        let in_mapping = source_vmo.read_uninit(buf.as_mut(), 0).unwrap();
        assert_eq!(in_mapping, source_contents);
    }

    #[fuchsia::test]
    fn is_fixed_size() {
        let buf = PageBuf::<[u8; 16]>::new(100).unwrap();
        assert!(buf.len() >= 100);
        let vmo_size = buf.vmo.get_size().unwrap();
        assert!(vmo_size >= 100 * 16);
        assert_eq!(buf.vmo.set_size(vmo_size * 2), Err(zx::Status::UNAVAILABLE));
    }
}
