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

#include <barrier>
#include <thread>
#include <vector>

#include <gtest/gtest.h>
#include <trace-reader/reader.h>
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
