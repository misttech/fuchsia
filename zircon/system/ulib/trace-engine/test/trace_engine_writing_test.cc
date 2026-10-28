// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/fit/defer.h>
#include <lib/trace-engine/buffer_internal.h>
#include <lib/trace-engine/context.h>
#include <lib/trace-engine/handler.h>
#include <lib/trace-engine/instrumentation.h>
#include <lib/zx/thread.h>

#include <barrier>
#include <thread>
#include <vector>

#include <gtest/gtest.h>
#include <trace-reader/reader.h>

#include "../context_impl.h"

using trace::internal::trace_buffer_header;

namespace {
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

struct TestTraceHandler : trace_handler_t {
  TestTraceHandler()
      : trace_handler_t{&test_ops},
        test_ops{.is_category_enabled = is_category_enabled,
                 .trace_started = [](trace_handler_t*) {},
                 .trace_stopped = [](trace_handler_t*, zx_status_t) {},
                 .trace_terminated = [](trace_handler_t*) {},
                 .notify_buffer_full =
                     [](trace_handler_t* handler, uint32_t wrapped_count, uint64_t durable_data) {
                       static_cast<TestTraceHandler*>(handler)->notify_buffer_full(wrapped_count,
                                                                                   durable_data);
                     },
                 .send_alert = [](trace_handler_t*, const char*) {}} {}

  void notify_buffer_full(uint32_t wrapped_count, uint64_t durable_data) {
    trace_buffer_header* header = reinterpret_cast<trace_buffer_header*>(buffer);
    size_t buffer_number = wrapped_count % 2;
    size_t buffer_position = buffer_number * header->rolling_buffer_size;
    uint8_t* rolling_buffer =
        buffer + sizeof(trace_buffer_header) + header->durable_buffer_size + buffer_position;
    std::span data_to_copy{rolling_buffer, header->rolling_data_end[buffer_number]};
    std::ranges::copy(data_to_copy, std::back_inserter(streamed_data));
    trace_engine_mark_buffer_saved(wrapped_count, durable_data);
    printf("Saved buffer: %u\n", wrapped_count);
  }

  static constexpr size_t kBufSize = 4096;
  std::vector<uint8_t> streamed_data;
  uint8_t buffer[kBufSize];
  trace_handler_ops test_ops;
};
}  // namespace

TEST(TraceEngineWritingTest, FillBufferOneshot) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  constexpr size_t kBufSize = 4096;
  uint8_t buffer[kBufSize];
  trace_handler_t handler{&ops};
  zx_status_t init = trace_engine_initialize(loop.dispatcher(), &handler,
                                             TRACE_BUFFERING_MODE_ONESHOT, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  constexpr size_t kNumThreads = 4;
  // Write enough records to fill the buffer and then some.
  // Each record is 16 bytes. The buffer is 4k.
  // So we need ~256 records to fill it.
  constexpr size_t kNumWritesPerThread = 128;

  std::vector<std::thread> threads;
  threads.reserve(kNumThreads);
  std::barrier barrier{kNumThreads, []() {}};
  for (size_t i = 0; i < kNumThreads; ++i) {
    threads.emplace_back([&barrier, i] {
      barrier.arrive_and_wait();
      for (size_t j = 0; j < kNumWritesPerThread; ++j) {
        trace_context_t* context = trace_acquire_context();
        ASSERT_TRUE(context);
        uint64_t* alloc = reinterpret_cast<uint64_t*>(trace_context_alloc_record(context, 16));
        if (alloc != nullptr) {
          alloc[0] = i;
          alloc[1] = j + 1;
        }
        trace_release_context(context);
      }
    });
  }

  for (auto& thread : threads) {
    thread.join();
  }

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();

  // Verify the header contents
  trace_buffer_header* header = reinterpret_cast<trace_buffer_header*>(buffer);
  ASSERT_EQ(header->durable_buffer_size, uint64_t{0});
  ASSERT_EQ(header->total_size, uint64_t{kBufSize});
  ASSERT_GT(header->num_records_dropped, uint64_t{0});
  ASSERT_EQ(header->wrapped_count, uint32_t{0});
  ASSERT_EQ(header->buffering_mode, TRACE_BUFFERING_MODE_ONESHOT);
  ASSERT_EQ(header->rolling_buffer_size, uint64_t{kBufSize} - sizeof(trace_buffer_header));
  // The buffer should be full.
  ASSERT_EQ(header->rolling_data_end[0], uint64_t{kBufSize} - sizeof(trace_buffer_header));
  ASSERT_EQ(header->rolling_data_end[1], uint64_t{0});

  uint64_t* rolling_buffer = reinterpret_cast<uint64_t*>(buffer + sizeof(trace_buffer_header));
  size_t prevs[kNumThreads] = {0};
  size_t num_records = header->rolling_data_end[0] / 8;
  for (size_t i = 0; i < num_records; i += 2) {
    uint64_t thread_id = rolling_buffer[i];
    uint64_t val = rolling_buffer[i + 1];
    if (thread_id < kNumThreads) {
      ASSERT_GT(val, prevs[thread_id]);
      prevs[thread_id] = val;
    }
  }
}

