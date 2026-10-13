// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fake_codec_adapter.h>
#include <fuchsia/media/cpp/fidl.h>
#include <fuchsia/mediacodec/cpp/fidl.h>
#include <fuchsia/sysmem/cpp/fidl.h>
#include <fuchsia/sysmem/cpp/fidl_test_base.h>
#include <fuchsia/sysmem2/cpp/fidl_test_base.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/sync/cpp/completion.h>

#include <atomic>
#include <thread>

#include <gtest/gtest.h>

#include "lib/media/codec_impl/codec_impl.h"
#include "src/lib/testing/loop_fixture/real_loop_fixture.h"

class CodecPacketForTest {
 public:
  static bool IsQueued(const CodecPacket* packet) { return packet->is_queued(); }
};

namespace {

constexpr uint32_t kInputMinBufferCountForCamping = 3;
constexpr uint64_t kInputBufferSize = 4096;
constexpr uint32_t kOutputBufferCount = 5;
constexpr uint64_t kOutputBufferSize = 256ULL * 144 * 4;
constexpr uint32_t kTestPacketValidLengthBytes = 16;
constexpr zx::duration kWaitTimeout = zx::sec(30);

void PopulateTestBufferSettings(fuchsia::sysmem2::BufferMemorySettings* buffer_settings,
                                uint64_t size_bytes) {
  buffer_settings->set_size_bytes(size_bytes);
  buffer_settings->set_raw_vmo_size(size_bytes);
  buffer_settings->set_is_physically_contiguous(false);
  buffer_settings->set_is_secure(false);
  buffer_settings->set_coherency_domain(fuchsia::sysmem2::CoherencyDomain::CPU);
  buffer_settings->mutable_heap()->set_heap_type("fuchsia.sysmem.HeapType.SYSTEM_RAM").set_id(0);
}

auto CreateDecoderParams() {
  fuchsia::mediacodec::CreateDecoder_Params params;

  params.mutable_input_details()->set_format_details_version_ordinal(0);
  return params;
}

auto CreateStreamBufferPartialSettings(
    uint64_t buffer_lifetime_ordinal, const fuchsia::media::StreamBufferConstraints& constraints,
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token) {
  fuchsia::media::StreamBufferPartialSettings settings;
  settings.set_buffer_lifetime_ordinal(buffer_lifetime_ordinal)
      .set_buffer_constraints_version_ordinal(constraints.buffer_constraints_version_ordinal())
      .set_sysmem_token(std::move(token));
  return settings;
}

auto CreateValidInputBufferCollectionConstraints() {
  fuchsia_sysmem2::BufferCollectionConstraints result;
  result.usage().emplace().cpu() =
      fuchsia::sysmem::cpuUsageRead | fuchsia::sysmem::cpuUsageReadOften;
  result.min_buffer_count_for_camping() = kInputMinBufferCountForCamping;
  // Must emplace the buffer_memory_constraints here, as enforced by CodecImpl.  Leaving all
  // buffer_memory_constraints fields default is fine.
  result.buffer_memory_constraints().emplace();
  return result;
}

auto CreateValidOutputBufferCollectionConstraints() {
  fuchsia_sysmem2::BufferCollectionConstraints result;
  result.usage().emplace().cpu() =
      fuchsia::sysmem::cpuUsageWrite | fuchsia::sysmem::cpuUsageWriteOften;
  result.min_buffer_count_for_camping() = kOutputBufferCount;
  auto& bmc = result.buffer_memory_constraints().emplace();
  bmc.min_size_bytes() = kOutputBufferSize;
  bmc.cpu_domain_supported() = true;
  return result;
}

auto CreateInputPacket() {
  fuchsia::media::Packet input_packet;
  input_packet.mutable_header()->set_buffer_lifetime_ordinal(1).set_packet_index(0);
  input_packet.set_buffer_index(0)
      .set_stream_lifetime_ordinal(1)
      .set_start_offset(0)
      .set_valid_length_bytes(kTestPacketValidLengthBytes);
  return input_packet;
}

class TestBufferCollection : public fuchsia::sysmem2::testing::BufferCollection_TestBase {
 public:
  TestBufferCollection() : binding_(this) {}

  void Bind(fidl::InterfaceRequest<fuchsia::sysmem2::BufferCollection> request) {
    binding_.Bind(std::move(request));
  }
  void NotImplemented_(const std::string& name) override {}

  void WaitForAllBuffersAllocated(WaitForAllBuffersAllocatedCallback callback) override {
    wait_callback_ = std::move(callback);
  }
  void CheckAllBuffersAllocated(CheckAllBuffersAllocatedCallback callback) override {
    fuchsia::sysmem2::BufferCollection_CheckAllBuffersAllocated_Result result;
    result.set_response({});
    callback(std::move(result));
  }
  void FailAllocation() {
    WaitForAllBuffersAllocatedCallback callback;
    callback.swap(wait_callback_);

    fuchsia::sysmem2::BufferCollection_WaitForAllBuffersAllocated_Result result;
    result.set_err(fuchsia::sysmem2::Error::CONSTRAINTS_INTERSECTION_EMPTY);
    callback(std::move(result));
  }
  void CompleteAllocation(uint32_t buffer_count, uint64_t size_bytes) {
    WaitForAllBuffersAllocatedCallback callback;
    callback.swap(wait_callback_);

    fuchsia::sysmem2::BufferCollectionInfo info;
    PopulateTestBufferSettings(info.mutable_settings()->mutable_buffer_settings(), size_bytes);
    for (uint32_t i = 0; i < buffer_count; ++i) {
      zx::vmo vmo;
      ZX_ASSERT(zx::vmo::create(size_bytes, 0, &vmo) == ZX_OK);
      fuchsia::sysmem2::VmoBuffer vmo_buffer;
      vmo_buffer.set_vmo(std::move(vmo));
      vmo_buffer.set_vmo_usable_start(0);
      info.mutable_buffers()->push_back(std::move(vmo_buffer));
    }

    fuchsia::sysmem2::BufferCollection_WaitForAllBuffersAllocated_Response response;
    response.set_buffer_collection_info(std::move(info));
    fuchsia::sysmem2::BufferCollection_WaitForAllBuffersAllocated_Result result;
    result.set_response(std::move(response));
    callback(std::move(result));
  }

  bool is_waiting() const { return !!wait_callback_; }

 private:
  fidl::Binding<fuchsia::sysmem2::BufferCollection> binding_;
  WaitForAllBuffersAllocatedCallback wait_callback_;
};

class TestAllocator : public fuchsia::sysmem2::testing::Allocator_TestBase {
 public:
  TestAllocator() : binding_(this) {}

  void Bind(fidl::InterfaceRequest<fuchsia::sysmem2::Allocator> request) {
    binding_.Bind(std::move(request));
  }
  void BindSharedCollection(
      ::fuchsia::sysmem2::AllocatorBindSharedCollectionRequest request) override {
    auto col = std::make_unique<TestBufferCollection>();
    col->Bind(std::move(*request.mutable_buffer_collection_request()));
    collections_.push_back(std::move(col));
  }

  void SetDebugClientInfo(::fuchsia::sysmem2::AllocatorSetDebugClientInfoRequest request) override {
  }

  void GetVmoInfo(::fuchsia::sysmem2::AllocatorGetVmoInfoRequest request,
                  GetVmoInfoCallback callback) override {
    uint64_t vmo_size = 0;
    ZX_ASSERT(request.vmo().get_size(&vmo_size) == ZX_OK);
    fuchsia::sysmem2::Allocator_GetVmoInfo_Response response;
    response.set_buffer_collection_id(1);
    response.set_buffer_index(0);
    response.set_constraints_ok(true);
    if (request.has_vmo_settings_to_check()) {
      response.set_vmo_settings_match(true);
    }
    PopulateTestBufferSettings(response.mutable_single_buffer_settings()->mutable_buffer_settings(),
                               vmo_size);
    callback(fuchsia::sysmem2::Allocator_GetVmoInfo_Result::WithResponse(std::move(response)));
  }

  void NotImplemented_(const std::string& name) override {
    // Unexpected.
    ZX_PANIC("NotImplemented_(): %s", name.c_str());
  }

  TestBufferCollection& collection(size_t index = 0) { return *collections_.at(index); }
  size_t collection_count() const { return collections_.size(); }

 private:
  fidl::Binding<fuchsia::sysmem2::Allocator> binding_;

  std::vector<std::unique_ptr<TestBufferCollection>> collections_;
};

class CodecImplFailures : public gtest::RealLoopFixture {
 public:
  using StreamProcessorPtr = ::fuchsia::media::StreamProcessorPtr;

  void TearDown() override { token_request_ = nullptr; }

  virtual std::unique_ptr<FakeCodecAdapter> CreateCodecAdapter() {
    auto codec_adapter = std::make_unique<FakeCodecAdapter>(codec_impl_->lock(), codec_impl_.get());
    codec_adapter_ = codec_adapter.get();
    return codec_adapter;
  }

