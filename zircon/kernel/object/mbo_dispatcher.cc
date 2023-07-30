// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/mbo_dispatcher.h"

zx_status_t MBODispatcher::Create(KernelHandle<MBODispatcher>* handle, zx_rights_t* rights) {
  fbl::AllocChecker ac;
  KernelHandle mbo(fbl::AdoptRef(new (&ac) MBODispatcher()));
  if (!ac.check())
    return ZX_ERR_NO_MEMORY;

  *rights = default_rights();
  *handle = ktl::move(mbo);
  return ZX_OK;
}

zx_status_t MBODispatcher::Set(MessagePacketPtr msg) {
  Guard<CriticalMutex> guard{get_lock()};
  // if (is_sent_)
  //   return ZX_ERR_BAD_STATE;
  message_ = ktl::move(msg);
  return ZX_OK;
}

// This is based on ChannelDispatcher::Read().
static zx_status_t MessageRead(MessagePacketPtr* message, uint32_t* msg_size,
                               uint32_t* msg_handle_count, MessagePacketPtr* out_msg,
                               bool may_discard) {
  if (!*message) {
    // We treat this as an empty message.  This saves us from having to
    // allocate an empty MessagePacket in the auto-reply case.
    *msg_size = 0;
    *msg_handle_count = 0;
    return ZX_OK;
  }

  auto max_size = *msg_size;
  auto max_handle_count = *msg_handle_count;

  *msg_size = (*message)->data_size();
  *msg_handle_count = (*message)->num_handles();
  zx_status_t rv = ZX_OK;
  if (*msg_size > max_size || *msg_handle_count > max_handle_count) {
    if (!may_discard)
      return ZX_ERR_BUFFER_TOO_SMALL;
    rv = ZX_ERR_BUFFER_TOO_SMALL;
  }

  *out_msg = ktl::move(*message);
  return rv;
}

zx_status_t MBODispatcher::Read(uint32_t* msg_size, uint32_t* msg_handle_count,
                                MessagePacketPtr* msg, bool may_discard) {
  canary_.Assert();

  Guard<CriticalMutex> guard{get_lock()};
  // if (is_sent_)
  //   return ZX_ERR_BAD_STATE;
  return MessageRead(&message_, msg_size, msg_handle_count, msg, may_discard);
}
