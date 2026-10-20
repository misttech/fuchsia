// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fit/defer.h>
#include <lib/trace-engine/context.h>
#include <lib/trace-engine/handler.h>
#include <lib/trace-engine/instrumentation.h>

#include <memory>
#include <thread>

#include <gtest/gtest.h>

#include "../context_impl.h"

namespace {
const unsigned char bytes_disabled[5] = {'b', 'y', 't', 'e', 's'};
const unsigned char bytes_enabled[11] = {'e', 'n', 'a', 'b', 'l', 'e', 'd', '_', 'c', 'a', 't'};
const char* str_disabled = "string";
const char* str_enabled = "enabled_cat";

bool is_category_enabled(trace_handler_t* handler, const char* category) {
  const std::string cat = category;
  return cat == "enabled_cat";
}

const trace_handler_ops ops{
    .is_category_enabled = is_category_enabled,
    .trace_started = [](trace_handler_t*) {},
    .trace_stopped = [](trace_handler_t*, zx_status_t) {},
    .trace_terminated = [](trace_handler_t*) {},
    .notify_buffer_full = [](trace_handler_t*, uint32_t, uint64_t) {},
    .send_alert = [](trace_handler_t*, const char*) {},
};
}  // namespace

// Verify registering byte strings get us the same ids.
TEST(TraceEngineTest, RegisterByteString) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  char buffer[4096];
  trace_handler_t handler{&ops};
  zx_status_t init = trace_engine_initialize(loop.dispatcher(), &handler,
                                             TRACE_BUFFERING_MODE_ONESHOT, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  trace_context_t* context = trace_acquire_context();
  ASSERT_TRUE(context);
  trace_string_ref_t ref;
  trace_context_register_bytestring(context, bytes_disabled, sizeof(bytes_disabled), &ref);
  trace_string_ref_t ref2;
  trace_context_register_bytestring(context, bytes_disabled, sizeof(bytes_disabled), &ref2);
  trace_string_ref_t ref3;
  trace_context_register_bytestring(context, bytes_enabled, sizeof(bytes_enabled), &ref3);
  trace_string_ref_t ref4;
  trace_context_register_string_literal(context, str_enabled, &ref4);
  // The same bytestring should get the same encoded_value
  ASSERT_EQ(ref.encoded_value, ref2.encoded_value);
  ASSERT_NE(ref.encoded_value, ref3.encoded_value);
  ASSERT_NE(ref2.encoded_value, ref3.encoded_value);

  // But a byte string and a string literal are not the same -- one has a trailing '\0'.
  ASSERT_NE(ref3.encoded_value, ref4.encoded_value);
  trace_release_context(context);

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();
}

// Verify registering strings get us the same ids.
TEST(TraceEngineTest, RegisterString) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  char buffer[4096];
  trace_handler_t handler{&ops};
  zx_status_t init = trace_engine_initialize(loop.dispatcher(), &handler,
                                             TRACE_BUFFERING_MODE_ONESHOT, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  trace_context_t* context = trace_acquire_context();
  ASSERT_TRUE(context);
  trace_string_ref_t ref;
  trace_context_register_string_literal(context, str_disabled, &ref);
  trace_string_ref_t ref2;
  trace_context_register_string_literal(context, str_disabled, &ref2);
  trace_string_ref_t ref3;
  trace_context_register_string_literal(context, str_enabled, &ref3);
  ASSERT_EQ(ref.encoded_value, ref2.encoded_value);
  ASSERT_NE(ref.encoded_value, ref3.encoded_value);
  ASSERT_NE(ref2.encoded_value, ref3.encoded_value);
  trace_release_context(context);

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();
}

