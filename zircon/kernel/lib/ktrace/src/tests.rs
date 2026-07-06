// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

use arch_rs::{InterruptDisableGuard, curr_cpu_num, max_num_cpus};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::{ffi, ptr};
use kernel::thread::{FxtRef, ThreadPtr};
use kstring::declare_interned_category;
use ktrace_rs::{
    Context, DROP_STATS_REF, DroppedRecordStats, InstantBootTicks, KTrace, KTraceBuffer,
    KTraceState, META_CAT, NUM_BYTES_REF, NUM_RECORDS_REF, complete, counter, duration_begin,
    duration_end, flow_begin, flow_end, flow_step, instant, kernel_object, kernel_object_always,
    rust_ktrace_init, rust_ktrace_init_cpu_buffer,
};
use spsc_buffer::Buffer;
use zx_status::Status;

declare_interned_category!(MEMORY_CAT, "kernel:memory", extern);
declare_interned_category!(SCHED_CAT, "kernel:sched", extern);
declare_interned_category!(CONTENTION_CAT, "kernel:contention", extern);
declare_interned_category!(IPC_CAT, "kernel:ipc", extern);
declare_interned_category!(IRQ_CAT, "kernel:irq", extern);

//
// Zircon kernel-tests FFI entry points.
//

/// Test-only FFI helper to write a single word record from Rust.
///
/// # Safety
///
/// This must be called with interrupts disabled.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_ktrace_test_interop(header: u64, val: u64) -> i32 {
    let ktrace = KTrace::get_instance();
    // SAFETY: The caller guarantees interrupts are disabled.
    if let Ok(mut res) = unsafe { ktrace.reserve(header) } {
        let _ = res.write_word(val);
        let _ = res.commit();
        0
    } else {
        -1
    }
}

/// Test-only FFI helper to exercise all ktrace macros from Rust.
///
/// # Safety
///
/// This must be called with interrupts disabled and KTrace active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_ktrace_test_macros() {
    // Exercise each macro. We'll write specific, distinguishable events.
    instant!("kernel:meta", "rust_instant", Context::Thread, "val" => 101u32);
    duration_begin!("kernel:meta", "rust_duration", Context::Thread, "val" => 102u32);
    duration_end!("kernel:meta", "rust_duration", Context::Thread, "val" => 103u32);
    counter!("kernel:meta", "rust_counter", 104u64, "val" => 105u32);
    flow_begin!("kernel:meta", "rust_flow", 106u64, "val" => 107u32);
    flow_step!("kernel:meta", "rust_flow", 106u64, "val" => 108u32);
    flow_end!("kernel:meta", "rust_flow", 106u64, "val" => 109u32);
    complete!("kernel:meta", "rust_complete", InstantBootTicks(110i64), "val" => 111u32);
    kernel_object!("kernel:meta", 112u64, 1u32, "rust_kernel_obj", "val" => 113u32);
    kernel_object_always!(114u64, 2u32, "rust_kernel_obj_always", "val" => 115u32);
}

/// Verifies KTraceBuffer initialization and size/metadata attributes.
#[unsafe(no_mangle)]
pub extern "C" fn rust_ktrace_test_init_and_size() -> bool {
    let mut storage = [0u8; 256];
    let mut inner_buf = unsafe { Buffer::from_raw_parts(storage.as_mut_ptr(), storage.len()) };
    let leaked_ref = unsafe { &mut *ptr::from_mut(&mut inner_buf) };
    let mut stats = DroppedRecordStats::default();
    let leaked_stats = unsafe { &mut *ptr::from_mut(&mut stats) };
    let kbuf = KTraceBuffer::new(leaked_ref, leaked_stats, 1, 100, 200);

    if kbuf.size() != 256 {
        return false;
    }
    if kbuf.process_koid != 100 {
        return false;
    }
    if kbuf.thread_koid != 200 {
        return false;
    }
    true
}