  void Create(fidl::InterfaceRequest<fuchsia::media::StreamProcessor> request,
              fit::function<void(FakeCodecAdapter*)> configure_adapter = nullptr) {
    fidl::InterfaceHandle<fuchsia::sysmem2::Allocator> sysmem;
    sysmem_request_ = sysmem.NewRequest();

    codec_impl_ = std::make_unique<CodecImpl>(
        fidl::ClientEnd<fuchsia_sysmem2::Allocator>(sysmem.TakeChannel()), nullptr, dispatcher(),
        thrd_current(), CreateDecoderParams(), std::move(request));

    auto codec_adapter = CreateCodecAdapter();
    if (configure_adapter) {
      configure_adapter(codec_adapter_);
    }
    codec_impl_->SetCoreCodecAdapter(std::move(codec_adapter));

    codec_impl_->BindAsync([this] {
      error_handler_ran_ = true;
      codec_adapter_ = nullptr;
      codec_impl_ = nullptr;
    });
  }

 protected:
  static bool IsPacketQueued(const CodecPacket* packet) {
    return CodecPacketForTest::IsQueued(packet);
  }

  uint64_t GetSharedFidlForStreamWaitCount() {
    return codec_impl_->GetSharedFidlForStreamWaitCountForTesting();
  }

  void WaitForSharedFidlForStreamWaitCount(uint64_t expected_count) {
    codec_impl_->WaitForSharedFidlForStreamWaitCountForTesting(expected_count);
  }

  void EmitOutputPacketFromCoreCodecThread(CodecPacket* packet, const CodecBuffer* buffer,
                                           std::atomic<bool>* emitted_flag = nullptr,
                                           zx::vmo* child_vmo_to_reset = nullptr) {
    std::thread([this, packet, buffer, emitted_flag, child_vmo_to_reset] {
      packet->SetBuffer(buffer);
      packet->SetStartOffset(0);
      packet->SetValidLengthBytes(kTestPacketValidLengthBytes);
      if (emitted_flag) {
        *emitted_flag = true;
      }
      static_cast<CodecAdapterEvents*>(codec_impl_.get())
          ->onCoreCodecOutputPacket(packet, false, false);
      if (child_vmo_to_reset) {
        child_vmo_to_reset->reset();
      }
    }).join();
  }

  void WaitForAndCompleteCollectionAllocation(TestAllocator& allocator, size_t collection_index,
                                              uint32_t buffer_count, uint64_t buffer_size) {
    ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
        [&allocator, collection_index, this] {
          return error_handler_ran_ || (allocator.collection_count() > collection_index &&
                                        allocator.collection(collection_index).is_waiting());
        },
        kWaitTimeout));
    ASSERT_FALSE(error_handler_ran_);
    allocator.collection(collection_index).CompleteAllocation(buffer_count, buffer_size);
  }

  // Just cache this request so that we can have a valid sysmem handle
  std::optional<fidl::InterfaceRequest<fuchsia::sysmem2::Allocator>> sysmem_request_;
  std::optional<fidl::InterfaceRequest<fuchsia::sysmem::BufferCollectionToken>> token_request_;

  bool error_handler_ran_ = false;
  std::unique_ptr<CodecImpl> codec_impl_;
  FakeCodecAdapter* codec_adapter_;
};

TEST_F(CodecImplFailures, InputBufferCollectionConstraintsCpuUsage) {
  StreamProcessorPtr processor;

  processor.events().OnInputConstraints = [this, &processor](auto input_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    token_request_ = token.NewRequest();

    auto buffer_collection_constraints = CreateValidInputBufferCollectionConstraints();
    // Setting write usage on input buffers is invalid and will result in codec
    // failure
    buffer_collection_constraints.usage()->cpu() =
        fuchsia::sysmem::cpuUsageWrite | fuchsia::sysmem::cpuUsageWriteOften;
    codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                   std::move(buffer_collection_constraints));

    processor->SetInputBufferPartialSettings(
        CreateStreamBufferPartialSettings(1, input_constraints, std::move(token)));
  };

  Create(processor.NewRequest());

  RunLoopUntil([this]() { return error_handler_ran_; });
  ASSERT_TRUE(error_handler_ran_);
}

TEST_F(CodecImplFailures, InputBufferCollectionConstraintsMinBufferCount) {
  StreamProcessorPtr processor;

  processor.events().OnInputConstraints = [this, &processor](auto input_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    token_request_ = token.NewRequest();

    auto buffer_collection_constraints = CreateValidInputBufferCollectionConstraints();
    // No buffers required for camping would be less than the minimum for the
    // server
    buffer_collection_constraints.min_buffer_count_for_camping() = 0;
    codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                   std::move(buffer_collection_constraints));

    processor->SetInputBufferPartialSettings(
        CreateStreamBufferPartialSettings(1, input_constraints, std::move(token)));
  };

  Create(processor.NewRequest());

  RunLoopUntil([this]() { return error_handler_ran_; });
  ASSERT_TRUE(error_handler_ran_);
}

TEST_F(CodecImplFailures, InputBufferCollectionSysmemFailure) {
  StreamProcessorPtr processor;

  processor.events().OnInputConstraints = [this, &processor](auto input_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    token_request_ = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                   CreateValidInputBufferCollectionConstraints());

    processor->SetInputBufferPartialSettings(
        CreateStreamBufferPartialSettings(1, input_constraints, std::move(token)));
  };

  Create(processor.NewRequest());

  TestAllocator allocator;
  ASSERT_TRUE(sysmem_request_.has_value());
  allocator.Bind(std::move(sysmem_request_.value()));
  sysmem_request_ = nullptr;

  RunLoopUntil([&allocator] {
    return allocator.collection_count() >= 1 && allocator.collection(0).is_waiting();
  });
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_TRUE(allocator.collection(0).is_waiting());

  allocator.collection(0).FailAllocation();

  RunLoopUntil([this] { return error_handler_ran_; });
  ASSERT_TRUE(error_handler_ran_);
}

TEST_F(CodecImplFailures, InputBufferCollectionSysmemFailureDuringDestruction) {
  StreamProcessorPtr processor;

  processor.events().OnInputConstraints = [this, &processor](auto input_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    token_request_ = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                   CreateValidInputBufferCollectionConstraints());

    processor->SetInputBufferPartialSettings(
        CreateStreamBufferPartialSettings(1, input_constraints, std::move(token)));
  };

  Create(processor.NewRequest());

  TestAllocator allocator;
  ASSERT_TRUE(sysmem_request_.has_value());
  allocator.Bind(std::move(sysmem_request_.value()));
  sysmem_request_ = nullptr;

  RunLoopUntil([&allocator] {
    return allocator.collection_count() >= 1 && allocator.collection(0).is_waiting();
  });
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_TRUE(allocator.collection(0).is_waiting());

  // Fail allocation and immediately destroy CodecImpl and close processor client.
  allocator.collection(0).FailAllocation();
  codec_impl_ = nullptr;
  processor = nullptr;

  // Run remaining loop tasks to ensure any pending FIDL error handlers on shared_fidl_thread
  // execute safely without Use-After-Free or misaligned address crashes.
  RunLoopUntilIdle();
}

TEST_F(CodecImplFailures, OutputPacketOnFirstBufferAddInNonDynamicMode) {
  StreamProcessorPtr processor;
  std::optional<fidl::InterfaceRequest<fuchsia::sysmem::BufferCollectionToken>>
      output_token_request;

  std::atomic<bool> output_packet_emitted = false;
  bool client_received_output_packet = false;
  CodecPacket* first_recycled_packet = nullptr;

  processor.events().OnInputConstraints = [this, &processor](auto input_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    token_request_ = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                   CreateValidInputBufferCollectionConstraints());

    processor->SetInputBufferPartialSettings(
        CreateStreamBufferPartialSettings(1, input_constraints, std::move(token)));
  };

  processor.events().OnOutputConstraints = [&](auto output_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    output_token_request = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kOutputPort,
                                                   CreateValidOutputBufferCollectionConstraints());

    processor->SetOutputBufferPartialSettings(CreateStreamBufferPartialSettings(
        1, output_constraints.buffer_constraints(), std::move(token)));
  };

  processor.events().OnOutputPacket = [&](fuchsia::media::Packet packet, bool error_before,
                                          bool error_during) {
    client_received_output_packet = true;
  };

  Create(processor.NewRequest(), [&](FakeCodecAdapter* adapter) {
    adapter->SetSupportsDynamicBuffers(true);

    adapter->SetOnRecycleOutputPacket([&](CodecPacket* packet) {
      if (!first_recycled_packet) {
        first_recycled_packet = packet;
      }
    });

    adapter->SetOnAddBuffer([&](CodecPort port, const CodecBuffer* buffer) {
      if (port == kOutputPort && buffer->index() == 0) {
        ASSERT_NE(first_recycled_packet, nullptr);
        // At this point inside ScopedUnlock of buffer 0 (out of 5):
        // - is_port_configured_[kOutputPort] is false (current_buffers.size() == 1 < 5)
        // - CompleteOutputBufferPartialSettings has not been received yet.
        // Emit an output packet from a core codec thread.
        EmitOutputPacketFromCoreCodecThread(first_recycled_packet, buffer, &output_packet_emitted);
      }
    });
  });

  TestAllocator allocator;
  ASSERT_TRUE(sysmem_request_.has_value());
  allocator.Bind(std::move(sysmem_request_.value()));
  sysmem_request_ = nullptr;

  // Complete input buffer allocation.
  WaitForAndCompleteCollectionAllocation(allocator, 0, kInputMinBufferCountForCamping,
                                         kInputBufferSize);

  // QueueInputPacket starts the stream; since IsCoreCodecRequiringOutputConfigForFormatDetection()
  // is true by default, StartNewStream sends OnOutputConstraints (with PausedOutput) and waits for
  // output configuration.
  processor->QueueInputPacket(CreateInputPacket());

  // Wait for output buffer collection to start waiting, then complete allocation.
  WaitForAndCompleteCollectionAllocation(allocator, 1, kOutputBufferCount, kOutputBufferSize);

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([&] { return error_handler_ran_ || output_packet_emitted; },
                                        kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_TRUE(output_packet_emitted);

  // Verify PausedOutput holds back the output packet prior to CompleteOutputBufferPartialSettings.
  RunLoopUntilIdle();
  ASSERT_FALSE(client_received_output_packet);

  // Now send CompleteOutputBufferPartialSettings so PausedOutput unpauses and delivers the packet.
  processor->CompleteOutputBufferPartialSettings(1);
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] { return error_handler_ran_ || client_received_output_packet; }, kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_TRUE(client_received_output_packet);

  processor.Unbind();
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([this] { return !codec_impl_; }, kWaitTimeout));
}