TEST(TraceEngineWritingTest, FillBufferCircular) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  constexpr size_t kBufSize = 4096;
  uint8_t buffer[kBufSize];
  trace_handler_t handler{&ops};
  zx_status_t init = trace_engine_initialize(loop.dispatcher(), &handler,
                                             TRACE_BUFFERING_MODE_CIRCULAR, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  constexpr size_t kNumThreads = 4;
  constexpr size_t kNumWritesPerThread = 1024;

  std::vector<std::thread> threads;
  threads.reserve(kNumThreads);
  std::barrier barrier{kNumThreads, []() {}};
  for (size_t i = 0; i < kNumThreads; ++i) {
    threads.emplace_back([&barrier, i] {
      barrier.arrive_and_wait();
      for (size_t j = 0; j < kNumWritesPerThread; ++j) {
        trace_context_t* context = trace_acquire_context();
        ASSERT_TRUE(context);
        uint64_t* alloc = reinterpret_cast<uint64_t*>(trace_context_alloc_record(context, 16));
        if (alloc != nullptr) {
          alloc[0] = i;
          alloc[1] = j + 1;
        }
        trace_release_context(context);
      }
    });
  }

  for (auto& thread : threads) {
    thread.join();
  }

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();

  trace_buffer_header* header = reinterpret_cast<trace_buffer_header*>(buffer);
  ASSERT_EQ(header->total_size, uint64_t{kBufSize});
  ASSERT_GE(header->wrapped_count, uint32_t{1});
  ASSERT_EQ(header->buffering_mode, TRACE_BUFFERING_MODE_CIRCULAR);

  auto verify_buffer = [&](uint64_t* rolling_buffer, size_t size_in_bytes) {
    size_t prevs[kNumThreads] = {0};
    for (size_t i = 0; i < size_in_bytes / 8; i += 2) {
      uint64_t thread = rolling_buffer[i];
      uint64_t val = rolling_buffer[i + 1];
      ASSERT_GE(val, prevs[thread]);
      prevs[thread] = val;
    }
  };

  uint64_t* rolling_buffer0 = reinterpret_cast<uint64_t*>(buffer + sizeof(trace_buffer_header) +
                                                          header->durable_buffer_size);
  verify_buffer(rolling_buffer0, header->rolling_data_end[0]);

  uint64_t* rolling_buffer1 =
      reinterpret_cast<uint64_t*>(buffer + sizeof(trace_buffer_header) +
                                  header->durable_buffer_size + header->rolling_buffer_size);
  verify_buffer(rolling_buffer1, header->rolling_data_end[1]);
}

