// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Pure C ABI (`driver_lab_c`) for the embedded `driver-lab` runtime (Spec
//! Phase 2 Section 3.2).
//!
//! Exposes opaque builder and server handles so C and C++ DFv2 drivers can
//! register live MMIO VMOs, state/knob VMOs, interrupt taps, and quiesce hooks
//! with the Rust `EmbeddedLabServer` core without duplicating policy, digest,
//! session, or audit logic in C++.

#![allow(clippy::missing_safety_doc)]

use driver_lab_rust::{DriverLabBuilder, EmbeddedLabServer};
use fuchsia_async as fasync;
use futures::StreamExt as _;
use futures::channel::mpsc;
use lab_proxy_core::access_policy::WritableRegister;
use std::ffi::{c_char, c_void};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::JoinHandle;

/// Opaque C handle wrapping a [`DriverLabBuilder`].
pub struct DriverLabBuilderHandle {
    inner: Option<DriverLabBuilder>,
}

/// Opaque C handle wrapping a running [`EmbeddedLabServer`] and its dedicated
/// background FIDL executor thread.
pub struct DriverLabServerHandle {
    inner: EmbeddedLabServer,
    channel_tx: Option<mpsc::UnboundedSender<zx::Channel>>,
    worker_thread: Option<JoinHandle<()>>,
}

impl Drop for DriverLabServerHandle {
    fn drop(&mut self) {
        self.inner.stop();
        // Drop the sender first so the background executor loop terminates.
        drop(self.channel_tx.take());
        if let Some(handle) = self.worker_thread.take() {
            let _ = handle.join();
        }
    }
}

struct SendContext(usize);

unsafe fn str_from_c_parts(ptr: *const c_char, len: usize, fallback: &str) -> String {
    if ptr.is_null() || len == 0 {
        return fallback.to_string();
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// Creates a new [`DriverLabBuilderHandle`] for the given node identity string.
///
/// # Safety
/// If `node_identity_len > 0`, `node_identity_ptr` must point to at least
/// `node_identity_len` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_builder_new(
    node_identity_ptr: *const c_char,
    node_identity_len: usize,
) -> *mut DriverLabBuilderHandle {
    let identity =
        unsafe { str_from_c_parts(node_identity_ptr, node_identity_len, "driver-lab.embedded") };
    Box::into_raw(Box::new(DriverLabBuilderHandle { inner: Some(DriverLabBuilder::new(identity)) }))
}

/// Destroys an unbuilt [`DriverLabBuilderHandle`].
///
/// # Safety
/// `builder` must be either null or a valid pointer returned by
/// [`driver_lab_builder_new`] that has not yet been passed to
/// [`driver_lab_builder_build`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_builder_destroy(builder: *mut DriverLabBuilderHandle) {
    if !builder.is_null() {
        unsafe {
            drop(Box::from_raw(builder));
        }
    }
}

/// Explicitly enables or disables the embedded server.
///
/// # Safety
/// `builder` must be a valid pointer returned by [`driver_lab_builder_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_builder_with_enabled(
    builder: *mut DriverLabBuilderHandle,
    enabled: bool,
) {
    let Some(handle) = (unsafe { builder.as_mut() }) else {
        return;
    };
    if let Some(inner) = handle.inner.take() {
        handle.inner = Some(inner.with_enabled(enabled));
    }
}

/// Sets whether mutating sessions are permitted on this embedded server.
///
/// # Safety
/// `builder` must be a valid pointer returned by [`driver_lab_builder_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_builder_with_allow_mutating_sessions(
    builder: *mut DriverLabBuilderHandle,
    allow: bool,
) {
    let Some(handle) = (unsafe { builder.as_mut() }) else {
        return;
    };
    if let Some(inner) = handle.inner.take() {
        handle.inner = Some(inner.with_allow_mutating_sessions(allow));
    }
}

