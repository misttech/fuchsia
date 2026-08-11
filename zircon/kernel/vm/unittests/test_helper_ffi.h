// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_UNITTESTS_TEST_HELPER_FFI_H_
#define ZIRCON_KERNEL_VM_UNITTESTS_TEST_HELPER_FFI_H_

#include <stdbool.h>
#include <stddef.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include "vm/vm_object_paged.h"

__BEGIN_CDECLS

zx_status_t cpp_make_committed_pager_vmo(size_t num_pages, bool trap_dirty, bool resizable,
                                         vm_page_t** out_pages, VmObjectPaged** out_vmo);

zx_status_t cpp_make_partially_committed_pager_vmo(size_t num_pages, size_t committed_pages,
                                                   bool trap_dirty, bool resizable,
                                                   bool ignore_requests, vm_page_t** out_pages,
                                                   VmObjectPaged** out_vmo);
__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_UNITTESTS_TEST_HELPER_FFI_H_
