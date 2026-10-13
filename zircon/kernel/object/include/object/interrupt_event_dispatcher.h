// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_INTERRUPT_EVENT_DISPATCHER_H_
#define ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_INTERRUPT_EVENT_DISPATCHER_H_

#include <sys/types.h>
#include <zircon/types.h>

#include <object/interrupt_dispatcher.h>

extern "C" {
zx_status_t rust_interrupt_event_dispatcher_create(KernelHandle<InterruptDispatcher>* handle_out,
                                                   zx_rights_t* rights_out, uint32_t vector,
                                                   uint32_t options,
                                                   bool allow_ack_without_port_for_test);
}

class InterruptEventDispatcher final {
 public:
  static zx_status_t Create(KernelHandle<InterruptDispatcher>* handle, zx_rights_t* rights,
                            uint32_t vector, uint32_t options,
                            bool allow_ack_without_port_for_test = false) {
    return rust_interrupt_event_dispatcher_create(handle, rights, vector, options,
                                                  allow_ack_without_port_for_test);
  }
};

#endif  // ZIRCON_KERNEL_OBJECT_INCLUDE_OBJECT_INTERRUPT_EVENT_DISPATCHER_H_
