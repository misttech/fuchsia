// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_tracepoint.h"

#include <gtest/gtest.h>

#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/tracepoint.h"
#include "src/developer/debug/zxdb/client/tracepoint_capture.h"
#include "src/developer/debug/zxdb/debug_adapter/context_test.h"

namespace zxdb {

namespace {

class RequestTracepointTest : public DebugAdapterContextTest {};

TEST_F(RequestTracepointTest, SetTracepointsRoundTrip) {
  InitializeDebugging();

  dap::ZxdbSetTracepointsRequest req;
  dap::ZxdbTracepointSpec spec;
  spec.function = "MyFunction";
  spec.max_matching_frames = 5;
  spec.max_hits = 10;
  spec.enabled = true;
  spec.expressions = dap::array<dap::string>{"x", "y"};
  spec.locals = true;
  spec.registers = true;
  req.tracepoints.push_back(spec);

  auto response = client().send(req);
  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got = response.get();
  EXPECT_FALSE(got.error);
  ASSERT_EQ(got.response.tracepoints.size(), 1u);
  EXPECT_GT(got.response.tracepoints[0].id, 0);
  EXPECT_TRUE(got.response.tracepoints[0].max_matching_frames.has_value());
  EXPECT_EQ(*got.response.tracepoints[0].max_matching_frames, 5);
  EXPECT_EQ(got.response.tracepoints[0].max_hits, 10);
  EXPECT_TRUE(got.response.tracepoints[0].enabled);
  EXPECT_FALSE(got.response.tracepoints[0].auto_disabled);

  EXPECT_EQ(context().dap_tracepoints().size(), 1u);
  EXPECT_EQ(session().system().GetTracepoints().size(), 1u);
  auto it = context().dap_tracepoints().find(got.response.tracepoints[0].id);
  ASSERT_NE(it, context().dap_tracepoints().end());
  ASSERT_TRUE(it->second);
  EXPECT_TRUE(it->second->spec().max_matching_frames.has_value());
  EXPECT_EQ(*it->second->spec().max_matching_frames, 5u);
}

TEST_F(RequestTracepointTest, ReconciliationAndIdPreservation) {
  InitializeDebugging();

  // Issue initial request with explicit client tracepoint id 10.
  dap::ZxdbSetTracepointsRequest req1;
  dap::ZxdbTracepointSpec spec1;
  spec1.id = 10;
  spec1.function = "FuncA";
  spec1.max_hits = 5;
  spec1.enabled = true;
  req1.tracepoints.push_back(spec1);

  auto res1 = client().send(req1);
  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got1 = res1.get();
  EXPECT_FALSE(got1.error);
  ASSERT_EQ(got1.response.tracepoints.size(), 1u);
  EXPECT_EQ(got1.response.tracepoints[0].id, 10);
  EXPECT_TRUE(got1.response.tracepoints[0].enabled);

  // Manually increment hit count on tp 10.
  auto it = context().dap_tracepoints().find(10);
  ASSERT_NE(it, context().dap_tracepoints().end());
  ASSERT_TRUE(it->second);
  it->second->set_hit_count(3);

  // Update id 10 in-place with new settings and register a new tracepoint without specifying an id.
  dap::ZxdbSetTracepointsRequest req2;
  spec1.max_hits = 20;
  spec1.enabled = true;
  req2.tracepoints.push_back(spec1);

  dap::ZxdbTracepointSpec spec2;
  spec2.function = "FuncB";
  req2.tracepoints.push_back(spec2);

  auto res2 = client().send(req2);
  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got2 = res2.get();
  EXPECT_FALSE(got2.error);
  ASSERT_EQ(got2.response.tracepoints.size(), 2u);
  EXPECT_EQ(got2.response.tracepoints[0].id, 10);
  EXPECT_EQ(got2.response.tracepoints[0].hit_count, 3);
  EXPECT_EQ(got2.response.tracepoints[0].max_hits, 20);

  int64_t new_id = got2.response.tracepoints[1].id;
  EXPECT_NE(new_id, 10);
  EXPECT_EQ(context().dap_tracepoints().size(), 2u);

  // Reconcile by omitting id 10 so it is deleted while preserving the newly added tracepoint.
  dap::ZxdbSetTracepointsRequest req3;
  spec2.id = new_id;
  req3.tracepoints.push_back(spec2);

  auto res3 = client().send(req3);
  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got3 = res3.get();
  EXPECT_FALSE(got3.error);
  ASSERT_EQ(got3.response.tracepoints.size(), 1u);
  EXPECT_EQ(got3.response.tracepoints[0].id, new_id);

  EXPECT_EQ(context().dap_tracepoints().size(), 1u);
  EXPECT_EQ(context().dap_tracepoints().find(10), context().dap_tracepoints().end());
}

TEST_F(RequestTracepointTest, WatchpointResolution) {
  InitializeDebugging();

  dap::ZxdbSetTracepointsRequest req;
  dap::ZxdbTracepointSpec spec;
  dap::ZxdbTracepointWatchSpec watch;
  watch.data_id = "1234:0x2000:8";
  watch.access_type = "readWrite";
  spec.watch = watch;
  req.tracepoints.push_back(spec);

  auto res = client().send(req);
  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got = res.get();
  EXPECT_FALSE(got.error);
  ASSERT_EQ(got.response.tracepoints.size(), 1u);

  ASSERT_EQ(context().dap_tracepoints().size(), 1u);
  Tracepoint* tp = context().dap_tracepoints().begin()->second.get();
  ASSERT_NE(tp, nullptr);
  ASSERT_TRUE(tp->spec().watch.has_value());
  EXPECT_EQ(tp->spec().watch->address, 0x2000u);
  EXPECT_EQ(tp->spec().watch->byte_size, 8u);
  EXPECT_EQ(tp->spec().watch->access_type, debug_ipc::BreakpointType::kReadWrite);
}

TEST_F(RequestTracepointTest, TracepointEventEmission) {
  InitializeDebugging();

  bool event_received = false;
  dap::ZxdbTracepointEvent received_event;
  client().registerHandler([&](const dap::ZxdbTracepointEvent& evt) {
    event_received = true;
    received_event = evt;
  });

  TracepointHitRecord hit;
  hit.tracepoint_id = 42;
  hit.location = "src/foo.cc:123";
  hit.hit_count = 3;
  hit.max_hits = 10;
  hit.auto_disabled = false;
  hit.thread_koid = 9999;
  hit.dwell_ms = 1.25;
  hit.truncated = true;

  TracepointFrameSummary summary;
  summary.name = "MyFunction";
  summary.file = "src/foo.cc";
  summary.line = 123;
  summary.ip = 0x12345;
  summary.sp = 0x54321;
  hit.capture.frame = summary;
  hit.capture.registers = std::map<std::string, std::string>{{"rax", "0x1"}};

  context().OnTracepointHit(nullptr, hit);
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  EXPECT_TRUE(event_received);
  EXPECT_EQ(received_event.tracepoint_id, 42);
  EXPECT_EQ(received_event.location, "src/foo.cc:123");
  EXPECT_EQ(received_event.hit_count, 3);
  EXPECT_EQ(received_event.max_hits, 10);
  EXPECT_FALSE(received_event.auto_disabled);
  EXPECT_TRUE(received_event.truncated);
  EXPECT_EQ(received_event.thread_koid, 9999);
  EXPECT_DOUBLE_EQ(received_event.dwell_ms, 1.25);
  EXPECT_TRUE(received_event.capture.matched);
  ASSERT_TRUE(received_event.capture.frame.has_value());
  EXPECT_EQ(received_event.capture.frame->name, "MyFunction");
  EXPECT_EQ(received_event.capture.frame->ip, 0x12345);
  ASSERT_TRUE(received_event.capture.registers.has_value());
  EXPECT_NE(received_event.capture.registers->find("rax"), received_event.capture.registers->end());

  // Error hits with matched = false and diagnostic errors serialize cleanly.
  event_received = false;
  TracepointHitRecord error_hit;
  error_hit.tracepoint_id = 42;
  error_hit.location = "src/foo.cc:123";
  error_hit.hit_count = 4;
  error_hit.max_hits = 4;
  error_hit.auto_disabled = true;
  error_hit.thread_koid = 9999;
  error_hit.capture.matched = false;
  error_hit.capture.errors = {"Condition error: bad_ident"};

  context().OnTracepointHit(nullptr, error_hit);
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  EXPECT_TRUE(event_received);
  EXPECT_TRUE(received_event.auto_disabled);
  EXPECT_FALSE(received_event.truncated);
  EXPECT_FALSE(received_event.capture.matched);
  EXPECT_FALSE(received_event.capture.frame.has_value());
  ASSERT_EQ(received_event.capture.errors.size(), 1u);
  EXPECT_EQ(received_event.capture.errors[0], "Condition error: bad_ident");
}

TEST_F(RequestTracepointTest, BoundaryTestsAndInvalidInputs) {
  InitializeDebugging();

  // Test zero KOID in dataId.
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec;
    dap::ZxdbTracepointWatchSpec watch;
    watch.data_id = "0:0x2000:8";
    spec.watch = watch;
    req.tracepoints.push_back(spec);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_FALSE(got.error);
    ASSERT_EQ(got.response.tracepoints.size(), 1u);
    EXPECT_FALSE(got.response.tracepoints[0].verified);
    EXPECT_TRUE(got.response.tracepoints[0].message.has_value());
  }