/// Verifies KTraceBuffer reservation, writing words, and committing.
#[unsafe(no_mangle)]
pub extern "C" fn rust_ktrace_test_write() -> bool {
    let _guard = InterruptDisableGuard::new();
    let mut storage = [0u8; 256];
    let mut inner_buf = unsafe { Buffer::from_raw_parts(storage.as_mut_ptr(), storage.len()) };
    let leaked_ref = unsafe { &mut *ptr::from_mut(&mut inner_buf) };
    let mut stats = DroppedRecordStats::default();
    let leaked_stats = unsafe { &mut *ptr::from_mut(&mut stats) };
    let mut kbuf = KTraceBuffer::new(leaked_ref, leaked_stats, 1, 100, 200);

    // Reserve 16 bytes (2 words).
    let header = 4u64 | (2u64 << 4);
    let mut res = match kbuf.reserve(header) {
        Ok(r) => r,
        Err(_) => return false,
    };

    // Write a word (8 bytes).
    if res.write_word(0xabcdef0123456789).is_err() {
        return false;
    }
    if res.commit().is_err() {
        return false;
    }

    // Read it back.
    let mut read_bytes = [0u8; 16];
    let read_len = match kbuf.read(
        |offset, src| {
            read_bytes[offset as usize..offset as usize + src.len()].copy_from_slice(src);
            Ok(())
        },
        16,
    ) {
        Ok(l) => l,
        Err(_) => return false,
    };

    if read_len != 16 {
        return false;
    }
    if u64::from_ne_bytes(read_bytes[0..8].try_into().unwrap()) != header {
        return false;
    }
    if u64::from_ne_bytes(read_bytes[8..16].try_into().unwrap()) != 0xabcdef0123456789 {
        return false;
    }
    true
}

