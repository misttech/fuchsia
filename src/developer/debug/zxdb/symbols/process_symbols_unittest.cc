// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/symbols/process_symbols.h"

#include <optional>

#include <gtest/gtest.h>

#include "src/developer/debug/ipc/records.h"
#include "src/developer/debug/zxdb/symbols/loaded_module_symbols.h"
#include "src/developer/debug/zxdb/symbols/module_symbols.h"
#include "src/developer/debug/zxdb/symbols/system_symbols.h"
#include "src/developer/debug/zxdb/symbols/target_symbols.h"
#include "src/developer/debug/zxdb/symbols/test_symbol_module.h"

namespace zxdb {

namespace {

class NotificationsImpl : public ProcessSymbols::Notifications {
 public:
  NotificationsImpl() = default;
  ~NotificationsImpl() = default;

  const std::vector<LoadedModuleSymbols*>& loaded() const { return loaded_; }
  const std::vector<uint64_t>& unloaded() const { return unloaded_; }
  int err_count() const { return err_count_; }

  void Clear() {
    loaded_.clear();
    unloaded_.clear();
    err_count_ = 0;
  }

  bool HaveOneUnloadFor(uint64_t addr) const {
    if (unloaded_.size() != 1u)
      return false;
    return unloaded_[0] == addr;
  }
  bool HaveOneLoadFor(uint64_t addr, const std::string& local_file) const {
    if (loaded_.size() != 1u)
      return false;
    return loaded_[0]->load_address() == addr &&
           loaded_[0]->module_symbols()->GetStatus().symbol_file == local_file;
  }

  // Notifications implementation.
  void DidLoadModuleSymbols(LoadedModuleSymbols* module) override { loaded_.push_back(module); }
  void WillUnloadModuleSymbols(LoadedModuleSymbols* module) override {
    unloaded_.push_back(module->load_address());
  }
  void OnSymbolLoadFailure(const Err& err) override { err_count_++; }

 private:
  std::vector<LoadedModuleSymbols*> loaded_;

  // Stored by load address (since the pointers are deleted).
  std::vector<uint64_t> unloaded_;

