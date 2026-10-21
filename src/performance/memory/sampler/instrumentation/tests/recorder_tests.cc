// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.memory.sampler/cpp/fidl.h>
#include <fidl/fuchsia.memory.sampler/cpp/natural_types.h>
#include <fidl/fuchsia.memory.sampler/cpp/wire_test_base.h>
#include <fidl/fuchsia.memory.sampler/cpp/wire_types.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/fidl/cpp/binding.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/sys/cpp/testing/component_context_provider.h>
#include <lib/zx/socket.h>

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <iostream>
#include <limits>
#include <span>
#include <string_view>
#include <vector>

#include <gtest/gtest.h>
#include <src/lib/testing/loop_fixture/test_loop_fixture.h>
#include <src/performance/memory/sampler/instrumentation/recorder.h>

namespace memory_sampler {

// Declared as a friend of `Recorder`; see recorder.h. Must live directly in
// `memory_sampler` rather than the anonymous namespace below for the friend
// declaration to name it.
struct RecorderTestPeer {
  // Clears the latch that suppresses repeated peer signalling, so that a test
  // can exercise a signalling path after an earlier one already fired.
  static void ResetPeerSignaled(Recorder& recorder) {
    recorder.peer_signaled_.store(false, std::memory_order_relaxed);
  }
};

namespace {

// NOLINTNEXTLINE(performance-no-int-to-ptr)
void* const kTestAddress = reinterpret_cast<void*>(0x1000);
constexpr size_t kTestSize = 100;

class SamplerImpl : public fidl::testing::WireTestBase<fuchsia_memory_sampler::Sampler> {
 public:
  SamplerImpl(async_dispatcher_t* dispatcher,
              fidl::ServerEnd<fuchsia_memory_sampler::Sampler> server_end)
      : binding_(fidl::BindServer(dispatcher, std::move(server_end), this)) {}

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    std::cerr << "Not implemented: " << name << '\n';
  }

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
  void SetSharedSocket(fuchsia_memory_sampler::wire::SamplerSetSharedSocketRequest* request,
                       SetSharedSocketCompleter::Sync& completer) override {
    socket_ = std::move(request->socket);
  }
#endif

  void Close() { binding_.Unbind(); }
  zx::socket& socket() { return socket_; }