// We should successfully check for both bytestring and c-string categories
TEST(TraceEngineTest, EnabledCategories) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  char buffer[4096];
  trace_handler_t handler{&ops};
  zx_status_t init = trace_engine_initialize(loop.dispatcher(), &handler,
                                             TRACE_BUFFERING_MODE_ONESHOT, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  ASSERT_FALSE(trace_is_category_bytestring_enabled(bytes_disabled, sizeof(bytes_disabled)));
  ASSERT_TRUE(trace_is_category_bytestring_enabled(bytes_enabled, sizeof(bytes_enabled)));
  ASSERT_FALSE(trace_is_category_enabled(str_disabled));
  ASSERT_TRUE(trace_is_category_enabled(str_enabled));

  trace_context_t* context = trace_acquire_context();
  ASSERT_TRUE(context);

  ASSERT_FALSE(trace_context_is_category_bytestring_enabled(context, bytes_disabled,
                                                            sizeof(bytes_disabled)));
  ASSERT_TRUE(
      trace_context_is_category_bytestring_enabled(context, bytes_enabled, sizeof(bytes_enabled)));

  ASSERT_FALSE(trace_context_is_category_enabled(context, str_disabled));
  ASSERT_TRUE(trace_context_is_category_enabled(context, str_enabled));
  trace_release_context(context);

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();
}

// We should successfully check for both bytestring and c-string categories
TEST(TraceEngineTest, CategoryRegistration) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  char buffer[4096];
  trace_handler_t handler{&ops};
  zx_status_t init = trace_engine_initialize(loop.dispatcher(), &handler,
                                             TRACE_BUFFERING_MODE_ONESHOT, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  trace_context_t* context = trace_acquire_context();
  ASSERT_TRUE(context);

  trace_string_ref_t ref;
  // This category isn't enabled
  ASSERT_FALSE(trace_context_register_category_bytestring(context, bytes_disabled,
                                                          sizeof(bytes_disabled), &ref));

  // This category should be enabled
  trace_string_ref_t ref2;
  ASSERT_TRUE(trace_context_register_category_bytestring(context, bytes_enabled,
                                                         sizeof(bytes_enabled), &ref2));
  trace_string_ref_t ref3;
  ASSERT_TRUE(trace_context_register_category_bytestring(context, bytes_enabled,
                                                         sizeof(bytes_enabled), &ref3));
  // And we should get the same id if we re-register
  ASSERT_EQ(ref2.encoded_value, ref3.encoded_value);

  trace_string_ref_t ref4;
  ASSERT_FALSE(trace_context_register_category_literal(context, str_disabled, &ref4));
  trace_string_ref_t ref5;
  ASSERT_TRUE(trace_context_register_category_literal(context, str_enabled, &ref5));
  trace_string_ref_t ref6;
  ASSERT_TRUE(trace_context_register_category_literal(context, str_enabled, &ref6));

  // And we should get the same id if we re-register
  ASSERT_EQ(ref5.encoded_value, ref6.encoded_value);

  // Byte strings and c strings are cached separately though
  ASSERT_NE(ref2.encoded_value, ref5.encoded_value);
  trace_release_context(context);

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();
}

TEST(TraceEngineTest, AcquireForCategory) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  char buffer[4096];
  trace_handler_t handler{&ops};
  zx_status_t init = trace_engine_initialize(loop.dispatcher(), &handler,
                                             TRACE_BUFFERING_MODE_ONESHOT, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  // This category isn't enabled
  trace_string_ref_t ref1;
  trace_context_t* context1 =
      trace_acquire_context_for_category_bytestring(bytes_disabled, sizeof(bytes_disabled), &ref1);
  ASSERT_FALSE(context1);

  // This category isn't enabled
  trace_string_ref_t ref2;
  trace_context_t* context2 =
      trace_acquire_context_for_category_bytestring(bytes_enabled, sizeof(bytes_enabled), &ref2);
  ASSERT_TRUE(context2);
  trace_release_context(context2);

  // This category isn't enabled
  trace_string_ref_t ref3;
  trace_context_t* context3 = trace_acquire_context_for_category(str_disabled, &ref3);
  ASSERT_FALSE(context3);

  // This category isn't enabled
  trace_string_ref_t ref4;
  trace_context_t* context4 = trace_acquire_context_for_category(str_enabled, &ref4);
  ASSERT_TRUE(context4);
  trace_release_context(context4);

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();
}

