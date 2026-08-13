// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// Test suite for Rust user_copy implementation.
#[cfg(ktest)]
#[unittest::suite(name = "user_copy_rust")]
mod tests {
    use unittest::{UserMemory, assert_eq, assert_nonnull, assert_null, assert_true, unwrap_ok};
    use user_copy::{UserInIovec, UserInOutPtr, UserInPtr, UserOutPtr, UserStringView};
    use zx_status::Status;
    use zx_types::zx_iovec_t;

    /// Test UserPtr offsets.
    #[test]
    fn offsets() {
        let base = 0x1000 as *mut u32;

        // UserInPtr offset tests
        let in_ptr = UserInPtr::new(base as *const u32);
        assert_nonnull!(in_ptr);
        assert_eq!(in_ptr.byte_offset(8).as_ptr(), 0x1008 as *const u32);
        assert_eq!(in_ptr.byte_offset(-4).as_ptr(), 0x0ffc as *const u32);
        assert_eq!(in_ptr.element_offset(3).as_ptr(), 0x100c as *const u32);

        let null_in = UserInPtr::<u32>::new(core::ptr::null());
        assert_null!(null_in.byte_offset(8));
        assert_null!(null_in.element_offset(3));

        let def_in = UserInPtr::<u32>::default();
        assert_null!(def_in);

        // UserOutPtr offset tests
        let out_ptr = UserOutPtr::new(base);
        assert_nonnull!(out_ptr);
        assert_eq!(out_ptr.byte_offset(8).as_ptr(), 0x1008 as *mut u32);
        assert_eq!(out_ptr.byte_offset(-4).as_ptr(), 0x0ffc as *mut u32);
        assert_eq!(out_ptr.element_offset(3).as_ptr(), 0x100c as *mut u32);

        let null_out = UserOutPtr::<u32>::new(core::ptr::null_mut());
        assert_null!(null_out.byte_offset(8));
        assert_null!(null_out.element_offset(3));

        let def_out = UserOutPtr::<u32>::default();
        assert_null!(def_out);

        // UserInOutPtr offset tests
        let inout_ptr = UserInOutPtr::new(base);
        assert_nonnull!(inout_ptr);
        assert_eq!(inout_ptr.byte_offset(8).as_ptr(), 0x1008 as *mut u32);
        assert_eq!(inout_ptr.byte_offset(-4).as_ptr(), 0x0ffc as *mut u32);
        assert_eq!(inout_ptr.element_offset(3).as_ptr(), 0x100c as *mut u32);

        let null_inout = UserInOutPtr::<u32>::new(core::ptr::null_mut());
        assert_null!(null_inout.byte_offset(8));
        assert_null!(null_inout.element_offset(3));

        let def_inout = UserInOutPtr::<u32>::default();
        assert_null!(def_inout);

        // UserStringView Default test
        let def_sv = UserStringView::default();
        assert_null!(def_sv.data);
        assert_true!(def_sv.is_empty());
    }

    /// Test CopyOut.
    #[test]
    fn copy_out() {
        let mut user = UserMemory::create(4096).unwrap();
        unwrap_ok!(user.commit_and_map(4096));

        let out_ptr = UserOutPtr::<u32>::new(user.base() as *mut u32);
        assert_nonnull!(out_ptr);
        unwrap_ok!(out_ptr.write(0xDEADBEEF));

        let mut temp = [0u8; 4];
        unwrap_ok!(user.vmo_read(&mut temp, 0));
        let val = u32::from_ne_bytes(temp);
        assert_eq!(val, 0xDEADBEEF);
    }

    /// Test CopyIn.
    #[test]
    fn copy_in() {
        let mut user = UserMemory::create(4096).unwrap();
        unwrap_ok!(user.commit_and_map(4096));
        unwrap_ok!(user.vmo_write(&0xDEADBEEF_u32.to_ne_bytes(), 0));

        let in_ptr = UserInPtr::<u32>::new(user.base() as *const u32);
        assert_nonnull!(in_ptr);
        let val = unwrap_ok!(in_ptr.read());
        assert_eq!(val, 0xDEADBEEF);
    }

    /// Test CopyFromUser.
    #[test]
    fn copy_from_user() {
        let mut user = UserMemory::create(4096).unwrap();
        unwrap_ok!(user.commit_and_map(4096));
        unwrap_ok!(user.vmo_write(&0xDEADBEEF_u32.to_ne_bytes(), 0));

        let in_ptr = UserInPtr::<u32>::new(user.base() as *const u32);
        assert_nonnull!(in_ptr);
        let mut temp = core::mem::MaybeUninit::uninit();
        let val_ref = unwrap_ok!(in_ptr.copy_from_user(&mut temp));
        assert_eq!(*val_ref, 0xDEADBEEF);
    }

