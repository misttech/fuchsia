// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_core::device::remote_block_device::remote_block_device_init;
use starnix_core::mm::MemoryAccessor as _;
use starnix_core::testing::{anon_test_file, map_object_anywhere, spawn_kernel_and_run};
use starnix_core::vfs::{SeekTarget, VecInputBuffer, VecOutputBuffer};
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::{BLKGETSIZE, BLKGETSIZE64};
use std::mem::MaybeUninit;
use zerocopy::FromBytes as _;

#[::fuchsia::test]
async fn test_remote_block_device_registry() {
    spawn_kernel_and_run(|locked, current_task| {
        let kernel = current_task.kernel();
        remote_block_device_init(locked, &current_task);
        let registry = kernel.remote_block_device_registry.clone();

        registry
            .create_remote_block_device_if_absent(locked, &current_task, "test", 1024)
            .expect("create_remote_block_device_if_absent failed.");

        let device = registry.open(0).expect("open failed.");
        let file = anon_test_file(locked, &current_task, device.create_file_ops(), OpenFlags::RDWR);

        let arg_addr = map_object_anywhere(locked, &current_task, &0u64);
        // TODO(https://fxbug.dev/129314): replace with MaybeUninit::uninit_array.
        let arg: MaybeUninit<[MaybeUninit<u8>; 8]> = MaybeUninit::uninit();
        // SAFETY: We are converting from an uninitialized array to an array
        // of uninitialized elements which is the same. See
        // https://doc.rust-lang.org/std/mem/union.MaybeUninit.html#initializing-an-array-element-by-element.
        let mut arg = unsafe { arg.assume_init() };

        file.ioctl(locked, &current_task, BLKGETSIZE64, arg_addr.into()).expect("ioctl failed");
        let value =
            u64::read_from_bytes(current_task.read_memory(arg_addr, &mut arg).unwrap()).unwrap();
        assert_eq!(value, 1024);

        file.ioctl(locked, &current_task, BLKGETSIZE, arg_addr.into()).expect("ioctl failed");
        let value =
            u64::read_from_bytes(current_task.read_memory(arg_addr, &mut arg).unwrap()).unwrap();
        assert_eq!(value, 2);

        let mut buf = VecOutputBuffer::new(512);
        file.read(locked, &current_task, &mut buf).expect("read failed.");
        assert_eq!(buf.data(), &[0u8; 512]);

        let mut buf = VecInputBuffer::from(vec![1u8; 512]);
        file.seek(locked, &current_task, SeekTarget::Set(0)).expect("seek failed");
        file.write(locked, &current_task, &mut buf).expect("write failed.");

        let mut buf = VecOutputBuffer::new(512);
        file.seek(locked, &current_task, SeekTarget::Set(0)).expect("seek failed");
        file.read(locked, &current_task, &mut buf).expect("read failed.");
        assert_eq!(buf.data(), &[1u8; 512]);
    });
}

#[::fuchsia::test]
async fn test_read_write_past_eof() {
    spawn_kernel_and_run(|locked, current_task| {
        let kernel = current_task.kernel();
        remote_block_device_init(locked, &current_task);
        let registry = kernel.remote_block_device_registry.clone();

        registry
            .create_remote_block_device_if_absent(locked, &current_task, "test", 1024)
            .expect("create_remote_block_device_if_absent failed.");

        let device = registry.open(0).expect("open failed.");
        let file = anon_test_file(locked, &current_task, device.create_file_ops(), OpenFlags::RDWR);

        file.seek(locked, &current_task, SeekTarget::End(0)).expect("seek failed");
        let mut buf = VecOutputBuffer::new(512);
        assert_eq!(file.read(locked, &current_task, &mut buf).expect("read failed."), 0);

        let mut buf = VecInputBuffer::from(vec![1u8; 512]);
        assert_eq!(file.write(locked, &current_task, &mut buf).expect("write failed."), 0);
    });
}
