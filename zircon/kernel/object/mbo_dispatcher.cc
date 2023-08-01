// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/mbo_dispatcher.h"

zx_status_t MBODispatcher::Create(fbl::RefPtr<MsgQueueDispatcher> msgqueue, uint64_t reply_key,
                                  KernelHandle<MBODispatcher>* handle, zx_rights_t* rights) {
  fbl::AllocChecker ac;
  KernelHandle mbo(fbl::AdoptRef(new (&ac) MBODispatcher()));
  if (!ac.check())
    return ZX_ERR_NO_MEMORY;

  // XXX: We could pass these via the constructor instead.
  mbo.dispatcher()->reply_queue_ = msgqueue;
  mbo.dispatcher()->reply_key_ = reply_key;

  *rights = default_rights();
  *handle = ktl::move(mbo);
  return ZX_OK;
}

zx_status_t MBODispatcher::Set(MessagePacketPtr msg) {
  Guard<CriticalMutex> guard{get_lock()};
  if (is_sent_)
    return ZX_ERR_BAD_STATE;
  message_ = ktl::move(msg);
  return ZX_OK;
}

void MBODispatcher::EnqueueReply(MessagePacketPtr msg) {
  // This increments the MBO's reference count.  Note that we could avoid
  // this atomic increment if WriteToChannel() instead took ownership of
  // the RefPtr held by the caller.
  msg->mbo_ = fbl::RefPtr<MBODispatcher>(this);

  msg->is_reply = true;
  reply_queue_->Write(ktl::move(msg));
}

void MBODispatcher::SetDequeuedReply(MessagePacketPtr msg) {
  Guard<CriticalMutex> guard{get_lock()};
  message_ = ktl::move(msg);
  is_sent_ = false;
}

zx_status_t CalleesRefDispatcher::Set(MessagePacketPtr msg) {
  Guard<CriticalMutex> guard{get_lock()};
  if (!mbo_)
    return ZX_ERR_NOT_CONNECTED;
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
  if (is_sent_)
    return ZX_ERR_BAD_STATE;
  return MessageRead(&message_, msg_size, msg_handle_count, msg, may_discard);
}

zx_status_t CalleesRefDispatcher::Read(uint32_t* msg_size, uint32_t* msg_handle_count,
                                       MessagePacketPtr* msg, bool may_discard) {
  canary_.Assert();

  Guard<CriticalMutex> guard{get_lock()};
  if (!mbo_)
    return ZX_ERR_NOT_CONNECTED;
  return MessageRead(&message_, msg_size, msg_handle_count, msg, may_discard);
}

zx_status_t MBODispatcher::WriteToChannel(const fbl::RefPtr<NewChannelDispatcher> channel) {
  MessagePacketPtr msg;
  {
    Guard<CriticalMutex> guard{get_lock()};
    if (!message_) {
      // TODO: We should treat this as an empty message instead.
      return ZX_ERR_BAD_STATE;
    }
    msg = ktl::move(message_);
    is_sent_ = true;
  }

  // This increments the MBO's reference count.  Note that we could avoid
  // this atomic increment if WriteToChannel() instead took ownership of
  // the RefPtr held by the caller.
  msg->mbo_ = fbl::RefPtr<MBODispatcher>(this);

  channel->Write(ktl::move(msg));
  return ZX_OK;
}

zx_status_t MsgQueueDispatcher::Create(KernelHandle<MsgQueueDispatcher>* handle,
                                       zx_rights_t* rights) {
  fbl::AllocChecker ac;
  KernelHandle mbo(fbl::AdoptRef(new (&ac) MsgQueueDispatcher()));
  if (!ac.check())
    return ZX_ERR_NO_MEMORY;

  *rights = default_rights();
  *handle = ktl::move(mbo);
  return ZX_OK;
}

void MsgQueueDispatcher::Write(MessagePacketPtr msg) {
  Guard<MonitoredSpinLock, IrqSave> guard{ThreadLock::Get(), SOURCE_TAG};
  // MsgQueueWaiter* waiter = waiters_.pop_front();
  // if (waiter) {
  //   waiter->result_msg = ktl::move(msg);
  //   waiter->wait_queue.WakeOne(/* reschedule= */ false, ZX_OK);
  // } else {
  messages_.push_back(ktl::move(msg));
  // }
}