 private:
  fidl::ServerBindingRef<fuchsia_memory_sampler::Sampler> binding_;
  zx::socket socket_;
};

std::vector<uint8_t> ReadDatagram(zx::socket& socket) {
  std::vector<uint8_t> buf(65536);
  size_t actual = 0;
  zx_status_t status = socket.read(0, buf.data(), buf.size(), &actual);
  if (status != ZX_OK) {
    return {};
  }
  buf.resize(actual);
  return buf;
}

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
void VerifyAllocationDatagram(const std::vector<uint8_t>& datagram, void* expected_address,
                              size_t expected_size, uint64_t expected_interval = 1) {
  auto unpersisted = fidl::InplaceUnpersist<fuchsia_memory_sampler::wire::SamplerDatagram>(
      std::span<uint8_t>(const_cast<uint8_t*>(datagram.data()), datagram.size()));
  ASSERT_TRUE(unpersisted.is_ok());
  auto& dg = unpersisted.value();
  EXPECT_TRUE(dg->is_record_allocation());
  auto& header = dg->record_allocation();
  EXPECT_EQ(header.address(), reinterpret_cast<uint64_t>(expected_address));
  EXPECT_EQ(header.size(), expected_size);
  EXPECT_GE(header.stack_trace().stack_frames().size(), 1U);
  EXPECT_TRUE(header.has_sampling_interval_bytes());
  EXPECT_EQ(header.sampling_interval_bytes(), expected_interval);
}

void VerifyDeallocationDatagram(const std::vector<uint8_t>& datagram, void* expected_address) {
  auto unpersisted = fidl::InplaceUnpersist<fuchsia_memory_sampler::wire::SamplerDatagram>(
      std::span<uint8_t>(const_cast<uint8_t*>(datagram.data()), datagram.size()));
  ASSERT_TRUE(unpersisted.is_ok());
  auto& dg = unpersisted.value();
  EXPECT_TRUE(dg->is_record_deallocation());
  auto& header = dg->record_deallocation();
  EXPECT_EQ(header.address(), reinterpret_cast<uint64_t>(expected_address));
  EXPECT_GE(header.stack_trace().stack_frames().size(), 1U);
}
#endif

PoissonSampler& GetSamplerThatAlwaysSamples() {
  class SampleIntervalGenerator : public PoissonSampler::SampleIntervalGenerator {
   public:
    size_t GetNextSampleInterval(uint64_t) override { return 1; }
  };

  thread_local PoissonSampler sampler{[] -> uint64_t { return 1; },
                                      std::make_unique<SampleIntervalGenerator>()};
  return sampler;
}

PoissonSampler& GetSamplerThatNeverSamples() {
  class SamplerIntervalGenerator : public PoissonSampler::SampleIntervalGenerator {
   public:
    size_t GetNextSampleInterval(uint64_t) override { return std::numeric_limits<size_t>::max(); }
  };
  thread_local PoissonSampler sampler{[] -> uint64_t { return 1; },
                                      std::make_unique<SamplerIntervalGenerator>()};
  return sampler;
}

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
// Recorder consulted by `GetSamplerThatFollowsBackpressure`. Tests that
// exercise backpressure install the recorder under test for their duration
// using `ScopedBackpressureRecorder`.
thread_local Recorder* g_backpressure_recorder = nullptr;

// Installs a recorder for `GetSamplerThatFollowsBackpressure` to consult, and
// uninstalls it on scope exit. Scoped rather than a bare assignment because the
// sampler outlives any single test: a fatal assertion returning early must not
// leave a dangling pointer behind for the next one.
class ScopedBackpressureRecorder {
 public:
  explicit ScopedBackpressureRecorder(Recorder* recorder) { g_backpressure_recorder = recorder; }
  ~ScopedBackpressureRecorder() { g_backpressure_recorder = nullptr; }

  ScopedBackpressureRecorder(const ScopedBackpressureRecorder&) = delete;
  ScopedBackpressureRecorder& operator=(const ScopedBackpressureRecorder&) = delete;
};

// A sampler that samples every allocation, like `GetSamplerThatAlwaysSamples`,
// but whose mean interval tracks the backpressure of `g_backpressure_recorder`
// the way the production sampler tracks the singleton. The fixed-interval
// samplers above cannot express this, so a test using them would never see the
// adapted interval reach a datagram.
PoissonSampler& GetSamplerThatFollowsBackpressure() {
  class SampleIntervalGenerator : public PoissonSampler::SampleIntervalGenerator {
   public:
    size_t GetNextSampleInterval(uint64_t) override { return 1; }
  };

  thread_local PoissonSampler sampler{[] {
                                        if (g_backpressure_recorder == nullptr) {
                                          return Recorder::kDefaultSamplingIntervalBytes;
                                        }
                                        g_backpressure_recorder->ApplyBackpressure();
                                        return g_backpressure_recorder->GetSamplingIntervalBytes();
                                      },
                                      std::make_unique<SampleIntervalGenerator>()};
  return sampler;
}

TEST(RecorderTest, MaybeRecordAllocation) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  async_dispatcher_t* dispatcher = loop.dispatcher();
  auto endpoints = fidl::CreateEndpoints<fuchsia_memory_sampler::Sampler>();
  SamplerImpl sampler{dispatcher, std::move(endpoints->server)};

  auto recorder = memory_sampler::Recorder::CreateRecorderForTesting(
      fidl::SyncClient{std::move(endpoints->client)}, GetSamplerThatAlwaysSamples);
  recorder.MaybeRecordAllocation(kTestAddress, kTestSize);

  loop.RunUntilIdle();

  ASSERT_TRUE(sampler.socket().is_valid());
  auto datagram = ReadDatagram(sampler.socket());
  VerifyAllocationDatagram(datagram, kTestAddress, kTestSize);
}

