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

#include <cstdint>
#include <cstring>
#include <iostream>
#include <limits>
#include <string_view>
#include <vector>

#include <gtest/gtest.h>
#include <src/lib/testing/loop_fixture/test_loop_fixture.h>
#include <src/performance/memory/sampler/instrumentation/recorder.h>

namespace memory_sampler {
namespace {
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
                              size_t expected_size) {
  auto unpersisted = fidl::InplaceUnpersist<fuchsia_memory_sampler::wire::SamplerDatagram>(
      cpp20::span<uint8_t>(const_cast<uint8_t*>(datagram.data()), datagram.size()));
  ASSERT_TRUE(unpersisted.is_ok());
  auto& dg = unpersisted.value();
  EXPECT_TRUE(dg->is_record_allocation());
  auto& header = dg->record_allocation();
  EXPECT_EQ(header.address(), reinterpret_cast<uint64_t>(expected_address));
  EXPECT_EQ(header.size(), expected_size);
  EXPECT_GE(header.stack_trace().stack_frames().size(), 1U);
}

void VerifyDeallocationDatagram(const std::vector<uint8_t>& datagram, void* expected_address) {
  auto unpersisted = fidl::InplaceUnpersist<fuchsia_memory_sampler::wire::SamplerDatagram>(
      cpp20::span<uint8_t>(const_cast<uint8_t*>(datagram.data()), datagram.size()));
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
    size_t GetNextSampleInterval(size_t) override { return 1; }
  };

  static PoissonSampler sampler{1, std::make_unique<SampleIntervalGenerator>()};
  return sampler;
}

PoissonSampler& GetSamplerThatNeverSamples() {
  class SamplerIntervalGenerator : public PoissonSampler::SampleIntervalGenerator {
   public:
    size_t GetNextSampleInterval(size_t) override { return std::numeric_limits<size_t>::max(); }
  };
  static PoissonSampler sampler{1, std::make_unique<SamplerIntervalGenerator>()};
  return sampler;
}

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
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

}  // namespace
}  // namespace memory_sampler