/// Verifies tracking of dropped records and their subsequent serialization when space becomes
/// available.
#[unsafe(no_mangle)]
pub extern "C" fn rust_ktrace_test_dropped_record_tracking() -> bool {
    let _guard = InterruptDisableGuard::new();
    let mut storage = [0u8; 128]; // small buffer
    let mut inner_buf = unsafe { Buffer::from_raw_parts(storage.as_mut_ptr(), storage.len()) };
    let leaked_ref = unsafe { &mut *ptr::from_mut(&mut inner_buf) };
    let mut stats = DroppedRecordStats::default();
    let leaked_stats = unsafe { &mut *ptr::from_mut(&mut stats) };
    let mut kbuf = KTraceBuffer::new(leaked_ref, leaked_stats, 1, 100, 200);

    // Reserve almost all space.
    // 128 bytes total. Let's reserve 96 bytes (12 words).
    let header = 4u64 | (12u64 << 4);
    let mut res = match kbuf.reserve(header) {
        Ok(r) => r,
        Err(_) => return false,
    };
    if res.write_bytes(&[0u8; 88]).is_err() {
        return false;
    }
    if res.commit().is_err() {
        return false;
    }

    // Now, try to reserve 64 bytes (8 words). This should fail because there are only 32 bytes
    // left.
    let header2 = 4u64 | (8u64 << 4);
    if kbuf.reserve(header2).err() != Some(Status::NO_SPACE) {
        return false;
    }

    // This failed reservation should have been tracked!
    if !kbuf.drop_stats.has_dropped() {
        return false;
    }
    if kbuf.drop_stats.num_dropped != 1 {
        return false;
    }
    if kbuf.drop_stats.bytes_dropped != 64 {
        return false;
    }

    // Now, drain the buffer to free all space.
    if kbuf.drain().is_err() {
        return false;
    }

    // Now, reserve 16 bytes (2 words).
    // Since first_dropped was set, this reservation should successfully write the 56-byte
    // dropped records duration event first!
    let header3 = 4u64 | (2u64 << 4);
    let mut res3 = match kbuf.reserve(header3) {
        Ok(r) => r,
        Err(_) => return false,
    };
    if res3.write_bytes(&[0u8; 8]).is_err() {
        return false;
    }
    if res3.commit().is_err() {
        return false;
    }

    // The dropped stats should have been reset!
    if kbuf.drop_stats.has_dropped() {
        return false;
    }
    if kbuf.drop_stats.num_dropped != 0 {
        return false;
    }
    if kbuf.drop_stats.bytes_dropped != 0 {
        return false;
    }

    // Let's read the buffer content.
    let mut read_bytes = [0u8; 72];
    let read_len = match kbuf.read(
        |offset, src| {
            read_bytes[offset as usize..offset as usize + src.len()].copy_from_slice(src);
            Ok(())
        },
        72,
    ) {
        Ok(l) => l,
        Err(_) => return false,
    };

    if read_len != 72 {
        return false;
    }

    // Verify the DroppedRecordDurationEvent header:
    let event_header = u64::from_ne_bytes(read_bytes[0..8].try_into().unwrap());
    if (event_header & 0xf) != 4 {
        return false;
    }
    if ((event_header >> 4) & 0xfff) != 7 {
        return false;
    }
    if ((event_header >> 16) & 0xf) != 4 {
        return false;
    }
    if ((event_header >> 20) & 0xf) != 2 {
        return false;
    }
    if ((event_header >> 24) & 0xff) != 1 {
        return false;
    }
    if ((event_header >> 32) & 0xffff) != u64::from(META_CAT.label().id()) {
        return false;
    }
    if ((event_header >> 48) & 0xffff) != u64::from(DROP_STATS_REF.id()) {
        return false;
    }

    // Verify process_koid (100) and thread_koid (200)
    if u64::from_ne_bytes(read_bytes[16..24].try_into().unwrap()) != 100 {
        return false;
    }
    if u64::from_ne_bytes(read_bytes[24..32].try_into().unwrap()) != 200 {
        return false;
    }

    // Verify the two arguments:
    let num_dropped_arg = u64::from_ne_bytes(read_bytes[32..40].try_into().unwrap());
    if (num_dropped_arg & 0xf) != 2 {
        return false;
    }
    if ((num_dropped_arg >> 4) & 0xfff) != 1 {
        return false;
    }
    if ((num_dropped_arg >> 16) & 0xffff) != u64::from(NUM_RECORDS_REF.id()) {
        return false;
    }
    if ((num_dropped_arg >> 32) & 0xffffffff) != 1 {
        return false;
    }

    let bytes_dropped_arg = u64::from_ne_bytes(read_bytes[40..48].try_into().unwrap());
    if (bytes_dropped_arg & 0xf) != 2 {
        return false;
    }
    if ((bytes_dropped_arg >> 16) & 0xffff) != u64::from(NUM_BYTES_REF.id()) {
        return false;
    }
    if ((bytes_dropped_arg >> 32) & 0xffffffff) != 64 {
        return false;
    }

    // Verify the reservation header3:
    let res_header = u64::from_ne_bytes(read_bytes[56..64].try_into().unwrap());
    if res_header != header3 {
        return false;
    }

    true
}