TEST_F(CodecImplFailures, OutputPacketWithOldOrdinalWhenPortDeconfigured) {
  StreamProcessorPtr processor;
  std::optional<fidl::InterfaceRequest<fuchsia::sysmem::BufferCollectionToken>>
      output_token_request;

  CodecPacket* saved_output_packet = nullptr;
  const CodecBuffer* saved_output_buffer = nullptr;
  zx::vmo retained_child_vmo;
  std::atomic<bool> stream_started = false;
  std::atomic<bool> remove_buffer_called = false;
  bool second_output_constraints_received = false;
  std::atomic<bool> old_packet_emitted_after_deconfig = false;
  std::atomic<bool> old_packet_recycled_back = false;

  processor.events().OnInputConstraints = [this, &processor](auto input_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    token_request_ = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                   CreateValidInputBufferCollectionConstraints());

    processor->SetInputBufferPartialSettings(
        CreateStreamBufferPartialSettings(1, input_constraints, std::move(token)));
  };

  // A real client responds to every action-required OnOutputConstraints by configuring new output
  // buffers. In this test, we only configure output buffers for the first OnOutputConstraints and
  // intentionally ignore the second one so that the output port remains in the deconfigured state
  // (is_port_configured_[kOutputPort] == false, buffer_lifetime_ordinal_[kOutputPort] == 2) while
  // we verify that a late output packet from ordinal 1 is cleanly recycled back without crashing.
  bool sent_output_settings = false;
  processor.events().OnOutputConstraints = [&](auto output_constraints) {
    if (!sent_output_settings) {
      sent_output_settings = true;
      fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
      output_token_request = token.NewRequest();

      codec_adapter_->SetBufferCollectionConstraints(
          kOutputPort, CreateValidOutputBufferCollectionConstraints());

      processor->SetOutputBufferPartialSettings(CreateStreamBufferPartialSettings(
          1, output_constraints.buffer_constraints(), std::move(token)));
    } else {
      second_output_constraints_received = true;
    }
  };

  Create(processor.NewRequest(), [&](FakeCodecAdapter* adapter) {
    adapter->SetSupportsDynamicBuffers(true);
    adapter->SetOnStartStream([&] { stream_started = true; });

    adapter->SetOnRecycleOutputPacket([&](CodecPacket* packet) {
      if (!saved_output_packet) {
        saved_output_packet = packet;
      } else if (old_packet_emitted_after_deconfig && packet == saved_output_packet) {
        old_packet_recycled_back = true;
      }
    });

    adapter->SetOnAddBuffer([&](CodecPort port, const CodecBuffer* buffer) {
      if (port == kOutputPort && buffer->index() == 0) {
        saved_output_buffer = buffer;
        // Hold a child VMO handle obtained during AddBuffer so the old buffer_lifetime_ordinal
        // remains active after EnsureBuffersNotConfigured resets until_remove_started_child_vmo_.
        retained_child_vmo = buffer->GetChildVmo();
      }
    });

    adapter->SetOnRemoveBuffer([&](CodecPort port, const CodecBuffer* buffer) {
      if (port == kOutputPort && buffer->index() == 0) {
        remove_buffer_called = true;
      }
    });
  });

  TestAllocator allocator;
  ASSERT_TRUE(sysmem_request_.has_value());
  allocator.Bind(std::move(sysmem_request_.value()));
  sysmem_request_ = nullptr;

  // Complete input buffer allocation.
  WaitForAndCompleteCollectionAllocation(allocator, 0, kInputMinBufferCountForCamping,
                                         kInputBufferSize);

  processor->QueueInputPacket(CreateInputPacket());

  // Complete output buffer allocation and CompleteOutputBufferPartialSettings(1).
  WaitForAndCompleteCollectionAllocation(allocator, 1, kOutputBufferCount, kOutputBufferSize);
  processor->CompleteOutputBufferPartialSettings(1);

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] { return error_handler_ran_ || (saved_output_buffer != nullptr && stream_started); },
      kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);

  // Trigger a mid-stream output constraints change so EnsureBuffersNotConfigured(kOutputPort)
  // sets is_port_configured_[kOutputPort] = false, buffer_lifetime_ordinal_[kOutputPort] = 2,
  // and resets buffer.until_remove_started_child_vmo_.
  uint64_t version = codec_adapter_->IncrementConstraintsVersion(kOutputPort);
  std::thread([this, version] {
    static_cast<CodecAdapterEvents*>(codec_impl_.get())
        ->onCoreCodecMidStreamOutputConstraintsChange2(version);
  }).join();

  // Wait until EnsureBuffersNotConfigured has completed and the second OnOutputConstraints event
  // has been received by the client.
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] {
        return error_handler_ran_ || (remove_buffer_called && second_output_constraints_received);
      },
      kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);

  // Now that EnsureBuffersNotConfigured has completed (until_remove_started_child_vmo_ is reset,
  // is_port_configured_[kOutputPort] is false, buffer_lifetime_ordinal_[kOutputPort] is 2), emit
  // an output packet from ordinal 1 and immediately release the adapter's retained_child_vmo so
  // that CodecPacket's buffer_keep_alive_ is solely responsible for keeping the buffer alive until
  // short_circuit_packet recycles it on shared_fidl_thread_.
  EmitOutputPacketFromCoreCodecThread(saved_output_packet, saved_output_buffer,
                                      &old_packet_emitted_after_deconfig, &retained_child_vmo);

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] {
        return error_handler_ran_ ||
               (old_packet_emitted_after_deconfig && old_packet_recycled_back);
      },
      kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_TRUE(old_packet_emitted_after_deconfig);
  ASSERT_TRUE(old_packet_recycled_back);

  processor.Unbind();
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([this] { return !codec_impl_; }, kWaitTimeout));
}

TEST_F(CodecImplFailures, RemoveBufferInNonDynamicModeForActiveOrdinalFails) {
  StreamProcessorPtr processor;

  processor.events().OnInputConstraints = [this, &processor](auto input_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    token_request_ = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                   CreateValidInputBufferCollectionConstraints());

    processor->SetInputBufferPartialSettings(
        CreateStreamBufferPartialSettings(1, input_constraints, std::move(token)));
    // Calling RemoveBuffer on the active buffer_lifetime_ordinal (1) when in non-dynamic buffer
    // mode must fail the protocol check at RemoveBufferInternal.
    fuchsia::media::StreamProcessorRemoveBufferRequest remove_request;
    remove_request.set_port(fuchsia::media::Port::INPUT)
        .set_buffer_lifetime_ordinal(1)
        .set_buffer_index(0);
    processor->RemoveBuffer(std::move(remove_request), [](auto) {});
  };

  Create(processor.NewRequest());

  RunLoopUntil([this] { return error_handler_ran_; });
  ASSERT_TRUE(error_handler_ran_);
}

