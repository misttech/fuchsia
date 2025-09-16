// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/boot-options/boot-options.h>

#include <phys/symbolize.h>

#include "handoff-prep.h"

void HandoffPrep::SetVersionString(ktl::string_view version) {
  constexpr ktl::string_view kSpace = " \t\r\n";
  size_t skip = version.find_first_not_of(kSpace);
  size_t trim = version.find_last_not_of(kSpace);
  if (skip == ktl::string_view::npos || trim == ktl::string_view::npos) {
    ZX_PANIC("version.txt of %zu chars empty after trimming whitespace", version.size());
  }
  trim = version.size() - (trim + 1);
  version.remove_prefix(skip);
  version.remove_suffix(trim);

  fbl::AllocChecker ac;
  ktl::string_view installed = New(handoff_->version_string, ac, version);
  if (!ac.check()) {
    ZX_PANIC("cannot allocate %zu chars of handoff space for version string", version.size());
  }
  ZX_ASSERT(installed == version);
  if (gBootOptions->phys_verbose) {
    if (skip + trim == 0) {
      printf("%s: zx_system_get_version_string (%zu chars): %.*s\n", ProgramName(), version.size(),
             static_cast<int>(version.size()), version.data());
    } else {
      printf("%s: zx_system_get_version_string (%zu chars trimmed from %zu): %.*s\n", ProgramName(),
             version.size(), version.size() + skip + trim, static_cast<int>(version.size()),
             version.data());
    }
  }
}
