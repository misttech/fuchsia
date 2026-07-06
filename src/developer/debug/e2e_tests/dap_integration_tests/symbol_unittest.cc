// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <filesystem>
#include <vector>

#include <gtest/gtest.h>

#include "src/developer/debug/zxdb/common/err.h"
#include "src/developer/debug/zxdb/common/host_util.h"
#include "src/developer/debug/zxdb/symbols/input_location.h"
#include "src/developer/debug/zxdb/symbols/location.h"
#include "src/developer/debug/zxdb/symbols/module_symbols.h"
#include "src/developer/debug/zxdb/symbols/resolve_options.h"
#include "src/developer/debug/zxdb/symbols/symbol_context.h"
#include "src/developer/debug/zxdb/symbols/system_symbols.h"

namespace {

TEST(DapSymbolTest, CrasherSymbolAvailable) {
  std::filesystem::path symbol_dir =
      std::filesystem::path(zxdb::GetSelfPath()).parent_path() / DAP_E2E_TESTS_SYMBOL_DIR;

  ASSERT_TRUE(std::filesystem::exists(symbol_dir))
      << "Symbol directory does not exist: " << symbol_dir;

  zxdb::SystemSymbols system_symbols([](const std::string&, zxdb::DebugSymbolFileType) {});
  system_symbols.build_id_index().AddBuildIdDir(symbol_dir.string());

  bool found_blind_write = false;

  for (const auto& entry : std::filesystem::recursive_directory_iterator(symbol_dir)) {
    if (!entry.is_regular_file() || entry.path().extension() != ".debug") {
      continue;
    }

    // As seen in BuildIDIndex::SearchBuildIdDirs, lookup splits build_id via:
    //   substr(0, 2) + "/" + substr(2)
    // Here we perform the inverse to reconstruct the Build ID string from the path.
    // Example: a file at ".build-id/a1/b2c3d4.debug" transforms to Build ID "a1b2c3d4".
    std::string build_id =
        entry.path().parent_path().filename().string() + entry.path().stem().string();

    fxl::RefPtr<zxdb::ModuleSymbols> debug_module;
    zxdb::Err err = system_symbols.GetModule("", build_id, false, &debug_module);
    if (err.has_error() || !debug_module) {
      continue;
    }

    zxdb::SymbolContext symbol_context(0x10000000);
    std::vector<zxdb::Location> locs = debug_module->ResolveInputLocation(
        symbol_context,
        zxdb::InputLocation(zxdb::Identifier(zxdb::IdentifierQualification::kGlobal,
                                             zxdb::IdentifierComponent("blind_write"))),
        zxdb::ResolveOptions());

    if (!locs.empty()) {
      found_blind_write = true;
      break;
    }
  }

  EXPECT_TRUE(found_blind_write) << "Failed to resolve 'blind_write' symbol in any module under "
                                 << symbol_dir;
}

}  // namespace
