// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_CHANNEL_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_CHANNEL_DISPATCHER_H_

#include <lib/object-constants.h>
#include <stdint.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/dispatcher.h>
#include <object/handle.h>

class ChannelDispatcher;

DECLARE_PEERED_DISPATCHER_RUST_PROTOS(ChannelDispatcher, rust_channel_dispatcher)

extern "C" {
zx_status_t rust_channel_dispatcher_create(KernelHandle<ChannelDispatcher>* handle0,
                                           KernelHandle<ChannelDispatcher>* handle1,
                                           zx_rights_t* rights);
void rust_channel_dispatcher_get_message_counts(const ChannelDispatcher* disp, uint64_t* current,
                                                uint64_t* max);
int64_t rust_channel_dispatcher_get_channel_full_count();
}  // extern "C"

class ChannelDispatcher final : public Dispatcher {
 public:
  struct MessageCounts {
    uint64_t current;
    uint64_t max;
  };

  static zx_status_t Create(KernelHandle<ChannelDispatcher>* handle0,
                            KernelHandle<ChannelDispatcher>* handle1, zx_rights_t* rights) {
    return rust_channel_dispatcher_create(handle0, handle1, rights);
  }

  static int64_t get_channel_full_count() {
    return rust_channel_dispatcher_get_channel_full_count();
  }

  explicit ChannelDispatcher(void* holder);
  ~ChannelDispatcher() final;

  DECLARE_PEERED_DISPATCHER_RUST_METHODS(rust_channel_dispatcher, ZX_OBJ_TYPE_CHANNEL, true)

  MessageCounts get_message_counts() const {
    MessageCounts counts{};
    rust_channel_dispatcher_get_message_counts(this, &counts.current, &counts.max);
    return counts;
  }

 protected:
  Lock<CriticalMutex>* get_lock() const final;

 private:
  OpaqueStorage<kChannelDispatcherStateSize, kChannelDispatcherStateAlign> opaque_storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_CHANNEL_DISPATCHER_H_
