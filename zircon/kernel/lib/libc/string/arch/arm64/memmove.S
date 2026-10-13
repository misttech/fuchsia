// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// The memmove implementation is verbatim from cortex-strings.

// We include it here so we can define aliases for the sanitizer runtime.  The
// assembler gives warnings if `.weak foo` and `.global foo` both appear,
// though the last one always wins.  To avoid touching the cortex-strings code
// temporarily redefine `global` as a macro; it's only used once in the file.
// Also redirect its tail call to `__unsanitized_memcpy` so sanitized builds
// don't re-enter `__asan_memcpy` (which rejects overlapping ranges).

#define global weak
#define memcpy __unsanitized_memcpy
#include "third_party/lib/cortex-strings/src/aarch64/memmove.S"
#undef memcpy
#undef global

.globl __unsanitized_memmove
.hidden __unsanitized_memmove
__unsanitized_memmove = memmove
