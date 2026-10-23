// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/llvm-profdata/llvm-profdata.h>
#include <zircon/assert.h>

#include <ktl/span.h>
#include <ktl/string_view.h>
#include <phys/elf-image.h>
#include <phys/symbolize.h>

#include <ktl/enforce.h>

// Publish llvm-profdata if present.
void ElfImage::PublishDebugdata(PublishDebugdataFunction publish_debugdata) const {
  // LlvmProfdata reads the running executable's own instrumentation sections,
  // so this is only correct for the self image.
  ZX_DEBUG_ASSERT(this == &gSymbolize->self());

  LlvmProfdata profdata;
  ZX_DEBUG_ASSERT(build_id_);
  profdata.Init(build_id_->desc);
  if (profdata.size_bytes() == 0) {
    return;
  }

  constexpr ktl::string_view kSinkName = LlvmProfdata::kDataSinkName;
  constexpr ktl::string_view kSuffix = LlvmProfdata::kFileSuffix;
  gSymbolize->DumpFile(LlvmProfdata::kAnnounce, profdata.size_bytes(), kSinkName, name_, kSuffix);
  ktl::span buffer = publish_debugdata(kSinkName, name_, kSuffix, profdata.size_bytes());

  // Copy the fixed data and live data values and then start updating the
  // handoff data in place.
  auto live_data = profdata.WriteFixedData(buffer);
  profdata.CopyLiveData(live_data);
  LlvmProfdata::UseLiveData(live_data);
}
