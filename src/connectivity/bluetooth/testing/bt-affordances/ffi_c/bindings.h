// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_BLUETOOTH_TESTING_BT_AFFORDANCES_FFI_C_BINDINGS_H_
#define SRC_CONNECTIVITY_BLUETOOTH_TESTING_BT_AFFORDANCES_FFI_C_BINDINGS_H_

#include <cstdarg>
#include <cstdint>
#include <cstdlib>
#include <new>
#include <ostream>

constexpr static const uintptr_t MAX_NUM_CHARACTERISTICS = 43;

struct UuidBytes {
  uint8_t value[16];
};

/// `characteristic_handles` may start with nonzero entries encoding the handles of GATT
/// characteristics discovered on the service. Up to 43 handles can be reported here.
///
/// `uuid` is the UUID in C string format including a null terminator.
struct DiscoveredService {
  uint64_t handle;
  uint32_t kind;
  int8_t uuid[37];
  uint64_t characteristic_handles[MAX_NUM_CHARACTERISTICS];
};

using DiscoverServicesCallback = void (*)(void *context, const DiscoveredService *service);

struct ReadCharacteristicResult {
  uint64_t handle;
  uint8_t value[512];
  uintptr_t value_len;
  bool maybe_truncated;
};

extern "C" {

/// Stop serving Rust affordances.
///
/// Returns ZX_STATUS_INTERNAL if Rust affordances exited with an error (check logs).
int32_t stop_rust_affordances();

/// Get identifier of peer with given `address`.
///
/// Returns 0 on error.
///
/// # Safety
///
/// The caller must ensure that `address` points to a valid C string encoding a BD_ADDR as a string
/// of bytes in little-endian order.
uint64_t get_peer_id(const char *address);

/// Parse a UUID from a string.
///
/// Returns a zeroed `UuidBytes` on error.
///
/// # Safety
///
/// The caller must ensure that `uuid_str` points to a valid C string.
UuidBytes uuid_from_string(const char *uuid_str);

/// Connect an L2CAP channel on a specific PSM to an already-connected peer. Calling this again will
/// result in the channel being closed after the new channel is opened.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t connect_l2cap_channel(uint64_t peer_id, uint16_t psm);

/// Disconnect an L2CAP channel if one exists.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t disconnect_l2cap();

/// Write data over the L2CAP channel if one exists.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
///
/// # Safety
///
/// The caller must ensure that `data` points to a valid buffer of `len` bytes.
int32_t write_l2cap(const uint8_t *data, uintptr_t len);

/// Set connection policy.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t set_connectability(bool connectable);

/// Connect to an LE peer with the given identifier.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t connect_le(uint64_t peer_id);

/// Publish a local GATT service with one characteristic. GATT requests to the service are logged.
///
/// Returns ZX_STATUS_INVALID_ARGS if UUID or `characteristic_properties` are invalid (check logs).
/// Returns ZX_STATUS_INTERNAL on error in bt-affordances (check logs).
///
/// # Safety
///
/// The caller must ensure that UUIDs are validly encoded as C strings.
int32_t publish_service(uint64_t handle, const char *uuid, uint64_t characteristic_handle,
                        const char *characteristic_uuid, uint16_t characteristic_properties,
                        uint16_t characteristic_permissions);

/// Discover GATT services.
///
/// The callback `cb` is invoked on every service. The `context` provided to this function is
/// included in each invocation of `cb`.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
///
/// # Safety
///
/// The caller must ensure `context` and `cb` point to valid memory & a valid callback.
int32_t discover_services(void *context, DiscoverServicesCallback cb);

/// Read the value of a GATT characteristic on the remote peer identified with the given handles.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
///
/// # Safety
///
/// The caller must ensure that `result` points to a valid `ReadCharacteristicResult` struct.
int32_t read_characteristic(uint64_t service_handle, uint64_t characteristic_handle,
                            ReadCharacteristicResult *result);

/// Advertise a BR/EDR service on the given `psm` until the first connection. Return the PeerId of
/// that connection. If no connection is established before `timeout` seconds elapse, return an
/// arbitrary valid PeerId (1). In case of error, return 0.
uint64_t advertise_service(uint16_t psm, uint64_t timeout);

/// Enable notifications/indications on the GATT characteristic with the given handles.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t register_characteristic_notifier(uint64_t service_handle, uint64_t characteristic_handle);

}  // extern "C"

#endif  // SRC_CONNECTIVITY_BLUETOOTH_TESTING_BT_AFFORDANCES_FFI_C_BINDINGS_H_
