// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

use core::ptr::NonNull;
use pin_init::PinInit;
use zx_status::Status;

/// Context returned along with a character read from the circular buffer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReadContext {
    /// The character that was read.
    pub c: u8,
    /// Whether the buffer transitioned away from being completely full as a result of this read.
    pub transitioned_from_full: bool,
}

/// A thread-safe, lock-protected circular byte buffer.
#[ksync::guarded]
#[repr(C)]
pub struct Cbuf {
    #[guarded_by(lock)]
    head: u32,

    #[guarded_by(lock)]
    tail: u32,

    #[guarded_by(lock)]
    len_pow2: u32,

    #[guarded_by(lock)]
    buf: Option<NonNull<u8>>,

    #[pin]
    event: ksync::KEvent,

    #[mutex]
    lock: ksync::KMutex<ksync::RawSpinlock>,
}

// SAFETY: Cbuf is safe to send across thread boundaries because all mutable access
// to its fields is protected by the internal spinlock (`lock`).
unsafe impl Send for Cbuf {}

// SAFETY: Cbuf is safe to share across thread boundaries because all mutable access
// to its fields is protected by the internal spinlock (`lock`).
unsafe impl Sync for Cbuf {}

impl Cbuf {
    /// Returns a pin initializer for a new, uninitialized `Cbuf`.
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init::pin_init!(Self {
            head: 0.into(),
            tail: 0.into(),
            len_pow2: 0.into(),
            buf: None.into(),
            event <- ksync::KEvent::init(false),
            lock <- ksync::KMutex::init(),
        })
    }

    /// Initializes the circular buffer with the specified size and backing memory region.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `buf` points to a valid memory region of at least `len` bytes,
    /// and that this memory region remains valid for the lifetime of `Cbuf`.
    pub unsafe fn initialize(&self, len: usize, buf: *mut u8) -> Result<(), Status> {
        if len == 0 || !len.is_power_of_two() {
            return Err(Status::INVALID_ARGS);
        }
        let len_pow2 = len.trailing_zeros();

        ksync::lock!(let mut guard = self.lock_lock());
        let fields = guard.as_mut().fields_mut();
        *fields.len_pow2 = len_pow2;
        *fields.buf = NonNull::new(buf);
        *fields.head = 0;
        *fields.tail = 0;

        Ok(())
    }

    /// Returns `true` if the buffer is currently full.
    pub fn full(&self) -> bool {
        ksync::lock!(let guard = self.lock_lock());
        let fields = guard.fields();
        is_full(*fields.head, *fields.tail, *fields.len_pow2)
    }

    /// Writes a single character to the buffer if space is available.
    ///
    /// Returns `1` if the write succeeded, or `0` if the buffer was full.
    pub fn write_char(&self, c: u8) -> usize {
        let wrote = {
            ksync::lock!(let mut guard = self.lock_lock());
            let fields = guard.fields_mut();
            if is_full(*fields.head, *fields.tail, *fields.len_pow2) {
                0
            } else {
                if let Some(buf) = fields.buf {
                    // SAFETY: `initialize` caller guarantees that `buf` is valid.
                    unsafe {
                        buf.as_ptr().add(*fields.head as usize).write(c);
                    }
                    inc_pointer(fields.head, 1, *fields.len_pow2);
                    1
                } else {
                    0
                }
            }
        };

        if wrote > 0 {
            self.event.signal();
        }
        wrote
    }

    /// Reads a single character from the buffer, returning it along with the transition context.
    ///
    /// If `block` is true, this function blocks until a character is available to read.
    /// If `block` is false, it returns `Err(Status::SHOULD_WAIT)` if no character is available.
    pub fn read_char_with_context(&self, block: bool) -> Result<ReadContext, Status> {
        loop {
            {
                ksync::lock!(let mut guard = self.lock_lock());
                let fields = guard.fields_mut();
                if *fields.tail != *fields.head {
                    if let Some(buf) = fields.buf {
                        // SAFETY: `initialize` caller guarantees that `buf` is valid.
                        let c = unsafe { buf.as_ptr().add(*fields.tail as usize).read() };
                        let transitioned_from_full =
                            is_full(*fields.head, *fields.tail, *fields.len_pow2);

                        inc_pointer(fields.tail, 1, *fields.len_pow2);
                        if *fields.tail == *fields.head {
                            self.event.unsignal();
                        }
                        return Ok(ReadContext { c, transitioned_from_full });
                    }
                }

                // Because the signal state does not 100% match the buffer state, it is critical
                // that the event is unsignaled when the buffer is found to be empty (not just when
                // it *transitions* to empty).
                self.event.unsignal();
            }

            if !block {
                return Err(Status::SHOULD_WAIT);
            }

            self.event.wait()?;
        }
    }

    /// Reads a single character from the buffer.
    ///
    /// If `block` is true, this function blocks until a character is available to read.
    /// If `block` is false, it returns `Err(Status::SHOULD_WAIT)` if no character is available.
    pub fn read_char(&self, block: bool) -> Result<u8, Status> {
        self.read_char_with_context(block).map(|ctx| ctx.c)
    }
}

#[inline]
fn inc_pointer(ptr: &mut u32, inc: u32, len_pow2: u32) {
    let mask = (1u32 << len_pow2) - 1;
    *ptr = ptr.wrapping_add(inc) & mask;
}

#[inline]
fn is_full(head: u32, tail: u32, len_pow2: u32) -> bool {
    if len_pow2 == 0 {
        return true;
    }
    let mask = (1u32 << len_pow2) - 1;
    let consumed = head.wrapping_sub(tail) & mask;
    let avail = (1u32 << len_pow2) - consumed - 1;
    avail == 0
}