TEST_F(CodecImplFailures,
       OutputPacketWithOldOrdinalDeliveredWithEnableOldOutputBuffersAndRemoveBuffer) {
  StreamProcessorPtr processor;
  std::optional<fidl::InterfaceRequest<fuchsia::sysmem::BufferCollectionToken>>
      output_token_request_1;
  std::optional<fidl::InterfaceRequest<fuchsia::sysmem::BufferCollectionToken>>
      output_token_request_2;

  CodecPacket* saved_output_packet = nullptr;
  const CodecBuffer* saved_output_buffer = nullptr;
  zx::vmo retained_child_vmo;
  std::atomic<bool> stream_started = false;
  std::atomic<bool> remove_buffer_called = false;
  bool second_output_constraints_received = false;
  fuchsia::media::StreamOutputConstraints second_output_constraints;
  std::atomic<bool> old_packet_emitted_after_deconfig = false;
  std::atomic<bool> old_packet_recycled_back = false;
  bool client_received_old_output_packet = false;
  bool remove_buffer_completed = false;
  bool remove_buffer_completed_before_output_packet = false;

  processor.events().OnInputConstraints = [this, &processor](auto input_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    token_request_ = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                   CreateValidInputBufferCollectionConstraints());

    processor->SetInputBufferPartialSettings(
        CreateStreamBufferPartialSettings(1, input_constraints, std::move(token)));
  };

  bool sent_first_output_settings = false;
  processor.events().OnOutputConstraints = [&](auto output_constraints) {
    if (!sent_first_output_settings) {
      sent_first_output_settings = true;
      fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
      output_token_request_1 = token.NewRequest();

      codec_adapter_->SetBufferCollectionConstraints(
          kOutputPort, CreateValidOutputBufferCollectionConstraints());

      processor->SetOutputBufferPartialSettings(CreateStreamBufferPartialSettings(
          1, output_constraints.buffer_constraints(), std::move(token)));
    } else {
      second_output_constraints = std::move(output_constraints);
      second_output_constraints_received = true;
    }
  };

  processor.events().OnOutputPacket = [&](fuchsia::media::Packet packet, bool error_before,
                                          bool error_during) {
    if (packet.header().buffer_lifetime_ordinal() == 1) {
      remove_buffer_completed_before_output_packet = remove_buffer_completed;
      client_received_old_output_packet = true;
      processor->RecycleOutputPacket(std::move(*packet.mutable_header()));
    }
  };

  Create(processor.NewRequest(), [&](FakeCodecAdapter* adapter) {
    adapter->SetSupportsDynamicBuffers(true);
    adapter->SetIsCoreCodecHwBased(kInputPort, true);
    adapter->SetIsCoreCodecHwBased(kOutputPort, true);
    adapter->SetOnStartStream([&] { stream_started = true; });

    adapter->SetOnRecycleOutputPacket([&](CodecPacket* packet) {
      if (!saved_output_packet) {
        saved_output_packet = packet;
      } else if (old_packet_emitted_after_deconfig && packet == saved_output_packet) {
        old_packet_recycled_back = true;
      }
    });

    adapter->SetOnAddBuffer([&](CodecPort port, const CodecBuffer* buffer) {
      if (port == kOutputPort && buffer->lifetime_ordinal() == 1 && buffer->index() == 0) {
        saved_output_buffer = buffer;
        retained_child_vmo = buffer->GetChildVmo();
      }
    });

    adapter->SetOnRemoveBuffer([&](CodecPort port, const CodecBuffer* buffer) {
      if (port == kOutputPort && buffer->lifetime_ordinal() == 1 && buffer->index() == 0) {
        remove_buffer_called = true;
      }
    });
  });

  TestAllocator allocator;
  ASSERT_TRUE(sysmem_request_.has_value());
  allocator.Bind(std::move(sysmem_request_.value()));
  sysmem_request_ = nullptr;

  processor->EnableOldOutputBuffers();

  // Complete input buffer allocation.
  WaitForAndCompleteCollectionAllocation(allocator, 0, kInputMinBufferCountForCamping,
                                         kInputBufferSize);

  processor->QueueInputPacket(CreateInputPacket());

  // Complete first output buffer allocation and CompleteOutputBufferPartialSettings(1).
  WaitForAndCompleteCollectionAllocation(allocator, 1, kOutputBufferCount, kOutputBufferSize);
  processor->CompleteOutputBufferPartialSettings(1);

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] { return error_handler_ran_ || (saved_output_buffer != nullptr && stream_started); },
      kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);

  // Trigger a mid-stream output constraints change so EnsureBuffersNotConfigured(kOutputPort)
  // sets port_settings_[kOutputPort] = nullptr, is_port_configured_[kOutputPort] = false,
  // buffer_lifetime_ordinal_[kOutputPort] = 2, and resets buffer.until_remove_started_child_vmo_,
  // while MidStreamOutputConstraintsChange retains PausedOutput until new output buffers are
  // configured.
  uint64_t version = codec_adapter_->IncrementConstraintsVersion(kOutputPort);
  std::thread([this, version] {
    static_cast<CodecAdapterEvents*>(codec_impl_.get())
        ->onCoreCodecMidStreamOutputConstraintsChange2(version);
  }).join();

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] {
        return error_handler_ran_ || (remove_buffer_called && second_output_constraints_received);
      },
      kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);

  // Client requests RemoveBuffer on ordinal 1 buffer 0 to be notified when removal completes.
  fuchsia::media::StreamProcessorRemoveBufferRequest remove_request;
  remove_request.set_port(fuchsia::media::Port::OUTPUT)
      .set_buffer_lifetime_ordinal(1)
      .set_buffer_index(0);
  processor->RemoveBuffer(std::move(remove_request), [&](auto) { remove_buffer_completed = true; });
  RunLoopUntilIdle();
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_FALSE(remove_buffer_completed);

  // While output is paused and port_settings_[kOutputPort] is nullptr, emit an output packet from
  // ordinal 1 (which goes through the full EnableOldOutputBuffers delivery path including
  // CacheFlushAndInvalidate via buffer->coherency_domain() and PostStreamOutputLocked's
  // GetKeepAlive()) and immediately drop the adapter's retained_child_vmo.
  EmitOutputPacketFromCoreCodecThread(saved_output_packet, saved_output_buffer,
                                      &old_packet_emitted_after_deconfig, &retained_child_vmo);

  // While output remains paused (before ordinal 3 is configured), neither OnOutputPacket nor the
  // RemoveBuffer completion callback should have fired.
  RunLoopUntilIdle();
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_FALSE(client_received_old_output_packet);
  ASSERT_FALSE(remove_buffer_completed);

  // Now configure ordinal 3 output buffers and CompleteOutputBufferPartialSettings(3) to unpause
  // output.
  fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token_2;
  output_token_request_2 = token_2.NewRequest();
  processor->SetOutputBufferPartialSettings(CreateStreamBufferPartialSettings(
      3, second_output_constraints.buffer_constraints(), std::move(token_2)));

  WaitForAndCompleteCollectionAllocation(allocator, 2, kOutputBufferCount, kOutputBufferSize);
  processor->CompleteOutputBufferPartialSettings(3);

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] {
        return error_handler_ran_ || (client_received_old_output_packet &&
                                      old_packet_recycled_back && remove_buffer_completed);
      },
      kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_TRUE(client_received_old_output_packet);
  ASSERT_FALSE(remove_buffer_completed_before_output_packet);
  ASSERT_TRUE(old_packet_recycled_back);
  ASSERT_TRUE(remove_buffer_completed);

  processor.Unbind();
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([this] { return !codec_impl_; }, kWaitTimeout));
}

TEST_F(CodecImplFailures,
       DynamicRemoveBufferRetainsUntilRemoveStartedChildVmoDuringCoreCodecRemoveBuffer) {
  StreamProcessorPtr processor;

  CodecPacket* saved_output_packet = nullptr;
  const CodecBuffer* saved_output_buffer = nullptr;
  zx::vmo retained_child_vmo;
  std::atomic<bool> stream_started = false;
  std::atomic<bool> remove_buffer_called = false;
  std::atomic<bool> packet_emitted_after_remove = false;
  std::atomic<bool> packet_recycled_back = false;
  bool client_received_output_packet = false;
  bool remove_buffer_completed = false;
  bool remove_buffer_completed_before_output_packet = false;

  processor.events().OnInputConstraints = [this, &processor](auto input_constraints) {
    codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                   CreateValidInputBufferCollectionConstraints());
    zx::vmo input_vmo;
    ASSERT_EQ(zx::vmo::create(kInputBufferSize, 0, &input_vmo), ZX_OK);
    fuchsia::media::StreamProcessorAddBufferRequest add_input;
    add_input.set_port(fuchsia::media::Port::INPUT)
        .set_buffer_constraints_version_ordinal(
            input_constraints.buffer_constraints_version_ordinal())
        .set_buffer_lifetime_ordinal(1)
        .set_buffer_index(0)
        .set_buffer(std::move(input_vmo));
    processor->AddBuffer(std::move(add_input));
    processor->QueueInputPacket(CreateInputPacket());
  };

  processor.events().OnOutputConstraints = [this, &processor](auto output_constraints) {
    codec_adapter_->SetBufferCollectionConstraints(kOutputPort,
                                                   CreateValidOutputBufferCollectionConstraints());
    zx::vmo output_vmo;
    ASSERT_EQ(zx::vmo::create(kOutputBufferSize, 0, &output_vmo), ZX_OK);
    fuchsia::media::StreamProcessorAddBufferRequest add_output;
    add_output.set_port(fuchsia::media::Port::OUTPUT)
        .set_buffer_constraints_version_ordinal(
            output_constraints.buffer_constraints().buffer_constraints_version_ordinal())
        .set_buffer_lifetime_ordinal(1)
        .set_buffer_index(0)
        .set_buffer(std::move(output_vmo));
    processor->AddBuffer(std::move(add_output));
  };

  processor.events().OnOutputPacket = [&](fuchsia::media::Packet packet, bool error_before,
                                          bool error_during) {
    remove_buffer_completed_before_output_packet = remove_buffer_completed;
    client_received_output_packet = true;
    processor->RecycleOutputPacket(std::move(*packet.mutable_header()));
  };

  Create(processor.NewRequest(), [&](FakeCodecAdapter* adapter) {
    adapter->SetSupportsDynamicBuffers(true);
    adapter->SetOnStartStream([&] { stream_started = true; });

    adapter->SetOnRecycleOutputPacket([&](CodecPacket* packet) {
      if (!saved_output_packet) {
        saved_output_packet = packet;
      } else if (packet_emitted_after_remove && packet == saved_output_packet) {
        packet_recycled_back = true;
      }
    });

    adapter->SetOnAddBuffer([&](CodecPort port, const CodecBuffer* buffer) {
      if (port == kOutputPort && buffer->index() == 0) {
        saved_output_buffer = buffer;
      }
    });

    adapter->SetOnRemoveBuffer([&](CodecPort port, const CodecBuffer* buffer) {
      if (port == kOutputPort && buffer->index() == 0) {
        // Obtain a child VMO handle during CoreCodecRemoveBuffer (while
        // until_remove_started_child_vmo_ is still populated in RemoveBufferInternal).
        retained_child_vmo = buffer->GetChildVmo();
        ASSERT_TRUE(retained_child_vmo.is_valid());
        remove_buffer_called = true;
      }
    });
  });

  TestAllocator allocator;
  ASSERT_TRUE(sysmem_request_.has_value());
  allocator.Bind(std::move(sysmem_request_.value()));
  sysmem_request_ = nullptr;

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] { return error_handler_ran_ || (saved_output_buffer != nullptr && stream_started); },
      kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);

  // Remove the active output buffer via StreamProcessor.RemoveBuffer.
  fuchsia::media::StreamProcessorRemoveBufferRequest remove_request;
  remove_request.set_port(fuchsia::media::Port::OUTPUT)
      .set_buffer_lifetime_ordinal(1)
      .set_buffer_index(0);
  processor->RemoveBuffer(std::move(remove_request), [&](auto) { remove_buffer_completed = true; });

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([&] { return error_handler_ran_ || remove_buffer_called; },
                                        kWaitTimeout));
  RunLoopUntilIdle();
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_FALSE(remove_buffer_completed);

  // Now that RemoveBufferInternal has reset until_remove_started_child_vmo_, emit an output packet
  // referencing the buffer (exercising the GetKeepAlive() -> CreateChildVmoFromParent() ->
  // create_child() fallback) and immediately release retained_child_vmo.
  EmitOutputPacketFromCoreCodecThread(saved_output_packet, saved_output_buffer,
                                      &packet_emitted_after_remove, &retained_child_vmo);

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] {
        return error_handler_ran_ ||
               (client_received_output_packet && packet_recycled_back && remove_buffer_completed);
      },
      kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_TRUE(client_received_output_packet);
  ASSERT_FALSE(remove_buffer_completed_before_output_packet);
  ASSERT_TRUE(packet_recycled_back);
  ASSERT_TRUE(remove_buffer_completed);

  processor.Unbind();
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([this] { return !codec_impl_; }, kWaitTimeout));
}

