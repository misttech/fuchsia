// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/debug_agent/zircon_system_interface.h"

#include <string_view>

#include <gtest/gtest.h>

#include "src/developer/debug/debug_agent/component_manager.h"
#include "src/developer/debug/debug_agent/system_interface.h"
#include "src/developer/debug/debug_agent/zircon_process_handle.h"
#include "src/developer/debug/debug_agent/zircon_utils.h"
#include "src/developer/debug/ipc/filter_utils.h"
#include "src/developer/debug/shared/test_with_loop.h"

namespace debug_agent {

namespace {

// Recursively walks the process tree and returns true if there is a process
// matching the given koid. Fills the process_name if such process can be found.
// Fills the component_info if the process belongs to some component.
bool FindProcess(const debug_ipc::ProcessTreeRecord& record, zx_koid_t koid_to_find,
                 std::string* process_name, std::vector<debug_ipc::ComponentInfo>* component_info) {
  if (record.koid == koid_to_find) {
    *process_name = record.name;
    return true;
  }
  for (const auto& child : record.children) {
    if (FindProcess(child, koid_to_find, process_name, component_info)) {
      if (!record.components.empty()) {
        component_info->insert(component_info->end(), record.components.begin(),
                               record.components.end());
      }
      return true;
    }
  }
  return false;
}

}  // namespace

class ZirconSystemInterfaceTest : public debug::TestWithLoop {};

TEST_F(ZirconSystemInterfaceTest, DISABLED_GetProcessTree) {
  ZirconSystemInterface system_interface;

  system_interface.zircon_component_manager().SetReadyCallback([&]() { loop().QuitNow(); });
  loop().Run();

  debug_ipc::ProcessTreeRecord root = system_interface.GetProcessTree();

  // The root node should be a job with some children.
  EXPECT_EQ(debug_ipc::ProcessTreeRecord::Type::kJob, root.type);
  EXPECT_FALSE(root.children.empty());

  // Query ourself.
  auto self = zx::process::self();
  zx_koid_t self_koid = zircon::KoidForObject(*self);
  ASSERT_NE(ZX_KOID_INVALID, self_koid);

  // Our koid should be somewhere in the tree.
  std::string process_name;
  std::vector<debug_ipc::ComponentInfo> all_component_info;
  EXPECT_TRUE(FindProcess(root, self_koid, &process_name, &all_component_info));

  // The process_name and component info should match
  EXPECT_EQ(zircon::NameForObject(*self), process_name);
  ASSERT_FALSE(all_component_info.empty());
  ASSERT_EQ(all_component_info.size(), 1ull);

  const auto& component_info = all_component_info[0];
  EXPECT_EQ(".", component_info.moniker);
  // The url will include a hash that cannot be compared.
  ASSERT_FALSE(component_info.url.empty());
  std::string_view prefix = "fuchsia-pkg://fuchsia.com/debug_agent_unit_tests";
  std::string_view suffix = "#meta/debug_agent_unit_tests.cm";
  ASSERT_GE(component_info.url.size(), prefix.size() + suffix.size());
  EXPECT_EQ(prefix, component_info.url.substr(0, prefix.size()));
  EXPECT_EQ(suffix, component_info.url.substr(component_info.url.size() - suffix.size()));
}

TEST_F(ZirconSystemInterfaceTest, DISABLED_FindComponentInfo) {
  ZirconSystemInterface system_interface;

  system_interface.zircon_component_manager().SetReadyCallback([&]() { loop().QuitNow(); });
  loop().Run();

  zx::process handle;
  zx::process::self()->duplicate(ZX_RIGHT_SAME_RIGHTS, &handle);
  ZirconProcessHandle self(std::move(handle));

  auto components = system_interface.GetComponentManager().FindComponentInfo(self);
  ASSERT_EQ(components.size(), 1ull);

  auto component_info = components[0];
  EXPECT_EQ(".", component_info.moniker);
  // The url will include a hash that cannot be compared.
  ASSERT_FALSE(component_info.url.empty());
  std::string_view prefix = "fuchsia-pkg://fuchsia.com/debug_agent_unit_tests";
  std::string_view suffix = "#meta/debug_agent_unit_tests.cm";
  ASSERT_GE(component_info.url.size(), prefix.size() + suffix.size());
  EXPECT_EQ(prefix, component_info.url.substr(0, prefix.size()));
  EXPECT_EQ(suffix, component_info.url.substr(component_info.url.size() - suffix.size()));
}

TEST_F(ZirconSystemInterfaceTest, FilterMatchComponents) {
  ZirconSystemInterface system_interface;

  // Create a job tree like this:
  // 1: root-job
  //   2: child_job1 fake/moniker
  //     ...
  //   5: child_job2 other/moniker
  //     ...

  constexpr zx_koid_t kRootJobKoid = 1;
  constexpr zx_koid_t kChildJob1Koid = 2;
  constexpr zx_koid_t kChildJob2Koid = 5;

  auto& parent_jobs = system_interface.parent_jobs_;
  parent_jobs.insert({kChildJob1Koid, kRootJobKoid});
  parent_jobs.insert({kChildJob2Koid, kRootJobKoid});

  auto& component_info = system_interface.zircon_component_manager().running_component_info_;
  component_info.insert(
      {kChildJob1Koid, {"fake/moniker", "fuchsia-pkg://fuchsia.com/component1#meta/component.cm"}});
  component_info.insert(
      {kChildJob2Koid,
       {"other/moniker", "fuchsia-pkg://fuchsia.com/some_other#meta/other_component.cm"}});

  system_interface.zircon_component_manager().SetReadyCallback([&]() { loop().QuitNow(); });
  loop().Run();

  debug_ipc::Filter filter{.type = debug_ipc::Filter::Type::kComponentName,
                           .pattern = "component.cm"};
  auto components = system_interface.GetComponentManager().FindComponentInfo(kChildJob1Koid);
  EXPECT_EQ(components.size(), 1u);
  EXPECT_TRUE(debug_ipc::FilterMatches(filter, "", components));

  filter = {.type = debug_ipc::Filter::Type::kComponentUrl,
            .pattern = "fuchsia-pkg://fuchsia.com/some_other#meta/other_component.cm"};
  components = system_interface.GetComponentManager().FindComponentInfo(kChildJob2Koid);
  EXPECT_EQ(components.size(), 1u);
  EXPECT_TRUE(debug_ipc::FilterMatches(filter, "", components));
}

}  // namespace debug_agent
