// Copyright 2023 The Fuchsia Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_STARNIX_LIB_USERCOPY_HERMETIC_COPY_H_
#define SRC_STARNIX_LIB_USERCOPY_HERMETIC_COPY_H_

#include <stddef.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

__BEGIN_CDECLS

// hermetic_copy performs a data copy like memcpy. It is compiled as a hermetic code bundle meaning
// that it does not branch or call out into any locations outside of the bundle while copying data
// so that faults from this routine can be identified unambiguously.
//
// At most one of `dest` and `source` may point to memory that can fault (i.e. user memory). The
// other buffer must be valid for all `count` bytes and must never fault. `ret_dest` selects whether
// the returned address is in `dest` (true) or `source` (false).
//
// Additional preconditions on arm64:
//  - `ret_dest` must be true if and only if `dest` is the buffer that can fault. The arm64
//    implementation aligns only the pointer selected by `ret_dest` to 8 bytes before copying 8
//    bytes at a time, so that no 8-byte access to the faultable buffer straddles a page boundary
//    and the copy stops exactly at the first inaccessible byte. If `ret_dest` selects the wrong
//    buffer, the copy can stop up to 7 bytes before the first inaccessible byte.
//  - The other buffer is accessed with 8-byte loads or stores that may be unaligned, so it must be
//    ordinary memory that supports unaligned accesses (e.g. not device memory).
uintptr_t hermetic_copy(volatile uint8_t* dest, const volatile uint8_t* source, size_t count,
                        bool ret_dest);

__END_CDECLS

#endif  // SRC_STARNIX_LIB_USERCOPY_HERMETIC_COPY_H_