TEST(RecorderTest, ForgetAllocation) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  async_dispatcher_t* dispatcher = loop.dispatcher();
  auto endpoints = fidl::CreateEndpoints<fuchsia_memory_sampler::Sampler>();
  SamplerImpl sampler{dispatcher, std::move(endpoints->server)};

  auto recorder = memory_sampler::Recorder::CreateRecorderForTesting(
      fidl::SyncClient{std::move(endpoints->client)}, GetSamplerThatAlwaysSamples);
  recorder.MaybeRecordAllocation(kTestAddress, kTestSize);
  recorder.MaybeForgetAllocation(kTestAddress);

  loop.RunUntilIdle();

  ASSERT_TRUE(sampler.socket().is_valid());
  auto alloc_dg = ReadDatagram(sampler.socket());
  VerifyAllocationDatagram(alloc_dg, kTestAddress, kTestSize);

  auto dealloc_dg = ReadDatagram(sampler.socket());
  VerifyDeallocationDatagram(dealloc_dg, kTestAddress);
}
#endif

TEST(RecorderTest, SetModulesInfo) {
  static constexpr size_t kMeaningfulModuleMapLength = 1U;
  static constexpr size_t kMeaningfulExecutableSegmentsLength = 1U;
  static char process_name[ZX_MAX_NAME_LEN];
  {
    const zx_handle_t process = zx_process_self();
    zx_object_get_property(process, ZX_PROP_NAME, process_name, ZX_MAX_NAME_LEN);
  }

  // Sampler server that verifies the expected process info was
  // communicated.
  class Sampler : public SamplerImpl {
   public:
    using SamplerImpl::SamplerImpl;
    void SetProcessInfo(fuchsia_memory_sampler::wire::SamplerSetProcessInfoRequest* request,
                        SetProcessInfoCompleter::Sync& completer) override {
      called_ = true;
      EXPECT_EQ(std::string_view{process_name}, request->process_name().get());

      EXPECT_LE(kMeaningfulModuleMapLength, request->module_map().size());

      auto& module_map = request->module_map()[0];
      EXPECT_GE(static_cast<size_t>(fuchsia_memory_sampler::kBuildIdBytes),
                module_map.build_id().size());
      EXPECT_LE(kMeaningfulExecutableSegmentsLength, module_map.executable_segments().size());

      auto& segment = module_map.executable_segments()[0];
      EXPECT_NE(0U, segment.start_address());
      EXPECT_NE(0U, segment.relative_address());
      EXPECT_NE(0U, segment.size());
    }
    ~Sampler() override { EXPECT_TRUE(called_); }

   private:
    bool called_ = false;
  };

  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  async_dispatcher_t* dispatcher = loop.dispatcher();
  auto endpoints = fidl::CreateEndpoints<fuchsia_memory_sampler::Sampler>();
  Sampler sampler{dispatcher, std::move(endpoints->server)};

  auto recorder = memory_sampler::Recorder::CreateRecorderForTesting(
      fidl::SyncClient{std::move(endpoints->client)}, GetSamplerThatAlwaysSamples);
  recorder.SetModulesInfo();

  loop.RunUntilIdle();
}

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
TEST(RecorderTest, SampledAllocationCausesSampledDeallocation) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  async_dispatcher_t* dispatcher = loop.dispatcher();
  auto endpoints = fidl::CreateEndpoints<fuchsia_memory_sampler::Sampler>();
  SamplerImpl sampler{dispatcher, std::move(endpoints->server)};

  auto recorder = memory_sampler::Recorder::CreateRecorderForTesting(
      fidl::SyncClient{std::move(endpoints->client)}, GetSamplerThatAlwaysSamples);

  recorder.MaybeRecordAllocation(kTestAddress, kTestSize);
  recorder.MaybeForgetAllocation(kTestAddress);
  loop.RunUntilIdle();

  ASSERT_TRUE(sampler.socket().is_valid());
  auto alloc_dg = ReadDatagram(sampler.socket());
  VerifyAllocationDatagram(alloc_dg, kTestAddress, kTestSize);

  auto dealloc_dg = ReadDatagram(sampler.socket());
  VerifyDeallocationDatagram(dealloc_dg, kTestAddress);
}