TEST(TraceEngineTest, AcquireForCategoryCached) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  char buffer[4096];
  trace_handler_t handler{&ops};
  zx_status_t init = trace_engine_initialize(loop.dispatcher(), &handler,
                                             TRACE_BUFFERING_MODE_ONESHOT, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  // This category isn't enabled
  trace_string_ref_t ref1;
  static trace_site_t site1{0};
  trace_context_t* context1 = trace_acquire_context_for_category_bytestring_cached(
      bytes_disabled, sizeof(bytes_disabled), &site1, &ref1);
  ASSERT_FALSE(context1);
  ASSERT_EQ(site1.state & 3, trace_site_state_t{1});  // kSiteStateDisabled = 1

  // Calling again should just use the cached value.
  context1 = trace_acquire_context_for_category_bytestring_cached(
      bytes_disabled, sizeof(bytes_disabled), &site1, &ref1);
  ASSERT_FALSE(context1);
  ASSERT_EQ(site1.state & 3, trace_site_state_t{1});  // kSiteStateDisabled = 1

  // This category is enabled
  trace_string_ref_t ref2;
  static trace_site_t site2{0};
  trace_context_t* context2 = trace_acquire_context_for_category_bytestring_cached(
      bytes_enabled, sizeof(bytes_enabled), &site2, &ref2);
  ASSERT_TRUE(context2);
  ASSERT_EQ(site2.state & 3, trace_site_state_t{2});  // kSiteStateEnabled = 2
  trace_release_context(context2);

  context2 = trace_acquire_context_for_category_bytestring_cached(
      bytes_enabled, sizeof(bytes_enabled), &site2, &ref2);
  ASSERT_TRUE(context2);
  ASSERT_EQ(site2.state & 3, trace_site_state_t{2});  // kSiteStateEnabled = 2
  trace_release_context(context2);

  // This category isn't enabled
  trace_string_ref_t ref3;
  static trace_site_t site3{0};
  trace_context_t* context3 =
      trace_acquire_context_for_category_cached(str_disabled, &site3, &ref3);
  ASSERT_FALSE(context3);
  ASSERT_EQ(site3.state & 3, trace_site_state_t{1});  // kSiteStateDisabled = 1

  context3 = trace_acquire_context_for_category_cached(str_disabled, &site3, &ref3);
  ASSERT_FALSE(context3);
  ASSERT_EQ(site3.state & 3, trace_site_state_t{1});  // kSiteStateDisabled = 1

  // This category is enabled
  trace_string_ref_t ref4;
  static trace_site_t site4{0};
  trace_context_t* context4 = trace_acquire_context_for_category_cached(str_enabled, &site4, &ref4);
  ASSERT_TRUE(context4);
  ASSERT_EQ(site4.state & 3, trace_site_state_t{2});  // kSiteStateEnabled = 2
  trace_release_context(context4);

  context4 = trace_acquire_context_for_category_cached(str_enabled, &site4, &ref4);
  ASSERT_TRUE(context4);
  ASSERT_EQ(site4.state & 3, trace_site_state_t{2});  // kSiteStateEnabled = 2
  trace_release_context(context4);

  // Once we stop, we should see all the caches reset.
  trace_engine_stop(ZX_OK);
  EXPECT_EQ(site1.state & 3, trace_site_state_t{0});
  EXPECT_EQ(site2.state & 3, trace_site_state_t{0});
  EXPECT_EQ(site3.state & 3, trace_site_state_t{0});
  EXPECT_EQ(site4.state & 3, trace_site_state_t{0});

  trace_engine_terminate();
  loop.RunUntilIdle();
}

TEST(TraceEngineTest, Alerts) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  char buffer[4096];
  struct alert_handler : trace_handler_t {
    explicit alert_handler(const trace_handler_ops* ops) : trace_handler_t{ops} {}
    std::vector<std::string> alerts;
  };

  trace_handler_ops alert_ops = ops;
  alert_ops.send_alert = [](trace_handler_t* handler, const char* alert) {
    reinterpret_cast<alert_handler*>(handler)->alerts.emplace_back(alert);
  };

  alert_handler handler{&alert_ops};
  zx_status_t init = trace_engine_initialize(loop.dispatcher(), &handler,
                                             TRACE_BUFFERING_MODE_ONESHOT, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  trace_context_t* context = trace_acquire_context();
  ASSERT_TRUE(context);
  trace_context_send_alert(context, "test");
  trace_context_send_alert_bytestring(context, bytes_disabled, sizeof(bytes_disabled));
  trace_release_context(context);

  ASSERT_EQ(handler.alerts.size(), size_t{2});
  ASSERT_EQ(handler.alerts[0], std::string{"test"});
  ASSERT_EQ(handler.alerts[1], std::string{"bytes"});

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();
}