zx_status_t MsgQueueDispatcher::Read(MessagePacketPtr* msg) {
  Guard<MonitoredSpinLock, IrqSave> guard{ThreadLock::Get(), SOURCE_TAG};
  *msg = messages_.pop_front();
  if (*msg) {
    return ZX_OK;
  }

  // MsgQueueWaiter waiter;
  // waiters_.push_back(&waiter);

  // auto current_thread = ThreadDispatcher::GetCurrent();
  // current_thread->core_thread_->interruptable_ = true;
  // zx_status_t status = waiter.wait_queue.Block(Deadline::infinite());
  // current_thread->core_thread_->interruptable_ = false;

  // if (status != ZX_OK) {
  //   // The thread was interrupted (killed or suspended).  No-one else
  //   // removed the waiter from the list, so we must do that here.
  //   waiters_.erase(waiter);
  //   return status;
  // }

  // *msg = ktl::move(waiter.result_msg);
  return ZX_OK;
}

zx_status_t CalleesRefDispatcher::Create(KernelHandle<CalleesRefDispatcher>* handle,
                                         zx_rights_t* rights) {
  fbl::AllocChecker ac;
  KernelHandle mbo(fbl::AdoptRef(new (&ac) CalleesRefDispatcher()));
  if (!ac.check())
    return ZX_ERR_NO_MEMORY;

  *rights = default_rights();
  *handle = ktl::move(mbo);
  return ZX_OK;
}

zx_status_t CalleesRefDispatcher::ReadFromMsgQueue(const fbl::RefPtr<MsgQueueDispatcher> msgqueue) {
  MessagePacketPtr msg;
  zx_status_t status = msgqueue->Read(&msg);
  if (status != ZX_OK) {
    return status;
  }
  return Populate(ktl::move(msg));
}

zx_status_t CalleesRefDispatcher::Populate(MessagePacketPtr msg) {
  if (msg->is_reply) {
    msg->is_reply = false;
    fbl::RefPtr<MBODispatcher> mbo = ktl::move(msg->mbo_);
    mbo->SetDequeuedReply(ktl::move(msg));
    return ZX_OK;
  }

  Guard<CriticalMutex> guard{get_lock()};
  if (mbo_) {
    // The CalleesRef is already in use.  We treat this as an error.  The
    // newly dequeued message is dropped, and its MBO will receive an
    // auto-reply.  The CalleesRef remains in the same state.
    //
    // Some alternatives would be:
    //  * Don't dequeue the message from the channel if the CalleesRef is
    //    already in use.  This is hard to implement without race
    //    conditions because the channel and the CalleesRef have separate
    //    locks, and we want to avoid claiming their locks at the same
    //    time.
    //  * Drop the CalleesRef's current message (and send an auto-reply for
    //    that) rather than dropping the newly dequeued message.  We don't
    //    do this because it might mask mistakes where programs fail to
    //    send replies explicitly.
    return ZX_ERR_BAD_STATE;
  }
  mbo_ = ktl::move(msg->mbo_);
  message_ = ktl::move(msg);
  return ZX_OK;
}

zx_status_t CalleesRefDispatcher::SendReply() {
  // Note that this avoids holding both the CalleesRef's lock and the MBO's
  // lock at the same time.
  fbl::RefPtr<MBODispatcher> mbo;
  MessagePacketPtr msg;
  {
    Guard<CriticalMutex> guard{get_lock()};
    if (!mbo_) {
      return ZX_ERR_NOT_CONNECTED;
    }
    if (!message_) {
      return ZX_ERR_BAD_STATE;
    }
    mbo = ktl::move(mbo_);
    msg = ktl::move(message_);
  }
  mbo->EnqueueReply(ktl::move(msg));
  return ZX_OK;
}

zx_status_t NewChannelDispatcher::Create(fbl::RefPtr<MsgQueueDispatcher> msgqueue, uint64_t key,
                                         KernelHandle<NewChannelDispatcher>* handle,
                                         zx_rights_t* rights) {
  fbl::AllocChecker ac;
  KernelHandle channel(fbl::AdoptRef(new (&ac) NewChannelDispatcher()));
  if (!ac.check())
    return ZX_ERR_NO_MEMORY;

  // XXX: We could pass these via the constructor instead.
  channel.dispatcher()->dest_queue_ = msgqueue;
  channel.dispatcher()->key_ = key;

  *rights = default_rights();
  *handle = ktl::move(channel);
  return ZX_OK;
}
