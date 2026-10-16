// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_LIB_ZBITL_INCLUDE_LIB_ZBITL_ASM_H_
#define SRC_LIB_ZBITL_INCLUDE_LIB_ZBITL_ASM_H_

// Assembly macros for defining ZBI headers in place.

#ifndef __ASSEMBLER__
#error "<lib/zbitl/asm.h> is only for assembly"
#endif

#include <lib/arch/asm.h>
#include <lib/zbitl/items/kernel.h>

#include <zbi/asm/zbi.h>

// clang-format off

/// Define the layout of a ZBI item header.
///
/// See zbi_header_t in <lib/zbi-format/zbi.h>.
///
/// Parameters
///
///   * name
///     - Required: Symbol name to define
///
///   * type
///     - Required: The ZBI item type. See ZBI_TYPE_* in <lib/zbi-format/zbi.h>.
///
///   * length
///     - Required: The length of the item's payload.
///
///   * extra
///     - Optional: The "extra" value of the item.
///     - Default: 0
///
///   * align
///     - Optional: alignment of the header. Must be greater than or equal to
///       ZBI_ALIGNMENT.
///     - Default: ZBI_ALIGNMENT
///
.macro .zbi.header.object name, type, length, extra=0, align=ZBI_ALIGNMENT
  .if \align < ZBI_ALIGNMENT
    .error "alignment must be >= ZBI_ALIGNMENT"
  .endif
  .object \name, rodata, align=\align, nosection=nosection
    .int \type              // type
    .int \length            // length
    .int \extra             // extra
    .int ZBI_FLAGS_VERSION  // flags
    .int 0, 0               // reserved0, reserved1
    .int ZBI_ITEM_MAGIC     // magic
    .int ZBI_ITEM_NO_CRC32  // crc32
  .end_object
.endm  // .zbi.header.object

/// Define a ZBI container header.
///
/// Parameters
///
///   * name
///     - Required: Symbol name to define
///
///   * length
///     - Required: The length of the container's payload.
///
.macro .zbi.container.object name, length, align=ZBI_ALIGNMENT
  .zbi.header.object \name, type=ZBI_TYPE_CONTAINER, length=\length, extra=ZBI_CONTAINER_MAGIC, align=\align
.endm  // .zbi.container.object

/// Defines a kernel ZBI.
///
/// Parameters
///
///   * name
///     - Required: Symbol name to define
///
///   * end
///     - Required: Name of the symbol pointing to the end of the kernel payload.
///
.macro .zbi.kernel.image.object name, end
  .zbi.container.object \name, length=(\end - .L.zbi.kernel.image.object.header.\name\().\@), align=ARCH_KERNEL_ALIGNMENT
  .zbi.header.object .L.zbi.kernel.image.object.header.\name\().\@, type=ARCH_KERNEL_TYPE, length=(\end - .L.zbi.kernel.image.object.payload.\name\().\@)
  .label .L.zbi.kernel.image.object.payload.\name\().\@, type=object
.endm //.zbi.kernel.image.object

// clang-format on

#endif  // SRC_LIB_ZBITL_INCLUDE_LIB_ZBITL_ASM_H_