TEST_F(CodecImplFailures, OutputPacketQueuedDuringPausedOutputShortCircuitedOnOutputReconfig) {
  StreamProcessorPtr processor;
  std::optional<fidl::InterfaceRequest<fuchsia::sysmem::BufferCollectionToken>>
      output_token_request_1;
  std::optional<fidl::InterfaceRequest<fuchsia::sysmem::BufferCollectionToken>>
      output_token_request_2;

  CodecPacket* saved_output_packet_1 = nullptr;
  const CodecBuffer* saved_output_buffer_1 = nullptr;
  CodecPacket* saved_output_packet_3 = nullptr;
  const CodecBuffer* saved_output_buffer_3 = nullptr;
  std::atomic<bool> packet_1_emitted = false;
  std::atomic<uint32_t> packet_1_recycle_count = 0;
  bool client_received_ordinal_1_packet = false;
  bool client_received_ordinal_3_packet = false;
  fuchsia::media::StreamOutputConstraints initial_output_constraints;

  processor.events().OnInputConstraints = [this, &processor](auto input_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    token_request_ = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                   CreateValidInputBufferCollectionConstraints());

    processor->SetInputBufferPartialSettings(
        CreateStreamBufferPartialSettings(1, input_constraints, std::move(token)));
  };

  processor.events().OnOutputConstraints = [&](auto output_constraints) {
    initial_output_constraints = std::move(output_constraints);
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    output_token_request_1 = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kOutputPort,
                                                   CreateValidOutputBufferCollectionConstraints());

    processor->SetOutputBufferPartialSettings(CreateStreamBufferPartialSettings(
        1, initial_output_constraints.buffer_constraints(), std::move(token)));
  };

  processor.events().OnOutputPacket = [&](fuchsia::media::Packet packet, bool error_before,
                                          bool error_during) {
    if (packet.header().buffer_lifetime_ordinal() == 1) {
      client_received_ordinal_1_packet = true;
    } else if (packet.header().buffer_lifetime_ordinal() == 3) {
      client_received_ordinal_3_packet = true;
      processor->RecycleOutputPacket(std::move(*packet.mutable_header()));
    }
  };

  Create(processor.NewRequest(), [&](FakeCodecAdapter* adapter) {
    adapter->SetSupportsDynamicBuffers(true);

    adapter->SetOnRecycleOutputPacket([&](CodecPacket* packet) {
      if (packet->buffer_lifetime_ordinal() == 1) {
        if (!saved_output_packet_1) {
          saved_output_packet_1 = packet;
        } else if (packet == saved_output_packet_1) {
          ++packet_1_recycle_count;
        }
      } else if (packet->buffer_lifetime_ordinal() == 3 && !saved_output_packet_3) {
        saved_output_packet_3 = packet;
      }
    });

    adapter->SetOnAddBuffer([&](CodecPort port, const CodecBuffer* buffer) {
      if (port == kOutputPort && buffer->lifetime_ordinal() == 1 && buffer->index() == 0) {
        saved_output_buffer_1 = buffer;
        ASSERT_NE(saved_output_packet_1, nullptr);
        // Emit an output packet from ordinal 1 while StartNewStream's PausedOutput is still active
        // (before CompleteOutputBufferPartialSettings(1) is ever sent).
        EmitOutputPacketFromCoreCodecThread(saved_output_packet_1, saved_output_buffer_1,
                                            &packet_1_emitted);
      } else if (port == kOutputPort && buffer->lifetime_ordinal() == 3 && buffer->index() == 0) {
        saved_output_buffer_3 = buffer;
      }
    });
  });

  TestAllocator allocator;
  ASSERT_TRUE(sysmem_request_.has_value());
  allocator.Bind(std::move(sysmem_request_.value()));
  sysmem_request_ = nullptr;

  WaitForAndCompleteCollectionAllocation(allocator, 0, kInputMinBufferCountForCamping,
                                         kInputBufferSize);

  processor->QueueInputPacket(CreateInputPacket());

  WaitForAndCompleteCollectionAllocation(allocator, 1, kOutputBufferCount, kOutputBufferSize);

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] { return error_handler_ran_ || packet_1_emitted.load(); }, kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);
  RunLoopUntilIdle();
  ASSERT_FALSE(client_received_ordinal_1_packet);
  ASSERT_EQ(packet_1_recycle_count.load(), 0u);

  // Without calling EnableOldOutputBuffers() and before CompleteOutputBufferPartialSettings(1),
  // supersede ordinal 1 with SetOutputBufferPartialSettings(3), which runs
  // EnsureBuffersNotConfigured(kOutputPort) while ordinal 1's packet is still queued in
  // output_queue_. EnsureBuffersNotConfigured must skip recycling the queued packet (`is_queued()`
  // is true), and when CompleteOutputBufferPartialSettings(3) unpauses output_queue_, the queued
  // closure must short-circuit and recycle ordinal 1's packet exactly once without delivering
  // OnOutputPacket for ordinal 1 to the client.
  fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token_2;
  output_token_request_2 = token_2.NewRequest();
  processor->SetOutputBufferPartialSettings(CreateStreamBufferPartialSettings(
      3, initial_output_constraints.buffer_constraints(), std::move(token_2)));

  WaitForAndCompleteCollectionAllocation(allocator, 2, kOutputBufferCount, kOutputBufferSize);
  processor->CompleteOutputBufferPartialSettings(3);

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] {
        return error_handler_ran_ ||
               (packet_1_recycle_count.load() == 1u && saved_output_buffer_3 != nullptr &&
                saved_output_packet_3 != nullptr);
      },
      kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_EQ(packet_1_recycle_count.load(), 1u);
  ASSERT_FALSE(client_received_ordinal_1_packet);

  // Emit an output packet from ordinal 3 to verify normal output delivery after the short-circuited
  // ordinal 1 packet.
  EmitOutputPacketFromCoreCodecThread(saved_output_packet_3, saved_output_buffer_3);
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] { return error_handler_ran_ || client_received_ordinal_3_packet; }, kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_FALSE(client_received_ordinal_1_packet);
  ASSERT_TRUE(client_received_ordinal_3_packet);
  ASSERT_EQ(packet_1_recycle_count.load(), 1u);

  processor.Unbind();
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([this] { return !codec_impl_; }, kWaitTimeout));
}

