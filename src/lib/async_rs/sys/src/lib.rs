// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Low-level Rust bindings for libasync.

#![allow(non_camel_case_types)]
#![no_std]

mod dispatcher;
mod irq;
mod paged_vmo;
mod receiver;
mod sequence_id;
mod task;
mod time;
mod trap;
mod wait;

pub use self::dispatcher::*;
pub use self::irq::*;
pub use self::paged_vmo::*;
pub use self::receiver::*;
pub use self::sequence_id::*;
pub use self::task::*;
pub use self::time::*;
pub use self::trap::*;
pub use self::wait::*;

#[cfg(test)]
mod tests {
    use core::ptr::{null, null_mut};

    use zx_types::{
        ZX_OK, ZX_TIME_INFINITE, zx_handle_t, zx_packet_user_t, zx_status_t, zx_time_t, zx_vaddr_t,
    };

    use super::*;

    #[test]
    fn test_dispatcher() {
        #[repr(C)]
        struct Test {
            dispatcher: async_dispatcher_t,

            now_called: bool,
            begin_wait_called: bool,
            cancel_wait_called: bool,
            post_task_called: bool,
            cancel_task_called: bool,
            queue_packet_called: bool,
            set_guest_bell_trap_called: bool,
        }

        impl Test {
            fn as_dispatcher(&mut self) -> *mut async_dispatcher_t {
                (self as *mut Self).cast()
            }
        }

        extern "C" fn test_now(dispatcher: *mut async_dispatcher_t) -> zx_time_t {
            let dispatcher = unsafe { &mut *dispatcher.cast::<Test>() };
            dispatcher.now_called = true;
            ZX_TIME_INFINITE
        }

        extern "C" fn test_begin_wait(
            dispatcher: *mut async_dispatcher_t,
            _: *mut async_wait_t,
        ) -> zx_status_t {
            let dispatcher = unsafe { &mut *dispatcher.cast::<Test>() };
            dispatcher.begin_wait_called = true;
            ZX_OK
        }

        extern "C" fn test_cancel_wait(
            dispatcher: *mut async_dispatcher_t,
            _: *mut async_wait_t,
        ) -> zx_status_t {
            let dispatcher = unsafe { &mut *dispatcher.cast::<Test>() };
            dispatcher.cancel_wait_called = true;
            ZX_OK
        }

        extern "C" fn test_post_task(
            dispatcher: *mut async_dispatcher_t,
            _: *mut async_task_t,
        ) -> zx_status_t {
            let dispatcher = unsafe { &mut *dispatcher.cast::<Test>() };
            dispatcher.post_task_called = true;
            ZX_OK
        }

        extern "C" fn test_cancel_task(
            dispatcher: *mut async_dispatcher_t,
            _: *mut async_task_t,
        ) -> zx_status_t {
            let dispatcher = unsafe { &mut *dispatcher.cast::<Test>() };
            dispatcher.cancel_task_called = true;
            ZX_OK
        }

        extern "C" fn test_queue_packet(
            dispatcher: *mut async_dispatcher_t,
            _: *mut async_receiver_t,
            _: *const zx_packet_user_t,
        ) -> zx_status_t {
            let dispatcher = unsafe { &mut *dispatcher.cast::<Test>() };
            dispatcher.queue_packet_called = true;
            ZX_OK
        }

        extern "C" fn test_set_guest_bell_trap(
            dispatcher: *mut async_dispatcher_t,
            _: *mut async_guest_bell_trap_t,
            _: zx_handle_t,
            _: zx_vaddr_t,
            _: usize,
        ) -> zx_status_t {
            let dispatcher = unsafe { &mut *dispatcher.cast::<Test>() };
            dispatcher.set_guest_bell_trap_called = true;
            ZX_OK
        }

        let ops = async_ops_t {
            version: ASYNC_OPS_V1,
            reserved: 0,
            v1: async_ops_v1_t {
                now: Some(test_now),
                begin_wait: Some(test_begin_wait),
                cancel_wait: Some(test_cancel_wait),
                post_task: Some(test_post_task),
                cancel_task: Some(test_cancel_task),
                queue_packet: Some(test_queue_packet),
                set_guest_bell_trap: Some(test_set_guest_bell_trap),
            },
            v2: async_ops_v2_t {
                bind_irq: None,
                unbind_irq: None,
                create_paged_vmo: None,
                detach_paged_vmo: None,
            },
            v3: async_ops_v3_t { get_sequence_id: None, check_sequence_id: None },
        };

        let mut test = Test {
            dispatcher: async_dispatcher_t { ops: &ops },

            now_called: false,
            begin_wait_called: false,
            cancel_wait_called: false,
            post_task_called: false,
            cancel_task_called: false,
            queue_packet_called: false,
            set_guest_bell_trap_called: false,
        };

        assert!(!test.now_called);
        unsafe {
            async_now(test.as_dispatcher());
        }
        assert!(test.now_called);

        assert!(!test.begin_wait_called);
        unsafe {
            async_begin_wait(test.as_dispatcher(), null_mut());
        }
        assert!(test.begin_wait_called);

        assert!(!test.cancel_wait_called);
        unsafe {
            async_cancel_wait(test.as_dispatcher(), null_mut());
        }
        assert!(test.cancel_wait_called);

        assert!(!test.post_task_called);
        unsafe {
            async_post_task(test.as_dispatcher(), null_mut());
        }
        assert!(test.post_task_called);

        assert!(!test.cancel_task_called);
        unsafe {
            async_cancel_task(test.as_dispatcher(), null_mut());
        }
        assert!(test.cancel_task_called);

        assert!(!test.queue_packet_called);
        unsafe {
            async_queue_packet(test.as_dispatcher(), null_mut(), null());
        }
        assert!(test.queue_packet_called);

        assert!(!test.set_guest_bell_trap_called);
        unsafe {
            async_set_guest_bell_trap(test.as_dispatcher(), null_mut(), 0, 0, 0);
        }
        assert!(test.set_guest_bell_trap_called);
    }
}
