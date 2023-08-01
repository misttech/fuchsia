// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MBO_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MBO_DISPATCHER_H_

#include <stdint.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <fbl/canary.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/ref_counted.h>
#include <kernel/event.h>
#include <kernel/mutex.h>
#include <ktl/unique_ptr.h>
#include <object/dispatcher.h>
#include <object/handle.h>
#include <object/message_packet.h>

class MBODispatcher final : public SoloDispatcher<MBODispatcher, ZX_RIGHTS_BASIC | ZX_RIGHTS_IO> {
 public:
  static zx_status_t Create(fbl::RefPtr<MsgQueueDispatcher> msgqueue, uint64_t reply_key,
                            KernelHandle<MBODispatcher>* handle, zx_rights_t* rights);

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_MBO; }

  zx_status_t Set(MessagePacketPtr msg);
  void EnqueueReply(MessagePacketPtr msg);
  void EnqueueAutoReply();
  void SetDequeuedReply(MessagePacketPtr msg);
  zx_status_t Read(uint32_t* msg_size, uint32_t* msg_handle_count, MessagePacketPtr* msg,
                   bool may_discard);

  zx_status_t WriteToChannel(const fbl::RefPtr<NewChannelDispatcher> channel);

 private:
  MBODispatcher() = default;

  // While is_sent_ is true:
  //  * There is a reference to the MBODispatcher, either from a
  //    MessagePacket that is enqueued on a channel, or from a
  //    CalleesRefDispatcher.
  //  * The MBO cannot be written, read, or sent on a channel.
  // While is_sent_ is false, the opposite is true.
  bool is_sent_ TA_GUARDED(get_lock()) = false;

  MessagePacketPtr message_ TA_GUARDED(get_lock());

  // These are currently set on creation and don't need locking.
  // TODO: Could make them const.
  fbl::RefPtr<MsgQueueDispatcher> reply_queue_;
  uint64_t reply_key_;
};

// A MsgQueueWaiter represents a thread waiting on a MsgQueueDispatcher.
struct MsgQueueWaiter final : public fbl::DoublyLinkedListable<MsgQueueWaiter*> {
  WaitQueue wait_queue;
  MessagePacketPtr result_msg;
};

class MsgQueueDispatcher final
    : public SoloDispatcher<MsgQueueDispatcher, ZX_RIGHTS_BASIC | ZX_RIGHTS_IO> {
 public:
  static zx_status_t Create(KernelHandle<MsgQueueDispatcher>* handle, zx_rights_t* rights);

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_MSGQUEUE; }

  void Write(MessagePacketPtr msg);
  zx_status_t Read(MessagePacketPtr* msg);

 private:
  MsgQueueDispatcher() = default;

  fbl::DoublyLinkedList<MessagePacketPtr> messages_ TA_GUARDED(thread_lock);
  fbl::DoublyLinkedList<MsgQueueWaiter*> waiters_ TA_GUARDED(thread_lock);
};

class CalleesRefDispatcher final
    : public SoloDispatcher<CalleesRefDispatcher, ZX_RIGHTS_BASIC | ZX_RIGHTS_IO> {
 public:
  ~CalleesRefDispatcher() final;

  static zx_status_t Create(KernelHandle<CalleesRefDispatcher>* handle, zx_rights_t* rights);

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_CALLEESREF; }

  zx_status_t Set(MessagePacketPtr msg);
  zx_status_t Read(uint32_t* msg_size, uint32_t* msg_handle_count, MessagePacketPtr* msg,
                   bool may_discard);

  zx_status_t ReadFromMsgQueue(const fbl::RefPtr<MsgQueueDispatcher> channel);
  zx_status_t Populate(MessagePacketPtr msg);
  zx_status_t SendReply();

 private:
  CalleesRefDispatcher() = default;

  MessagePacketPtr message_ TA_GUARDED(get_lock());
  fbl::RefPtr<MBODispatcher> mbo_ TA_GUARDED(get_lock());
};

class NewChannelDispatcher final
    : public SoloDispatcher<NewChannelDispatcher, ZX_RIGHTS_BASIC | ZX_RIGHTS_IO> {
 public:
  static zx_status_t Create(fbl::RefPtr<MsgQueueDispatcher> msgqueue, uint64_t key,
                            KernelHandle<NewChannelDispatcher>* handle, zx_rights_t* rights);

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_NEWCHANNEL; }

  void Write(MessagePacketPtr msg) { dest_queue_->Write(ktl::move(msg)); }

 private:
  NewChannelDispatcher() = default;

  // These are currently set on creation and don't need locking.
  // TODO: Could make them const.
  fbl::RefPtr<MsgQueueDispatcher> dest_queue_;
  uint64_t key_;
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MBO_DISPATCHER_H_
