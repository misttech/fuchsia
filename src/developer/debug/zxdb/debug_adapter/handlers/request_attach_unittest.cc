// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_attach.h"

#include <gtest/gtest.h>

#include "src/developer/debug/zxdb/client/filter.h"
#include "src/developer/debug/zxdb/client/mock_remote_api.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/target.h"
#include "src/developer/debug/zxdb/debug_adapter/context_test.h"

namespace zxdb {

namespace {

class RequestAttachTest : public DebugAdapterContextTest {
 protected:
  dap::ResponseOrError<dap::AttachResponse> SendAttachRequest(const dap::AttachRequestZxdb& req) {
    auto response = client().send(req);
    context().OnStreamReadable();
    RunClient();
    return response.get();
  }
};

TEST_F(RequestAttachTest, AttachKoid) {
  InitializeDebugging();

  // Send attach request with numeric KOID (Process ID).
  dap::AttachRequestZxdb req = {};
  req.process = "12345";
  auto got = SendAttachRequest(req);
  EXPECT_FALSE(got.error);

  // Verify that it triggered target->Attach() natively and did NOT create any filters.
  auto filters = context().session()->system().GetFilters();
  EXPECT_EQ(filters.size(), 0u);

  auto targets = context().session()->system().GetTargets();
  ASSERT_EQ(targets.size(), 1u);
  EXPECT_EQ(targets[0]->GetState(), Target::State::kAttaching);

  ASSERT_TRUE(mock_remote_api()->last_attach().has_value());
  EXPECT_EQ(mock_remote_api()->last_attach()->koid, 12345u);
  EXPECT_EQ(mock_remote_api()->last_attach()->config.priority,
            debug_ipc::AttachConfig::Priority::kStrong);
  EXPECT_EQ(mock_remote_api()->last_attach()->config.target, debug_ipc::TaskType::kProcess);
}

TEST_F(RequestAttachTest, AttachKoidWeak) {
  InitializeDebugging();

  dap::AttachRequestZxdb req = {};
  req.process = "12345";
  req.weak = true;
  auto got = SendAttachRequest(req);
  EXPECT_FALSE(got.error);

  auto filters = context().session()->system().GetFilters();
  EXPECT_EQ(filters.size(), 0u);

  auto targets = context().session()->system().GetTargets();
  ASSERT_EQ(targets.size(), 1u);
  EXPECT_EQ(targets[0]->GetState(), Target::State::kAttaching);

  ASSERT_TRUE(mock_remote_api()->last_attach().has_value());
  EXPECT_EQ(mock_remote_api()->last_attach()->koid, 12345u);
  EXPECT_EQ(mock_remote_api()->last_attach()->config.priority,
            debug_ipc::AttachConfig::Priority::kWeak);
  EXPECT_EQ(mock_remote_api()->last_attach()->config.target, debug_ipc::TaskType::kProcess);
}

TEST_F(RequestAttachTest, AttachKoidInvalidZero) {
  InitializeDebugging();

  dap::AttachRequestZxdb req = {};
  req.process = "0";
  auto got = SendAttachRequest(req);
  EXPECT_TRUE(got.error);
  EXPECT_EQ(got.error.message, "Invalid process ID \"0\"");

  auto filters = context().session()->system().GetFilters();
  EXPECT_EQ(filters.size(), 0u);

  auto targets = context().session()->system().GetTargets();
  ASSERT_EQ(targets.size(), 1u);
  EXPECT_EQ(targets[0]->GetState(), Target::State::kNone);

  EXPECT_FALSE(mock_remote_api()->last_attach().has_value());
}

TEST_F(RequestAttachTest, AttachKoidWithExactFlagCreatesFilter) {
  InitializeDebugging();

  dap::AttachRequestZxdb req = {};
  req.process = "12345";
  req.exact = true;
  auto got = SendAttachRequest(req);
  EXPECT_FALSE(got.error);

  EXPECT_FALSE(mock_remote_api()->last_attach().has_value());
  auto filters = context().session()->system().GetFilters();
  ASSERT_EQ(filters.size(), 1u);
  EXPECT_EQ(filters[0]->pattern(), "12345");
  EXPECT_EQ(filters[0]->type(), debug_ipc::Filter::Type::kProcessName);
  EXPECT_FALSE(filters[0]->weak());
  EXPECT_FALSE(filters[0]->job_only());
}

TEST_F(RequestAttachTest, AttachKoidWithExactAndWeakFlagsCreatesFilter) {
  InitializeDebugging();

  dap::AttachRequestZxdb req = {};
  req.process = "12345";
  req.exact = true;
  req.weak = true;
  auto got = SendAttachRequest(req);
  EXPECT_FALSE(got.error);

  EXPECT_FALSE(mock_remote_api()->last_attach().has_value());
  auto filters = context().session()->system().GetFilters();
  ASSERT_EQ(filters.size(), 1u);
  EXPECT_EQ(filters[0]->pattern(), "12345");
  EXPECT_EQ(filters[0]->type(), debug_ipc::Filter::Type::kProcessName);
  EXPECT_TRUE(filters[0]->weak());
  EXPECT_FALSE(filters[0]->job_only());
}

TEST_F(RequestAttachTest, AttachKoidWithJobOnlyFlagCreatesFilter) {
  InitializeDebugging();

  dap::AttachRequestZxdb req = {};
  req.process = "12345";
  req.job_only = true;
  auto got = SendAttachRequest(req);
  EXPECT_FALSE(got.error);

  EXPECT_FALSE(mock_remote_api()->last_attach().has_value());
  auto filters = context().session()->system().GetFilters();
  ASSERT_EQ(filters.size(), 1u);
  EXPECT_EQ(filters[0]->pattern(), "12345");
  EXPECT_EQ(filters[0]->type(), debug_ipc::Filter::Type::kProcessNameSubstr);
  EXPECT_FALSE(filters[0]->weak());
  EXPECT_TRUE(filters[0]->job_only());
}

TEST_F(RequestAttachTest, AttachKoidWithJobOnlyAndWeakFlagsCreatesFilter) {
  InitializeDebugging();

  dap::AttachRequestZxdb req = {};
  req.process = "12345";
  req.job_only = true;
  req.weak = true;
  auto got = SendAttachRequest(req);
  EXPECT_FALSE(got.error);

  EXPECT_FALSE(mock_remote_api()->last_attach().has_value());
  auto filters = context().session()->system().GetFilters();
  ASSERT_EQ(filters.size(), 1u);
  EXPECT_EQ(filters[0]->pattern(), "12345");
  EXPECT_EQ(filters[0]->type(), debug_ipc::Filter::Type::kProcessNameSubstr);
  EXPECT_TRUE(filters[0]->weak());
  EXPECT_TRUE(filters[0]->job_only());
}

TEST_F(RequestAttachTest, AttachKoidWithExactAndJobOnlyFlagsCreatesFilter) {
  InitializeDebugging();

  dap::AttachRequestZxdb req = {};
  req.process = "12345";
  req.exact = true;
  req.job_only = true;
  auto got = SendAttachRequest(req);
  EXPECT_FALSE(got.error);

  EXPECT_FALSE(mock_remote_api()->last_attach().has_value());
  auto filters = context().session()->system().GetFilters();
  ASSERT_EQ(filters.size(), 1u);
  EXPECT_EQ(filters[0]->pattern(), "12345");
  EXPECT_EQ(filters[0]->type(), debug_ipc::Filter::Type::kProcessName);
  EXPECT_FALSE(filters[0]->weak());
  EXPECT_TRUE(filters[0]->job_only());
}

TEST_F(RequestAttachTest, AttachKoidWithExactJobOnlyAndWeakFlagsCreatesFilter) {
  InitializeDebugging();

  dap::AttachRequestZxdb req = {};
  req.process = "12345";
  req.exact = true;
  req.job_only = true;
  req.weak = true;
  auto got = SendAttachRequest(req);
  EXPECT_FALSE(got.error);

  EXPECT_FALSE(mock_remote_api()->last_attach().has_value());
  auto filters = context().session()->system().GetFilters();
  ASSERT_EQ(filters.size(), 1u);
  EXPECT_EQ(filters[0]->pattern(), "12345");
  EXPECT_EQ(filters[0]->type(), debug_ipc::Filter::Type::kProcessName);
  EXPECT_TRUE(filters[0]->weak());
  EXPECT_TRUE(filters[0]->job_only());
}

TEST_F(RequestAttachTest, AttachKoidWithRecursiveAndWeakFlagsAttachesByKoid) {
  InitializeDebugging();

  dap::AttachRequestZxdb req = {};
  req.process = "12345";
  req.recursive = true;
  req.weak = true;
  auto got = SendAttachRequest(req);
  EXPECT_FALSE(got.error);

  auto filters = context().session()->system().GetFilters();
  EXPECT_EQ(filters.size(), 0u);

  ASSERT_TRUE(mock_remote_api()->last_attach().has_value());
  EXPECT_EQ(mock_remote_api()->last_attach()->koid, 12345u);
  EXPECT_EQ(mock_remote_api()->last_attach()->config.priority,
            debug_ipc::AttachConfig::Priority::kWeak);
}

TEST_F(RequestAttachTest, AttachProcessName) {
  InitializeDebugging();

  // Send attach request with standard process name substring pattern
  dap::AttachRequestZxdb req = {};
  req.process = "test";
  auto got = SendAttachRequest(req);
  EXPECT_FALSE(got.error);

  // Verify direct API filter creation
  auto filters = context().session()->system().GetFilters();
  ASSERT_EQ(filters.size(), 1u);
  EXPECT_EQ(filters[0]->pattern(), "test");
  EXPECT_EQ(filters[0]->type(), debug_ipc::Filter::Type::kProcessNameSubstr);
}

TEST_F(RequestAttachTest, AttachComponentName) {
  InitializeDebugging();

  // Send attach request with component manifest name ending in .cm.
  dap::AttachRequestZxdb req = {};
  req.process = "my_component.cm";
  auto got = SendAttachRequest(req);
  EXPECT_FALSE(got.error);

  // Verify component name filter type
  auto filters = context().session()->system().GetFilters();
  ASSERT_EQ(filters.size(), 1u);
  EXPECT_EQ(filters[0]->pattern(), "my_component.cm");
  EXPECT_EQ(filters[0]->type(), debug_ipc::Filter::Type::kComponentName);
}

TEST_F(RequestAttachTest, AttachRecursive) {
  InitializeDebugging();

  // Send attach request with moniker suffix and recursive flag set
  dap::AttachRequestZxdb req = {};
  req.process = "realm/component";
  req.recursive = true;
  auto got = SendAttachRequest(req);
  EXPECT_FALSE(got.error);

  // Verify recursive moniker suffix filter type
  auto filters = context().session()->system().GetFilters();
  ASSERT_EQ(filters.size(), 1u);
  EXPECT_EQ(filters[0]->pattern(), "realm/component");
  EXPECT_EQ(filters[0]->type(), debug_ipc::Filter::Type::kComponentMonikerSuffix);
  EXPECT_TRUE(filters[0]->recursive());
}

TEST_F(RequestAttachTest, AttachWeakAndExactFilter) {
  InitializeDebugging();

  dap::AttachRequestZxdb req = {};
  req.process = "driver_host";
  req.weak = true;
  req.exact = true;
  req.job_only = true;
  auto got = SendAttachRequest(req);
  EXPECT_FALSE(got.error);

  auto filters = context().session()->system().GetFilters();
  ASSERT_EQ(filters.size(), 1u);
  EXPECT_EQ(filters[0]->pattern(), "driver_host");
  EXPECT_EQ(filters[0]->type(), debug_ipc::Filter::Type::kProcessName);
  EXPECT_TRUE(filters[0]->weak());
  EXPECT_TRUE(filters[0]->job_only());
}

TEST_F(RequestAttachTest, AttachEmptyPattern) {
  InitializeDebugging();

  // Send invalid empty attach request
  dap::AttachRequestZxdb req = {};
  req.process = "";
  auto got = SendAttachRequest(req);

  // Verify immediate fail-fast validation returned a protocol error
  EXPECT_TRUE(got.error);
}

TEST_F(RequestAttachTest, AttachDuplicateDeduplication) {
  InitializeDebugging();

  dap::AttachRequestZxdb req1 = {};
  req1.process = "test_component.cm";
  req1.recursive = true;
  req1.weak = true;
  req1.job_only = true;
  EXPECT_FALSE(SendAttachRequest(req1).error);

  auto filters = context().session()->system().GetFilters();
  ASSERT_EQ(filters.size(), 1u);
  EXPECT_TRUE(filters[0]->recursive());
  EXPECT_TRUE(filters[0]->weak());
  EXPECT_TRUE(filters[0]->job_only());

  dap::AttachRequestZxdb req2 = {};
  req2.process = "test_component.cm";
  EXPECT_FALSE(SendAttachRequest(req2).error);

  // Duplicate requests within the same DAP session should reuse the existing filter
  // and synchronize its option flags so stale settings are not retained.
  filters = context().session()->system().GetFilters();
  ASSERT_EQ(filters.size(), 1u);
  EXPECT_FALSE(filters[0]->recursive());
  EXPECT_FALSE(filters[0]->weak());
  EXPECT_FALSE(filters[0]->job_only());
}

TEST_F(RequestAttachTest, MultipleFiltersTearDown) {
  InitializeDebugging();

  for (const char* pattern : {"proc1", "proc2", "proc3"}) {
    dap::AttachRequestZxdb req = {};
    req.process = pattern;
    EXPECT_FALSE(SendAttachRequest(req).error);
  }

  auto filters = context().session()->system().GetFilters();
  EXPECT_EQ(filters.size(), 3u);

  // Teardown will now occur when the test exits, verifying that
  // DeleteAllFilters() does not crash when multiple filters are present.
}

}  // namespace

}  // namespace zxdb
