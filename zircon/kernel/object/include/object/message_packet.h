// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MESSAGE_PACKET_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MESSAGE_PACKET_H_

#include <stdint.h>
#include <zircon/types.h>

#include <ktl/unique_ptr.h>
#include <object/handle.h>

class Handle;
class MessagePacket;
namespace internal {
struct MessagePacketDeleter;
}  // namespace internal

// Definition of a MessagePacket's specific pointer type.  Message packets must
// be managed using this specific type of pointer, because MessagePackets have a
// specific custom deletion requirement.
using MessagePacketPtr = ktl::unique_ptr<MessagePacket, internal::MessagePacketDeleter>;

extern "C" {
zx_status_t rust_message_packet_create_kernel(const uint8_t* data, size_t data_size,
                                              size_t num_handles, MessagePacket** out);
void rust_message_packet_delete(MessagePacket* packet);
Handle** rust_message_packet_get_mutable_handles(MessagePacket* packet);
void rust_message_packet_set_owns_handles(MessagePacket* packet, bool owns_handles);
}  // extern "C"

class MessagePacket final {
 public:
  // Creates a message packet containing the provided data and space for
  // |num_handles| handles. The handles array is uninitialized and must
  // be completely overwritten by clients.
  static zx_status_t Create(const char* data, uint32_t data_size, uint32_t num_handles,
                            MessagePacketPtr* msg);

  Handle** mutable_handles() { return rust_message_packet_get_mutable_handles(this); }

  void set_owns_handles(bool own_handles) {
    rust_message_packet_set_owns_handles(this, own_handles);
  }

 private:
  MessagePacket() = default;
  ~MessagePacket() = default;

  friend struct internal::MessagePacketDeleter;
  static void recycle(MessagePacket* packet) { rust_message_packet_delete(packet); }
};

namespace internal {
struct MessagePacketDeleter {
  void operator()(MessagePacket* packet) const noexcept { MessagePacket::recycle(packet); }
};
}  // namespace internal

inline zx_status_t MessagePacket::Create(const char* data, uint32_t data_size, uint32_t num_handles,
                                         MessagePacketPtr* msg) {
  MessagePacket* raw = nullptr;
  zx_status_t status = rust_message_packet_create_kernel(reinterpret_cast<const uint8_t*>(data),
                                                         data_size, num_handles, &raw);
  if (status != ZX_OK) {
    return status;
  }
  msg->reset(raw);
  return ZX_OK;
}

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MESSAGE_PACKET_H_