/// Registers an MMIO or state-bank VMO with the builder.
///
/// Duplicates `vmo_handle` with `ZX_RIGHT_SAME_RIGHTS` so the caller retains
/// ownership of `vmo_handle`.
///
/// # Safety
/// `builder` must be a valid [`DriverLabBuilderHandle`] pointer. `name_ptr`
/// must be valid for `name_len` bytes, and `out_id` must be either null or a
/// valid pointer to a `u32`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_builder_with_mmio_vmo(
    builder: *mut DriverLabBuilderHandle,
    name_ptr: *const c_char,
    name_len: usize,
    vmo_handle: zx::sys::zx_handle_t,
    offset: usize,
    size: usize,
    out_id: *mut u32,
) -> zx::sys::zx_status_t {
    let Some(handle) = (unsafe { builder.as_mut() }) else {
        return zx::sys::ZX_ERR_INVALID_ARGS;
    };
    let Some(inner) = handle.inner.as_mut() else {
        return zx::sys::ZX_ERR_BAD_STATE;
    };
    if vmo_handle == zx::sys::ZX_HANDLE_INVALID || size == 0 {
        return zx::sys::ZX_ERR_INVALID_ARGS;
    }
    let name = unsafe { str_from_c_parts(name_ptr, name_len, "mmio") };
    let unowned_vmo = unsafe { zx::Unowned::<zx::Vmo>::from_raw_handle(vmo_handle) };
    match inner.add_mmio_vmo(name, &unowned_vmo, offset, size as u64) {
        Ok(id) => {
            if !out_id.is_null() {
                unsafe {
                    *out_id = id;
                }
            }
            zx::sys::ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// Replaces the writable 32-bit register offsets on a previously registered
/// MMIO or state-bank resource.
///
/// # Safety
/// `builder` must be valid, and if `offsets_len > 0`, `offsets_ptr` must point
/// to `offsets_len` valid `u64` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_builder_with_writable_registers(
    builder: *mut DriverLabBuilderHandle,
    resource_id: u32,
    offsets_ptr: *const u64,
    offsets_len: usize,
) {
    let Some(handle) = (unsafe { builder.as_mut() }) else {
        return;
    };
    let Some(inner) = handle.inner.as_mut() else {
        return;
    };
    let offsets: &[u64] = if offsets_ptr.is_null() || offsets_len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(offsets_ptr, offsets_len) }
    };
    let writable_registers = offsets
        .iter()
        .map(|&offset| WritableRegister {
            offset,
            width: 4,
            allow_mask: 0xFFFF_FFFF,
            allow_rmw: true,
            require_precondition: false,
            precondition_mask: 0,
            readback: false,
        })
        .collect();
    inner.set_writable_registers(resource_id, writable_registers);
}

/// Replaces the writable 32-bit registers on a previously registered MMIO or
/// state-bank resource, limiting each register at `offsets_ptr[i]` to the
/// bits set in `masks_ptr[i]`.
///
/// Host writes to a masked register must use a write mask within its allowed
/// bits, so read-modify-write preserves the rest of the register. Use this
/// for control registers that also hold reset or run/stop bits.
///
/// # Safety
/// `builder` must be valid, and if `len > 0`, `offsets_ptr` and `masks_ptr`
/// must each point to `len` valid elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_builder_with_masked_writable_registers(
    builder: *mut DriverLabBuilderHandle,
    resource_id: u32,
    offsets_ptr: *const u64,
    masks_ptr: *const u32,
    len: usize,
) {
    let Some(handle) = (unsafe { builder.as_mut() }) else {
        return;
    };
    let Some(inner) = handle.inner.as_mut() else {
        return;
    };
    let (offsets, masks): (&[u64], &[u32]) =
        if offsets_ptr.is_null() || masks_ptr.is_null() || len == 0 {
            (&[], &[])
        } else {
            unsafe {
                (
                    std::slice::from_raw_parts(offsets_ptr, len),
                    std::slice::from_raw_parts(masks_ptr, len),
                )
            }
        };
    let writable_registers = offsets
        .iter()
        .zip(masks)
        .map(|(&offset, &allow_mask)| WritableRegister {
            offset,
            width: 4,
            allow_mask,
            allow_rmw: true,
            require_precondition: false,
            precondition_mask: 0,
            readback: false,
        })
        .collect();
    inner.set_writable_registers(resource_id, writable_registers);
}