  // Test malformed dataId without proper colons.
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec;
    dap::ZxdbTracepointWatchSpec watch;
    watch.data_id = "1234_0x2000_8";
    spec.watch = watch;
    req.tracepoints.push_back(spec);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_FALSE(got.error);
    ASSERT_EQ(got.response.tracepoints.size(), 1u);
    EXPECT_FALSE(got.response.tracepoints[0].verified);
    EXPECT_TRUE(got.response.tracepoints[0].message.has_value());
  }

  // Test address overflow in dataId.
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec;
    dap::ZxdbTracepointWatchSpec watch;
    watch.data_id = "1234:0xffffffffffffffff:16";
    spec.watch = watch;
    req.tracepoints.push_back(spec);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_FALSE(got.error);
    ASSERT_EQ(got.response.tracepoints.size(), 1u);
    EXPECT_FALSE(got.response.tracepoints[0].verified);
    EXPECT_TRUE(got.response.tracepoints[0].message.has_value());
  }

  // Test non-positive DAP id (id <= 0).
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec;
    spec.id = 0;
    spec.function = "Func";
    req.tracepoints.push_back(spec);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_TRUE(got.error);
  }

  // Test non-positive line (line <= 0).
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec;
    spec.file = "foo.cc";
    spec.line = 0;
    req.tracepoints.push_back(spec);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_TRUE(got.error);
  }

  // Test non-positive maxMatchingFrames (maxMatchingFrames <= 0).
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec;
    spec.function = "Func";
    spec.max_matching_frames = 0;
    req.tracepoints.push_back(spec);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_TRUE(got.error);
  }

  // Test non-positive maxHits (maxHits <= 0).
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec;
    spec.function = "Func";
    spec.max_hits = 0;
    req.tracepoints.push_back(spec);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_TRUE(got.error);
  }

  // Test non-positive watch byteSize (byteSize <= 0).
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec;
    dap::ZxdbTracepointWatchSpec watch;
    watch.address = 0x1000;
    watch.byte_size = 0;
    spec.watch = watch;
    req.tracepoints.push_back(spec);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_TRUE(got.error);
  }
}