TEST(TraceEngineTest, FlushBuffer) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  char buffer[4096];
  struct flush_handler : trace_handler_t {
    explicit flush_handler(const trace_handler_ops* ops) : trace_handler_t{ops} {}
    bool flushed = false;
  };

  trace_handler_ops flush_ops = ops;
  flush_ops.notify_buffer_full = [](trace_handler_t* handler, uint32_t, uint64_t) {
    reinterpret_cast<flush_handler*>(handler)->flushed = true;
  };

  flush_handler handler{&flush_ops};
  // We can't flush if the trace hasn't started
  ASSERT_EQ(ZX_ERR_BAD_STATE, trace_engine_flush_buffer());
  zx_status_t init = trace_engine_initialize(
      loop.dispatcher(), &handler, TRACE_BUFFERING_MODE_STREAMING, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  ASSERT_EQ(ZX_ERR_BAD_STATE, trace_engine_flush_buffer());
  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  // We should get an ok result, but shouldn't actually trigger a flush since there is no data.
  ASSERT_EQ(ZX_OK, trace_engine_flush_buffer());
  ASSERT_FALSE(handler.flushed);

  trace_context_t* context = trace_acquire_context();
  ASSERT_TRUE(context);
  void* data = trace_context_alloc_record(context, 32);
  uint64_t* ptr = reinterpret_cast<uint64_t*>(data);
  ptr[0] = 1;
  ptr[1] = 2;
  ptr[2] = 3;
  ptr[3] = 4;
  trace_release_context(context);
  ASSERT_FALSE(handler.flushed);

  trace_engine_flush_buffer();
  loop.RunUntilIdle();
  ASSERT_TRUE(handler.flushed);

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();
}

TEST(TraceEngineTest, OversizedEventDropped) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  char buffer[65536];
  trace_handler_t handler{&ops};
  zx_status_t init = trace_engine_initialize(loop.dispatcher(), &handler,
                                             TRACE_BUFFERING_MODE_ONESHOT, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  trace_context_t* context = trace_acquire_context();
  ASSERT_TRUE(context);

  trace_context* raw_context = reinterpret_cast<trace_context*>(context);
  uint64_t initial_dropped = raw_context->num_records_dropped();

  std::vector<char> large_str(20000, 'A');
  large_str.push_back('\0');

  trace_string_ref_t category_ref = trace_make_inline_c_string_ref("enabled_cat");
  trace_string_ref_t name_ref = trace_make_inline_c_string_ref("name");
  trace_string_ref_t arg_name = trace_make_inline_c_string_ref("arg");
  trace_string_ref_t arg_val_ref = trace_make_inline_c_string_ref(large_str.data());
  trace_arg_t args[2] = {
      trace_make_arg(arg_name, trace_make_string_arg_value(arg_val_ref)),
      trace_make_arg(arg_name, trace_make_string_arg_value(arg_val_ref)),
  };

  trace_thread_ref_t thread_ref = trace_make_unknown_thread_ref();

  trace_context_write_instant_event_record(context, 12345, &thread_ref, &category_ref, &name_ref,
                                           TRACE_SCOPE_PROCESS, args, 2);

  ASSERT_EQ(raw_context->num_records_dropped(), initial_dropped + 1);

  trace_release_context(context);

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();
}