/// Replaces the hard-denied byte ranges (`[starts[i], ends[i])`) on a
/// previously registered MMIO resource.
///
/// # Safety
/// `builder` must be valid, and if `ranges_len > 0`, `starts_ptr` and
/// `ends_ptr` must each point to `ranges_len` valid `u64` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_builder_with_hard_denied_ranges(
    builder: *mut DriverLabBuilderHandle,
    resource_id: u32,
    starts_ptr: *const u64,
    ends_ptr: *const u64,
    ranges_len: usize,
) {
    let Some(handle) = (unsafe { builder.as_mut() }) else {
        return;
    };
    let Some(inner) = handle.inner.as_mut() else {
        return;
    };
    let (starts, ends): (&[u64], &[u64]) =
        if starts_ptr.is_null() || ends_ptr.is_null() || ranges_len == 0 {
            (&[], &[])
        } else {
            unsafe {
                (
                    std::slice::from_raw_parts(starts_ptr, ranges_len),
                    std::slice::from_raw_parts(ends_ptr, ranges_len),
                )
            }
        };
    let ranges = starts
        .iter()
        .zip(ends.iter())
        .filter_map(|(&start, &end)| (start < end).then_some(start..end))
        .collect();
    inner.set_hard_denied_ranges(resource_id, ranges);
}

/// Registers a named interrupt resource for event tapping and returns its
/// assigned `ResourceId` (or `u32::MAX` if `builder` is invalid).
///
/// # Safety
/// `builder` must be valid, and if `name_len > 0`, `name_ptr` must point to
/// `name_len` valid bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_builder_with_interrupt(
    builder: *mut DriverLabBuilderHandle,
    name_ptr: *const c_char,
    name_len: usize,
) -> u32 {
    let Some(handle) = (unsafe { builder.as_mut() }) else {
        return u32::MAX;
    };
    let Some(inner) = handle.inner.as_mut() else {
        return u32::MAX;
    };
    let name = unsafe { str_from_c_parts(name_ptr, name_len, "irq") };
    inner.add_interrupt(name)
}

/// Registers a synchronous C quiesce hook invoked with `paused = true` when a
/// mutating session acquires the mutation lease and `paused = false` when the
/// session closes or the server stops.
///
/// # Safety
/// `builder` must be valid. `callback` and `context` must remain valid and
/// thread-safe for the lifetime of the built [`DriverLabServerHandle`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_builder_with_quiesce_hook(
    builder: *mut DriverLabBuilderHandle,
    callback: Option<extern "C" fn(context: *mut c_void, paused: bool)>,
    context: *mut c_void,
) {
    let Some(handle) = (unsafe { builder.as_mut() }) else {
        return;
    };
    let Some(cb) = callback else {
        return;
    };
    let Some(inner) = handle.inner.as_mut() else {
        return;
    };
    let send_ctx = SendContext(context as usize);
    inner.with_quiesce_hook(move |paused| {
        let raw_ctx = send_ctx.0 as *mut c_void;
        cb(raw_ctx, paused);
    });
}

