// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef __GBL_PROTOCOL_UTILS_H__
#define __GBL_PROTOCOL_UTILS_H__

#include <efi/boot-services.h>
#include <efi/types.h>

#define GBL_PROTOCOL_MAJOR_REV(x) (((x) >> 16) & 0xFFFF)
#define GBL_PROTOCOL_MINOR_REV(x) ((x) & 0xFFFF)

#define GBL_PROTOCOL_REVISION(major, minor) ((((major) & 0xFFFF) << 16) | ((minor) & 0xFFFF))

// Macro for defining enums with explicit width.
//
// It is an ergonomics and safety benefit to explicitly define
// the width of enums in the EFI interfaces defined and used by GBL.
//
// The following conventions are used for enums:
// * The enum is named using CamelCase.
// * Enum variants are defined in ALL_CAPS and are prefixed
//   with the enum name in ALL_CAPS.
// * By default enum variants start at `0` and increment.
// * If the value for the first enum variant is `0` it is omitted.
//
// e.g.
//
// EFI_ENUM(EfiMollusc, uintptr_t,
//          EFI_MOLLUSC_UNKNOWN,
//          EFI_MOLLUSC_SQUID = 1 << 0,
//          EFI_MOLLUSC_CLAM = 1 << 1,
//          EFI_MOLLUSC_WHELK = 1 << 2);
//
// If you are using C++ and your compiler does not support C++11,
// you can explicitly disable the strongly typed enum by
// defining `GBL_EFI_DISABLE_CPP_ENUMS`.
#if defined(__cplusplus) && !defined(GBL_EFI_DISABLE_CPP_ENUMS)
#define EFI_ENUM(camelname, width, ...) enum class camelname : width { __VA_ARGS__ }
#else
#define EFI_ENUM(camelname, width, ...) \
  enum { __VA_ARGS__ };                 \
  typedef width camelname
#endif

typedef efi_status EfiStatus;
typedef uint64_t EfiPhysicalAddr;
typedef efi_memory_descriptor EfiMemoryDescriptor;

#endif  // __GBL_PROTOCOL_UTILS_H__