  int err_count_ = 0;
};

}  // namespace

TEST(ProcessSymbols, SetModules_Probe) {
  std::string test_file_name = TestSymbolModule::GetCheckedInTestFileName();
  std::string test_file_build_id = TestSymbolModule::kCheckedInBuildId;
  SystemSymbols system(nullptr);
  system.build_id_index().AddOneFile(test_file_name);

  TargetSymbols target(&system);

  NotificationsImpl notifications;
  ProcessSymbols process(&notifications, &target);

  // Add a new module.
  constexpr uint64_t base1 = 0x1234567890;
  std::vector<debug_ipc::Module> ipc;
  debug_ipc::Module ipc_module;
  ipc_module.base = base1;
  ipc_module.build_id = test_file_build_id;
  ipc_module.name = "module1.so";
  ipc.push_back(ipc_module);
  process.SetModules(ipc, false);

  // Should have gotten one add notifications and no others.
  ASSERT_EQ(1u, notifications.loaded().size());
  LoadedModuleSymbols* loaded_symbols = notifications.loaded()[0];
  EXPECT_EQ(base1, loaded_symbols->load_address());
  EXPECT_EQ(test_file_name, loaded_symbols->module_symbols()->GetStatus().symbol_file);
  EXPECT_EQ(test_file_name, system.build_id_index().EntryForBuildID(test_file_build_id).binary);
  EXPECT_EQ(0, notifications.err_count());
}

TEST(ProcessSymbols, SetModules) {
  // This uses two different build IDs mapping to the same file. SystemSymbols only deals with build
  // IDs rather than file uniqueness, so this will create two separate entries (what we need, even
  // though we only have one symbol test file).
  std::string fake_build_id_1 = "12345";
  std::string fake_build_id_2 = "67890";
  std::string test_file_name = TestSymbolModule::GetTestFileName();
  SystemSymbols system(nullptr);
  system.build_id_index().AddBuildIDMappingForTest(fake_build_id_1, test_file_name);
  system.build_id_index().AddBuildIDMappingForTest(fake_build_id_2, test_file_name);

  TargetSymbols target(&system);

  NotificationsImpl notifications;
  ProcessSymbols process(&notifications, &target);

  // Add a new module.
  constexpr uint64_t base1 = 0x1234567890;
  std::vector<debug_ipc::Module> ipc;
  debug_ipc::Module ipc_module;
  ipc_module.base = base1;
  ipc_module.build_id = fake_build_id_1;
  ipc_module.name = "module1.so";
  ipc.push_back(ipc_module);
  process.SetModules(ipc, false);

  // Should have gotten one add notifications and no others.
  ASSERT_EQ(1u, notifications.loaded().size());
  LoadedModuleSymbols* loaded_symbols = notifications.loaded()[0];
  EXPECT_EQ(base1, loaded_symbols->load_address());
  EXPECT_EQ(test_file_name, loaded_symbols->module_symbols()->GetStatus().symbol_file);
  EXPECT_EQ(0, notifications.err_count());

  // Replace with a different one at the same address.
  notifications.Clear();
  ipc[0].build_id = fake_build_id_2;
  process.SetModules(ipc, false);
  EXPECT_EQ(0, notifications.err_count());

  // Should have one unload and one load.
  EXPECT_TRUE(notifications.HaveOneUnloadFor(base1));
  EXPECT_TRUE(notifications.HaveOneLoadFor(base1, test_file_name));

  // Remove the second one and add back the first one at a different address.
  constexpr uint64_t base2 = 0x9876543210;
  notifications.Clear();
  ipc[0].build_id = fake_build_id_1;
  ipc[0].base = base2;
  process.SetModules(ipc, false);
  EXPECT_EQ(0, notifications.err_count());

  // Should have one unload and one load.
  EXPECT_TRUE(notifications.HaveOneUnloadFor(base1));
  EXPECT_TRUE(notifications.HaveOneLoadFor(base2, test_file_name));

  // Remove everything.
  notifications.Clear();
  ipc.clear();
  process.SetModules(ipc, false);
  EXPECT_EQ(0, notifications.err_count());

  // Should have one unload and no load.
  EXPECT_TRUE(notifications.HaveOneUnloadFor(base2));
  EXPECT_EQ(0u, notifications.loaded().size());
}

// A forced reload must release the old symbols before loading the new ones. Otherwise both copies
// of every module are alive at the same time, which doubles the peak memory usage for large
// modules.
TEST(ProcessSymbols, SetModules_ForceReloadReleasesOldSymbolsFirst) {
  std::string fake_build_id = "12345";
  std::string test_file_name = TestSymbolModule::GetTestFileName();
  SystemSymbols system(nullptr);
  system.build_id_index().AddBuildIDMappingForTest(fake_build_id, test_file_name);

  TargetSymbols target(&system);

  // Records whether the old symbols are still alive at the point where loading is about to start.
  class ReloadNotifications : public NotificationsImpl {
   public:
    fxl::WeakPtr<ModuleSymbols> old_symbols;
    std::optional<bool> old_alive_at_load_start;

    void WillLoadModuleSymbols(int num_modules) override {
      if (old_alive_at_load_start.has_value())
        old_alive_at_load_start = !!old_symbols;
    }
  };
  ReloadNotifications notifications;
  ProcessSymbols process(&notifications, &target);

  constexpr uint64_t base = 0x1234567890;
  std::vector<debug_ipc::Module> ipc;
  debug_ipc::Module ipc_module;
  ipc_module.base = base;
  ipc_module.build_id = fake_build_id;
  ipc_module.name = "module1.so";
  ipc.push_back(ipc_module);
  process.SetModules(ipc, false);

  ASSERT_EQ(1u, notifications.loaded().size());
  fxl::RefPtr<ModuleSymbols> old_ref = notifications.loaded()[0]->module_symbols_ref();
  ASSERT_TRUE(old_ref);
  notifications.old_symbols = old_ref->GetWeakPtr();
  old_ref = nullptr;  // The process and target now hold the only references.
  ASSERT_TRUE(notifications.old_symbols);

  // Also populate the recent-modules cache that normally keeps symbols alive across process
  // restarts, so the test covers ReleaseAllModules() dropping that reference too.
  target.WillDestroyProcess();

  // Force reload the same module list. Arm the check with the failing value so a missing
  // WillLoadModuleSymbols() call is detected rather than passing vacuously.
  notifications.Clear();
  notifications.old_alive_at_load_start = true;
  process.SetModules(ipc, true);
  EXPECT_EQ(0, notifications.err_count());

  // The old module should have been reported as unloaded and destroyed before the new load
  // started, and a new module should have been loaded. Note that the new ModuleSymbols may be
  // allocated at the same address as the old one since the old one is freed first, so pointer
  // identity can't be compared.
  EXPECT_TRUE(notifications.HaveOneUnloadFor(base));
  ASSERT_TRUE(notifications.old_alive_at_load_start.has_value());
  EXPECT_FALSE(*notifications.old_alive_at_load_start);
  EXPECT_FALSE(notifications.old_symbols);
  EXPECT_TRUE(notifications.HaveOneLoadFor(base, test_file_name));
}

TEST(ProcessSymbols, ModuleLength) {
  std::string fake_build_id = "12345";
  std::string test_file_name = TestSymbolModule::GetTestFileName();
  SystemSymbols system(nullptr);
  system.build_id_index().AddBuildIDMappingForTest(fake_build_id, test_file_name);

  TargetSymbols target(&system);

  NotificationsImpl notifications;
  ProcessSymbols process(&notifications, &target);

  // Add a new module.
  constexpr uint64_t kBase = 0x100000000;
  std::vector<debug_ipc::Module> ipc;
  debug_ipc::Module ipc_module;
  ipc_module.base = kBase;
  ipc_module.build_id = fake_build_id;
  ipc_module.name = "module1.so";
  ipc.push_back(ipc_module);
  process.SetModules(ipc, false);

  // Valid address for the module should return it.
  EXPECT_TRUE(process.GetModuleForAddress(kBase + 1));

  // Before the module's address shouldn't match anything.
  EXPECT_FALSE(process.GetModuleForAddress(kBase - 1));

  // After the module's end shouldn't match anything (this assumes the test module is less than
  // 0x10000000 long).
  EXPECT_FALSE(process.GetModuleForAddress(kBase + 0x10000000));
}

}  // namespace zxdb