TEST(TraceEngineTest, RegisterStringAcrossThreadsDeduplicates) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  // With a 4096-byte buffer in CIRCULAR mode, the durable buffer is 240 bytes.
  // Without cross-thread deduplication, 32 threads registering the same string literals would
  // overflow the durable buffer and fall back to inline string references.
  alignas(uint64_t) char buffer[4096];
  trace_handler_t handler{&ops};
  zx_status_t init = trace_engine_initialize(loop.dispatcher(), &handler,
                                             TRACE_BUFFERING_MODE_CIRCULAR, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  trace_context_t* main_ctx = trace_acquire_context();
  ASSERT_TRUE(main_ctx);
  trace_string_ref_t expected_str_ref;
  trace_context_register_string_literal(main_ctx, str_enabled, &expected_str_ref);
  trace_string_ref_t expected_bytes_ref;
  trace_context_register_bytestring(main_ctx, bytes_enabled, sizeof(bytes_enabled),
                                    &expected_bytes_ref);
  ASSERT_TRUE(trace_is_indexed_string_ref(&expected_str_ref));
  ASSERT_TRUE(trace_is_indexed_string_ref(&expected_bytes_ref));
  trace_release_context(main_ctx);

  constexpr size_t kNumThreads = 32;
  std::thread threads[kNumThreads];
  for (size_t i = 0; i < kNumThreads; ++i) {
    threads[i] = std::thread([&]() {
      trace_context_t* ctx = trace_acquire_context();
      ASSERT_TRUE(ctx);
      trace_string_ref_t str_ref;
      trace_context_register_string_literal(ctx, str_enabled, &str_ref);
      trace_string_ref_t bytes_ref;
      trace_context_register_bytestring(ctx, bytes_enabled, sizeof(bytes_enabled), &bytes_ref);
      EXPECT_EQ(str_ref.encoded_value, expected_str_ref.encoded_value);
      EXPECT_EQ(bytes_ref.encoded_value, expected_bytes_ref.encoded_value);
      trace_release_context(ctx);
    });
  }
  for (auto& t : threads) {
    t.join();
  }

  trace_engine_stop(ZX_OK);
  loop.RunUntilIdle();

  // Restarting with TRACE_START_CLEAR_ENTIRE_BUFFER resets the durable buffer and string table
  // and increments the context generation so existing thread-local caches are invalidated.
  start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  main_ctx = trace_acquire_context();
  ASSERT_TRUE(main_ctx);
  // Register in reverse order on the same main thread to verify the thread-local cache was
  // invalidated when the buffer and process-wide string table were cleared.
  trace_string_ref_t restarted_bytes_ref;
  trace_context_register_bytestring(main_ctx, bytes_enabled, sizeof(bytes_enabled),
                                    &restarted_bytes_ref);
  trace_string_ref_t restarted_str_ref;
  trace_context_register_string_literal(main_ctx, str_enabled, &restarted_str_ref);
  EXPECT_EQ(restarted_bytes_ref.encoded_value, expected_str_ref.encoded_value);
  EXPECT_EQ(restarted_str_ref.encoded_value, expected_bytes_ref.encoded_value);
  trace_release_context(main_ctx);

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();
}

TEST(TraceEngineTest, RegisterStringThreadLocalCacheOverflowUsesProcessTable) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  // In ONESHOT mode the entire 16384-byte buffer is used for records. Registering 300 distinct
  // string literal addresses (> 256 thread-local cache capacity) across multiple rounds and
  // threads must deduplicate via the lockless process-wide string table without allocating new
  // string indices on repeat lookups.
  alignas(uint64_t) char buffer[16384];
  trace_handler_t handler{&ops};
  ASSERT_EQ(ZX_OK, trace_engine_initialize(loop.dispatcher(), &handler,
                                           TRACE_BUFFERING_MODE_ONESHOT, buffer, sizeof(buffer)));
  loop.RunUntilIdle();
  ASSERT_EQ(ZX_OK, trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER));
  loop.RunUntilIdle();

  constexpr size_t kNumStrings = 300;
  char literals[kNumStrings][2];
  for (size_t i = 0; i < kNumStrings; ++i) {
    literals[i][0] = 's';
    literals[i][1] = '\0';
  }
  trace_string_ref_t expected_refs[kNumStrings];

  trace_context_t* main_ctx = trace_acquire_context();
  ASSERT_TRUE(main_ctx);
  for (size_t i = 0; i < kNumStrings; ++i) {
    trace_context_register_string_literal(main_ctx, literals[i], &expected_refs[i]);
    ASSERT_TRUE(trace_is_indexed_string_ref(&expected_refs[i]));
  }
  // Strings [256, 300) overflowed the 256-entry thread-local cache; re-registering them on the
  // same thread and from a second thread must return the exact same indexed references.
  for (size_t i = 0; i < kNumStrings; ++i) {
    trace_string_ref_t ref;
    trace_context_register_string_literal(main_ctx, literals[i], &ref);
    EXPECT_EQ(ref.encoded_value, expected_refs[i].encoded_value);
  }
  trace_release_context(main_ctx);

  std::thread worker([&]() {
    trace_context_t* ctx = trace_acquire_context();
    ASSERT_TRUE(ctx);
    for (size_t i = 0; i < kNumStrings; ++i) {
      trace_string_ref_t ref;
      trace_context_register_string_literal(ctx, literals[i], &ref);
      EXPECT_EQ(ref.encoded_value, expected_refs[i].encoded_value);
    }
    trace_release_context(ctx);
  });
  worker.join();

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();
}