TEST(RecorderTest, MaybeForgetAllocationIsNoOpIfAllocationWasNotSampled) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  async_dispatcher_t* dispatcher = loop.dispatcher();
  auto endpoints = fidl::CreateEndpoints<fuchsia_memory_sampler::Sampler>();
  SamplerImpl sampler{dispatcher, std::move(endpoints->server)};

  auto recorder = memory_sampler::Recorder::CreateRecorderForTesting(
      fidl::SyncClient{std::move(endpoints->client)}, GetSamplerThatNeverSamples);

  // Initial forget while having never recorded.
  recorder.MaybeForgetAllocation(kTestAddress);
  // Does not record.
  recorder.MaybeRecordAllocation(kTestAddress, kTestSize);
  // Forget after maybe recording, but actually not recording.
  recorder.MaybeForgetAllocation(kTestAddress);
  loop.RunUntilIdle();

  ASSERT_TRUE(sampler.socket().is_valid());
  auto dg = ReadDatagram(sampler.socket());
  EXPECT_TRUE(dg.empty());
}
#endif

TEST(RecorderTest, MaybeRecordAllocationFidlFallback) {
  static constexpr size_t kMeaningfulStackTraceLength = 1U;

  // Sampler server that verifies the expected allocation was recorded via FIDL.
  class Sampler : public SamplerImpl {
   public:
    using SamplerImpl::SamplerImpl;
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
    void RecordAllocation(fuchsia_memory_sampler::wire::RecordAllocationEvent* request,
                          RecordAllocationCompleter::Sync& completer) override {
      called_ = true;
      EXPECT_EQ(reinterpret_cast<uint64_t>(kTestAddress), request->address());
      EXPECT_EQ(kTestSize, request->size());
      EXPECT_LE(kMeaningfulStackTraceLength, request->stack_trace().stack_frames().size());
    }
#else
    void RecordAllocation(fuchsia_memory_sampler::wire::SamplerRecordAllocationRequest* request,
                          RecordAllocationCompleter::Sync& completer) override {
      called_ = true;
      EXPECT_EQ(reinterpret_cast<uint64_t>(kTestAddress), request->address());
      EXPECT_EQ(kTestSize, request->size());
      EXPECT_LE(kMeaningfulStackTraceLength, request->stack_trace().stack_frames().size());
    }
#endif
    ~Sampler() override { EXPECT_TRUE(called_); }

   private:
    bool called_ = false;
  };

  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  async_dispatcher_t* dispatcher = loop.dispatcher();
  auto endpoints = fidl::CreateEndpoints<fuchsia_memory_sampler::Sampler>();
  Sampler sampler{dispatcher, std::move(endpoints->server)};

  auto recorder = memory_sampler::Recorder::CreateRecorderForTesting(
      fidl::SyncClient{std::move(endpoints->client)}, GetSamplerThatAlwaysSamples,
      /*use_socket=*/false);
  recorder.MaybeRecordAllocation(kTestAddress, kTestSize);

  loop.RunUntilIdle();
}