    /// Test CopySliceFromUser.
    #[test]
    fn copy_slice_from_user() {
        let mut user = UserMemory::create(4096).unwrap();
        unwrap_ok!(user.commit_and_map(4096));
        let vals = [10u32, 20u32, 30u32];
        let mut bytes = [0u8; 12];
        for i in 0..3 {
            bytes[i * 4..(i + 1) * 4].copy_from_slice(&vals[i].to_ne_bytes());
        }
        unwrap_ok!(user.vmo_write(&bytes, 0));

        let in_ptr = UserInPtr::<u32>::new(user.base() as *const u32);
        assert_nonnull!(in_ptr);
        let mut out = [core::mem::MaybeUninit::uninit(); 3];
        let out_slice = unwrap_ok!(in_ptr.copy_slice_from_user(&mut out));
        assert_eq!(out_slice[0], 10);
        assert_eq!(out_slice[1], 20);
        assert_eq!(out_slice[2], 30);
    }

    /// Test faults.
    #[test]
    fn faults() {
        let out_ptr = UserOutPtr::<u32>::new(core::ptr::null_mut());
        assert_null!(out_ptr);
        assert_true!(out_ptr.write(0xDEADBEEF).err() == Some(Status::INVALID_ARGS));

        let in_ptr = UserInPtr::<u32>::new(core::ptr::null());
        assert_null!(in_ptr);
        assert_true!(in_ptr.read().err() == Some(Status::INVALID_ARGS));

        let mut temp = core::mem::MaybeUninit::uninit();
        assert_true!(in_ptr.copy_from_user(&mut temp).err() == Some(Status::INVALID_ARGS));

        let mut temp_slice = [core::mem::MaybeUninit::uninit(); 1];
        assert_true!(
            in_ptr.copy_slice_from_user(&mut temp_slice).err() == Some(Status::INVALID_ARGS)
        );

        let bad_addr = usize::MAX as *mut u32;
        let out_ptr = UserOutPtr::<u32>::new(bad_addr);
        assert_true!(out_ptr.write(0xDEADBEEF).err() == Some(Status::INVALID_ARGS));

        let in_ptr = UserInPtr::<u32>::new(bad_addr as *const u32);
        assert_true!(in_ptr.read().err() == Some(Status::INVALID_ARGS));
    }

    /// Test IovecCapacity.
    #[test]
    fn iovec_capacity() {
        let mut user = UserMemory::create(4096).unwrap();
        unwrap_ok!(user.commit_and_map(4096));

        let vec = [
            zx_iovec_t { buffer: core::ptr::null(), capacity: 348 },
            zx_iovec_t { buffer: core::ptr::null(), capacity: 58 },
        ];

        let bytes = unsafe {
            core::slice::from_raw_parts(vec.as_ptr() as *const u8, core::mem::size_of_val(&vec))
        };
        unwrap_ok!(user.vmo_write(bytes, 0));

        let in_ptr = UserInPtr::<zx_iovec_t>::new(user.base() as *const zx_iovec_t);
        assert_nonnull!(in_ptr);

        let iovec = UserInIovec::new(in_ptr, 2);
        let total_capacity = unwrap_ok!(iovec.get_total_capacity());
        assert_eq!(total_capacity, 406);
    }

    /// Test IovecForeach.
    #[test]
    fn iovec_foreach() {
        let mut user = UserMemory::create(4096).unwrap();
        unwrap_ok!(user.commit_and_map(4096));

        let vec = [
            zx_iovec_t { buffer: core::ptr::null(), capacity: 7 },
            zx_iovec_t { buffer: core::ptr::null(), capacity: 11 },
            zx_iovec_t { buffer: core::ptr::null(), capacity: 13 },
        ];

        let bytes = unsafe {
            core::slice::from_raw_parts(vec.as_ptr() as *const u8, core::mem::size_of_val(&vec))
        };
        unwrap_ok!(user.vmo_write(bytes, 0));

        let in_ptr = UserInPtr::<zx_iovec_t>::new(user.base() as *const zx_iovec_t);
        assert_nonnull!(in_ptr);

        let iovec = UserInIovec::new(in_ptr, 3);
        let mut product = 2usize;
        let res = iovec.for_each(|_buf, cap| {
            product = product.wrapping_mul(cap);
            Ok(())
        });
        unwrap_ok!(res);
        assert_eq!(product, 2002);
    }

    /// Test StringView.
    #[test]
    fn string_view() {
        let mut user = UserMemory::create(4096).unwrap();
        unwrap_ok!(user.commit_and_map(4096));
        let k_string = b"Hello, Fuchsia!\0";
        unwrap_ok!(user.vmo_write(k_string, 0));

        let in_ptr = UserInPtr::<u8>::new(user.base() as *const u8);
        assert_nonnull!(in_ptr);

        let sv = UserStringView { data: in_ptr, length: k_string.len() };
        let mut buf = [core::mem::MaybeUninit::uninit(); 32];
        let out_slice = unwrap_ok!(sv.copy_slice_from_user(&mut buf));
        assert_true!(out_slice == k_string);

        // Buffer too small should return INVALID_ARGS
        let mut small_buf = [core::mem::MaybeUninit::uninit(); 5];
        assert_true!(sv.copy_slice_from_user(&mut small_buf).err() == Some(Status::INVALID_ARGS));
    }
}