TEST_F(RequestTracepointTest, TracepointHitWithRegisteredTracepoint) {
  InitializeDebugging();

  dap::ZxdbSetTracepointsRequest req;
  dap::ZxdbTracepointSpec spec;
  spec.id = 77;
  spec.function = "MyFunction";
  spec.max_hits = 15;
  req.tracepoints.push_back(spec);

  auto set_res = client().send(req);
  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got_set = set_res.get();
  EXPECT_FALSE(got_set.error);
  ASSERT_EQ(got_set.response.tracepoints.size(), 1u);
  EXPECT_EQ(got_set.response.tracepoints[0].id, 77);

  auto it = context().dap_tracepoints().find(77);
  ASSERT_NE(it, context().dap_tracepoints().end());
  Tracepoint* tp = it->second.get();
  ASSERT_NE(tp, nullptr);

  bool event_received = false;
  dap::ZxdbTracepointEvent received_event;
  client().registerHandler([&](const dap::ZxdbTracepointEvent& evt) {
    event_received = true;
    received_event = evt;
  });

  TracepointHitRecord hit;
  hit.tracepoint_id = tp->id();
  hit.location = "MyFunction";
  hit.hit_count = 1;
  hit.max_hits = 15;
  hit.auto_disabled = false;
  hit.thread_koid = 8888;
  hit.dwell_ms = 0.5;

  TracepointFrameRecord rec;
  rec.frame_index = 0;
  rec.ip = 0x2000;
  rec.sp = 0x3000;
  rec.name = "MyFunction";
  rec.trust = debug_ipc::StackFrame::Trust::kContext;
  rec.file = "test.cc";
  rec.line = 10;
  rec.elided = false;
  hit.capture.backtrace = std::vector<TracepointFrameRecord>{rec};

  TracepointEvaluation eval;
  eval.result = "42";
  eval.type = "int";
  hit.capture.evaluations["x + 1"] = eval;

  hit.capture.errors = {"Minor capture warning"};

  context().OnTracepointHit(tp, hit);
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  EXPECT_TRUE(event_received);
  EXPECT_EQ(received_event.tracepoint_id, 77);
  EXPECT_EQ(received_event.location, "MyFunction");
  ASSERT_TRUE(received_event.capture.backtrace.has_value());
  ASSERT_EQ(received_event.capture.backtrace->size(), 1u);
  EXPECT_EQ((*received_event.capture.backtrace)[0].ip, 0x2000);
  EXPECT_EQ((*received_event.capture.backtrace)[0].name, "MyFunction");
  ASSERT_TRUE(received_event.capture.evaluations.has_value());
  EXPECT_TRUE(received_event.capture.evaluations->count("x + 1"));
  EXPECT_EQ(received_event.capture.errors.size(), 1u);
  EXPECT_EQ(received_event.capture.errors[0], "Minor capture warning");

  // Deleting the tracepoint should remove it from dap_tracepoints_.
  context().WillDestroyTracepoint(tp);
  EXPECT_EQ(context().dap_tracepoints().find(77), context().dap_tracepoints().end());
}