/// Consumes `builder`, validates registered resources, starts the background
/// FIDL executor thread (if enabled), and writes the resulting
/// [`DriverLabServerHandle`] to `*out_server`.
///
/// Note: `builder` is always consumed and freed by this call, even on error.
///
/// # Safety
/// `builder` must be a valid pointer returned by [`driver_lab_builder_new`],
/// and `out_server` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_builder_build(
    builder: *mut DriverLabBuilderHandle,
    out_server: *mut *mut DriverLabServerHandle,
) -> zx::sys::zx_status_t {
    if out_server.is_null() {
        if !builder.is_null() {
            unsafe {
                drop(Box::from_raw(builder));
            }
        }
        return zx::sys::ZX_ERR_INVALID_ARGS;
    }
    unsafe {
        *out_server = std::ptr::null_mut();
    }
    if builder.is_null() {
        return zx::sys::ZX_ERR_INVALID_ARGS;
    }
    let mut boxed_builder = unsafe { Box::from_raw(builder) };
    let Some(inner_builder) = boxed_builder.inner.take() else {
        return zx::sys::ZX_ERR_BAD_STATE;
    };
    let server = match inner_builder.build() {
        Ok(s) => s,
        Err(_) => return zx::sys::ZX_ERR_INVALID_ARGS,
    };

    let (channel_tx, worker_thread) = if server.is_enabled() {
        let (tx, mut rx) = mpsc::unbounded::<zx::Channel>();
        let server_clone = server.clone();
        let thread_res =
            std::thread::Builder::new().name("driver-lab-c-server".to_string()).spawn(move || {
                let mut executor = fasync::LocalExecutor::default();
                executor.run_singlethreaded(async move {
                    let scope = fasync::Scope::new_with_name("driver-lab-c-scope");
                    while let Some(channel) = rx.next().await {
                        server_clone.serve_proxy_channel(scope.to_handle(), channel);
                    }
                });
            });
        match thread_res {
            Ok(join_handle) => (Some(tx), Some(join_handle)),
            Err(_) => return zx::sys::ZX_ERR_NO_RESOURCES,
        }
    } else {
        (None, None)
    };

    let server_handle =
        Box::new(DriverLabServerHandle { inner: server, channel_tx, worker_thread });
    unsafe {
        *out_server = Box::into_raw(server_handle);
    }
    zx::sys::ZX_OK
}

/// Returns whether the embedded server is enabled.
///
/// # Safety
/// `server` must be null or a valid [`DriverLabServerHandle`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_server_is_enabled(
    server: *const DriverLabServerHandle,
) -> bool {
    let Some(handle) = (unsafe { server.as_ref() }) else {
        return false;
    };
    handle.inner.is_enabled()
}

/// Returns whether the driver is currently quiesced due to an active mutating
/// session.
///
/// # Safety
/// `server` must be null or a valid [`DriverLabServerHandle`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_server_is_quiesced(
    server: *const DriverLabServerHandle,
) -> bool {
    let Some(handle) = (unsafe { server.as_ref() }) else {
        return false;
    };
    handle.inner.is_quiesced()
}

/// Consumes a raw `fuchsia.driver.lab/Proxy` server-end channel handle and
/// spawns a connection handler on the server's background executor.
///
/// Always takes ownership of `proxy_channel` (closing it on error if valid).
///
/// # Safety
/// `server` must be null or a valid [`DriverLabServerHandle`] pointer, and
/// `proxy_channel` must be either `ZX_HANDLE_INVALID` or an owned channel
/// handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_server_serve_proxy(
    server: *const DriverLabServerHandle,
    proxy_channel: zx::sys::zx_handle_t,
) -> zx::sys::zx_status_t {
    if proxy_channel == zx::sys::ZX_HANDLE_INVALID {
        return zx::sys::ZX_ERR_INVALID_ARGS;
    }
    let channel = zx::Channel::from(unsafe { zx::NullableHandle::from_raw(proxy_channel) });
    let Some(handle) = (unsafe { server.as_ref() }) else {
        return zx::sys::ZX_ERR_INVALID_ARGS;
    };
    if !handle.inner.is_enabled() {
        return zx::sys::ZX_ERR_NOT_SUPPORTED;
    }
    let Some(tx) = handle.channel_tx.as_ref() else {
        return zx::sys::ZX_ERR_BAD_STATE;
    };
    match tx.unbounded_send(channel) {
        Ok(()) => zx::sys::ZX_OK,
        Err(_) => zx::sys::ZX_ERR_PEER_CLOSED,
    }
}

/// Taps an interrupt event from the driver's interrupt service routine.
///
/// # Safety
/// `server` must be null or a valid [`DriverLabServerHandle`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_server_notify_interrupt(
    server: *const DriverLabServerHandle,
    resource_id: u32,
) {
    if let Some(handle) = unsafe { server.as_ref() } {
        let _ = handle.inner.notify_interrupt(resource_id);
    }
}

