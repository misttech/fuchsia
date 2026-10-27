// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_INSTRUMENTATION_PRIVATE_H_
#define ZIRCON_KERNEL_LIB_INSTRUMENTATION_PRIVATE_H_

#include <stddef.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <ktl/span.h>
#include <ktl/string_view.h>
#include <ktl/type_traits.h>

class Handle;
struct PhysVmo;

struct InstrumentationDataVmo {
  // A descriptive string used in the kernel log message.
  ktl::string_view announce;

  // Name of the kind of data, matching the fuchsia.debugdata.Publish argument.
  ktl::string_view sink_name;

  // The contents are described as "up to N units", where N is bytes/scale.
  ktl::string_view units = "bytes";
  size_t scale = 1;

  Handle* handle = nullptr;
};

// profile.cc
InstrumentationDataVmo LlvmProfdataVmo();

// sancov_ffi.cc
InstrumentationDataVmo SancovGetPcVmo();
InstrumentationDataVmo SancovGetCountsVmo();

__BEGIN_CDECLS

// Matches `InstrumentationDataVmoFfi` in vmo_ffi.rs.
struct InstrumentationDataVmoFfi {
  const char* announce;
  size_t announce_len;
  const char* sink_name;
  size_t sink_name_len;
  const char* units;
  size_t units_len;
  size_t scale;
  Handle* handle;
};

void cpp_llvm_profdata_vmo(InstrumentationDataVmoFfi* out);
void cpp_sancov_get_pc_vmo(InstrumentationDataVmoFfi* out);
void cpp_sancov_get_counts_vmo(InstrumentationDataVmoFfi* out);

__END_CDECLS

static_assert(sizeof(InstrumentationDataVmoFfi) == 64);
static_assert(alignof(InstrumentationDataVmoFfi) == 8);
static_assert(offsetof(InstrumentationDataVmoFfi, announce) == 0);
static_assert(offsetof(InstrumentationDataVmoFfi, announce_len) == 8);
static_assert(offsetof(InstrumentationDataVmoFfi, sink_name) == 16);
static_assert(offsetof(InstrumentationDataVmoFfi, sink_name_len) == 24);
static_assert(offsetof(InstrumentationDataVmoFfi, units) == 32);
static_assert(offsetof(InstrumentationDataVmoFfi, units_len) == 40);
static_assert(offsetof(InstrumentationDataVmoFfi, scale) == 48);
static_assert(offsetof(InstrumentationDataVmoFfi, handle) == 56);
static_assert(ktl::is_trivial_v<InstrumentationDataVmoFfi>);

#endif  // ZIRCON_KERNEL_LIB_INSTRUMENTATION_PRIVATE_H_
