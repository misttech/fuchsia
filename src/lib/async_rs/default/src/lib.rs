// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_rs_sys::async_dispatcher_t;

unsafe extern "C" {
    pub fn async_get_default_dispatcher() -> *mut async_dispatcher_t;
    pub fn async_set_default_dispatcher(dispatcher: *mut async_dispatcher_t);
}

#[cfg(test)]
mod tests {
    use core::ptr::null;

    use super::*;

    #[test]
    fn test_default() {
        let mut foo = async_dispatcher_t { ops: null() };
        let mut bar = async_dispatcher_t { ops: null() };
        let foo_dispatcher = &mut foo as *mut _;
        let bar_dispatcher = &mut bar as *mut _;

        unsafe {
            async_set_default_dispatcher(foo_dispatcher);
        }
        let result = unsafe { async_get_default_dispatcher() };
        assert_eq!(result, foo_dispatcher);

        unsafe {
            async_set_default_dispatcher(bar_dispatcher);
        }
        let result = unsafe { async_get_default_dispatcher() };
        assert_eq!(result, bar_dispatcher);

        unsafe {
            async_set_default_dispatcher(foo_dispatcher);
        }
        let result = unsafe { async_get_default_dispatcher() };
        assert_eq!(result, foo_dispatcher);
    }
}
