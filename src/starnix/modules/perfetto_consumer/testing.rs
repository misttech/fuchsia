// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Test helpers shared by the unit tests in this crate.

use fuchsia_trace::BufferingMode;
use std::ffi::c_uint;

/// Returns `len` bytes of test data: 0, 1, 2, ..., wrapping at 256.
pub(crate) fn test_bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| i as u8).collect()
}

/// Serializes the tests that depend on whether a trace session is running: trace-engine's
/// state is global to the test process. `TraceSession` also holds it to keep its calls into
/// trace-engine's test fixture from overlapping.
static TRACE_ENGINE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Locks `TRACE_ENGINE_LOCK`. A test that panicked while holding it doesn't fail the others.
pub(crate) fn lock_trace_engine() -> std::sync::MutexGuard<'static, ()> {
    TRACE_ENGINE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

// `attach_to_thread_t` (from <trace-test-utils/fixture.h>) and `trace_buffering_mode_t` are C
// enums with no negative enumerators, so clang gives them the underlying type `unsigned int`.
const NO_ATTACH_TO_THREAD: c_uint = 0; // kNoAttachToThread
pub(crate) const TRACE_BUFFER_SIZE: usize = 1 << 16;
// Only `TraceSession` calls these, and only while it holds `TRACE_ENGINE_LOCK`.
unsafe extern "C" {
    fn fixture_set_up(attach_to_thread: c_uint, mode: c_uint, buffer_size: usize);
    fn fixture_initialize_and_start_tracing();
    fn fixture_stop_and_terminate_tracing();
    fn fixture_tear_down();
}

/// A trace session run in this process by trace-engine's test fixture, so that
/// `ProlongedContext::acquire()` succeeds while it exists. It holds `TRACE_ENGINE_LOCK` for its
/// whole life, so a test that starts one must not lock it too. Dropping it waits until
/// trace-engine has stopped, so it hangs if a prolonged context is still held.
pub(crate) struct TraceSession {
    /// Held from before the fixture is set up until after it is torn down: `Drop::drop()`
    /// runs before the fields are dropped.
    _lock: std::sync::MutexGuard<'static, ()>,
}

impl TraceSession {
    pub(crate) fn start() -> Self {
        let lock = lock_trace_engine();
        // SAFETY: The fixture functions take no pointers. fixture.cc keeps the fixture in a
        // global without synchronization, which `fixture_set_up()` requires to be empty and
        // the other functions require to be set. So the calls must not overlap, and set-up
        // must come first.
        // - Only a `TraceSession` calls these functions, and only while it holds
        //   `TRACE_ENGINE_LOCK` (`lock` here, `_lock` in `drop()`), so no calls overlap.
        // - An earlier `TraceSession` tore the fixture down in `drop()` before it released
        //   the lock, including when a panic unwound through it, or, if it was leaked, still
        //   holds the lock. So no fixture is set up here.
        // - fixture.cc is built with `-fno-exceptions`, so neither call can unwind, and
        //   `Self` below takes over `lock` and tears the fixture down.
        // The fixture runs trace-engine on its own async loop thread (`kNoAttachToThread`).
        unsafe {
            fixture_set_up(
                NO_ATTACH_TO_THREAD,
                BufferingMode::OneShot as c_uint,
                TRACE_BUFFER_SIZE,
            );
            fixture_initialize_and_start_tracing();
        }
        Self { _lock: lock }
    }
}

impl Drop for TraceSession {
    fn drop(&mut self) {
        // SAFETY: `start()` set up the fixture and started tracing, and `self._lock` still
        // holds `TRACE_ENGINE_LOCK`, so no other fixture call overlaps these.
        // `fixture_tear_down()` frees the trace buffer. Before that,
        // `fixture_stop_and_terminate_tracing()` waits for trace-engine to reach Stopped,
        // which it does only once no context is held, and terminates it. The context count
        // is then zero, and only a new `trace_engine_start()` makes it nonzero, so no context
        // can be acquired that uses the freed buffer.
        unsafe {
            fixture_stop_and_terminate_tracing();
            fixture_tear_down();
        }
    }
}