/// Verifies direct invocation of KTraceBuffer::emit_drop_stats.
#[unsafe(no_mangle)]
pub extern "C" fn rust_ktrace_test_emit_drop_stats() -> bool {
    let _guard = InterruptDisableGuard::new();
    let mut storage = [0u8; 128];
    let mut inner_buf = unsafe { Buffer::from_raw_parts(storage.as_mut_ptr(), storage.len()) };
    let leaked_ref = unsafe { &mut *ptr::from_mut(&mut inner_buf) };
    let mut stats = DroppedRecordStats::default();
    let leaked_stats = unsafe { &mut *ptr::from_mut(&mut stats) };
    let mut kbuf = KTraceBuffer::new(leaked_ref, leaked_stats, 1, 100, 200);

    // 1. Force a failed reservation to track a dropped record.
    let header = 4u64 | (32u64 << 4);
    if kbuf.reserve(header).err() != Some(Status::NO_SPACE) {
        return false;
    }

    if !kbuf.drop_stats.has_dropped() {
        return false;
    }
    if kbuf.drop_stats.num_dropped != 1 {
        return false;
    }
    if kbuf.drop_stats.bytes_dropped != 256 {
        return false;
    }

    // 2. Call emit_drop_stats directly.
    if kbuf.emit_drop_stats().is_err() {
        return false;
    }

    if kbuf.drop_stats.has_dropped() {
        return false;
    }
    if kbuf.drop_stats.num_dropped != 0 {
        return false;
    }
    if kbuf.drop_stats.bytes_dropped != 0 {
        return false;
    }

    // 3. Read and verify the event.
    let mut read_bytes = [0u8; 56];
    let read_len = match kbuf.read(
        |offset, src| {
            read_bytes[offset as usize..offset as usize + src.len()].copy_from_slice(src);
            Ok(())
        },
        56,
    ) {
        Ok(l) => l,
        Err(_) => return false,
    };

    if read_len != 56 {
        return false;
    }

    let event_header = u64::from_ne_bytes(read_bytes[0..8].try_into().unwrap());
    if (event_header & 0xf) != 4 {
        return false;
    }
    if ((event_header >> 4) & 0xfff) != 7 {
        return false;
    }
    if ((event_header >> 16) & 0xf) != 4 {
        return false;
    }
    if ((event_header >> 20) & 0xf) != 2 {
        return false;
    }
    if ((event_header >> 24) & 0xff) != 1 {
        return false;
    }
    if ((event_header >> 32) & 0xffff) != u64::from(META_CAT.label().id()) {
        return false;
    }
    if ((event_header >> 48) & 0xffff) != u64::from(DROP_STATS_REF.id()) {
        return false;
    }

    if u64::from_ne_bytes(read_bytes[16..24].try_into().unwrap()) != 100 {
        return false;
    }
    if u64::from_ne_bytes(read_bytes[24..32].try_into().unwrap()) != 200 {
        return false;
    }

    let num_dropped_arg = u64::from_ne_bytes(read_bytes[32..40].try_into().unwrap());
    if (num_dropped_arg & 0xf) != 2 {
        return false;
    }
    if ((num_dropped_arg >> 16) & 0xffff) != u64::from(NUM_RECORDS_REF.id()) {
        return false;
    }
    if ((num_dropped_arg >> 32) & 0xffffffff) != 1 {
        return false;
    }

    let bytes_dropped_arg = u64::from_ne_bytes(read_bytes[40..48].try_into().unwrap());
    if (bytes_dropped_arg & 0xf) != 2 {
        return false;
    }
    if ((bytes_dropped_arg >> 16) & 0xffff) != u64::from(NUM_BYTES_REF.id()) {
        return false;
    }
    if ((bytes_dropped_arg >> 32) & 0xffffffff) != 256 {
        return false;
    }

    true
}