TEST(RecorderTest, ForgetAllocationFidlFallback) {
  static constexpr size_t kMeaningfulStackTraceLength = 1U;

  // Sampler server that verifies the expected deallocation was recorded via FIDL.
  class Sampler : public SamplerImpl {
   public:
    using SamplerImpl::SamplerImpl;
#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
    void RecordAllocation(fuchsia_memory_sampler::wire::RecordAllocationEvent* request,
                          RecordAllocationCompleter::Sync& completer) override {}
    void RecordDeallocation(fuchsia_memory_sampler::wire::RecordDeallocationEvent* request,
                            RecordDeallocationCompleter::Sync& completer) override {
      called_ = true;
      EXPECT_EQ(reinterpret_cast<uint64_t>(kTestAddress), request->address());
      EXPECT_LE(kMeaningfulStackTraceLength, request->stack_trace().stack_frames().size());
    }
#else
    void RecordAllocation(fuchsia_memory_sampler::wire::SamplerRecordAllocationRequest* request,
                          RecordAllocationCompleter::Sync& completer) override {}
    void RecordDeallocation(fuchsia_memory_sampler::wire::SamplerRecordDeallocationRequest* request,
                            RecordDeallocationCompleter::Sync& completer) override {
      called_ = true;
      EXPECT_EQ(reinterpret_cast<uint64_t>(kTestAddress), request->address());
      EXPECT_LE(kMeaningfulStackTraceLength, request->stack_trace().stack_frames().size());
    }
#endif
    ~Sampler() override { EXPECT_TRUE(called_); }

   private:
    bool called_ = false;
  };

  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  async_dispatcher_t* dispatcher = loop.dispatcher();
  auto endpoints = fidl::CreateEndpoints<fuchsia_memory_sampler::Sampler>();
  Sampler sampler{dispatcher, std::move(endpoints->server)};

  auto recorder = memory_sampler::Recorder::CreateRecorderForTesting(
      fidl::SyncClient{std::move(endpoints->client)}, GetSamplerThatAlwaysSamples,
      /*use_socket=*/false);
  recorder.MaybeRecordAllocation(kTestAddress, kTestSize);
  recorder.MaybeForgetAllocation(kTestAddress);

  loop.RunUntilIdle();
}

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
TEST(RecorderTest, DisconnectsOnSocketPeerClosed) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  async_dispatcher_t* dispatcher = loop.dispatcher();
  auto endpoints = fidl::CreateEndpoints<fuchsia_memory_sampler::Sampler>();
  SamplerImpl sampler{dispatcher, std::move(endpoints->server)};

  auto recorder = memory_sampler::Recorder::CreateRecorderForTesting(
      fidl::SyncClient{std::move(endpoints->client)}, GetSamplerThatAlwaysSamples);

  loop.RunUntilIdle();
  ASSERT_TRUE(sampler.socket().is_valid());
  EXPECT_FALSE(recorder.is_disabled());

  // Close the server end of the socket.
  sampler.socket().reset();

  // Next allocation triggers socket write, detects ZX_ERR_PEER_CLOSED, and disconnects.
  recorder.MaybeRecordAllocation(kTestAddress, kTestSize);
  EXPECT_TRUE(recorder.is_disabled());

  // Subsequent allocation/deallocation calls should be no-ops and not crash.
  recorder.MaybeRecordAllocation(kTestAddress, kTestSize);
  recorder.MaybeForgetAllocation(kTestAddress);
  EXPECT_TRUE(recorder.is_disabled());
}

TEST(RecorderTest, SignalsPeerAndDropsAllocationOnBufferFull) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  async_dispatcher_t* dispatcher = loop.dispatcher();
  auto endpoints = fidl::CreateEndpoints<fuchsia_memory_sampler::Sampler>();
  SamplerImpl sampler{dispatcher, std::move(endpoints->server)};

  auto recorder = memory_sampler::Recorder::CreateRecorderForTesting(
      fidl::SyncClient{std::move(endpoints->client)}, GetSamplerThatAlwaysSamples);

  loop.RunUntilIdle();
  ASSERT_TRUE(sampler.socket().is_valid());

  // Fill the socket buffer until writes saturate.
  void* overflow_address = nullptr;
  zx_signals_t signals = 0;
  for (uintptr_t i = 1; i <= 10000; ++i) {
    // NOLINTNEXTLINE(performance-no-int-to-ptr)
    void* addr = reinterpret_cast<void*>(0x10000 + i);
    recorder.MaybeRecordAllocation(addr, kTestSize);
    if (sampler.socket().wait_one(ZX_USER_SIGNAL_0, zx::time::infinite_past(), &signals) == ZX_OK &&
        (signals & ZX_USER_SIGNAL_0)) {
      overflow_address = addr;
      break;
    }
  }

  ASSERT_NE(overflow_address, nullptr);
  EXPECT_FALSE(recorder.is_disabled());

  // Drain the socket so it becomes writable again.
  while (!ReadDatagram(sampler.socket()).empty()) {
  }

  // Deallocating the overflow address should NOT send any datagram, because it was
  // dropped and never tracked in recorded_allocations_.
  recorder.MaybeForgetAllocation(overflow_address);
  EXPECT_TRUE(ReadDatagram(sampler.socket()).empty());
}