TEST(TraceEngineWritingTest, FillBufferStreaming) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  TestTraceHandler handler;

  zx_status_t init =
      trace_engine_initialize(loop.dispatcher(), &handler, TRACE_BUFFERING_MODE_STREAMING,
                              handler.buffer, sizeof(handler.buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  constexpr size_t kNumThreads = 4;
  constexpr size_t kNumWritesPerThread = 1024;

  std::vector<std::thread> threads;
  threads.reserve(kNumThreads);
  std::barrier barrier{kNumThreads + 1, []() {}};
  for (size_t i = 0; i < kNumThreads; ++i) {
    threads.emplace_back([&barrier, i] {
      barrier.arrive_and_wait();
      for (size_t j = 0; j < kNumWritesPerThread; ++j) {
        trace_context_t* context = trace_acquire_context();
        ASSERT_TRUE(context);
        uint64_t* alloc = reinterpret_cast<uint64_t*>(trace_context_alloc_record(context, 16));
        if (alloc != nullptr) {
          alloc[0] = i;
          alloc[1] = j + 1;
        }
        trace_release_context(context);
        std::this_thread::sleep_for(std::chrono::microseconds(10));
      }
    });
  }
  barrier.arrive_and_wait();
  loop.Run(zx::deadline_after(zx::sec(1)));

  for (auto& thread : threads) {
    thread.join();
  }

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();

  trace_buffer_header* header = reinterpret_cast<trace_buffer_header*>(handler.buffer);
  ASSERT_EQ(header->total_size, uint64_t{handler.kBufSize});
  ASSERT_EQ(header->buffering_mode, TRACE_BUFFERING_MODE_STREAMING);

  ASSERT_FALSE(handler.streamed_data.empty());
  auto* data = reinterpret_cast<uint64_t*>(handler.streamed_data.data());
  size_t num_records = handler.streamed_data.size() / 8;
  size_t prevs[kNumThreads] = {0};
  for (size_t i = 0; i < num_records; i += 2) {
    uint64_t thread = data[i];
    uint64_t value = data[i + 1];
    ASSERT_GT(value, prevs[thread]);
    prevs[thread] = value;
  }

  auto verify_buffer = [&](uint64_t* rolling_buffer, size_t size_in_bytes) {
    size_t prevs[kNumThreads] = {0};
    for (size_t i = 0; i < size_in_bytes / 8; i += 2) {
      uint64_t thread = rolling_buffer[i];
      uint64_t val = rolling_buffer[i + 1];
      ASSERT_GE(val, prevs[thread]);
      prevs[thread] = val;
    }
  };

  printf("Wrapped count: %u\n", header->wrapped_count);
  printf("Rolling data end 0: %lu\n", header->rolling_data_end[0]);
  printf("Rolling data end 1: %lu\n", header->rolling_data_end[1]);
  uint64_t* rolling_buffer0 = reinterpret_cast<uint64_t*>(
      handler.buffer + sizeof(trace_buffer_header) + header->durable_buffer_size);
  verify_buffer(rolling_buffer0, header->rolling_data_end[0]);

  uint64_t* rolling_buffer1 =
      reinterpret_cast<uint64_t*>(handler.buffer + sizeof(trace_buffer_header) +
                                  header->durable_buffer_size + header->rolling_buffer_size);
  verify_buffer(rolling_buffer1, header->rolling_data_end[1]);
}

// Verifies that when the durable buffer becomes full, subsequent string, thread, and virtual
// thread registrations fall back to inline references and tracing continues recording events into
// the rolling buffers rather than artificially stopping the trace.
TEST(TraceEngineWritingTest, DurableOverflowFallsBackToInlineRefsWithoutStoppingTracing) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  constexpr size_t kBufSize = 4096;
  alignas(uint64_t) uint8_t buffer[kBufSize];
  trace_handler_t handler{&ops};

  zx_status_t init = trace_engine_initialize(loop.dispatcher(), &handler,
                                             TRACE_BUFFERING_MODE_CIRCULAR, buffer, sizeof(buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  trace_context_t* context = trace_acquire_context();
  ASSERT_TRUE(context);

  // Exhaust the durable buffer completely (down to 8-byte words).
  while (trace_context_alloc_durable_record(context, 8) != nullptr) {
  }

  // Subsequent string and thread registrations must gracefully fall back to inline references, and
  // process/thread kKernelObject info records must fall back to the active rolling buffer.
  trace_string_ref_t cat_ref;
  trace_context_register_string_literal(context, "enabled_cat", &cat_ref);
  EXPECT_TRUE(trace_is_inline_string_ref(&cat_ref));

  trace_string_ref_t name_ref;
  const unsigned char kEventName[] = {'f', 'a', 'l', 'l', 'b', 'a', 'c', 'k'};
  trace_context_register_bytestring(context, kEventName, sizeof(kEventName), &name_ref);
  EXPECT_TRUE(trace_is_inline_string_ref(&name_ref));

  constexpr zx_koid_t kProcessKoid = 42;
  trace_context_write_process_info_record(context, kProcessKoid, &name_ref);

  trace_thread_ref_t thread_ref;
  trace_context_register_current_thread(context, &thread_ref);
  EXPECT_TRUE(trace_is_inline_thread_ref(&thread_ref));

  trace_thread_ref_t vthread_ref;
  trace_context_register_vthread(context, ZX_KOID_INVALID, "fallback_vthread", 99, &vthread_ref);
  EXPECT_TRUE(trace_is_inline_thread_ref(&vthread_ref));

  // For kBufSize = 4096 (128-byte header + 256-byte durable buffer), each rolling buffer is
  // (4096 - 128 - 256) / 2 = 1856 bytes. After the 3 fallback kKernelObject records (~136 bytes),
  // rolling buffer 0 has ~1720 bytes left, fitting 26 64-byte inline kInstant events (8 header +
  // 8 timestamp + 16 inline thread + 16 inline category + 8 inline name + 8 scope). Writing 48
  // events puts 26 in rolling buffer 0 and 22 (1408 bytes) in rolling buffer 1, wrapping once
  // without overwriting rolling buffer 0. Release and re-acquire the context on each write so
  // ServiceBuffers() can mark the wrapped circular buffer as serviced.
  constexpr size_t kNumEvents = 48;
  for (size_t i = 0; i < kNumEvents; ++i) {
    trace_context_write_instant_event_record(context, 1000 + i, &vthread_ref, &cat_ref, &name_ref,
                                             TRACE_SCOPE_THREAD, nullptr, 0);
    trace_release_context(context);
    context = trace_acquire_context();
    ASSERT_TRUE(context);
  }

  trace_release_context(context);

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();

  trace_buffer_header* header = reinterpret_cast<trace_buffer_header*>(buffer);
  EXPECT_EQ(header->durable_data_end, header->durable_buffer_size);
  EXPECT_EQ(header->num_records_dropped, 0u);
  EXPECT_EQ(header->wrapped_count, 1u);

  std::vector<std::string> errors;
  size_t process_object_count = 0;
  size_t thread_object_count = 0;
  size_t event_count = 0;
  trace::TraceReader reader(
      [&](trace::Record record) {
        if (record.type() == trace::RecordType::kKernelObject) {
          const auto& obj = record.GetKernelObject();
          if (obj.object_type == ZX_OBJ_TYPE_PROCESS) {
            EXPECT_EQ(obj.koid, kProcessKoid);
            EXPECT_EQ(obj.name, "fallback");
            ++process_object_count;
          } else if (obj.object_type == ZX_OBJ_TYPE_THREAD) {
            ++thread_object_count;
          }
        } else if (record.type() == trace::RecordType::kEvent) {
          ++event_count;
        }
      },
      [&errors](std::string_view err) { errors.emplace_back(err); });

  for (size_t b = 0; b < 2; ++b) {
    uint8_t* rolling_buffer = buffer + sizeof(trace_buffer_header) + header->durable_buffer_size +
                              b * header->rolling_buffer_size;
    size_t rolling_words = header->rolling_data_end[b] / sizeof(uint64_t);
    ASSERT_GT(rolling_words, 0u);
    trace::Chunk chunk(reinterpret_cast<const uint64_t*>(rolling_buffer), rolling_words);
    EXPECT_TRUE(reader.ReadRecords(chunk));
  }
  EXPECT_TRUE(errors.empty());
  EXPECT_EQ(process_object_count, 1u);
  EXPECT_EQ(thread_object_count, 2u);
  EXPECT_EQ(event_count, kNumEvents);
}

// Verifies that in streaming mode, once the dedicated durable buffer fills up, subsequent durable
// records (strings, threads, virtual threads, and kernel object info records) overflow into the
// active rolling buffer so their storage is reclaimed as rolling buffers are saved, transient
// rolling-buffer-full conditions do not permanently disable index registration, and restarting with
// TRACE_START_CLEAR_NONDURABLE_BUFFER resets the durable buffer and caches when overflow occurred.
TEST(TraceEngineWritingTest, StreamingDurableOverflowUsesRollingBuffersAndRecoversFromFull) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  TestTraceHandler handler;

  char original_name[ZX_MAX_NAME_LEN] = {};
  ASSERT_EQ(
      zx_object_get_property(zx_thread_self(), ZX_PROP_NAME, original_name, sizeof(original_name)),
      ZX_OK);
  auto restore_name = fit::defer([&original_name] {
    zx_object_set_property(zx_thread_self(), ZX_PROP_NAME, original_name, strlen(original_name));
  });
  constexpr char kMainThreadName[] = "test_main_thread";
  ASSERT_EQ(zx_object_set_property(zx_thread_self(), ZX_PROP_NAME, kMainThreadName,
                                   strlen(kMainThreadName)),
            ZX_OK);

  zx_status_t init =
      trace_engine_initialize(loop.dispatcher(), &handler, TRACE_BUFFERING_MODE_STREAMING,
                              handler.buffer, sizeof(handler.buffer));
  ASSERT_EQ(init, ZX_OK);
  loop.RunUntilIdle();

  zx_status_t start = trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  // Fill 240 of the 256 bytes in the dedicated durable buffer (16-byte init record + 7 * 32-byte
  // string records), leaving 16 bytes of unallocated tail space in the durable buffer.
  unsigned char durable_strings[7][24];
  {
    trace_context_t* context = trace_acquire_context();
    ASSERT_TRUE(context);
    for (size_t i = 0; i < 7; ++i) {
      memset(durable_strings[i], 'A' + static_cast<char>(i), sizeof(durable_strings[i]));
      trace_string_ref_t ref;
      trace_context_register_bytestring(context, durable_strings[i], sizeof(durable_strings[i]),
                                        &ref);
      EXPECT_TRUE(trace_is_indexed_string_ref(&ref));
    }
    trace_release_context(context);
  }

  // Register 64 additional 24-byte strings (> 2048 bytes of kString records) and 3 vthreads
  // (leaving 1 of the 4 thread-local ContextCache::thread_table slots free for
  // kTransientVThreadId below) across rolling buffer saves, verifying they overflow into rolling
  // buffers and still get indexed references. On the first overflow string, also write a 16-byte
  // process info record referencing that overflowed string to verify that latching
  // durable_overflowed_ prevents the 16-byte record from allocating in the 16-byte tail of
  // durable_buffer_ ahead of the string.
  constexpr zx_koid_t kProcessKoid = 777;
  constexpr size_t kNumOverflowStrings = 64;
  unsigned char overflow_strings[kNumOverflowStrings][24];
  trace_encoded_string_ref_t last_string_index = 0;
  trace_encoded_thread_ref_t last_thread_index = 0;
  for (size_t i = 0; i < kNumOverflowStrings; ++i) {
    memset(overflow_strings[i], 'a' + static_cast<char>(i % 26), sizeof(overflow_strings[i]));
    overflow_strings[i][0] = static_cast<unsigned char>('0' + (i % 10));

    trace_context_t* context = trace_acquire_context();
    ASSERT_TRUE(context);
    trace_string_ref_t str_ref;
    trace_context_register_bytestring(context, overflow_strings[i], sizeof(overflow_strings[i]),
                                      &str_ref);
    EXPECT_TRUE(trace_is_indexed_string_ref(&str_ref));

    if (i == 0) {
      trace_context_write_process_info_record(context, kProcessKoid, &str_ref);
    }

    trace_thread_ref_t vthread_ref;
    trace_context_register_vthread(context, ZX_KOID_INVALID, "stream_vthread", (i % 3) + 1,
                                   &vthread_ref);
    EXPECT_TRUE(trace_is_indexed_thread_ref(&vthread_ref));

    trace_string_ref_t cat_ref;
    trace_context_register_string_literal(context, "enabled_cat", &cat_ref);
    EXPECT_TRUE(trace_is_indexed_string_ref(&cat_ref));

    last_string_index = std::max({last_string_index, str_ref.encoded_value, cat_ref.encoded_value});
    last_thread_index = std::max(last_thread_index, vthread_ref.encoded_value);

    trace_context_write_instant_event_record(context, 2000 + i, &vthread_ref, &cat_ref, &str_ref,
                                             TRACE_SCOPE_THREAD, nullptr, 0);
    trace_release_context(context);
    loop.RunUntilIdle();
  }

  // Now exhaust both rolling buffers using valid events without running the dispatcher loop so
  // buffer saves are pending, then verify that transient registration failures recover once the
  // loop services the rolling buffers.
  const char* kTransientStr = "transient_full_string_25b";
  constexpr trace_vthread_id_t kTransientVThreadId = 555;
  constexpr trace_vthread_id_t kRollbackVThreadId = 556;
  {
    trace_context_t* context = trace_acquire_context();
    ASSERT_TRUE(context);

    trace_string_ref_t cat_ref;
    trace_context_register_string_literal(context, "enabled_cat", &cat_ref);
    ASSERT_TRUE(trace_is_indexed_string_ref(&cat_ref));

    trace_thread_ref_t existing_vthread_ref;
    trace_context_register_vthread(context, ZX_KOID_INVALID, "stream_vthread", 1,
                                   &existing_vthread_ref);
    ASSERT_TRUE(trace_is_indexed_thread_ref(&existing_vthread_ref));

    // Write 48-byte kInstant events (8 header + 8 timestamp + 24 inline name + 8 scope) until one
    // is dropped. Because each rolling buffer is 1856 bytes (38 * 48 + 32), wrapping into the
    // second rolling buffer and writing 38 48-byte events leaves exactly 32 bytes free in the
    // active rolling buffer while the first rolling buffer is full (pending service).
    trace_string_ref_t filler_name_ref = trace_make_inline_c_string_ref("filler_event_name_17chr");
    ASSERT_EQ(context->num_records_dropped(), 0u);
    while (context->num_records_dropped() == 0u) {
      trace_context_write_instant_event_record(context, 3000, &existing_vthread_ref, &cat_ref,
                                               &filler_name_ref, TRACE_SCOPE_THREAD, nullptr, 0);
    }
    EXPECT_EQ(context->num_records_dropped(), 1u);

    // With 32 bytes remaining in the active rolling buffer:
    // 1) kTransientStr (25 bytes -> 40-byte kString record) cannot fit, returns an inline ref
    //    without latching in the cache, and releases its allocated string index.
    trace_string_ref_t transient_str_ref;
    trace_context_register_string_literal(context, kTransientStr, &transient_str_ref);
    EXPECT_TRUE(trace_is_inline_string_ref(&transient_str_ref));
    EXPECT_EQ(context->num_records_dropped(), 2u);

    // 2) RegisterVThread claims the 4th thread-local ThreadEntry slot for kTransientVThreadId;
    //    WriteThreadInfoRecord (56 bytes) cannot fit, drops once (not twice), marks
    //    entry->retry_pending, and skips the 24-byte WriteThreadRecord that would otherwise fit in
    //    the remaining 32 bytes without a thread name record.
    trace_thread_ref_t transient_vthread_ref;
    trace_context_register_vthread(context, ZX_KOID_INVALID, "transient_vthread",
                                   kTransientVThreadId, &transient_vthread_ref);
    EXPECT_TRUE(trace_is_inline_thread_ref(&transient_vthread_ref));
    EXPECT_EQ(context->num_records_dropped(), 3u);

    // 3) trace_context_register_current_thread's WriteThreadInfoRecord (48 bytes) likewise cannot
    //    fit, drops once, and skips the 24-byte WriteThreadRecord without latching in the cache.
    trace_thread_ref_t transient_thread_ref;
    trace_context_register_current_thread(context, &transient_thread_ref);
    EXPECT_TRUE(trace_is_inline_thread_ref(&transient_thread_ref));
    EXPECT_EQ(context->num_records_dropped(), 4u);

    // 4) kRollbackVThreadId exceeds the 4-entry thread-local cache (entry == nullptr) and uses an
    //    empty name, so its 32-byte WriteThreadInfoRecord (8 header + 8 koid + 16 process koid
    //    arg) succeeds and consumes the exact 32 remaining bytes (confirming neither earlier
    //    failed thread registration wrote a 24-byte kThread record), while the subsequent 24-byte
    //    WriteThreadRecord fails on 0 remaining bytes and rolls back its allocated thread index.
    trace_thread_ref_t rollback_vthread_ref;
    trace_context_register_vthread(context, ZX_KOID_INVALID, "", kRollbackVThreadId,
                                   &rollback_vthread_ref);
    EXPECT_TRUE(trace_is_inline_thread_ref(&rollback_vthread_ref));
    EXPECT_EQ(context->num_records_dropped(), 5u);

    // With 0 bytes left, uncached string copy and explicit thread registrations also fall back to
    // inline references and release their indices.
    trace_string_ref_t copy_ref;
    trace_context_register_string_copy(context, "copy_str", 8, &copy_ref);
    EXPECT_TRUE(trace_is_inline_string_ref(&copy_ref));
    trace_thread_ref_t explicit_thread_ref;
    trace_context_register_thread(context, kProcessKoid, 888, &explicit_thread_ref);
    EXPECT_TRUE(trace_is_inline_thread_ref(&explicit_thread_ref));
    EXPECT_EQ(context->num_records_dropped(), 7u);

    // Until a rolling buffer is saved, the failed string, cached vthread (entry != nullptr),
    // uncached vthread (entry == nullptr, throttled via vthread_table_), and current thread
    // registrations are not retried: they return inline references without writing, so no more
    // records are dropped.
    trace_context_register_string_literal(context, kTransientStr, &transient_str_ref);
    EXPECT_TRUE(trace_is_inline_string_ref(&transient_str_ref));
    trace_context_register_vthread(context, ZX_KOID_INVALID, "transient_vthread",
                                   kTransientVThreadId, &transient_vthread_ref);
    EXPECT_TRUE(trace_is_inline_thread_ref(&transient_vthread_ref));
    trace_context_register_vthread(context, ZX_KOID_INVALID, "", kRollbackVThreadId,
                                   &rollback_vthread_ref);
    EXPECT_TRUE(trace_is_inline_thread_ref(&rollback_vthread_ref));
    trace_context_register_current_thread(context, &transient_thread_ref);
    EXPECT_TRUE(trace_is_inline_thread_ref(&transient_thread_ref));
    EXPECT_EQ(context->num_records_dropped(), 7u);

    trace_release_context(context);
  }

  // Service the first full rolling buffer.
  loop.RunUntilIdle();

  // Retrying on the same thread must now succeed, reclaim the rolled-back string and thread
  // indices, and emit valid records into the newly serviced rolling buffer.
  {
    trace_context_t* context = trace_acquire_context();
    ASSERT_TRUE(context);
    trace_string_ref_t cat_ref;
    trace_context_register_string_literal(context, "enabled_cat", &cat_ref);
    EXPECT_TRUE(trace_is_indexed_string_ref(&cat_ref));

    trace_string_ref_t recovered_str_ref;
    trace_context_register_string_literal(context, kTransientStr, &recovered_str_ref);
    EXPECT_TRUE(trace_is_indexed_string_ref(&recovered_str_ref));
    EXPECT_EQ(recovered_str_ref.encoded_value,
              static_cast<trace_encoded_string_ref_t>(last_string_index + 1u));

    trace_thread_ref_t recovered_vthread_ref;
    trace_context_register_vthread(context, ZX_KOID_INVALID, "transient_vthread",
                                   kTransientVThreadId, &recovered_vthread_ref);
    EXPECT_TRUE(trace_is_indexed_thread_ref(&recovered_vthread_ref));
    EXPECT_EQ(recovered_vthread_ref.encoded_value, last_thread_index + 1u);

    trace_thread_ref_t recovered_thread_ref;
    trace_context_register_current_thread(context, &recovered_thread_ref);
    EXPECT_TRUE(trace_is_indexed_thread_ref(&recovered_thread_ref));
    EXPECT_EQ(recovered_thread_ref.encoded_value, last_thread_index + 2u);

    trace_thread_ref_t recovered_rollback_vthread_ref;
    trace_context_register_vthread(context, ZX_KOID_INVALID, "", kRollbackVThreadId,
                                   &recovered_rollback_vthread_ref);
    EXPECT_TRUE(trace_is_indexed_thread_ref(&recovered_rollback_vthread_ref));
    EXPECT_EQ(recovered_rollback_vthread_ref.encoded_value, last_thread_index + 3u);

    trace_context_write_instant_event_record(context, 4000, &recovered_vthread_ref, &cat_ref,
                                             &recovered_str_ref, TRACE_SCOPE_THREAD, nullptr, 0);
    trace_context_write_instant_event_record(context, 4001, &recovered_thread_ref, &cat_ref,
                                             &recovered_str_ref, TRACE_SCOPE_THREAD, nullptr, 0);
    trace_release_context(context);
  }

  // Service the second rolling buffer that was rolled away from on recovery, then stop the engine.
  loop.RunUntilIdle();
  trace_engine_stop(ZX_OK);
  loop.RunUntilIdle();

  trace_buffer_header* header = reinterpret_cast<trace_buffer_header*>(handler.buffer);
  // durable_overflowed_ latched at 240 bytes (16-byte init + 7 * 32-byte durable strings), so
  // subsequent 16-byte durable records did not allocate in the remaining 16-byte tail of the
  // 256-byte durable buffer.
  EXPECT_EQ(header->durable_data_end, 240u);
  EXPECT_LT(header->durable_data_end, header->durable_buffer_size);
  ASSERT_FALSE(handler.streamed_data.empty());

  std::vector<std::string> errors;
  size_t process_object_count = 0;
  bool saw_transient_vthread_object = false;
  bool saw_main_thread_object = false;
  size_t recovered_event_count = 0;
  std::string expected_process_name(reinterpret_cast<const char*>(overflow_strings[0]),
                                    sizeof(overflow_strings[0]));
  trace::TraceReader reader(
      [&](const trace::Record& record) {
        if (record.type() == trace::RecordType::kKernelObject) {
          const auto& obj = record.GetKernelObject();
          if (obj.object_type == ZX_OBJ_TYPE_PROCESS) {
            EXPECT_EQ(obj.koid, kProcessKoid);
            EXPECT_EQ(obj.name, expected_process_name);
            ++process_object_count;
          } else if (obj.object_type == ZX_OBJ_TYPE_THREAD) {
            if (obj.name == "transient_vthread") {
              saw_transient_vthread_object = true;
            } else if (obj.name == kMainThreadName) {
              saw_main_thread_object = true;
            }
          }
        } else if (record.type() == trace::RecordType::kEvent) {
          if (record.GetEvent().name == kTransientStr) {
            ++recovered_event_count;
          }
        }
      },
      [&errors](std::string_view err) { errors.emplace_back(err); });

  uint8_t* durable_ptr = handler.buffer + sizeof(trace_buffer_header);
  trace::Chunk durable_chunk(reinterpret_cast<const uint64_t*>(durable_ptr),
                             header->durable_data_end / sizeof(uint64_t));
  EXPECT_TRUE(reader.ReadRecords(durable_chunk));

  trace::Chunk streamed_chunk(reinterpret_cast<const uint64_t*>(handler.streamed_data.data()),
                              handler.streamed_data.size() / sizeof(uint64_t));
  EXPECT_TRUE(reader.ReadRecords(streamed_chunk));

  size_t active_buffer = header->wrapped_count % 2;
  uint8_t* final_rolling_ptr =
      durable_ptr + header->durable_buffer_size + active_buffer * header->rolling_buffer_size;
  size_t final_rolling_words = header->rolling_data_end[active_buffer] / sizeof(uint64_t);
  ASSERT_GT(final_rolling_words, 0u);
  trace::Chunk final_rolling_chunk(reinterpret_cast<const uint64_t*>(final_rolling_ptr),
                                   final_rolling_words);
  EXPECT_TRUE(reader.ReadRecords(final_rolling_chunk));

  EXPECT_TRUE(errors.empty());
  EXPECT_EQ(process_object_count, 1u);
  EXPECT_TRUE(saw_transient_vthread_object);
  EXPECT_TRUE(saw_main_thread_object);
  EXPECT_EQ(recovered_event_count, 2u);

  // Restart with TRACE_START_CLEAR_NONDURABLE_BUFFER and verify that ClearRollingBuffers() resets
  // the durable buffer, index counters, and caches because durable_overflowed_ was set.
  handler.streamed_data.clear();
  start = trace_engine_start(TRACE_START_CLEAR_NONDURABLE_BUFFER);
  ASSERT_EQ(start, ZX_OK);
  loop.RunUntilIdle();

  {
    trace_context_t* context = trace_acquire_context();
    ASSERT_TRUE(context);

    trace_string_ref_t restarted_str_ref;
    trace_context_register_string_literal(context, kTransientStr, &restarted_str_ref);
    EXPECT_EQ(restarted_str_ref.encoded_value,
              trace_make_indexed_string_ref(TRACE_ENCODED_STRING_REF_MIN_INDEX).encoded_value);

    trace_string_ref_t restarted_cat_ref;
    trace_context_register_string_literal(context, "enabled_cat", &restarted_cat_ref);
    EXPECT_TRUE(trace_is_indexed_string_ref(&restarted_cat_ref));

    trace_thread_ref_t restarted_vthread_ref;
    trace_context_register_vthread(context, ZX_KOID_INVALID, "transient_vthread",
                                   kTransientVThreadId, &restarted_vthread_ref);
    EXPECT_EQ(restarted_vthread_ref.encoded_value,
              trace_make_indexed_thread_ref(TRACE_ENCODED_THREAD_REF_MIN_INDEX).encoded_value);

    trace_thread_ref_t restarted_thread_ref;
    trace_context_register_current_thread(context, &restarted_thread_ref);
    EXPECT_TRUE(trace_is_indexed_thread_ref(&restarted_thread_ref));

    trace_context_write_instant_event_record(context, 5000, &restarted_thread_ref,
                                             &restarted_cat_ref, &restarted_str_ref,
                                             TRACE_SCOPE_THREAD, nullptr, 0);
    trace_release_context(context);
  }

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();

  EXPECT_EQ(header->wrapped_count, 0u);
  EXPECT_GT(header->durable_data_end, 16u);
  EXPECT_LT(header->durable_data_end, header->durable_buffer_size);

  std::vector<std::string> restarted_errors;
  size_t restarted_event_count = 0;
  trace::TraceReader restarted_reader(
      [&](const trace::Record& record) {
        if (record.type() == trace::RecordType::kEvent && record.GetEvent().name == kTransientStr) {
          ++restarted_event_count;
        }
      },
      [&restarted_errors](std::string_view err) { restarted_errors.emplace_back(err); });

  trace::Chunk restarted_durable_chunk(reinterpret_cast<const uint64_t*>(durable_ptr),
                                       header->durable_data_end / sizeof(uint64_t));
  EXPECT_TRUE(restarted_reader.ReadRecords(restarted_durable_chunk));

  uint8_t* restarted_rolling0_ptr = durable_ptr + header->durable_buffer_size;
  trace::Chunk restarted_rolling0_chunk(reinterpret_cast<const uint64_t*>(restarted_rolling0_ptr),
                                        header->rolling_data_end[0] / sizeof(uint64_t));
  EXPECT_TRUE(restarted_reader.ReadRecords(restarted_rolling0_chunk));

  EXPECT_TRUE(restarted_errors.empty());
  EXPECT_EQ(restarted_event_count, 1u);
}

// Verifies that restarting a streaming session with TRACE_START_CLEAR_NONDURABLE_BUFFER when
// durable_overflowed_ is false resets the rolling buffers while retaining the durable buffer,
// process-wide and thread-local caches, and index counters so new registrations continue past
// preserved indices.
TEST(TraceEngineWritingTest, StreamingClearNondurableRetainsDurableWhenNotOverflowed) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  TestTraceHandler handler;

  ASSERT_EQ(trace_engine_initialize(loop.dispatcher(), &handler, TRACE_BUFFERING_MODE_STREAMING,
                                    handler.buffer, sizeof(handler.buffer)),
            ZX_OK);
  loop.RunUntilIdle();

  ASSERT_EQ(trace_engine_start(TRACE_START_CLEAR_ENTIRE_BUFFER), ZX_OK);
  loop.RunUntilIdle();

  trace_string_ref_t cat_ref{};
  trace_thread_ref_t retained_vthread_ref{};
  trace_string_ref_t retained_str_ref{};
  {
    trace_context_t* context = trace_acquire_context();
    ASSERT_TRUE(context);

    trace_context_register_string_literal(context, "enabled_cat", &cat_ref);
    ASSERT_TRUE(trace_is_indexed_string_ref(&cat_ref));

    trace_context_register_vthread(context, ZX_KOID_INVALID, "vt1", 1, &retained_vthread_ref);
    ASSERT_TRUE(trace_is_indexed_thread_ref(&retained_vthread_ref));

    trace_context_register_string_literal(context, "retained_str", &retained_str_ref);
    ASSERT_TRUE(trace_is_indexed_string_ref(&retained_str_ref));

    trace_string_ref_t cleared_name_ref = trace_make_inline_c_string_ref("cleared_rolling_event");
    trace_context_write_instant_event_record(context, 1000, &retained_vthread_ref, &cat_ref,
                                             &cleared_name_ref, TRACE_SCOPE_THREAD, nullptr, 0);
    trace_release_context(context);
  }

  trace_engine_stop(ZX_OK);
  loop.RunUntilIdle();

  auto* header = reinterpret_cast<trace_buffer_header*>(handler.buffer);
  const uint64_t first_durable_end = header->durable_data_end;
  ASSERT_GT(first_durable_end, 16u);
  ASSERT_LT(first_durable_end, header->durable_buffer_size);
  ASSERT_GT(header->rolling_data_end[0], 0u);
  EXPECT_TRUE(handler.streamed_data.empty());

  ASSERT_EQ(trace_engine_start(TRACE_START_CLEAR_NONDURABLE_BUFFER), ZX_OK);
  loop.RunUntilIdle();

  // Verify the process-wide string and vthread tables retained Session 1's entries across restart.
  std::thread worker([&] {
    trace_context_t* ctx = trace_acquire_context();
    ASSERT_TRUE(ctx);
    trace_string_ref_t worker_str_ref;
    trace_context_register_string_literal(ctx, "retained_str", &worker_str_ref);
    EXPECT_EQ(worker_str_ref.encoded_value, retained_str_ref.encoded_value);
    trace_thread_ref_t worker_vthread_ref;
    trace_context_register_vthread(ctx, ZX_KOID_INVALID, "vt1", 1, &worker_vthread_ref);
    EXPECT_EQ(worker_vthread_ref.encoded_value, retained_vthread_ref.encoded_value);
    trace_release_context(ctx);
  });
  worker.join();

  {
    trace_context_t* context = trace_acquire_context();
    ASSERT_TRUE(context);

    trace_string_ref_t same_cat_ref;
    trace_context_register_string_literal(context, "enabled_cat", &same_cat_ref);
    EXPECT_EQ(same_cat_ref.encoded_value, cat_ref.encoded_value);

    trace_string_ref_t same_str_ref;
    trace_context_register_string_literal(context, "retained_str", &same_str_ref);
    EXPECT_EQ(same_str_ref.encoded_value, retained_str_ref.encoded_value);

    trace_thread_ref_t same_vthread_ref;
    trace_context_register_vthread(context, ZX_KOID_INVALID, "vt1", 1, &same_vthread_ref);
    EXPECT_EQ(same_vthread_ref.encoded_value, retained_vthread_ref.encoded_value);

    trace_string_ref_t new_str_ref;
    trace_context_register_string_literal(context, "after_clear", &new_str_ref);
    EXPECT_EQ(new_str_ref.encoded_value,
              static_cast<trace_encoded_string_ref_t>(retained_str_ref.encoded_value + 1u));

    trace_thread_ref_t new_vthread_ref;
    trace_context_register_vthread(context, ZX_KOID_INVALID, "vt2", 2, &new_vthread_ref);
    EXPECT_EQ(new_vthread_ref.encoded_value, retained_vthread_ref.encoded_value + 1u);

    trace_context_write_instant_event_record(context, 2000, &retained_vthread_ref, &cat_ref,
                                             &retained_str_ref, TRACE_SCOPE_THREAD, nullptr, 0);
    trace_context_write_instant_event_record(context, 2001, &new_vthread_ref, &cat_ref,
                                             &new_str_ref, TRACE_SCOPE_THREAD, nullptr, 0);
    trace_release_context(context);
  }

  trace_engine_stop(ZX_OK);
  trace_engine_terminate();
  loop.RunUntilIdle();

  EXPECT_EQ(header->wrapped_count, 0u);
  EXPECT_GT(header->durable_data_end, first_durable_end);
  EXPECT_LT(header->durable_data_end, header->durable_buffer_size);
  EXPECT_TRUE(handler.streamed_data.empty());

  std::vector<std::string> errors;
  bool saw_cleared_event = false;
  bool saw_retained_event = false;
  bool saw_new_event = false;
  bool saw_vt1 = false;
  bool saw_vt2 = false;
  trace::TraceReader reader(
      [&](const trace::Record& record) {
        if (record.type() == trace::RecordType::kKernelObject) {
          const auto& obj = record.GetKernelObject();
          if (obj.object_type == ZX_OBJ_TYPE_THREAD) {
            if (obj.name == "vt1") {
              saw_vt1 = true;
            } else if (obj.name == "vt2") {
              saw_vt2 = true;
            }
          }
        } else if (record.type() == trace::RecordType::kEvent) {
          const auto& name = record.GetEvent().name;
          if (name == "cleared_rolling_event") {
            saw_cleared_event = true;
          } else if (name == "retained_str") {
            saw_retained_event = true;
          } else if (name == "after_clear") {
            saw_new_event = true;
          }
        }
      },
      [&errors](std::string_view err) { errors.emplace_back(err); });

  uint8_t* durable_ptr = handler.buffer + sizeof(trace_buffer_header);
  trace::Chunk durable_chunk(reinterpret_cast<const uint64_t*>(durable_ptr),
                             header->durable_data_end / sizeof(uint64_t));
  EXPECT_TRUE(reader.ReadRecords(durable_chunk));

  uint8_t* rolling0_ptr = durable_ptr + header->durable_buffer_size;
  trace::Chunk rolling0_chunk(reinterpret_cast<const uint64_t*>(rolling0_ptr),
                              header->rolling_data_end[0] / sizeof(uint64_t));
  EXPECT_TRUE(reader.ReadRecords(rolling0_chunk));

  EXPECT_TRUE(errors.empty());
  EXPECT_FALSE(saw_cleared_event);
  EXPECT_TRUE(saw_retained_event);
  EXPECT_TRUE(saw_new_event);
  EXPECT_TRUE(saw_vt1);
  EXPECT_TRUE(saw_vt2);
}
