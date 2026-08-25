// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_SYSCALLS_OBJECT_PROPERTY_PRIV_H_
#define ZIRCON_KERNEL_LIB_SYSCALLS_OBJECT_PROPERTY_PRIV_H_

#include <stddef.h>
#include <zircon/types.h>

extern "C" {

zx_status_t cpp_object_get_property(zx_handle_t handle_value, uint32_t property, void* raw_value,
                                    size_t size);

zx_status_t cpp_object_set_property(zx_handle_t handle_value, uint32_t property,
                                    const void* raw_value, size_t size);

}  // extern "C"

#endif  // ZIRCON_KERNEL_LIB_SYSCALLS_OBJECT_PROPERTY_PRIV_H_