TEST(RecorderTest, SignalsPeerOnDeallocationBufferFull) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  async_dispatcher_t* dispatcher = loop.dispatcher();
  auto endpoints = fidl::CreateEndpoints<fuchsia_memory_sampler::Sampler>();
  SamplerImpl sampler{dispatcher, std::move(endpoints->server)};

  auto recorder = memory_sampler::Recorder::CreateRecorderForTesting(
      fidl::SyncClient{std::move(endpoints->client)}, GetSamplerThatAlwaysSamples);

  loop.RunUntilIdle();
  ASSERT_TRUE(sampler.socket().is_valid());

  // Record tracked allocations while the socket has space.
  std::vector<void*> tracked_addresses;
  for (uintptr_t i = 1; i <= 50; ++i) {
    // NOLINTNEXTLINE(performance-no-int-to-ptr)
    void* addr = reinterpret_cast<void*>(0x5000 + (i * 8));
    recorder.MaybeRecordAllocation(addr, kTestSize);
    tracked_addresses.push_back(addr);
  }

  // Fill the socket buffer with other allocations until full.
  bool buffer_saturated = false;
  for (uintptr_t i = 1; i <= 10000; ++i) {
    // NOLINTNEXTLINE(performance-no-int-to-ptr)
    recorder.MaybeRecordAllocation(reinterpret_cast<void*>(0x20000 + i), kTestSize);
    zx_signals_t fill_sigs = 0;
    if (sampler.socket().wait_one(ZX_USER_SIGNAL_0, zx::time::infinite_past(), &fill_sigs) ==
            ZX_OK &&
        (fill_sigs & ZX_USER_SIGNAL_0)) {
      buffer_saturated = true;
      break;
    }
  }
  ASSERT_TRUE(buffer_saturated);

  // Clear the signal on the server end and reset the client latch so we can verify that
  // deallocation asserts it.
  ASSERT_EQ(sampler.socket().signal(ZX_USER_SIGNAL_0, 0), ZX_OK);
  RecorderTestPeer::ResetPeerSignaled(recorder);
  zx_signals_t sigs = 0;
  EXPECT_NE(sampler.socket().wait_one(ZX_USER_SIGNAL_0, zx::time::infinite_past(), &sigs), ZX_OK);

  // Now deallocate tracked addresses until saturation is hit on deallocation.
  // Because deallocation datagrams are slightly smaller than allocation datagrams,
  // the first deallocation may fit in the leftover buffer slack, but subsequent
  // deallocations will exhaust the slack and assert ZX_USER_SIGNAL_0.
  bool deallocation_signaled = false;
  for (void* addr : tracked_addresses) {
    recorder.MaybeForgetAllocation(addr);
    if (sampler.socket().wait_one(ZX_USER_SIGNAL_0, zx::time::infinite_past(), &sigs) == ZX_OK &&
        (sigs & ZX_USER_SIGNAL_0)) {
      deallocation_signaled = true;
      break;
    }
  }

  EXPECT_TRUE(deallocation_signaled);
  EXPECT_FALSE(recorder.is_disabled());
}
#endif

