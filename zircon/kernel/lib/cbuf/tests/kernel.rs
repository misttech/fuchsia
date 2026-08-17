// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// Test suite for Rust cbuf implementation.
#[cfg(ktest)]
#[unittest::suite(name = "cbuf_rust")]
mod tests {
    use cbuf::Cbuf;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicU32, Ordering};
    use pin_init::stack_pin_init;
    use unittest::{assert_eq, assert_ok, assert_true, unwrap_ok};
    use zx_status::Status;
    use zx_types::ZX_TIME_INFINITE;

    use crate::kernel::thread;

    const ZX_ERR_INTERNAL_INTR_KILLED: i32 = -502;

    /// Test that the cbuf constructor initializes it to full until initialized.
    #[test]
    fn constructor() {
        stack_pin_init!(let cbuf = Cbuf::init());
        assert_true!(cbuf.full());

        let mut buf = [0u8; 4];
        // SAFETY: `buf` is valid for cbuf lifetime.
        unsafe {
            unwrap_ok!(cbuf.initialize(buf.len(), buf.as_mut_ptr()));
        }
        assert_true!(!cbuf.full());
    }

    /// Test basic read and write operations.
    #[test]
    fn read_write() {
        stack_pin_init!(let cbuf = Cbuf::init());

        let mut buf = [0u8; 4];
        // SAFETY: `buf` is valid for cbuf lifetime.
        unsafe {
            unwrap_ok!(cbuf.initialize(buf.len(), buf.as_mut_ptr()));
        }

        assert_true!(!cbuf.full());

        // Nothing to read, don't wait.
        assert_true!(cbuf.read_char(false) == Err(Status::SHOULD_WAIT));

        // Write some characters.
        let data = b"ABC";
        for &c in data {
            assert_eq!(cbuf.write_char(c), 1);
        }
        assert_true!(cbuf.full());

        // Writing when full should return 0.
        assert_eq!(cbuf.write_char(b'D'), 0);

        // Read them back.
        for (i, &expected) in data.iter().enumerate() {
            let res = unwrap_ok!(cbuf.read_char_with_context(true));
            assert_eq!(res.transitioned_from_full, i == 0);
            assert_eq!(res.c, expected);
        }
        assert_true!(!cbuf.full());
    }

    extern "C" fn reader_thread_entry(arg: *mut c_void) -> i32 {
        // SAFETY: arg is a valid pointer to a Cbuf pinned on the parent thread's stack.
        let cbuf = unsafe { &*(arg as *const Cbuf) };
        loop {
            match cbuf.read_char(true) {
                Ok(_) => {}
                Err(status) => return status.into_raw(),
            }
        }
    }

    /// Test concurrent read and write operations to check for races.
    #[test]
    fn read_write_race() {
        stack_pin_init!(let cbuf = Cbuf::init());

        let mut buf = [0u8; 4];
        // SAFETY: `buf` is valid for cbuf lifetime.
        unsafe {
            unwrap_ok!(cbuf.initialize(buf.len(), buf.as_mut_ptr()));
        }

        let thread_name = c"cbuf_rust_race".as_ptr();
        let cbuf_ptr = &*cbuf as *const Cbuf as *mut c_void;

        // SAFETY: we pass reader_thread_entry and valid pointers. The thread is joined
        // before `cbuf` (and `buf`) goes out of scope.
        unsafe {
            let thread = unwrap_ok!(thread::spawn(thread_name, reader_thread_entry, cbuf_ptr));

            for _ in 0..1000 {
                while cbuf.write_char(b'A') == 0 {
                    thread::r#yield();
                }
            }

            thread.kill();

            let ret = unwrap_ok!(thread.join(ZX_TIME_INFINITE));
            assert_eq!(ret, ZX_ERR_INTERNAL_INTR_KILLED);
        }
    }

    /// Test initialization limits (size 0, non-power of two).
    #[test]
    fn init_limits() {
        stack_pin_init!(let cbuf = Cbuf::init());
        let mut buf = [0u8; 4];

        // Size 0 should fail.
        unsafe {
            assert_true!(cbuf.initialize(0, buf.as_mut_ptr()) == Err(Status::INVALID_ARGS));
        }

        // Non-power of two should fail.
        unsafe {
            assert_true!(cbuf.initialize(3, buf.as_mut_ptr()) == Err(Status::INVALID_ARGS));
            assert_true!(cbuf.initialize(5, buf.as_mut_ptr()) == Err(Status::INVALID_ARGS));
        }

        // Power of two should succeed.
        unsafe {
            assert_ok!(cbuf.initialize(4, buf.as_mut_ptr()));
        }
    }

    /// Test uninitialized cbuf operations.
    #[test]
    fn uninitialized() {
        stack_pin_init!(let cbuf = Cbuf::init());

        assert_true!(cbuf.full());
        assert_eq!(cbuf.write_char(b'A'), 0);
        assert_true!(cbuf.read_char(false) == Err(Status::SHOULD_WAIT));
    }

    /// Test buffer wrap around behavior.
    #[test]
    fn wrap_around() {
        stack_pin_init!(let cbuf = Cbuf::init());
        let mut buf = [0u8; 4];

        unsafe {
            assert_ok!(cbuf.initialize(buf.len(), buf.as_mut_ptr()));
        }

        // Write 3 chars (capacity is 3)
        assert_eq!(cbuf.write_char(b'A'), 1);
        assert_eq!(cbuf.write_char(b'B'), 1);
        assert_eq!(cbuf.write_char(b'C'), 1);

        assert_true!(cbuf.full());

        // Read 3 chars
        assert_eq!(unwrap_ok!(cbuf.read_char(false)), b'A');
        assert_eq!(unwrap_ok!(cbuf.read_char(false)), b'B');
        assert_eq!(unwrap_ok!(cbuf.read_char(false)), b'C');

        assert_true!(!cbuf.full());

        // Write 2 chars (wraps pointers)
        assert_eq!(cbuf.write_char(b'D'), 1);
        assert_eq!(cbuf.write_char(b'E'), 1);

        // Read 2 chars (wraps pointers)
        assert_eq!(unwrap_ok!(cbuf.read_char(false)), b'D');
        assert_eq!(unwrap_ok!(cbuf.read_char(false)), b'E');

        // Should be empty
        assert_true!(cbuf.read_char(false) == Err(Status::SHOULD_WAIT));
    }

    struct BlockingReadContext {
        cbuf: *mut Cbuf,
        state: *const AtomicU32, // 0: init, 1: about to read, 2: read done, 3: error
        read_char: *mut u8,
    }

    // SAFETY: We only pass valid pointers and don't share mutability unsafely.
    unsafe impl Send for BlockingReadContext {}

    extern "C" fn blocking_reader_entry(arg: *mut c_void) -> i32 {
        let ctx = unsafe { &*(arg as *const BlockingReadContext) };
        let cbuf = unsafe { &*ctx.cbuf };
        let state = unsafe { &*ctx.state };

        state.store(1, Ordering::SeqCst);
        let c = cbuf.read_char(true); // Should block until written.

        match c {
            Ok(val) => {
                unsafe { *ctx.read_char = val };
                state.store(2, Ordering::SeqCst);
                0
            }
            Err(status) => {
                state.store(3, Ordering::SeqCst); // error
                status.into_raw()
            }
        }
    }

    /// Test blocking read operation.
    #[test]
    fn blocking_read() {
        stack_pin_init!(let cbuf = Cbuf::init());

        let mut buf = [0u8; 4];
        // SAFETY: `buf` is valid for cbuf lifetime.
        unsafe {
            assert_ok!(cbuf.initialize(buf.len(), buf.as_mut_ptr()));
        }

        let state = AtomicU32::new(0);
        let mut read_char = 0u8;

        let mut ctx = BlockingReadContext {
            cbuf: &*cbuf as *const Cbuf as *mut Cbuf,
            state: &state,
            read_char: &mut read_char,
        };

        let thread_name = c"cbuf_blocking_read".as_ptr();
        let ctx_ptr = &mut ctx as *mut BlockingReadContext as *mut c_void;

        unsafe {
            let thread = unwrap_ok!(thread::spawn(thread_name, blocking_reader_entry, ctx_ptr));

            // Wait until the reader thread is about to read.
            while state.load(Ordering::SeqCst) < 1 {
                thread::r#yield();
            }

            // Wait until the reader thread is actually blocked.
            while !thread.is_blocked() {
                thread::r#yield();
                // If it failed and exited, break.
                if state.load(Ordering::SeqCst) == 3 {
                    break;
                }
            }

            if state.load(Ordering::SeqCst) == 3 {
                thread.join(ZX_TIME_INFINITE).ok();
                panic!("reader thread failed early");
            }

            // Double check it is indeed blocked and state is 1.
            assert_true!(thread.is_blocked());
            assert_eq!(state.load(Ordering::SeqCst), 1);

            // Now write a char. This should wake it up.
            assert_eq!(cbuf.write_char(b'X'), 1);

            // Wait for reader thread to complete.
            let ret = unwrap_ok!(thread.join(ZX_TIME_INFINITE));

            assert_ok!(Status::ok(ret));
            assert_eq!(state.load(Ordering::SeqCst), 2);
            assert_eq!(read_char, b'X');
        }
    }
}