/// Stops accepting new sessions, releases any active mutation lease and
/// quiesce hook, and cancels pending interrupt waiters.
///
/// # Safety
/// `server` must be null or a valid [`DriverLabServerHandle`] pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_server_stop(server: *const DriverLabServerHandle) {
    if let Some(handle) = unsafe { server.as_ref() } {
        handle.inner.stop();
    }
}

/// Stops and destroys the [`DriverLabServerHandle`], joining its background
/// executor thread.
///
/// # Safety
/// `server` must be null or a valid pointer produced by
/// [`driver_lab_builder_build`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_server_destroy(server: *mut DriverLabServerHandle) {
    if !server.is_null() {
        unsafe {
            drop(Box::from_raw(server));
        }
    }
}

// ---------------------------------------------------------------------------
// Global C singleton state / knob bank helpers for legacy C files
// ---------------------------------------------------------------------------

struct GlobalStateBank {
    base_ptr: usize,
    size: usize,
}

static GLOBAL_STATE_BANK: Mutex<Option<GlobalStateBank>> = Mutex::new(None);

/// Registers a mapped state/knob VMO buffer as the process-wide global bank for
/// [`driver_lab_global_set_state_u32`] and [`driver_lab_global_get_knob_u32`].
///
/// # Safety
/// `base_ptr` must point to a valid, 4-byte-aligned mapped VMO region of at
/// least `size` bytes that remains mapped until unregistered via
/// [`driver_lab_global_unregister_state_bank`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn driver_lab_global_register_state_bank(base_ptr: *mut u8, size: usize) {
    let mut guard = GLOBAL_STATE_BANK.lock().unwrap();
    if base_ptr.is_null() || size == 0 {
        *guard = None;
    } else {
        *guard = Some(GlobalStateBank { base_ptr: base_ptr as usize, size });
    }
}

/// Unregisters `base_ptr` from the global state/knob bank if it matches the
/// currently registered pointer.
#[unsafe(no_mangle)]
pub extern "C" fn driver_lab_global_unregister_state_bank(base_ptr: *const u8) {
    let mut guard = GLOBAL_STATE_BANK.lock().unwrap();
    if let Some(current) = guard.as_ref()
        && (base_ptr.is_null() || current.base_ptr == base_ptr as usize)
    {
        *guard = None;
    }
}

/// Writes a 32-bit driver telemetry/state word at `offset` in the globally
/// registered state VMO bank (no-op if no global bank is registered or if
/// `offset` is out of bounds / unaligned).
#[unsafe(no_mangle)]
pub extern "C" fn driver_lab_global_set_state_u32(offset: u32, value: u32) {
    let guard = GLOBAL_STATE_BANK.lock().unwrap();
    let Some(bank) = guard.as_ref() else {
        return;
    };
    let off = offset as usize;
    if !off.is_multiple_of(4) || off.checked_add(4).is_none_or(|end| end > bank.size) {
        return;
    }
    let cell = unsafe { AtomicU32::from_ptr((bank.base_ptr + off) as *mut u32) };
    cell.store(value, Ordering::SeqCst);
}

/// Reads a 32-bit host-tunable knob word at `offset` from the globally
/// registered state VMO bank, returning `default_val` if no global bank is
/// registered or if `offset` is out of bounds / unaligned.
#[unsafe(no_mangle)]
pub extern "C" fn driver_lab_global_get_knob_u32(offset: u32, default_val: u32) -> u32 {
    let guard = GLOBAL_STATE_BANK.lock().unwrap();
    let Some(bank) = guard.as_ref() else {
        return default_val;
    };
    let off = offset as usize;
    if !off.is_multiple_of(4) || off.checked_add(4).is_none_or(|end| end > bank.size) {
        return default_val;
    }
    let cell = unsafe { AtomicU32::from_ptr((bank.base_ptr + off) as *mut u32) };
    cell.load(Ordering::SeqCst)
}
