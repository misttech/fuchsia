// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_DRIVER_LAB_CPP_INCLUDE_LIB_DRIVER_LAB_DRIVER_LAB_C_H_
#define SRC_DEVICES_DRIVER_LAB_CPP_INCLUDE_LIB_DRIVER_LAB_DRIVER_LAB_C_H_

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

__BEGIN_CDECLS

// Opaque handle to a `driver-lab` embedded server builder.
typedef struct DriverLabBuilderHandle DriverLabBuilderHandle;

// Opaque handle to a running `driver-lab` embedded server and its background
// FIDL executor thread.
typedef struct DriverLabServerHandle DriverLabServerHandle;

// Callback invoked when a mutating host session acquires (`paused = true`) or
// releases (`paused = false`) the single-writer mutation lease.
typedef void (*driver_lab_quiesce_callback_t)(void* context, bool paused);

// Creates a new `DriverLabBuilderHandle` for the given stable node identity.
DriverLabBuilderHandle* driver_lab_builder_new(const char* node_identity_ptr,
                                               size_t node_identity_len);

// Destroys an unbuilt `DriverLabBuilderHandle`. Do not call after
// `driver_lab_builder_build`, which consumes the builder automatically.
void driver_lab_builder_destroy(DriverLabBuilderHandle* builder);

// Explicitly enables or disables the embedded server.
void driver_lab_builder_with_enabled(DriverLabBuilderHandle* builder, bool enabled);

// Configures whether mutating sessions are permitted on this embedded server.
void driver_lab_builder_with_allow_mutating_sessions(DriverLabBuilderHandle* builder, bool allow);

// Registers an MMIO or state-bank VMO with the builder.
//
// Ownership: `vmo_handle` is borrowed and duplicated internally with
// `ZX_RIGHT_SAME_RIGHTS`. The caller retains ownership of `vmo_handle`.
// On success, writes the assigned resource ID to `*out_id` (if non-null) and
// returns `ZX_OK`.
zx_status_t driver_lab_builder_with_mmio_vmo(DriverLabBuilderHandle* builder, const char* name_ptr,
                                             size_t name_len, zx_handle_t vmo_handle, size_t offset,
                                             size_t size, uint32_t* out_id);

// Replaces the writable 32-bit register offsets on `resource_id`.
void driver_lab_builder_with_writable_registers(DriverLabBuilderHandle* builder,
                                                uint32_t resource_id, const uint64_t* offsets_ptr,
                                                size_t offsets_len);

// Replaces the writable 32-bit registers on `resource_id`, limiting the
// register at `offsets_ptr[i]` to the bits set in `masks_ptr[i]`. Host writes
// must use a write mask within those bits.
void driver_lab_builder_with_masked_writable_registers(DriverLabBuilderHandle* builder,
                                                       uint32_t resource_id,
                                                       const uint64_t* offsets_ptr,
                                                       const uint32_t* masks_ptr, size_t len);

// Replaces the hard-denied byte ranges (`[starts_ptr[i], ends_ptr[i])`) on
// `resource_id`.
void driver_lab_builder_with_hard_denied_ranges(DriverLabBuilderHandle* builder,
                                                uint32_t resource_id, const uint64_t* starts_ptr,
                                                const uint64_t* ends_ptr, size_t ranges_len);

// Registers an interrupt resource for event tapping and returns its assigned
// resource ID (or `UINT32_MAX` on invalid builder).
uint32_t driver_lab_builder_with_interrupt(DriverLabBuilderHandle* builder, const char* name_ptr,
                                           size_t name_len);

// Registers a synchronous quiesce callback invoked when a mutating session
// acquires or releases the mutation lease.
void driver_lab_builder_with_quiesce_hook(DriverLabBuilderHandle* builder,
                                          driver_lab_quiesce_callback_t callback, void* context);

// Consumes `builder`, validates registered resources, starts the background
// FIDL executor thread (if enabled), and writes the resulting server handle to
// `*out_server`. Always frees `builder`.
zx_status_t driver_lab_builder_build(DriverLabBuilderHandle* builder,
                                     DriverLabServerHandle** out_server);

// Returns whether the embedded server is enabled.
bool driver_lab_server_is_enabled(const DriverLabServerHandle* server);

// Returns whether the driver is currently quiesced by an active mutating
// session.
bool driver_lab_server_is_quiesced(const DriverLabServerHandle* server);

// Consumes `proxy_channel` (a `fuchsia.driver.lab/Proxy` server-end channel
// handle) and serves it on the embedded server's background executor.
zx_status_t driver_lab_server_serve_proxy(const DriverLabServerHandle* server,
                                          zx_handle_t proxy_channel);

// Taps an interrupt event from the driver's interrupt handler.
void driver_lab_server_notify_interrupt(const DriverLabServerHandle* server, uint32_t resource_id);

// Stops accepting new sessions, releases any active mutation lease and quiesce
// hook, and cancels pending interrupt waiters.
void driver_lab_server_stop(const DriverLabServerHandle* server);

// Stops and destroys `server`, joining its background executor thread.
void driver_lab_server_destroy(DriverLabServerHandle* server);

// Registers a mapped state/knob VMO buffer as the process-wide global bank for
// `driver_lab_global_set_state_u32` and `driver_lab_global_get_knob_u32`.
void driver_lab_global_register_state_bank(uint8_t* base_ptr, size_t size);

// Unregisters `base_ptr` from the process-wide global state/knob bank.
void driver_lab_global_unregister_state_bank(const uint8_t* base_ptr);

// Writes a 32-bit telemetry/state word at `offset` in the globally registered
// state VMO bank (no-op if no global bank is registered or `offset` is out of
// bounds).
void driver_lab_global_set_state_u32(uint32_t offset, uint32_t value);

// Reads a 32-bit host-tunable knob word at `offset` from the globally
// registered state VMO bank, returning `default_val` if no global bank is
// registered or `offset` is out of bounds.
uint32_t driver_lab_global_get_knob_u32(uint32_t offset, uint32_t default_val);

__END_CDECLS

#endif  // SRC_DEVICES_DRIVER_LAB_CPP_INCLUDE_LIB_DRIVER_LAB_DRIVER_LAB_C_H_
