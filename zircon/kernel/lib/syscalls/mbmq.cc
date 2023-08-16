// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <inttypes.h>
#include <lib/syscalls/forward.h>
#include <zircon/errors.h>

#include <object/mbo_dispatcher.h>

// zx_status_t zx_mbo_create
zx_status_t sys_mbo_create(uint32_t options, zx_handle_t msgqueue_handle, uint64_t reply_key,
                           zx_handle_t* out) {
  if (options != 0u)
    return ZX_ERR_INVALID_ARGS;

  auto up = ProcessDispatcher::GetCurrent();

  fbl::RefPtr<MsgQueueDispatcher> msgqueue;
  zx_status_t status = up->handle_table().GetDispatcher(*up, msgqueue_handle, &msgqueue);
  if (status != ZX_OK) {
    return status;
  }

  KernelHandle<MBODispatcher> handle;
  zx_rights_t rights;

  zx_status_t result = MBODispatcher::Create(ktl::move(msgqueue), reply_key, &handle, &rights);
  if (result != ZX_OK)
    return result;
  return up->MakeAndAddHandle(ktl::move(handle), rights, out);
}

// zx_status_t zx_msgqueue_create
zx_status_t sys_msgqueue_create(uint32_t options, zx_handle_t* out) {
  if (options != 0u)
    return ZX_ERR_INVALID_ARGS;

  auto up = ProcessDispatcher::GetCurrent();

  KernelHandle<MsgQueueDispatcher> handle;
  zx_rights_t rights;

  zx_status_t result = MsgQueueDispatcher::Create(&handle, &rights);
  if (result != ZX_OK)
    return result;
  return up->MakeAndAddHandle(ktl::move(handle), rights, out);
}

// zx_status_t zx_calleesref_create
zx_status_t sys_calleesref_create(uint32_t options, zx_handle_t* out) {
  if (options != 0u)
    return ZX_ERR_INVALID_ARGS;

  auto up = ProcessDispatcher::GetCurrent();

  KernelHandle<CalleesRefDispatcher> handle;
  zx_rights_t rights;

  zx_status_t result = CalleesRefDispatcher::Create(&handle, &rights);
  if (result != ZX_OK)
    return result;
  return up->MakeAndAddHandle(ktl::move(handle), rights, out);
}

// zx_status_t zx_channel_write_mbo
zx_status_t sys_channel_write_mbo(zx_handle_t channel_handle, zx_handle_t mbo_handle) {
  auto up = ProcessDispatcher::GetCurrent();

  fbl::RefPtr<NewChannelDispatcher> channel;
  zx_status_t status =
      up->handle_table().GetDispatcherWithRights(*up, channel_handle, ZX_RIGHT_WRITE, &channel);
  if (status != ZX_OK) {
    return status;
  }

  fbl::RefPtr<MBODispatcher> mbo;
  status = up->handle_table().GetDispatcher(*up, mbo_handle, &mbo);
  if (status != ZX_OK) {
    return status;
  }

  return mbo->WriteToChannel(channel);
}

// zx_status_t zx_msgqueue_create_channel
zx_status_t sys_msgqueue_create_channel(zx_handle_t msgqueue_handle, uint64_t key,
                                        zx_handle_t* out) {
  auto up = ProcessDispatcher::GetCurrent();

  fbl::RefPtr<MsgQueueDispatcher> msgqueue;
  zx_status_t status = up->handle_table().GetDispatcher(*up, msgqueue_handle, &msgqueue);
  if (status != ZX_OK) {
    return status;
  }

  KernelHandle<NewChannelDispatcher> handle;
  zx_rights_t rights;

  zx_status_t result = NewChannelDispatcher::Create(ktl::move(msgqueue), key, &handle, &rights);
  if (result != ZX_OK)
    return result;
  return up->MakeAndAddHandle(ktl::move(handle), rights, out);
}

// zx_status_t zx_msgqueue_wait
zx_status_t sys_msgqueue_wait(zx_handle_t channel_handle, zx_handle_t calleesref_handle) {
  auto up = ProcessDispatcher::GetCurrent();

  fbl::RefPtr<MsgQueueDispatcher> channel;
  zx_status_t status =
      up->handle_table().GetDispatcherWithRights(*up, channel_handle, ZX_RIGHT_READ, &channel);
  if (status != ZX_OK) {
    return status;
  }

  fbl::RefPtr<CalleesRefDispatcher> calleesref;
  status = up->handle_table().GetDispatcher(*up, calleesref_handle, &calleesref);
  if (status != ZX_OK) {
    return status;
  }

  return calleesref->ReadFromMsgQueue(channel);
}

// zx_status_t zx_calleesref_send_reply
zx_status_t sys_calleesref_send_reply(zx_handle_t calleesref_handle) {
  auto up = ProcessDispatcher::GetCurrent();

  fbl::RefPtr<CalleesRefDispatcher> calleesref;
  zx_status_t status = up->handle_table().GetDispatcher(*up, calleesref_handle, &calleesref);
  if (status != ZX_OK) {
    return status;
  }

  return calleesref->SendReply();
}
