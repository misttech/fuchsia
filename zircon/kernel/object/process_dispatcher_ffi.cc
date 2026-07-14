// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <object/handle.h>
#include <object/process_dispatcher.h>

extern "C" {

ProcessDispatcher* cpp_process_dispatcher_current() { return ProcessDispatcher::GetCurrent(); }

zx_status_t cpp_process_dispatcher_make_and_add_handle(ProcessDispatcher* process,
                                                       KernelHandle<Dispatcher>* handle,
                                                       zx_rights_t rights,
                                                       zx_handle_t* out_handle) {
  return process->MakeAndAddHandle(ktl::move(*handle), rights, out_handle);
}

}  // extern "C"