TEST_F(RequestTracepointTest, RawBacktraceImpliesBacktrace) {
  InitializeDebugging();

  dap::ZxdbSetTracepointsRequest req;
  dap::ZxdbTracepointSpec spec;
  spec.function = "MyFunction";
  spec.raw_backtrace = true;
  spec.backtrace = false;
  req.tracepoints.push_back(spec);

  auto res = client().send(req);
  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got = res.get();
  EXPECT_FALSE(got.error);
  ASSERT_EQ(got.response.tracepoints.size(), 1u);

  ASSERT_EQ(context().dap_tracepoints().size(), 1u);
  Tracepoint* tp = context().dap_tracepoints().begin()->second.get();
  ASSERT_NE(tp, nullptr);
  EXPECT_TRUE(tp->spec().raw_backtrace);
  EXPECT_TRUE(tp->spec().backtrace);
}

TEST_F(RequestTracepointTest, UnrecognizedAccessType) {
  InitializeDebugging();

  dap::ZxdbSetTracepointsRequest req;
  dap::ZxdbTracepointSpec spec;
  dap::ZxdbTracepointWatchSpec watch;
  watch.data_id = "1234:0x2000:8";
  watch.access_type = "sideways";
  spec.watch = watch;
  req.tracepoints.push_back(spec);

  auto res = client().send(req);
  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got = res.get();
  EXPECT_FALSE(got.error);
  ASSERT_EQ(got.response.tracepoints.size(), 1u);
  EXPECT_FALSE(got.response.tracepoints[0].verified);
  EXPECT_EQ(got.response.tracepoints[0].message.value(), "Unknown accessType: sideways");
}

TEST_F(RequestTracepointTest, DeleteDapTracepointsSafeguard) {
  InitializeDebugging();

  dap::ZxdbSetTracepointsRequest req;
  dap::ZxdbTracepointSpec spec1;
  spec1.id = 1;
  spec1.function = "Func1";
  dap::ZxdbTracepointSpec spec2;
  spec2.id = 2;
  spec2.function = "Func2";
  req.tracepoints.push_back(spec1);
  req.tracepoints.push_back(spec2);

  auto res = client().send(req);
  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  EXPECT_EQ(context().dap_tracepoints().size(), 2u);

  // Calling DeleteDapTracepoints() should delete all tracepoints without iterator invalidation.
  context().DeleteDapTracepoints();
  EXPECT_TRUE(context().dap_tracepoints().empty());
}

TEST_F(RequestTracepointTest, DuplicateAndAutoAssignedIdCollision) {
  InitializeDebugging();

  // 1. Duplicate explicit IDs in the same request must be rejected with an error.
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec1;
    spec1.id = 5;
    spec1.function = "FuncA";
    dap::ZxdbTracepointSpec spec2;
    spec2.id = 5;
    spec2.function = "FuncB";
    req.tracepoints.push_back(spec1);
    req.tracepoints.push_back(spec2);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_TRUE(got.error);
  }

  // 2. Auto-assigned ID followed by an explicit id: 1 in the same request must not collide.
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec_auto;
    spec_auto.function = "FuncAuto";
    dap::ZxdbTracepointSpec spec_explicit;
    spec_explicit.id = 1;
    spec_explicit.function = "FuncExplicit";
    req.tracepoints.push_back(spec_auto);
    req.tracepoints.push_back(spec_explicit);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_FALSE(got.error);
    ASSERT_EQ(got.response.tracepoints.size(), 2u);
    EXPECT_EQ(got.response.tracepoints[0].id, 2);
    EXPECT_EQ(got.response.tracepoints[1].id, 1);
    EXPECT_EQ(context().dap_tracepoints().size(), 2u);
  }
}