TEST(RecorderTest, DisconnectsOnFidlPeerClosed) {
  auto endpoints = fidl::CreateEndpoints<fuchsia_memory_sampler::Sampler>();
  // Close the server end of the FIDL channel immediately.
  endpoints->server.reset();

  auto recorder = memory_sampler::Recorder::CreateRecorderForTesting(
      fidl::SyncClient{std::move(endpoints->client)}, GetSamplerThatAlwaysSamples,
      /*use_socket=*/false);

  EXPECT_FALSE(recorder.is_disabled());

  // Next allocation triggers fallback FIDL call, detects channel error, and disconnects without
  // asserting.
  recorder.MaybeRecordAllocation(kTestAddress, kTestSize);
  EXPECT_TRUE(recorder.is_disabled());

  // Subsequent allocation/deallocation calls should be no-ops and not crash.
  recorder.MaybeRecordAllocation(kTestAddress, kTestSize);
  recorder.MaybeForgetAllocation(kTestAddress);
  EXPECT_TRUE(recorder.is_disabled());
}

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
// Tests that socket fullness detection via `zx_info_socket_t` functions as expected
// under `ZX_SOCKET_DATAGRAM`.
//
// This test verifies that:
// 1. An empty socket reports `tx_buf_size == 0` with a non-zero `tx_buf_max`.
// 2. Writing datagrams dynamically increases `tx_buf_size` by the exact payload size written.
// 3. Reading datagrams from the peer dynamically decreases `tx_buf_size`.
// 4. When all datagrams are consumed, `tx_buf_size` returns to 0.
// 5. As the buffer fills towards capacity, `tx_buf_size / tx_buf_max` reflects the ratio
//    of buffered data, and when full, writes fail with `ZX_ERR_SHOULD_WAIT`.
TEST(RecorderTest, SocketFullnessDetectionUnderDatagramMode) {
  zx::socket writer, reader;
  ASSERT_EQ(zx::socket::create(ZX_SOCKET_DATAGRAM, &writer, &reader), ZX_OK);

  zx_info_socket_t info;
  ASSERT_EQ(writer.get_info(ZX_INFO_SOCKET, &info, sizeof(info), nullptr, nullptr), ZX_OK);
  EXPECT_GT(info.tx_buf_max, 0u);
  EXPECT_EQ(info.tx_buf_size, 0u);

  // Write a 1024-byte datagram.
  std::vector<uint8_t> payload(1024, 0x42);
  size_t actual = 0;
  ASSERT_EQ(writer.write(0, payload.data(), payload.size(), &actual), ZX_OK);
  EXPECT_EQ(actual, 1024u);

  ASSERT_EQ(writer.get_info(ZX_INFO_SOCKET, &info, sizeof(info), nullptr, nullptr), ZX_OK);
  EXPECT_EQ(info.tx_buf_size, 1024u);

  // Write another 2048-byte datagram.
  std::vector<uint8_t> payload2(2048, 0x43);
  ASSERT_EQ(writer.write(0, payload2.data(), payload2.size(), &actual), ZX_OK);
  EXPECT_EQ(actual, 2048u);

  ASSERT_EQ(writer.get_info(ZX_INFO_SOCKET, &info, sizeof(info), nullptr, nullptr), ZX_OK);
  EXPECT_EQ(info.tx_buf_size, 1024u + 2048u);

  // Read the first datagram from the reader endpoint.
  std::vector<uint8_t> read_buf(4096);
  ASSERT_EQ(reader.read(0, read_buf.data(), read_buf.size(), &actual), ZX_OK);
  EXPECT_EQ(actual, 1024u);

  ASSERT_EQ(writer.get_info(ZX_INFO_SOCKET, &info, sizeof(info), nullptr, nullptr), ZX_OK);
  EXPECT_EQ(info.tx_buf_size, 2048u);

  // Read the second datagram.
  ASSERT_EQ(reader.read(0, read_buf.data(), read_buf.size(), &actual), ZX_OK);
  EXPECT_EQ(actual, 2048u);

  ASSERT_EQ(writer.get_info(ZX_INFO_SOCKET, &info, sizeof(info), nullptr, nullptr), ZX_OK);
  EXPECT_EQ(info.tx_buf_size, 0u);

  // Fill up the socket buffer to test high occupancy and boundary behavior.
  const size_t chunk_size = 4096;
  std::vector<uint8_t> chunk(chunk_size, 0xAA);
  size_t total_written = 0;
  while (true) {
    zx_status_t status = writer.write(0, chunk.data(), chunk.size(), &actual);
    if (status == ZX_ERR_SHOULD_WAIT) {
      break;
    }
    ASSERT_EQ(status, ZX_OK);
    total_written += actual;
  }

  ASSERT_EQ(writer.get_info(ZX_INFO_SOCKET, &info, sizeof(info), nullptr, nullptr), ZX_OK);
  EXPECT_EQ(info.tx_buf_size, total_written);
  double usage = static_cast<double>(info.tx_buf_size) / static_cast<double>(info.tx_buf_max);
  // Transmit buffer should be close to or at capacity.
  EXPECT_GT(usage, 0.85);
  EXPECT_LE(usage, 1.0);
}

