// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/object-constants.h>

#include <new>

#include <kernel/ffi.h>
#include <object/port_dispatcher.h>

static_assert(sizeof(PortInterruptPacket) == kPortInterruptPacketSize);
static_assert(alignof(PortInterruptPacket) == kPortInterruptPacketAlign);

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_port_dispatcher_packet_init(PortInterruptPacket* packet) {
  new (packet) PortInterruptPacket{};
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_port_dispatcher_packet_destroy(PortInterruptPacket* packet) {
  packet->~PortInterruptPacket();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_port_dispatcher_packet_get_key(const PortInterruptPacket* packet) {
  return packet->key;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_port_dispatcher_packet_set_key(PortInterruptPacket* packet,
                                                          uint64_t key) {
  packet->key = key;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_instant_boot_t
cpp_port_dispatcher_packet_get_timestamp(const PortInterruptPacket* packet) {
  return packet->timestamp;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_port_dispatcher_packet_set_timestamp(PortInterruptPacket* packet,
                                                                zx_instant_boot_t timestamp) {
  packet->timestamp = timestamp;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_port_dispatcher_can_bind_to_interrupt(const PortDispatcher* port) {
  return port->can_bind_to_interrupt();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_port_dispatcher_queue_interrupt_packet(PortDispatcher* port,
                                                                  PortInterruptPacket* packet,
                                                                  zx_instant_boot_t timestamp) {
  return port->QueueInterruptPacket(packet, timestamp);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_port_dispatcher_remove_interrupt_packet(PortDispatcher* port,
                                                                   PortInterruptPacket* packet) {
  return port->RemoveInterruptPacket(packet);
}

}  // extern "C"