TEST_F(RequestTracepointTest, InvalidSpecDeletesOldTracepoint) {
  InitializeDebugging();

  // Create an initial valid tracepoint with explicit id 42.
  dap::ZxdbSetTracepointsRequest req1;
  dap::ZxdbTracepointSpec spec1;
  spec1.id = 42;
  spec1.function = "ValidFunction";
  req1.tracepoints.push_back(spec1);

  auto res1 = client().send(req1);
  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got1 = res1.get();
  EXPECT_FALSE(got1.error);
  ASSERT_EQ(got1.response.tracepoints.size(), 1u);
  EXPECT_EQ(got1.response.tracepoints[0].id, 42);
  EXPECT_EQ(session().system().GetTracepoints().size(), 1u);

  // Update id 42 with an invalid spec (file specified without line).
  dap::ZxdbSetTracepointsRequest req2;
  dap::ZxdbTracepointSpec invalid_spec;
  invalid_spec.id = 42;
  invalid_spec.file = "foo.cc";
  req2.tracepoints.push_back(invalid_spec);

  auto res2 = client().send(req2);
  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunPendingClientCalls();

  auto got2 = res2.get();
  EXPECT_FALSE(got2.error);
  ASSERT_EQ(got2.response.tracepoints.size(), 1u);
  EXPECT_FALSE(got2.response.tracepoints[0].verified);

  // The old tracepoint must be removed from the system.
  EXPECT_TRUE(session().system().GetTracepoints().empty());
  EXPECT_TRUE(context().dap_tracepoints().empty());
}

TEST_F(RequestTracepointTest, SpecValidationEdgeCases) {
  InitializeDebugging();

  // Negative tracepoint address.
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec;
    spec.address = -10;
    req.tracepoints.push_back(spec);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_FALSE(got.error);
    ASSERT_EQ(got.response.tracepoints.size(), 1u);
    EXPECT_FALSE(got.response.tracepoints[0].verified);
    EXPECT_EQ(got.response.tracepoints[0].message.value(), "Invalid negative tracepoint address.");
  }

  // File without line.
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec;
    spec.file = "main.cc";
    req.tracepoints.push_back(spec);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_FALSE(got.error);
    ASSERT_EQ(got.response.tracepoints.size(), 1u);
    EXPECT_FALSE(got.response.tracepoints[0].verified);
    EXPECT_EQ(got.response.tracepoints[0].message.value(),
              "Both file and line must be specified together for a line tracepoint.");
  }

  // Negative watch address.
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec;
    dap::ZxdbTracepointWatchSpec watch;
    watch.address = -1;
    spec.watch = watch;
    req.tracepoints.push_back(spec);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_FALSE(got.error);
    ASSERT_EQ(got.response.tracepoints.size(), 1u);
    EXPECT_FALSE(got.response.tracepoints[0].verified);
    EXPECT_EQ(got.response.tracepoints[0].message.value(), "Invalid negative watch address.");
  }

  // Watch address range overflow.
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec;
    dap::ZxdbTracepointWatchSpec watch;
    watch.data_id = "1234:0xfffffffffffffffe:1";
    watch.byte_size = 4;
    spec.watch = watch;
    req.tracepoints.push_back(spec);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_FALSE(got.error);
    ASSERT_EQ(got.response.tracepoints.size(), 1u);
    EXPECT_FALSE(got.response.tracepoints[0].verified);
    EXPECT_EQ(got.response.tracepoints[0].message.value(),
              "Watch address range overflows 64-bit address space.");
  }

  // Watch dataId with byteSize override.
  {
    dap::ZxdbSetTracepointsRequest req;
    dap::ZxdbTracepointSpec spec;
    dap::ZxdbTracepointWatchSpec watch;
    watch.data_id = "1234:0x2000:4";
    watch.byte_size = 8;
    spec.watch = watch;
    req.tracepoints.push_back(spec);

    auto res = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunPendingClientCalls();

    auto got = res.get();
    EXPECT_FALSE(got.error);
    ASSERT_EQ(got.response.tracepoints.size(), 1u);
    auto it = context().dap_tracepoints().find(got.response.tracepoints[0].id);
    ASSERT_NE(it, context().dap_tracepoints().end());
    ASSERT_TRUE(it->second);
    ASSERT_TRUE(it->second->spec().watch.has_value());
    EXPECT_EQ(it->second->spec().watch->byte_size, 8u);
  }
}

}  // namespace

}  // namespace zxdb