TEST_F(CodecImplFailures, OutputPacketQueuedDuringMidStreamFormatChangeNonDynamicBuffers) {
  StreamProcessorPtr processor;
  std::optional<fidl::InterfaceRequest<fuchsia::sysmem::BufferCollectionToken>>
      output_token_request_1;
  std::optional<fidl::InterfaceRequest<fuchsia::sysmem::BufferCollectionToken>>
      output_token_request_2;

  CodecPacket* saved_output_packet_1 = nullptr;
  const CodecBuffer* saved_output_buffer_1 = nullptr;
  CodecPacket* saved_output_packet_3 = nullptr;
  const CodecBuffer* saved_output_buffer_3 = nullptr;
  std::atomic<bool> packet_1_emitted = false;
  std::atomic<uint32_t> packet_1_post_emit_recycle_count = 0;
  bool client_received_ordinal_1_packet = false;
  bool client_received_ordinal_3_packet = false;
  fuchsia::media::StreamOutputConstraints initial_output_constraints;

  std::atomic<bool> stream_started = false;
  std::atomic<bool> re_config_prepare_entered = false;
  libsync::Completion allow_re_config_prepare_to_finish;
  bool ensure_output_buffers_not_configured_checked_queued = false;
  std::atomic<bool> closed_buffer_lifetime_ordinal_1 = false;
  uint32_t output_constraints_count = 0;

  processor.events().OnInputConstraints = [this, &processor](auto input_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    token_request_ = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                   CreateValidInputBufferCollectionConstraints());

    processor->SetInputBufferPartialSettings(
        CreateStreamBufferPartialSettings(1, input_constraints, std::move(token)));
  };

  processor.events().OnOutputConstraints = [&](auto output_constraints) {
    ++output_constraints_count;
    initial_output_constraints = std::move(output_constraints);
    if (output_constraints_count == 1) {
      fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
      output_token_request_1 = token.NewRequest();

      codec_adapter_->SetBufferCollectionConstraints(
          kOutputPort, CreateValidOutputBufferCollectionConstraints());

      processor->SetOutputBufferPartialSettings(CreateStreamBufferPartialSettings(
          1, initial_output_constraints.buffer_constraints(), std::move(token)));
    }
  };

  processor.events().OnOutputPacket = [&](fuchsia::media::Packet packet, bool error_before,
                                          bool error_during) {
    if (packet.header().buffer_lifetime_ordinal() == 1) {
      client_received_ordinal_1_packet = true;
    } else if (packet.header().buffer_lifetime_ordinal() == 3) {
      client_received_ordinal_3_packet = true;
      processor->RecycleOutputPacket(std::move(*packet.mutable_header()));
    }
  };

  Create(processor.NewRequest(), [&](FakeCodecAdapter* adapter) {
    adapter->SetSupportsDynamicBuffers(false);

    adapter->SetOnStartStream([&] { stream_started = true; });

    adapter->SetOnMidStreamOutputBufferReConfigPrepare([&] {
      re_config_prepare_entered = true;
      ZX_ASSERT(allow_re_config_prepare_to_finish.Wait(kWaitTimeout) == ZX_OK);
    });

    adapter->SetOnEnsureBuffersNotConfigured([&](CodecPort port) {
      if (port == kOutputPort && packet_1_emitted.load() &&
          !ensure_output_buffers_not_configured_checked_queued) {
        EXPECT_TRUE(IsPacketQueued(saved_output_packet_1));
        ensure_output_buffers_not_configured_checked_queued = true;
      }
    });

    adapter->SetOnRecycleOutputPacket([&](CodecPacket* packet) {
      if (packet->buffer_lifetime_ordinal() == 1) {
        if (!saved_output_packet_1) {
          saved_output_packet_1 = packet;
        } else if (packet == saved_output_packet_1 && packet_1_emitted.load()) {
          ++packet_1_post_emit_recycle_count;
        }
      } else if (packet->buffer_lifetime_ordinal() == 3 && !saved_output_packet_3) {
        saved_output_packet_3 = packet;
      }
    });

    adapter->SetOnAddBuffer([&](CodecPort port, const CodecBuffer* buffer) {
      if (port == kOutputPort && buffer->lifetime_ordinal() == 1 && buffer->index() == 0) {
        saved_output_buffer_1 = buffer;
      } else if (port == kOutputPort && buffer->lifetime_ordinal() == 3 && buffer->index() == 0) {
        saved_output_buffer_3 = buffer;
      }
    });

    adapter->SetOnCloseBufferLifetimeOrdinal([&](CodecPort port, uint64_t ordinal) {
      if (port == kOutputPort && ordinal == 1) {
        closed_buffer_lifetime_ordinal_1 = true;
        saved_output_packet_1 = nullptr;
      }
    });
  });

  TestAllocator allocator;
  ASSERT_TRUE(sysmem_request_.has_value());
  allocator.Bind(std::move(sysmem_request_.value()));
  sysmem_request_ = nullptr;

  WaitForAndCompleteCollectionAllocation(allocator, 0, kInputMinBufferCountForCamping,
                                         kInputBufferSize);

  processor->QueueInputPacket(CreateInputPacket());

  WaitForAndCompleteCollectionAllocation(allocator, 1, kOutputBufferCount, kOutputBufferSize);
  processor->CompleteOutputBufferPartialSettings(1);

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] {
        return error_handler_ran_ || (saved_output_buffer_1 != nullptr &&
                                      saved_output_packet_1 != nullptr && stream_started.load());
      },
      kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);

  // Initiate a mid-stream output constraints change from the core codec thread.
  // MidStreamOutputConstraintsChange first runs StartIgnoringClientOldOutputConfig on
  // shared_fidl_queue_ and then calls CoreCodecMidStreamOutputBufferReConfigPrepare() on
  // StreamControl before enqueuing the second RunSyncOnSharedFidlForStream task
  // (EnsureBuffersNotConfigured(kOutputPort) + GenerateAndSendNewOutputConstraints(paused_output)).
  std::thread([this] {
    static_cast<CodecAdapterEvents*>(codec_impl_.get())
        ->onCoreCodecMidStreamOutputConstraintsChange(true);
  }).join();

  // Pump shared_fidl_dispatcher_ until the first RunSyncOnSharedFidlForStream
  // (StartIgnoringClientOldOutputConfig) has run and StreamControl has entered
  // CoreCodecMidStreamOutputBufferReConfigPrepare().
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] { return error_handler_ran_ || re_config_prepare_entered.load(); }, kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);

  // Release CoreCodecMidStreamOutputBufferReConfigPrepare() while not pumping
  // shared_fidl_dispatcher_, and wait deterministically until StreamControl has enqueued the second
  // RunSyncOnSharedFidlForStream closure (EnsureBuffersNotConfigured +
  // GenerateAndSendNewOutputConstraints) onto shared_fidl_queue_.
  uint64_t wait_count_before = GetSharedFidlForStreamWaitCount();
  allow_re_config_prepare_to_finish.Signal();
  WaitForSharedFidlForStreamWaitCount(wait_count_before + 1);

  EmitOutputPacketFromCoreCodecThread(saved_output_packet_1, saved_output_buffer_1,
                                      &packet_1_emitted);
  // Before shared_fidl_queue_ runs EnsureBuffersNotConfigured (Task 1) and the queued
  // short_circuit_packet closure (Task 2), saved_output_packet_1 is queued with
  // saved_output_buffer_1 attached.
  ASSERT_TRUE(IsPacketQueued(saved_output_packet_1));
  ASSERT_EQ(saved_output_packet_1->buffer(), saved_output_buffer_1);

  // Pumping shared_fidl_queue_ runs Task 1 (EnsureBuffersNotConfigured, which skips
  // saved_output_packet_1 because is_queued() is true, followed by
  // GenerateAndSendNewOutputConstraints posting Task 3), then Task 2 (saved_output_packet_1's
  // short_circuit_packet closure, which sees 1 < 2 and clears SetBuffer(nullptr) without calling
  // CoreCodecRecycleOutputPacket), then Task 3 (OnOutputConstraints). Releasing the packet's
  // buffer drops the last remaining child VMO handle, triggering ZX_VMO_ZERO_CHILDREN ->
  // DoCodecBufferDelete -> DeleteBuffer -> CoreCodecCloseBufferLifetimeOrdinal(kOutputPort, 1),
  // which destroys ordinal 1's packets (including saved_output_packet_1).
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] { return error_handler_ran_ || output_constraints_count == 2; }, kWaitTimeout));
  RunLoopUntilIdle();
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_TRUE(ensure_output_buffers_not_configured_checked_queued);
  ASSERT_FALSE(client_received_ordinal_1_packet);
  ASSERT_EQ(packet_1_post_emit_recycle_count.load(), 0u);
  ASSERT_TRUE(closed_buffer_lifetime_ordinal_1.load());
  ASSERT_EQ(saved_output_packet_1, nullptr);

  // Complete the mid-stream output buffer reconfiguration to ordinal 3 so that subsequent output
  // packet delivery on ordinal 3 can be verified.
  fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token_2;
  output_token_request_2 = token_2.NewRequest();
  processor->SetOutputBufferPartialSettings(CreateStreamBufferPartialSettings(
      3, initial_output_constraints.buffer_constraints(), std::move(token_2)));

  WaitForAndCompleteCollectionAllocation(allocator, 2, kOutputBufferCount, kOutputBufferSize);
  processor->CompleteOutputBufferPartialSettings(3);

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] {
        return error_handler_ran_ ||
               (saved_output_buffer_3 != nullptr && saved_output_packet_3 != nullptr);
      },
      kWaitTimeout));
  RunLoopUntilIdle();
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_FALSE(client_received_ordinal_1_packet);
  ASSERT_EQ(packet_1_post_emit_recycle_count.load(), 0u);
  ASSERT_EQ(saved_output_packet_1, nullptr);

  // Emit an output packet from ordinal 3 to verify normal output delivery after the short-circuited
  // ordinal 1 packet.
  EmitOutputPacketFromCoreCodecThread(saved_output_packet_3, saved_output_buffer_3);
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] { return error_handler_ran_ || client_received_ordinal_3_packet; }, kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_FALSE(client_received_ordinal_1_packet);
  ASSERT_TRUE(client_received_ordinal_3_packet);
  ASSERT_EQ(packet_1_post_emit_recycle_count.load(), 0u);

  processor.Unbind();
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([this] { return !codec_impl_; }, kWaitTimeout));
}

