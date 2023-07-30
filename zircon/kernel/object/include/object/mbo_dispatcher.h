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
  static zx_status_t Create(KernelHandle<MBODispatcher>* handle, zx_rights_t* rights);

  zx_obj_type_t get_type() const final { return ZX_OBJ_TYPE_MBO; }

  zx_status_t Set(MessagePacketPtr msg);
  zx_status_t Read(uint32_t* msg_size, uint32_t* msg_handle_count, MessagePacketPtr* msg,
                   bool may_discard);

 private:
  MBODispatcher() = default;

  MessagePacketPtr message_ TA_GUARDED(get_lock());
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_MBO_DISPATCHER_H_