TEST(TraceEngineTest, RegisterStringProcessTableFullDegradesGracefully) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  // 1100 distinct string literal addresses exceed the 1024-entry process-wide string table.
  // Registrations that can no longer be cached must still produce valid indexed references, and
  // entries that were cached while the table had room must keep deduplicating across threads.
  constexpr size_t kBufferSize = 256 * 1024;
  auto buffer = std::make_unique<char[]>(kBufferSize);
  trace_handler_t handler{&ops};
  ASSERT_EQ(ZX_OK,
            trace_engine_initialize(loop.dispatcher(), &handler, TRACE_BUFFERING_MODE_ONESHOT,
                                    buffer.get(), kBufferSize));
  loop.RunUntilIdle();
  ASSERT_EQ(ZX_OK, trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER));
  loop.RunUntilIdle();

  constexpr size_t kNumStrings = 1100;
  constexpr size_t kNumCachedStrings = 256;
  auto literals = std::make_unique<char[][2]>(kNumStrings);
  for (size_t i = 0; i < kNumStrings; ++i) {
    literals[i][0] = 's';
    literals[i][1] = '\0';
  }
  auto expected_refs = std::make_unique<trace_string_ref_t[]>(kNumStrings);

  trace_context_t* main_ctx = trace_acquire_context();
  ASSERT_TRUE(main_ctx);
  for (size_t i = 0; i < kNumStrings; ++i) {
    trace_context_register_string_literal(main_ctx, literals[i], &expected_refs[i]);
    ASSERT_TRUE(trace_is_indexed_string_ref(&expected_refs[i])) << i;
  }
  trace_release_context(main_ctx);

  std::thread worker([&]() {
    trace_context_t* ctx = trace_acquire_context();
    ASSERT_TRUE(ctx);
    for (size_t i = 0; i < kNumStrings; ++i) {
      trace_string_ref_t ref;
      trace_context_register_string_literal(ctx, literals[i], &ref);
      EXPECT_TRUE(trace_is_indexed_string_ref(&ref)) << i;
      // Strings registered while the table was at most 25% full are cached with bounded probing
      // and must resolve to the same index from another thread.
      if (i < kNumCachedStrings) {
        EXPECT_EQ(ref.encoded_value, expected_refs[i].encoded_value) << i;
      }
    }
    trace_release_context(ctx);
  });
  worker.join();

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();
}