TEST_F(CodecImplFailures, RecycleOutputPacketWhileQueued) {
  StreamProcessorPtr processor;
  std::optional<fidl::InterfaceRequest<fuchsia::sysmem::BufferCollectionToken>>
      output_token_request;

  CodecPacket* saved_output_packet = nullptr;
  const CodecBuffer* saved_output_buffer = nullptr;
  std::atomic<bool> packet_emitted = false;
  zx_status_t channel_epitaph = ZX_OK;

  processor.set_error_handler([&](zx_status_t status) { channel_epitaph = status; });

  processor.events().OnInputConstraints = [this, &processor](auto input_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    token_request_ = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                   CreateValidInputBufferCollectionConstraints());

    processor->SetInputBufferPartialSettings(
        CreateStreamBufferPartialSettings(1, input_constraints, std::move(token)));
  };

  processor.events().OnOutputConstraints = [&](auto output_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    output_token_request = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kOutputPort,
                                                   CreateValidOutputBufferCollectionConstraints());

    processor->SetOutputBufferPartialSettings(CreateStreamBufferPartialSettings(
        1, output_constraints.buffer_constraints(), std::move(token)));
  };

  Create(processor.NewRequest(), [&](FakeCodecAdapter* adapter) {
    adapter->SetSupportsDynamicBuffers(true);

    adapter->SetOnRecycleOutputPacket([&](CodecPacket* packet) {
      if (!saved_output_packet) {
        saved_output_packet = packet;
      }
    });

    adapter->SetOnAddBuffer([&](CodecPort port, const CodecBuffer* buffer) {
      if (port == kOutputPort && buffer->lifetime_ordinal() == 1 && buffer->index() == 0) {
        saved_output_buffer = buffer;
        ASSERT_NE(saved_output_packet, nullptr);
        // Emit an output packet while StartNewStream's PausedOutput is still active (before
        // CompleteOutputBufferPartialSettings(1) is sent), so saved_output_packet->is_queued() is
        // true and saved_output_packet->is_free() is false.
        EmitOutputPacketFromCoreCodecThread(saved_output_packet, saved_output_buffer,
                                            &packet_emitted);
      }
    });
  });

  TestAllocator allocator;
  ASSERT_TRUE(sysmem_request_.has_value());
  allocator.Bind(std::move(sysmem_request_.value()));
  sysmem_request_ = nullptr;

  WaitForAndCompleteCollectionAllocation(allocator, 0, kInputMinBufferCountForCamping,
                                         kInputBufferSize);
  processor->QueueInputPacket(CreateInputPacket());
  WaitForAndCompleteCollectionAllocation(allocator, 1, kOutputBufferCount, kOutputBufferSize);

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([&] { return error_handler_ran_ || packet_emitted.load(); },
                                        kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_TRUE(IsPacketQueued(saved_output_packet));
  ASSERT_FALSE(saved_output_packet->is_free());

  // Sending RecycleOutputPacket while the packet is still queued in output_queue_ (not yet
  // delivered to the client via OnOutputPacket) is a client protocol error and must fail the
  // channel cleanly without crashing or tripping assertions when output_queue_ drains during
  // shutdown.
  fuchsia::media::PacketHeader bad_recycle;
  bad_recycle.set_buffer_lifetime_ordinal(1).set_packet_index(0);
  processor->RecycleOutputPacket(std::move(bad_recycle));

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] { return error_handler_ran_ && !codec_impl_ && channel_epitaph != ZX_OK; },
      kWaitTimeout));
  EXPECT_EQ(channel_epitaph, ZX_ERR_PEER_CLOSED);
}

TEST_F(CodecImplFailures, FailStreamWhilePausedOutputShortCircuitsQueuedPacket) {
  StreamProcessorPtr processor;
  std::optional<fidl::InterfaceRequest<fuchsia::sysmem::BufferCollectionToken>>
      output_token_request;

  CodecPacket* saved_output_packet = nullptr;
  const CodecBuffer* saved_output_buffer = nullptr;
  std::atomic<bool> packet_emitted = false;
  std::atomic<uint32_t> post_emit_recycle_count = 0;
  bool client_received_output_format = false;
  bool client_received_output_packet = false;
  bool stream_failed_seen = false;

  processor.events().OnInputConstraints = [this, &processor](auto input_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    token_request_ = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                   CreateValidInputBufferCollectionConstraints());

    processor->SetInputBufferPartialSettings(
        CreateStreamBufferPartialSettings(1, input_constraints, std::move(token)));
  };

  processor.events().OnOutputConstraints = [&](auto output_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    output_token_request = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kOutputPort,
                                                   CreateValidOutputBufferCollectionConstraints());

    processor->SetOutputBufferPartialSettings(CreateStreamBufferPartialSettings(
        1, output_constraints.buffer_constraints(), std::move(token)));
  };

  processor.events().OnOutputFormat = [&](fuchsia::media::StreamOutputFormat output_format) {
    client_received_output_format = true;
  };

  processor.events().OnOutputPacket = [&](fuchsia::media::Packet output_packet,
                                          bool error_detected_before, bool error_detected_during) {
    client_received_output_packet = true;
  };

  processor.events().OnStreamFailed = [&](uint64_t stream_lifetime_ordinal,
                                          fuchsia::media::StreamError error) {
    EXPECT_EQ(stream_lifetime_ordinal, 1u);
    EXPECT_EQ(error, fuchsia::media::StreamError::DECODER_DATA_PARSING);
    stream_failed_seen = true;
  };

  Create(processor.NewRequest(), [&](FakeCodecAdapter* adapter) {
    adapter->SetSupportsDynamicBuffers(true);

    adapter->SetOnRecycleOutputPacket([&](CodecPacket* packet) {
      if (!saved_output_packet) {
        saved_output_packet = packet;
      } else if (packet_emitted.load() && packet == saved_output_packet) {
        ++post_emit_recycle_count;
      }
    });

    adapter->SetOnAddBuffer([&](CodecPort port, const CodecBuffer* buffer) {
      if (port == kOutputPort && buffer->lifetime_ordinal() == 1 && buffer->index() == 0) {
        saved_output_buffer = buffer;
        ASSERT_NE(saved_output_packet, nullptr);
        EmitOutputPacketFromCoreCodecThread(saved_output_packet, saved_output_buffer,
                                            &packet_emitted);
      }
    });
  });

  processor->EnableOnStreamFailed();

  TestAllocator allocator;
  ASSERT_TRUE(sysmem_request_.has_value());
  allocator.Bind(std::move(sysmem_request_.value()));
  sysmem_request_ = nullptr;

  WaitForAndCompleteCollectionAllocation(allocator, 0, kInputMinBufferCountForCamping,
                                         kInputBufferSize);
  processor->QueueInputPacket(CreateInputPacket());
  WaitForAndCompleteCollectionAllocation(allocator, 1, kOutputBufferCount, kOutputBufferSize);

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([&] { return error_handler_ran_ || packet_emitted.load(); },
                                        kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_TRUE(IsPacketQueued(saved_output_packet));
  ASSERT_FALSE(saved_output_packet->is_free());

  // Fail the stream from the core codec thread while PausedOutput is still holding back
  // output_queue_ (before CompleteOutputBufferPartialSettings(1) is sent). onCoreCodecFailStream
  // marks the stream future_discarded, waking StartNewStream so ~PausedOutput() drains
  // output_queue_ and short-circuits/recycles saved_output_packet without delivering OnOutputFormat
  // or OnOutputPacket to the client.
  std::thread([this] {
    static_cast<CodecAdapterEvents*>(codec_impl_.get())
        ->onCoreCodecFailStream(fuchsia::media::StreamError::DECODER_DATA_PARSING);
  }).join();

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [&] {
        return error_handler_ran_ || (stream_failed_seen && post_emit_recycle_count.load() == 1u);
      },
      kWaitTimeout));
  RunLoopUntilIdle();
  ASSERT_FALSE(error_handler_ran_);
  EXPECT_TRUE(stream_failed_seen);
  EXPECT_FALSE(client_received_output_format);
  EXPECT_FALSE(client_received_output_packet);
  EXPECT_EQ(post_emit_recycle_count.load(), 1u);
  EXPECT_FALSE(IsPacketQueued(saved_output_packet));
  EXPECT_TRUE(saved_output_packet->is_free());
  EXPECT_EQ(saved_output_packet->buffer(), nullptr);

  // Also verify that closing the failed stream from the client succeeds cleanly (exercising
  // idempotent SetFutureDiscarded()).
  processor->CloseCurrentStream(1, false, false);
  RunLoopUntilIdle();
  ASSERT_FALSE(error_handler_ran_);

  processor.Unbind();
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([this] { return !codec_impl_; }, kWaitTimeout));
}

