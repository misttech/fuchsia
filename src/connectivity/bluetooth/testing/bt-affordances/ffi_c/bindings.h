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

/// `address_type` is 1 for Public or 2 for Random, corresponding to the values of
/// fuchsia.bluetooth/AddressType.
struct DiscoveredPeer {
  uint64_t id;
  uint8_t address_type;
  uint8_t address[6];
};

/// `peer` is only valid for the duration of this callback.
using GetKnownPeersCallback = void (*)(void *context, const DiscoveredPeer *peer);

/// `address_type` is 1 for Public, 2 for Random, or 0 if no address was provided. These values
/// correspond to fuchsia.bluetooth/AddressType. If no address was provided, `address` is zero.
struct LePeer {
  uint64_t id;
  uint8_t address_type;
  uint8_t address[6];
  bool connectable;
  char name[248];
};

/// `peer` is only valid for the duration of this callback.
using LeScanCallback = void (*)(void *context, const LePeer *peer);

/// `characteristic_handles` may start with nonzero entries encoding the handles of GATT
/// characteristics discovered on the service. Up to 43 handles can be reported here.
///
/// `uuid` is the UUID in C string format including a null terminator.
struct DiscoveredService {
  uint64_t handle;
  uint32_t kind;
  char uuid[37];
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

/// Populates `addr_byte_buff` with public address of active host.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
///
/// # Safety
///
/// The caller must ensure that `addr_byte_buff` points to a valid buffer of 6 bytes.
int32_t read_local_address(uint8_t *addr_byte_buff);

/// Get all peers discovered by the system.
///
/// The callback `cb` is invoked on every peer. The `context` provided to this function is included
/// in each invocation of `cb`.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
///
/// # Safety
///
/// The caller must ensure `context` and `cb` point to valid memory & a valid callback.
int32_t get_known_peers(void *context, GetKnownPeersCallback cb);

/// Get identifier of peer with given `address`.
///
/// Returns 0 on error.
///
/// # Safety
///
/// The caller must ensure that `address` points to a valid C string encoding a BD_ADDR as a string
/// of bytes in little-endian order.
uint64_t get_peer_id(const char *address);

/// Connect to peer with given identifier.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t connect_peer(uint64_t peer_id);

/// Disconnect all logical links (BR/EDR & LE) to peer with given identifier.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t disconnect_peer(uint64_t peer_id);

/// Initiate pairing with peer with given identifier.
///
/// `le_security_level` is only relevant for LE pairing. Specify 1 for Encrypted or 2 for
/// Authenticated. All other values are interpreted as unset, defaulting to Authenticated. See
/// fuchsia.bluetooth.sys/PairingOptions for details.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t pair(uint64_t peer_id, uint32_t le_security_level, bool bondable);

/// Remove all bonding information and disconnect peer with given identifier, if found.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t forget_peer(uint64_t peer_id);

/// Connect an L2CAP channel on a specific PSM to an already-connected peer. Calling this again will
/// result in the channel being closed after the new channel is opened.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t connect_l2cap_channel(uint64_t peer_id, uint16_t psm);

/// Disconnect an L2CAP channel if one exists.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t disconnect_l2cap();

/// Start or stop general discovery procedure.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t set_discovery(bool discovery);

/// Start or revoke discoverability.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t set_discoverability(bool discoverable);

/// Set connection policy.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t set_connectability(bool connectable);

/// Scan for all nearby LE peripherals and broadcasters.
///
/// The callback `cb` is invoked on every LE peer found or updated. The `context` provided to this
/// function is included in each invocation of `cb`.
///
/// Calling this while a scan is ongoing drops and overwrites the existing scan.
///
/// Returns ZX_STATUS_INTERNAL if scan was unable to start because of an error (check logs).
///
/// # Safety
///
/// The caller must ensure `context` and `cb` point to valid memory & a valid callback.
int32_t start_le_scan(void *context, LeScanCallback cb);

/// Stop an ongoing LE scan.
///
/// Returns ZX_STATUS_BAD_STATE if no scan was ongoing.
int32_t stop_le_scan();

/// Connect to an LE peer with the given identifier.
///
/// Returns ZX_STATUS_INTERNAL on error (check logs).
int32_t connect_le(uint64_t peer_id);

/// Start advertising as an LE peripheral, accept the first connection, and return the PeerId of
/// its initiator. If no LE peer connects within `timeout` seconds, then return an arbitrary valid
/// PeerId (1). In case of error, return 0.
///
/// `address_type` is 1 for Public or 2 for Random. All other values are interpreted as unset, in
/// which case the address type will be Public or Random depending on if privacy is enabled in the
/// system. See fuchsia.bluetooth.le/AdvertisingParameters for details.
uint64_t advertise_peripheral(bool connectable, uint8_t address_type, uint64_t timeout);

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

}  // extern "C"

#endif  // SRC_CONNECTIVITY_BLUETOOTH_TESTING_BT_AFFORDANCES_FFI_C_BINDINGS_H_
