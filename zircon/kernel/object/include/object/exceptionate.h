// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_EXCEPTIONATE_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_EXCEPTIONATE_H_

#include <lib/object-constants.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <object/channel_dispatcher.h>
#include <object/handle.h>

class Exceptionate;

extern "C" {
void rust_exceptionate_init_storage(void* storage, uint32_t type);
void rust_exceptionate_drop_storage(void* storage);
zx_status_t rust_exceptionate_set_channel(Exceptionate* exceptionate,
                                          KernelHandle<ChannelDispatcher>* channel,
                                          zx_rights_t thread_rights, zx_rights_t process_rights);
void rust_exceptionate_shutdown(Exceptionate* exceptionate);
bool rust_exceptionate_has_valid_channel(const Exceptionate* exceptionate);
}

// Kernel-owned exception channel endpoint.
//
// This class is thread-safe, does not require external synchronization.
// Note: Not marked `final` because `JobDispatcher::DebugExceptionate` inherits from it.
class Exceptionate {
 public:
  // |type| must be a valid ZX_EXCEPTION_CHANNEL_TYPE_* constant.
  explicit Exceptionate(uint32_t type) { rust_exceptionate_init_storage(&storage_, type); }

  // Shuts the underlying channel down if it's still connected to be sure the
  // userspace endpoint gets the PEER_CLOSED signal.
  //
  // In most cases the task wants to manually shutdown the exceptionate when
  // transitioning to a dead state, but in some cases tasks can be destroyed
  // without registering the dead state e.g. childless jobs.
  ~Exceptionate() { rust_exceptionate_drop_storage(&storage_); }

  Exceptionate(const Exceptionate&) = delete;
  Exceptionate& operator=(const Exceptionate&) = delete;
  Exceptionate(Exceptionate&&) = delete;
  Exceptionate& operator=(Exceptionate&&) = delete;

  // Sets the backing ChannelDispatcher endpoint.
  //
  // The exception channel is first-come-first-served, so if there is
  // already a valid channel in place (i.e. has a live peer) this will
  // fail.
  //
  // The |*_rights| arguments give the rights to assign to task handles
  // provided through this exception channel. A value of 0 indicates that
  // the handle should not be made available through this channel.
  //
  // Returns:
  //   ZX_ERR_INVALID_ARGS if |channel_handle| is null.
  //   ZX_ERR_ALREADY_BOUND if there is already a valid channel.
  //   ZX_ERR_BAD_STATE if Shutdown() has already been called.
  zx_status_t SetChannel(KernelHandle<ChannelDispatcher> channel_handle, zx_rights_t thread_rights,
                         zx_rights_t process_rights) {
    return rust_exceptionate_set_channel(this, &channel_handle, thread_rights, process_rights);
  }

  // Removes any exception channel, which will signal PEER_CLOSED for the
  // userspace endpoint.
  //
  // Any further attempt to set a new channel will return ZX_ERR_BAD_STATE.
  void Shutdown() { rust_exceptionate_shutdown(this); }

  // Returns true if the channel exists and has a valid userspace peer.
  bool HasValidChannel() const { return rust_exceptionate_has_valid_channel(this); }

 private:
  OpaqueStorage<kExceptionateStorageSize, kExceptionateStorageAlign> storage_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_EXCEPTIONATE_H_