/// Verifies the full global lifecycle of KTrace: initialization, category bitmasks,
/// CPU buffer allocation, and high-level tracing macro execution.
#[unsafe(no_mangle)]
pub extern "C" fn rust_ktrace_test_global_lifecycle() -> bool {
    let _guard = InterruptDisableGuard::new();
    // Initialize indices of the categories we're testing.
    META_CAT.set_index(0, kstring::interned_category::InternedCategory::INVALID_INDEX);
    MEMORY_CAT.set_index(1, kstring::interned_category::InternedCategory::INVALID_INDEX);
    SCHED_CAT.set_index(2, kstring::interned_category::InternedCategory::INVALID_INDEX);
    CONTENTION_CAT.set_index(3, kstring::interned_category::InternedCategory::INVALID_INDEX);
    IPC_CAT.set_index(4, kstring::interned_category::InternedCategory::INVALID_INDEX);
    IRQ_CAT.set_index(5, kstring::interned_category::InternedCategory::INVALID_INDEX);

    // 1. Initialize the global KTrace instance with the system CPU count buffers and a local
    // mock state.
    let num_cpus = max_num_cpus();
    let mut local_state = KTraceState {
        categories_bitmask: AtomicU32::new(0),
        writes_enabled: AtomicBool::new(false),
    };
    let local_state_ptr = ptr::from_mut(&mut local_state).cast::<ffi::c_void>();

    let status = unsafe { rust_ktrace_init(num_cpus, local_state_ptr) };
    if status != 0 {
        return false;
    }

    let ktrace = KTrace::get_instance();

    // 2. Verify initial states.
    if ktrace.writes_enabled() {
        return false;
    }
    if ktrace.categories_bitmask() != 0 {
        return false;
    }
    if ktrace.is_category_enabled(&META_CAT) {
        return false;
    }
    if ktrace.is_category_enabled(&IRQ_CAT) {
        return false;
    }

    // 3. Test writes_enabled.
    local_state.writes_enabled.store(true, Ordering::Release);
    if !ktrace.writes_enabled() {
        return false;
    }
    local_state.writes_enabled.store(false, Ordering::Release);
    if ktrace.writes_enabled() {
        return false;
    }

    // 4. Test categories_bitmask.
    let mask = (1 << MEMORY_CAT.index()) | (1 << CONTENTION_CAT.index());
    local_state.categories_bitmask.store(mask, Ordering::Release);
    if ktrace.categories_bitmask() != mask {
        return false;
    }
    if ktrace.is_category_enabled(&META_CAT) {
        return false;
    }
    if !ktrace.is_category_enabled(&MEMORY_CAT) {
        return false;
    }
    if ktrace.is_category_enabled(&SCHED_CAT) {
        return false;
    }
    if !ktrace.is_category_enabled(&CONTENTION_CAT) {
        return false;
    }
    if ktrace.is_category_enabled(&IPC_CAT) {
        return false;
    }

    // 5. Test CPU buffer initialization and reserve.
    let mut storage = [0u8; 256];
    let mut inner_buf = unsafe { Buffer::from_raw_parts(storage.as_mut_ptr(), storage.len()) };
    let inner_buf_ptr = ptr::from_mut(&mut inner_buf).cast::<ffi::c_void>();
    let mut stats = DroppedRecordStats::default();
    let stats_ptr = ptr::from_mut(&mut stats).cast::<ffi::c_void>();

    // Initialize current CPU buffer.
    let cpu = curr_cpu_num();
    let init_status = unsafe {
        rust_ktrace_init_cpu_buffer(
            cpu, // cpu_num
            inner_buf_ptr,
            stats_ptr,
            100, // process_koid
            200, // thread_koid
            1,   // cpu_ref_header_entry
        )
    };
    if init_status != 0 {
        return false;
    }

    // If writes are disabled, reserving should fail.
    let header = 4u64 | (2u64 << 4); // Event with 2 words
    if unsafe { ktrace.reserve(header) }.err() != Some(Status::BAD_STATE) {
        return false;
    }

    // Enable writes.
    local_state.writes_enabled.store(true, Ordering::Release);

    // Now reserve should succeed!
    let mut res = match unsafe { ktrace.reserve(header) } {
        Ok(r) => r,
        Err(_) => return false,
    };
    if res.write_word(0x1234567890abcdef).is_err() {
        return false;
    }
    if res.commit().is_err() {
        return false;
    }

    // Read and verify the written record from the current CPU buffer.
    let ptr = ktrace.get_buffer(cpu as usize);
    if ptr.is_null() {
        return false;
    }
    let buf = unsafe { &*ptr };

    let mut read_bytes = [0u8; 16];
    let read_len = match buf.read(
        |offset, src| {
            read_bytes[offset as usize..offset as usize + src.len()].copy_from_slice(src);
            Ok(())
        },
        16,
    ) {
        Ok(l) => l,
        Err(_) => return false,
    };

    if read_len != 16 {
        return false;
    }
    if u64::from_ne_bytes(read_bytes[0..8].try_into().unwrap()) != header {
        return false;
    }
    if u64::from_ne_bytes(read_bytes[8..16].try_into().unwrap()) != 0x1234567890abcdef {
        return false;
    }

    // 6. Test Rust macros!
    let mask = (1 << META_CAT.index()) | (1 << MEMORY_CAT.index());
    local_state.categories_bitmask.store(mask, Ordering::Release);
    if !ktrace.is_category_enabled(&META_CAT) {
        return false;
    }

    // Let's emit an instant event using the macro!
    instant!(META_CAT, "my_event", "arg1" => 42i32, "arg2" => "hello");

    // Now let's read the record back and verify it!
    let mut read_bytes = [0u8; 128];
    let read_len = match buf.read(
        |offset, src| {
            read_bytes[offset as usize..offset as usize + src.len()].copy_from_slice(src);
            Ok(())
        },
        56,
    ) {
        Ok(l) => l,
        Err(_) => return false,
    };

    if read_len != 56 {
        return false;
    }

    // Verify Header Word
    let header_word = u64::from_ne_bytes(read_bytes[0..8].try_into().unwrap());
    if (header_word & 0xf) != 4 {
        return false;
    }
    if ((header_word >> 4) & 0xfff) != 7 {
        return false;
    }
    if ((header_word >> 16) & 0xf) != 0 {
        return false;
    }
    if ((header_word >> 20) & 0xf) != 2 {
        return false;
    }
    if ((header_word >> 32) & 0xffff) != u64::from(META_CAT.label().id()) {
        return false;
    }
    let name_id = (header_word >> 48) & 0xffff;
    if name_id == 0 {
        return false;
    }

    // Verify Timestamp (word 1)
    let timestamp_word = u64::from_ne_bytes(read_bytes[8..16].try_into().unwrap());
    if timestamp_word == 0 {
        return false;
    }

    // Verify KOIDs (words 2 & 3)
    let process_koid = u64::from_ne_bytes(read_bytes[16..24].try_into().unwrap());
    let thread_koid = u64::from_ne_bytes(read_bytes[24..32].try_into().unwrap());
    // SAFETY: Tests run after threading has been initialized.
    let FxtRef { pid: expected_proc, tid: expected_thread } =
        unsafe { ThreadPtr::current().fxt_ref() };
    if process_koid != expected_proc {
        return false;
    }
    if thread_koid != expected_thread {
        return false;
    }

    // Verify Argument 1 ("arg1" => 42i32) (word 4)
    let arg1_header = u64::from_ne_bytes(read_bytes[32..40].try_into().unwrap());
    if (arg1_header & 0xf) != 1 {
        return false;
    }
    if ((arg1_header >> 32) & 0xffffffff) != 42 {
        return false;
    }
    let arg1_name_id = (arg1_header >> 16) & 0xffff;
    if arg1_name_id == 0 {
        return false;
    }

    // Verify Argument 2 ("arg2" => "hello") (words 5 & 6)
    let arg2_header = u64::from_ne_bytes(read_bytes[40..48].try_into().unwrap());
    if (arg2_header & 0xf) != 6 {
        return false;
    }
    if ((arg2_header >> 4) & 0xf) != 1 {
        return false;
    }
    if ((arg2_header >> 32) & 0xffffffff) != 5 {
        return false;
    }
    let arg2_name_id = (arg2_header >> 16) & 0xffff;
    if arg2_name_id == 0 {
        return false;
    }

    let arg2_val = &read_bytes[48..56];
    if &arg2_val[0..5] != b"hello" {
        return false;
    }
    if &arg2_val[5..8] != &[0, 0, 0] {
        return false;
    }

    true
}
