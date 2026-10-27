// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use ksync::{IrqSavePolicy, KCell, KMutex, LockClass, RawSpinlock, guarded};

/// Backing size of [`PanicBuffer`]; see [`PanicBuffer::MAX_SIZE`].
const MAX_SIZE: usize = 2048;

/// A fixed-size, NUL-terminated buffer for storing formatted panic/assert messages.
///
/// While this buffer is safe for concurrent use by multiple threads, concurrent use may result in
/// message loss or (logical) message corruption.
#[guarded]
pub(super) struct PanicBuffer {
    #[mutex]
    lock: KMutex<RawSpinlock>,
    #[guarded_by(lock)]
    pos: usize,
    #[guarded_by(lock)]
    buffer: [u8; MAX_SIZE],
}

impl PanicBuffer {
    /// This value should be small enough so that an instance of this type can fit on the stack,
    /// but large enough to capture the last few lines emitted by an assert failure or panic to
    /// ensure any formatted message is captured for later analysis.
    ///
    /// Note, increasing this size will not necessarily ensure that more of the assert/panic
    /// message is captured.  There are other limits in place that may truncate the message.  See
    /// [`crashlog_to_string`](super::crashlog_to_string).
    pub(super) const MAX_SIZE: usize = MAX_SIZE;

    /// Creates an empty panic buffer.
    ///
    /// This is a `const fn` so that the global [`PANIC_BUFFER`] is statically initialized and
    /// usable at any point during boot.
    pub(super) const fn new() -> Self {
        Self {
            lock: KMutex::new(RawSpinlock::const_init(<PanicBufferLockClass as LockClass>::ID)),
            pos: KCell::new(0),
            buffer: KCell::new([0; MAX_SIZE]),
        }
    }

    /// Append `bytes` to the buffer.
    ///
    /// Silently drops data if the buffer is full.
    ///
    /// TODO(maniscalco): Given that the formatted message arguments are more valuable than the
    /// format string, consider giving this class circular buffer semantics.
    pub(super) fn append(&self, bytes: &[u8]) {
        ksync::lock!(let mut guard = self.lock_lock_policy::<IrqSavePolicy>());
        let fields = guard.as_mut().fields_mut();
        let pos = *fields.pos;
        let space_available = fields.buffer.len() - pos - 1;
        if space_available > 0 {
            let num_to_copy = bytes.len().min(space_available);
            fields.buffer[pos..pos + num_to_copy].copy_from_slice(&bytes[..num_to_copy]);
            *fields.pos += num_to_copy;
        }
    }

    /// Calls `f` with the buffer's contents, while holding the buffer's lock.
    ///
    /// `f` must not write to stdout, panic, or block: those paths append to this buffer and would
    /// deadlock on its spinlock.
    pub(super) fn with_contents<R>(&self, f: impl FnOnce(&[u8]) -> R) -> R {
        ksync::lock!(let guard = self.lock_lock_policy::<IrqSavePolicy>());
        f(&guard.buffer()[..*guard.pos()])
    }
}

impl Default for PanicBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// The global panic buffer.
pub(super) static PANIC_BUFFER: PanicBuffer = PanicBuffer::new();