TEST_F(CodecImplFailures, QueueInputPacketOutOfBoundsFailsAndShutsDownCleanly) {
  StreamProcessorPtr processor;

  processor.events().OnInputConstraints = [&](auto input_constraints) {
    fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
    token_request_ = token.NewRequest();

    codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                   CreateValidInputBufferCollectionConstraints());

    processor->SetInputBufferPartialSettings(
        CreateStreamBufferPartialSettings(1, input_constraints, std::move(token)));
  };

  Create(processor.NewRequest(), [&](FakeCodecAdapter* adapter) {
    adapter->SetIsCoreCodecRequiringOutputConfigForFormatDetection(false);
  });

  TestAllocator allocator;
  ASSERT_TRUE(sysmem_request_.has_value());
  allocator.Bind(std::move(sysmem_request_.value()));
  sysmem_request_ = nullptr;

  WaitForAndCompleteCollectionAllocation(allocator, 0, kInputMinBufferCountForCamping,
                                         kInputBufferSize);

  // Queue an input packet whose start_offset + valid_length_bytes exceeds kInputBufferSize.
  // QueueInputPacket_StreamControl must reject the packet before marking codec_packet busy or
  // attaching buffer_keep_alive_, failing the channel and allowing CodecImpl to shut down cleanly.
  fuchsia::media::Packet bad_packet = CreateInputPacket();
  bad_packet.set_start_offset(kInputBufferSize - 16);
  bad_packet.set_valid_length_bytes(32);
  processor->QueueInputPacket(std::move(bad_packet));

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([this] { return error_handler_ran_; }, kWaitTimeout));
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([this] { return !codec_impl_; }, kWaitTimeout));
}

class FakeCodecAdapterWithFormatTracking : public FakeCodecAdapter {
 public:
  using FakeCodecAdapter::FakeCodecAdapter;

  void CoreCodecQueueInputFormatDetails(
      const fuchsia::media::FormatDetails& per_stream_override_format_details) override {
    last_format_details_ = fidl::Clone(per_stream_override_format_details);
    call_count_++;
  }

  bool IsCoreCodecRequiringOutputConfigForFormatDetection() override { return false; }

  std::optional<fuchsia::media::FormatDetails> last_format_details_;
  std::atomic<int> call_count_{0};
};

class CodecImplFormatTrackingFailures : public CodecImplFailures {
 public:
  std::unique_ptr<FakeCodecAdapter> CreateCodecAdapter() override {
    auto codec_adapter = std::make_unique<FakeCodecAdapterWithFormatTracking>(codec_impl_->lock(),
                                                                              codec_impl_.get());
    codec_adapter_ = codec_adapter.get();
    return codec_adapter;
  }

  void ConfigureInputBuffers(StreamProcessorPtr& processor) {
    processor.events().OnInputConstraints = [this, &processor](auto input_constraints) {
      fidl::InterfaceHandle<fuchsia::sysmem::BufferCollectionToken> token;
      token_request_ = token.NewRequest();

      auto buffer_collection_constraints = CreateValidInputBufferCollectionConstraints();
      codec_adapter_->SetBufferCollectionConstraints(kInputPort,
                                                     std::move(buffer_collection_constraints));

      processor->SetInputBufferPartialSettings(
          CreateStreamBufferPartialSettings(1, input_constraints, std::move(token)));
    };

    processor.events().OnFreeInputPacket = [this](fuchsia::media::PacketHeader free_input_packet) {
      free_input_packet_count_++;
    };

    Create(processor.NewRequest());

    allocator_ = std::make_unique<TestAllocator>();
    ASSERT_TRUE(sysmem_request_.has_value());
    allocator_->Bind(std::move(sysmem_request_.value()));
    sysmem_request_ = nullptr;

    WaitForAndCompleteCollectionAllocation(*allocator_, 0, kInputMinBufferCountForCamping,
                                           kInputBufferSize);
  }

  static void QueueInputPacket(StreamProcessorPtr& processor,
                               uint64_t stream_lifetime_ordinal = 1) {
    auto packet = CreateInputPacket();
    packet.set_stream_lifetime_ordinal(stream_lifetime_ordinal);
    processor->QueueInputPacket(std::move(packet));
  }

  int free_input_packet_count() const { return free_input_packet_count_; }

 private:
  std::unique_ptr<TestAllocator> allocator_;
  int free_input_packet_count_ = 0;
};

TEST_F(CodecImplFormatTrackingFailures, IdenticalFormatDetailsSucceeds) {
  StreamProcessorPtr processor;
  ConfigureInputBuffers(processor);

  // Call QueueInputFormatDetails with version ordinal 0 and same content (empty).
  fuchsia::media::FormatDetails details;
  details.set_format_details_version_ordinal(0);

  processor->QueueInputFormatDetails(1, std::move(details));
  QueueInputPacket(processor);

  auto* tracking_adapter = static_cast<FakeCodecAdapterWithFormatTracking*>(codec_adapter_);
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [this] { return error_handler_ran_ || free_input_packet_count() == 1; }, kWaitTimeout));

  ASSERT_FALSE(error_handler_ran_);
  ASSERT_EQ(tracking_adapter->call_count_.load(), 1);
  ASSERT_TRUE(tracking_adapter->last_format_details_.has_value());
  ASSERT_EQ(tracking_adapter->last_format_details_->format_details_version_ordinal(), 0u);

  // Call QueueInputFormatDetails with version ordinal 1 (greater than 0).
  fuchsia::media::FormatDetails details2;
  details2.set_format_details_version_ordinal(1);
  details2.set_mime_type("video/h264");

  processor->QueueInputFormatDetails(1, std::move(details2));
  QueueInputPacket(processor);
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [this] { return error_handler_ran_ || free_input_packet_count() == 2; }, kWaitTimeout));

  ASSERT_FALSE(error_handler_ran_);
  ASSERT_EQ(tracking_adapter->call_count_.load(), 2);
  ASSERT_TRUE(tracking_adapter->last_format_details_.has_value());
  ASSERT_EQ(tracking_adapter->last_format_details_->format_details_version_ordinal(), 1u);
  ASSERT_EQ(tracking_adapter->last_format_details_->mime_type(), "video/h264");

  // Call QueueInputFormatDetails with version ordinal 1 and identical content.
  fuchsia::media::FormatDetails details3;
  details3.set_format_details_version_ordinal(1);
  details3.set_mime_type("video/h264");

  processor->QueueInputFormatDetails(1, std::move(details3));
  QueueInputPacket(processor);
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [this] { return error_handler_ran_ || free_input_packet_count() == 3; }, kWaitTimeout));

  ASSERT_FALSE(error_handler_ran_);
  ASSERT_EQ(tracking_adapter->call_count_.load(), 3);

  // Call QueueInputFormatDetails with version ordinal 1 but different content (mime type).
  // Currently CodecImpl logs an ERROR without failing the channel and still forwards the updated
  // format details.
  fuchsia::media::FormatDetails details4;
  details4.set_format_details_version_ordinal(1);
  details4.set_mime_type("video/vp9");

  processor->QueueInputFormatDetails(1, std::move(details4));
  QueueInputPacket(processor);
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [this] { return error_handler_ran_ || free_input_packet_count() == 4; }, kWaitTimeout));

  ASSERT_FALSE(error_handler_ran_);
  ASSERT_EQ(tracking_adapter->call_count_.load(), 4);
  ASSERT_TRUE(tracking_adapter->last_format_details_.has_value());
  ASSERT_EQ(tracking_adapter->last_format_details_->format_details_version_ordinal(), 1u);
  ASSERT_EQ(tracking_adapter->last_format_details_->mime_type(), "video/vp9");

  // Start a new stream (stream_lifetime_ordinal 3) with version ordinal 0 (dropping back to the
  // initial ordinal across a stream switch).
  fuchsia::media::FormatDetails details5;
  details5.set_format_details_version_ordinal(0);

  processor->QueueInputFormatDetails(3, std::move(details5));
  QueueInputPacket(processor, 3);
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [this] { return error_handler_ran_ || free_input_packet_count() == 5; }, kWaitTimeout));

  ASSERT_FALSE(error_handler_ran_);
  ASSERT_EQ(tracking_adapter->call_count_.load(), 5);
  ASSERT_TRUE(tracking_adapter->last_format_details_.has_value());
  ASSERT_EQ(tracking_adapter->last_format_details_->format_details_version_ordinal(), 0u);
  ASSERT_FALSE(tracking_adapter->last_format_details_->has_mime_type());
}

TEST_F(CodecImplFormatTrackingFailures, DecreasingFormatDetailsVersionFails) {
  StreamProcessorPtr processor;
  ConfigureInputBuffers(processor);

  // First call with version 1 (greater than 0)
  fuchsia::media::FormatDetails details1;
  details1.set_format_details_version_ordinal(1);
  processor->QueueInputFormatDetails(1, std::move(details1));
  QueueInputPacket(processor);

  auto* tracking_adapter = static_cast<FakeCodecAdapterWithFormatTracking*>(codec_adapter_);
  ASSERT_TRUE(RunLoopWithTimeoutOrUntil(
      [this] { return error_handler_ran_ || free_input_packet_count() == 1; }, kWaitTimeout));
  ASSERT_FALSE(error_handler_ran_);
  ASSERT_EQ(tracking_adapter->call_count_.load(), 1);

  // Second call with version 0 (less than 1)
  fuchsia::media::FormatDetails details2;
  details2.set_format_details_version_ordinal(0);
  processor->QueueInputFormatDetails(1, std::move(details2));

  ASSERT_TRUE(RunLoopWithTimeoutOrUntil([this] { return error_handler_ran_; }, kWaitTimeout));
  ASSERT_TRUE(error_handler_ran_);
}

}  // namespace