TEST(TraceEngineTest, RegisterVThreadDeduplicatesAcrossThreadsAndCacheOverflow) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  // With an 8192-byte buffer in CIRCULAR mode, ComputeBufferSizes() sets the durable buffer to
  // 512 bytes (8064 / 16 = 504, rounded up by 8 for 16-byte alignment of the remaining rolling
  // buffer space). Starting the engine and registering 6 virtual threads (> kMaxThreadEntries = 4)
  // writes one 16-byte kInitialization record, one 16-byte kString("process"), 6 40-byte
  // kKernelObject records, and 6 24-byte kThread records into the durable buffer (416 bytes
  // total). Without process-wide vthread deduplication, re-registering vthreads 5 and 6 on the
  // same thread (when entry == nullptr) would repeatedly call
  // trace_context_write_thread_info_record (emitting kKernelObject) and degrade to inline thread
  // refs, while registering them from additional threads would also call WriteThreadRecord
  // (emitting both kKernelObject and kThread) and overflow the durable buffer.
  alignas(uint64_t) char buffer[8192];
  trace_handler_t handler{&ops};
  zx_status_t init = trace_engine_initialize(loop.dispatcher(), &handler,
                                             TRACE_BUFFERING_MODE_CIRCULAR, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  constexpr size_t kNumVThreads = 6;
  trace_thread_ref_t expected_refs[kNumVThreads];

  trace_context_t* main_ctx = trace_acquire_context();
  ASSERT_TRUE(main_ctx);
  for (size_t i = 0; i < kNumVThreads; ++i) {
    trace_context_register_vthread(main_ctx, ZX_KOID_INVALID, "vthread", i + 1, &expected_refs[i]);
    EXPECT_TRUE(trace_is_indexed_thread_ref(&expected_refs[i]));
  }

  // Repeatedly registering all 6 vthreads on the same thread (where vthreads 5 and 6 exceed
  // the 4-entry thread-local cache) must return the same indexed refs without writing new records.
  for (size_t round = 0; round < 16; ++round) {
    for (size_t i = 0; i < kNumVThreads; ++i) {
      trace_thread_ref_t ref;
      trace_context_register_vthread(main_ctx, ZX_KOID_INVALID, "vthread", i + 1, &ref);
      EXPECT_EQ(ref.encoded_value, expected_refs[i].encoded_value);

      trace_string_ref_t name_ref = trace_make_inline_c_string_ref("vthread");
      trace_thread_ref_t by_ref;
      trace_context_register_vthread_by_ref(main_ctx, ZX_KOID_INVALID, &name_ref, i + 1, &by_ref);
      EXPECT_EQ(by_ref.encoded_value, expected_refs[i].encoded_value);
    }
  }
  trace_release_context(main_ctx);

  // Registering the same vthreads across worker threads must reuse the process-wide registrations.
  constexpr size_t kNumThreads = 8;
  for (size_t t_idx = 0; t_idx < kNumThreads; ++t_idx) {
    std::thread t([&] {
      trace_context_t* ctx = trace_acquire_context();
      ASSERT_TRUE(ctx);
      for (size_t i = 0; i < kNumVThreads; ++i) {
        trace_thread_ref_t ref;
        trace_context_register_vthread(ctx, ZX_KOID_INVALID, "vthread", i + 1, &ref);
        EXPECT_EQ(ref.encoded_value, expected_refs[i].encoded_value);
      }
      trace_release_context(ctx);
    });
    t.join();
  }

  main_ctx = trace_acquire_context();
  ASSERT_TRUE(main_ctx);
  // Write an event to verify the durable buffer did not overflow and stop tracing.
  trace_string_ref_t cat_ref = trace_make_inline_c_string_ref("enabled_cat");
  trace_string_ref_t name_ref = trace_make_inline_c_string_ref("ev");
  trace_context_write_instant_event_record(main_ctx, 100, &expected_refs[kNumVThreads - 1],
                                           &cat_ref, &name_ref, TRACE_SCOPE_THREAD, nullptr, 0);
  EXPECT_EQ(reinterpret_cast<trace_context*>(main_ctx)->num_records_dropped(), 0u);
  trace_release_context(main_ctx);

  trace_engine_stop(ZX_OK);
  loop.RunUntilIdle();

  // Restarting with TRACE_START_CLEAR_ENTIRE_BUFFER resets the vthread table and generation.
  start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  main_ctx = trace_acquire_context();
  ASSERT_TRUE(main_ctx);
  trace_thread_ref_t restarted_ref;
  trace_context_register_vthread(main_ctx, ZX_KOID_INVALID, "vthread", kNumVThreads,
                                 &restarted_ref);
  EXPECT_EQ(restarted_ref.encoded_value, expected_refs[0].encoded_value);
  trace_release_context(main_ctx);

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();
}

TEST(TraceEngineTest, MarkRecordsDroppedUpdatesCounterAndHeader) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  alignas(uint64_t) char buffer[4096];
  trace_handler_t handler{&ops};
  zx_status_t init = trace_engine_initialize(loop.dispatcher(), &handler,
                                             TRACE_BUFFERING_MODE_ONESHOT, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  trace_context_t* context = trace_acquire_context();
  ASSERT_TRUE(context);
  EXPECT_EQ(reinterpret_cast<trace_context*>(context)->num_records_dropped(), 0u);

  trace_context_mark_records_dropped(context, 5);
  EXPECT_EQ(reinterpret_cast<trace_context*>(context)->num_records_dropped(), 5u);

  trace_context_mark_records_dropped(context, 3);
  EXPECT_EQ(reinterpret_cast<trace_context*>(context)->num_records_dropped(), 8u);
  trace_release_context(context);

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();

  const auto* header = reinterpret_cast<const trace::internal::trace_buffer_header*>(buffer);
  EXPECT_EQ(header->num_records_dropped, 8u);
}