// Tests that Recorder adapts its sampling interval based on the socket's occupancy.
TEST(RecorderTest, DynamicBackpressureAdaptsToSocketOccupancy) {
  async::Loop loop(&kAsyncLoopConfigNeverAttachToThread);
  async_dispatcher_t* dispatcher = loop.dispatcher();
  auto endpoints = fidl::CreateEndpoints<fuchsia_memory_sampler::Sampler>();
  SamplerImpl sampler{dispatcher, std::move(endpoints->server)};

  auto recorder = memory_sampler::Recorder::CreateRecorderForTesting(
      fidl::SyncClient{std::move(endpoints->client)}, GetSamplerThatFollowsBackpressure);
  ScopedBackpressureRecorder scoped_recorder{&recorder};

  loop.RunUntilIdle();
  ASSERT_TRUE(sampler.socket().is_valid());

  // Initially, socket is empty: sampling interval should be the default.
  EXPECT_EQ(recorder.GetSamplingIntervalBytes(),
            memory_sampler::Recorder::kDefaultSamplingIntervalBytes);

  // Fill the socket by recording allocations without reading them on the server side.
  // Record allocations until the sampling interval increases to the maximum backpressure (8 MiB).
  for (size_t i = 0; i < 2000; ++i) {
    // NOLINTNEXTLINE(performance-no-int-to-ptr)
    recorder.MaybeRecordAllocation(reinterpret_cast<void*>(0x2000 + (i * 8)), 100);
    if (recorder.GetSamplingIntervalBytes() >= static_cast<uint64_t>(8 * 1024 * 1024)) {
      break;
    }
  }

  EXPECT_EQ(recorder.GetSamplingIntervalBytes(), 8 * 1024 * 1024u);

  // Drain the socket from the server side, tracking the intervals that actually
  // reached the wire. Each datagram reports the interval that was in effect
  // when its sampling threshold was drawn, so the buffered datagrams should
  // show the interval climbing above the default as the socket filled up.
  std::vector<uint8_t> drain_buf(65536);
  size_t actual = 0;
  uint64_t max_interval_on_wire = 0;
  while (sampler.socket().read(0, drain_buf.data(), drain_buf.size(), &actual) == ZX_OK) {
    auto unpersisted = fidl::InplaceUnpersist<fuchsia_memory_sampler::wire::SamplerDatagram>(
        std::span<uint8_t>(drain_buf.data(), actual));
    ASSERT_TRUE(unpersisted.is_ok());
    auto& datagram = unpersisted.value();
    ASSERT_TRUE(datagram->is_record_allocation());
    max_interval_on_wire =
        std::max(max_interval_on_wire, datagram->record_allocation().sampling_interval_bytes());
  }
  EXPECT_GT(max_interval_on_wire, memory_sampler::Recorder::kDefaultSamplingIntervalBytes);

  // Once drained, the next backpressure evaluation should drop back to the default.
  recorder.ApplyBackpressure();
  EXPECT_EQ(recorder.GetSamplingIntervalBytes(),
            memory_sampler::Recorder::kDefaultSamplingIntervalBytes);
}
#endif

}  // namespace
}  // namespace memory_sampler
