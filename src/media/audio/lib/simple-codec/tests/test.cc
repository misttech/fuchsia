// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.audio.signalprocessing/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async-loop/loop.h>
#include <lib/async-testing/dispatcher_stub.h>
#include <lib/async/cpp/task.h>
#include <lib/async/sequence_id.h>
#include <lib/inspect/testing/cpp/zxtest/inspect.h>
#include <lib/simple-codec/simple-codec-client.h>
#include <lib/simple-codec/simple-codec-server.h>
#include <lib/sync/completion.h>

#include <array>
#include <atomic>
#include <memory>
#include <thread>

#include <zxtest/zxtest.h>

#include "src/devices/testing/mock-ddk/mock-device.h"

namespace audio {

namespace audio_fidl = ::fuchsia::hardware::audio;
namespace signal_fidl = ::fuchsia::hardware::audio::signalprocessing;

namespace {
const std::array<uint8_t, fuchsia::hardware::audio::UNIQUE_ID_SIZE> kTestUniqueId = {
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15,
};
const char* kExpectedUniqueIdStr = "000102030405060708090A0B0C0D0E0F";
const char* kTestManufacturer = "test man";
const char* kTestProduct = "test prod";
const uint32_t kTestInstanceCount = 123;

class SimpleCodecTest : public inspect::InspectTestHelper, public zxtest::Test {};

// Server tests.
class TestCodec : public SimpleCodecServer {
 public:
  explicit TestCodec(zx_device_t* parent) : SimpleCodecServer(parent) {}
  zx::result<fidl::ClientEnd<fuchsia_hardware_audio::Codec>> GetClient() {
    zx::channel channel_remote;
    fidl::ClientEnd<fuchsia_hardware_audio::Codec> channel_local;
    zx_status_t status = zx::channel::create(0, &channel_local.channel(), &channel_remote);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    status = CodecConnect(std::move(channel_remote));
    if (status != ZX_OK) {
      return zx::error(status);
    }
    return zx::success(std::move(channel_local));
  }
  zx_status_t Shutdown() override { return ZX_OK; }
  zx::result<DriverIds> Initialize() override {
    return zx::ok(DriverIds{.vendor_id = 0, .device_id = 0, .instance_count = kTestInstanceCount});
  }
  zx_status_t Reset() override { return ZX_ERR_NOT_SUPPORTED; }
  Info GetInfo() override {
    return {
        .unique_id = kTestUniqueId,
        .manufacturer = kTestManufacturer,
        .product_name = kTestProduct,
    };
  }
  zx_status_t Stop() override { return ZX_ERR_NOT_SUPPORTED; }
  zx_status_t Start() override { return ZX_OK; }
  DaiSupportedFormats GetDaiFormats() override { return {}; }
  zx::result<CodecFormatInfo> SetDaiFormat(const DaiFormat& format) override {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  GainFormat GetGainFormat() override {
    return GainFormat{
        .can_mute = true,
        .can_agc = true,
    };
  }
  GainState GetGainState() override { return gain_state_; }
  void SetGainState(GainState state) override { gain_state_ = state; }
  inspect::Inspector& inspect() { return SimpleCodecServer::inspect(); }
  uint64_t GetTopologyId() { return SimpleCodecServer::GetTopologyId(); }
  uint64_t GetGainPeId() { return SimpleCodecServer::GetGainPeId(); }
  uint64_t GetMutePeId() { return SimpleCodecServer::GetMutePeId(); }
  uint64_t GetAgcPeId() { return SimpleCodecServer::GetAgcPeId(); }
  // Binds |channel| through |dispatcher|, which must forward to dispatcher().
  zx_status_t BindClientOn(zx::channel channel, async_dispatcher_t* dispatcher) {
    return BindClient(std::move(channel), dispatcher);
  }

 private:
  GainState gain_state_ = {};
};

class TestCodecWithSignalProcessing final : public SimpleCodecServer,
                                            public signal_fidl::SignalProcessing {
 public:
  explicit TestCodecWithSignalProcessing(zx_device_t* parent) : SimpleCodecServer(parent) {}

  zx_status_t Shutdown() override { return ZX_OK; }
  zx::result<DriverIds> Initialize() override {
    return zx::ok(DriverIds{.vendor_id = 0, .device_id = 0, .instance_count = kTestInstanceCount});
  }
  zx_status_t Reset() override { return ZX_ERR_NOT_SUPPORTED; }
  Info GetInfo() override {
    return {
        .unique_id = std::array<uint8_t, audio_fidl::UNIQUE_ID_SIZE>(kTestUniqueId),
        .manufacturer = kTestManufacturer,
        .product_name = kTestProduct,
    };
  }
  zx_status_t Stop() override { return ZX_ERR_NOT_SUPPORTED; }
  zx_status_t Start() override { return ZX_OK; }
  bool SupportsSignalProcessing() override { return true; }
  void SignalProcessingConnect(
      fidl::InterfaceRequest<signal_fidl::SignalProcessing> signal_processing) override {
    signal_processing_binding_.emplace(this, std::move(signal_processing), dispatcher());
  }
  void GetElements(GetElementsCallback callback) override {
    signal_fidl::Element pe;
    pe.set_id(kAglPeId);
    pe.set_type(signal_fidl::ElementType::AUTOMATIC_GAIN_LIMITER);

    std::vector<signal_fidl::Element> pes;
    pes.emplace_back(std::move(pe));
    signal_fidl::Reader_GetElements_Response response(std::move(pes));
    signal_fidl::Reader_GetElements_Result result;
    result.set_response(std::move(response));
    callback(std::move(result));
  }
  void SetElementState(uint64_t processing_element_id, signal_fidl::SettableElementState state,
                       SetElementStateCallback callback) override {
    ASSERT_EQ(processing_element_id, kAglPeId);
    ASSERT_TRUE(state.has_started());
    agl_mode_ = state.started() && (!state.has_bypassed() || !state.bypassed());
    callback(signal_fidl::SignalProcessing_SetElementState_Result::WithResponse(
        signal_fidl::SignalProcessing_SetElementState_Response()));
  }
  void WatchElementState(uint64_t processing_element_id,
                         WatchElementStateCallback callback) override {}
  void GetTopologies(GetTopologiesCallback callback) override {
    signal_fidl::EdgePair edge;
    edge.processing_element_id_from = kAglPeId;
    edge.processing_element_id_to = kAglPeId;

    std::vector<signal_fidl::EdgePair> edges;
    edges.emplace_back(edge);

    signal_fidl::Topology topology;
    topology.set_id(kTopologyId);
    topology.set_processing_elements_edge_pairs(edges);

    std::vector<signal_fidl::Topology> topologies;
    topologies.emplace_back(std::move(topology));

    signal_fidl::Reader_GetTopologies_Response response(std::move(topologies));
    signal_fidl::Reader_GetTopologies_Result result;
    result.set_response(std::move(response));
    callback(std::move(result));
  }
  void WatchTopology(WatchTopologyCallback callback) override {}
  void SetTopology(uint64_t topology_id, SetTopologyCallback callback) override {
    if (topology_id != kTopologyId) {
      callback(signal_fidl::SignalProcessing_SetTopology_Result::WithErr(ZX_ERR_INVALID_ARGS));
      return;
    }
    callback(signal_fidl::SignalProcessing_SetTopology_Result::WithResponse(
        signal_fidl::SignalProcessing_SetTopology_Response()));
  }
  DaiSupportedFormats GetDaiFormats() override { return {}; }
  zx::result<CodecFormatInfo> SetDaiFormat(const DaiFormat& format) override {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  GainFormat GetGainFormat() override { return {}; }
  GainState GetGainState() override { return gain_state_; }
  void SetGainState(GainState state) override { gain_state_ = state; }

  bool agl_mode() const { return agl_mode_; }
  inspect::Inspector& inspect() { return SimpleCodecServer::inspect(); }

 protected:
  void handle_unknown_method(uint64_t ordinal, bool method_has_response) override {
    zxlogf(ERROR, "Unknown method with ordinal %zd", ordinal);
  }

 private:
  static constexpr uint64_t kAglPeId = 1;
  static constexpr uint64_t kTopologyId = 1;
  GainState gain_state_ = {};
  bool agl_mode_ = false;
  std::optional<fidl::Binding<signal_fidl::SignalProcessing>> signal_processing_binding_;
};

// Forwards every operation to its target dispatcher, except that the first BeginWait() blocks after
// it has been forwarded, until Resume() is called. This holds open the window between a binding
// starting to wait on its channel and the code that bound it continuing.
class PausingDispatcher final : public async::DispatcherStub {
 public:
  void set_target(async_dispatcher_t* target) { target_ = target; }

  zx::time Now() override { return zx::time(async_now(target_)); }
  zx_status_t BeginWait(async_wait_t* wait) override {
    zx_status_t status = async_begin_wait(target_, wait);
    if (status == ZX_OK && !paused_once_.exchange(true)) {
      sync_completion_signal(&paused_);
      sync_completion_wait(&resume_, ZX_TIME_INFINITE);
    }
    return status;
  }
  zx_status_t CancelWait(async_wait_t* wait) override { return async_cancel_wait(target_, wait); }
  zx_status_t PostTask(async_task_t* task) override { return async_post_task(target_, task); }
  zx_status_t CancelTask(async_task_t* task) override { return async_cancel_task(target_, task); }
  zx_status_t GetSequenceId(async_sequence_id_t* out_sequence_id, const char** out_error) override {
    return async_get_sequence_id(target_, out_sequence_id, out_error);
  }
  zx_status_t CheckSequenceId(async_sequence_id_t sequence_id, const char** out_error) override {
    return async_check_sequence_id(target_, sequence_id, out_error);
  }

  void WaitUntilPaused() { sync_completion_wait(&paused_, ZX_TIME_INFINITE); }
  void Resume() { sync_completion_signal(&resume_); }

 private:
  async_dispatcher_t* target_ = nullptr;
  std::atomic<bool> paused_once_ = false;
  sync_completion_t paused_;
  sync_completion_t resume_;
};

// Tracks the outcome of one pipelined request.
struct PipelinedReply {
  // Returns true if the request completed within |timeout|, with or without a reply.
  bool Completed(zx::duration timeout) {
    return sync_completion_wait(&done, timeout.get()) == ZX_OK;
  }
  // Waits for the request to complete and returns whether it got a reply.
  bool Succeeded() {
    sync_completion_wait(&done, ZX_TIME_INFINITE);
    return ok.load();
  }

  sync_completion_t done;
  std::atomic<bool> ok = false;
};

struct PipelinedReplies {
  PipelinedReply elements;
  PipelinedReply gain;
  PipelinedReply mute;
  PipelinedReply agc;
  PipelinedReply topologies;  // Requested last: its completion implies all were dispatched.
};

// Returns a callback that records the outcome of the request tracked by |reply|, keeping |replies|
// (which owns |reply|) alive for as long as the callback exists.
auto RecordOutcome(std::shared_ptr<PipelinedReplies> replies, PipelinedReply* reply) {
  return [replies = std::move(replies), reply](auto& result) {
    reply->ok = result.ok();
    sync_completion_signal(&reply->done);
  };
}

// Client side of a codec connection whose requests are all written before any server accepts the
// connection: SignalProcessingConnect on the codec channel, then GetElements, a first
// WatchElementState for the gain, mute and AGC elements, and GetTopologies on the new
// SignalProcessing channel. This is a superset of what SimpleCodecClient::SetCodec() writes before
// its first reply (SignalProcessingConnect and GetElements), so every request races the binding.
class PipelinedClient {
 public:
  PipelinedClient(async_dispatcher_t* dispatcher, TestCodec& codec)
      : replies_(std::make_shared<PipelinedReplies>()) {
    auto codec_endpoints = fidl::Endpoints<fuchsia_hardware_audio::Codec>::Create();
    auto signal_endpoints =
        fidl::Endpoints<fuchsia_hardware_audio_signalprocessing::SignalProcessing>::Create();
    codec_client_ = std::move(codec_endpoints.client);
    codec_server_ = codec_endpoints.server.TakeChannel();
    connect_status_ = fidl::WireCall(codec_client_)
                          ->SignalProcessingConnect(std::move(signal_endpoints.server))
                          .status();
    signal_client_.Bind(std::move(signal_endpoints.client), dispatcher,
                        fidl::ObserveTeardown([this] { sync_completion_signal(&torn_down_); }));
    signal_client_->GetElements().ThenExactlyOnce(RecordOutcome(replies_, &replies_->elements));
    signal_client_->WatchElementState(codec.GetGainPeId())
        .ThenExactlyOnce(RecordOutcome(replies_, &replies_->gain));
    signal_client_->WatchElementState(codec.GetMutePeId())
        .ThenExactlyOnce(RecordOutcome(replies_, &replies_->mute));
    signal_client_->WatchElementState(codec.GetAgcPeId())
        .ThenExactlyOnce(RecordOutcome(replies_, &replies_->agc));
    signal_client_->GetTopologies().ThenExactlyOnce(RecordOutcome(replies_, &replies_->topologies));
  }

  ~PipelinedClient() {
    // Close the SignalProcessing channel before the codec channel.
    signal_client_.AsyncTeardown();
    sync_completion_wait(&torn_down_, ZX_TIME_INFINITE);
  }

  zx_status_t connect_status() const { return connect_status_; }
  zx::channel TakeServerChannel() { return std::move(codec_server_); }
  PipelinedReplies& replies() { return *replies_; }

 private:
  std::shared_ptr<PipelinedReplies> replies_;
  fidl::ClientEnd<fuchsia_hardware_audio::Codec> codec_client_;
  zx::channel codec_server_;
  zx_status_t connect_status_ = ZX_ERR_INTERNAL;
  sync_completion_t torn_down_;
  fidl::WireSharedClient<fuchsia_hardware_audio_signalprocessing::SignalProcessing> signal_client_;
};

TEST_F(SimpleCodecTest, ChannelConnection) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();
  zx::result<fidl::ClientEnd<fuchsia_hardware_audio::Codec>> codec_client_end = codec->GetClient();
  ASSERT_OK(codec_client_end.status_value());
  SimpleCodecClient client;
  ASSERT_OK(client.SetCodec(std::move(*codec_client_end)));

  auto info = client.GetInfo();
  ASSERT_TRUE(info.is_ok());
  ASSERT_TRUE(info->unique_id.has_value());
  ASSERT_EQ(*info->unique_id, kTestUniqueId);
  ASSERT_EQ(info->manufacturer.compare(kTestManufacturer), 0);
  ASSERT_EQ(info->product_name.compare(kTestProduct), 0);
}

TEST_F(SimpleCodecTest, GainState) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();
  zx::result<fidl::ClientEnd<fuchsia_hardware_audio::Codec>> codec_client_end = codec->GetClient();
  ASSERT_OK(codec_client_end.status_value());
  SimpleCodecClient client;
  ASSERT_OK(client.SetCodec(std::move(*codec_client_end)));

  // Defaults to false/0db.
  {
    auto state = client.GetGainState();
    ASSERT_TRUE(state.is_ok());
    ASSERT_EQ(state->muted, false);
    ASSERT_EQ(state->agc_enabled, false);
    ASSERT_EQ(state->gain, 0.f);
  }

  // Still all set to false/0db.
  {
    auto state = client.GetGainState();
    ASSERT_TRUE(state.is_ok());
    ASSERT_EQ(state->muted, false);
    ASSERT_EQ(state->agc_enabled, false);
    ASSERT_EQ(state->gain, 0.f);
  }

  // Inspect also set to false/0db.
  {
    ASSERT_NO_FATAL_FAILURE(ReadInspect(codec->inspect().DuplicateVmo()));
    auto* simple_codec = hierarchy().GetByPath({"simple_codec"});
    ASSERT_NO_FATAL_FAILURE(
        CheckProperty(simple_codec->node(), "gain_db", inspect::DoublePropertyValue(0.f)));
    ASSERT_NO_FATAL_FAILURE(
        CheckProperty(simple_codec->node(), "muted", inspect::BoolPropertyValue(false)));
  }

  // Set gain now.
  client.SetGainState({.gain = 1.23f, .muted = true, .agc_enabled = true});

  // Values updated eventually.
  for (;;) {
    auto state = client.GetGainState();
    ASSERT_TRUE(state.is_ok());
    if (state->muted && state->agc_enabled && state->gain == 1.23f) {
      break;
    }
  }

  // Inspect values updated too.
  {
    ASSERT_NO_FATAL_FAILURE(ReadInspect(codec->inspect().DuplicateVmo()));
    auto* simple_codec = hierarchy().GetByPath({"simple_codec"});
    ASSERT_NO_FATAL_FAILURE(
        CheckProperty(simple_codec->node(), "gain_db", inspect::DoublePropertyValue(1.23f)));
    ASSERT_NO_FATAL_FAILURE(
        CheckProperty(simple_codec->node(), "muted", inspect::BoolPropertyValue(true)));
  }
}

TEST_F(SimpleCodecTest, DefaultTopology) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();

  zx::channel channel_remote, channel_local;
  ASSERT_OK(zx::channel::create(0, &channel_local, &channel_remote));
  ASSERT_OK(codec->CodecConnect(std::move(channel_remote)));
  audio_fidl::CodecSyncPtr codec_client;
  codec_client.Bind(std::move(channel_local));

  fidl::InterfaceHandle<signal_fidl::SignalProcessing> signal_processing_handle;
  fidl::InterfaceRequest<signal_fidl::SignalProcessing> signal_processing_request =
      signal_processing_handle.NewRequest();
  ASSERT_OK(codec_client->SignalProcessingConnect(std::move(signal_processing_request)));
  fidl::SynchronousInterfacePtr signal_processing_client = signal_processing_handle.BindSync();

  // We should get 3 PEs with gain, mute and AGC support.
  {
    signal_fidl::Reader_GetElements_Result result;
    ASSERT_OK(signal_processing_client->GetElements(&result));
    ASSERT_FALSE(result.is_err());
    ASSERT_EQ(result.response().processing_elements.size(), 3);
    ASSERT_EQ(result.response().processing_elements[0].type(), signal_fidl::ElementType::GAIN);
    ASSERT_EQ(result.response().processing_elements[1].type(), signal_fidl::ElementType::MUTE);
    ASSERT_EQ(result.response().processing_elements[2].type(),
              signal_fidl::ElementType::AUTOMATIC_GAIN_CONTROL);
  }

  // Only one topology.
  {
    signal_fidl::Reader_GetTopologies_Result result;
    ASSERT_OK(signal_processing_client->GetTopologies(&result));
    ASSERT_FALSE(result.is_err());
    ASSERT_EQ(result.response().topologies.size(), 1);
    ASSERT_EQ(result.response().topologies[0].id(), codec->GetTopologyId());
    ASSERT_EQ(result.response().topologies[0].processing_elements_edge_pairs().size(), 2);
    ASSERT_EQ(result.response()
                  .topologies[0]
                  .processing_elements_edge_pairs()[0]
                  .processing_element_id_from,
              codec->GetGainPeId());
    ASSERT_EQ(result.response()
                  .topologies[0]
                  .processing_elements_edge_pairs()[0]
                  .processing_element_id_to,
              codec->GetMutePeId());
    ASSERT_EQ(result.response()
                  .topologies[0]
                  .processing_elements_edge_pairs()[1]
                  .processing_element_id_from,
              codec->GetMutePeId());
    ASSERT_EQ(result.response()
                  .topologies[0]
                  .processing_elements_edge_pairs()[1]
                  .processing_element_id_to,
              codec->GetAgcPeId());
  }

  // Set the only topology must work.
  {
    signal_fidl::SignalProcessing_SetTopology_Result result;
    ASSERT_OK(signal_processing_client->SetTopology(codec->GetTopologyId(), &result));
    ASSERT_FALSE(result.is_err());
  }

  // Set the an incorrect topology id must fail.
  {
    signal_fidl::SignalProcessing_SetTopology_Result result;
    ASSERT_OK(signal_processing_client->SetTopology(codec->GetTopologyId() + 1, &result));
    ASSERT_TRUE(result.is_err());
  }
}

TEST_F(SimpleCodecTest, SetDaiFormat) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();
  zx::result<fidl::ClientEnd<fuchsia_hardware_audio::Codec>> codec_client_end = codec->GetClient();
  ASSERT_OK(codec_client_end.status_value());
  SimpleCodecClient client;
  client.SetCodec(std::move(*codec_client_end));

  DaiFormat format = {
      .sample_format = audio_fidl::DaiSampleFormat::PCM_SIGNED,
      .frame_format = FrameFormat::I2S,
  };
  zx::result<CodecFormatInfo> codec_format_info = client.SetDaiFormat(format);
  ASSERT_EQ(codec_format_info.status_value(), ZX_ERR_NOT_SUPPORTED);
}

TEST_F(SimpleCodecTest, PlugStateHardwired) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();

  // First client.
  {
    zx::channel channel_remote, channel_local;
    ASSERT_OK(zx::channel::create(0, &channel_local, &channel_remote));
    ASSERT_OK(codec->CodecConnect(std::move(channel_remote)));
    audio_fidl::CodecSyncPtr codec_client;
    codec_client.Bind(std::move(channel_local));

    audio_fidl::PlugState out_plug_state;
    ASSERT_OK(codec_client->WatchPlugState(&out_plug_state));
    ASSERT_EQ(out_plug_state.plugged(), true);
    ASSERT_GT(out_plug_state.plug_state_time(), 0);
  }
  // Second client.
  {
    zx::channel channel_remote, channel_local;
    ASSERT_OK(zx::channel::create(0, &channel_local, &channel_remote));
    ASSERT_OK(codec->CodecConnect(std::move(channel_remote)));
    audio_fidl::CodecSyncPtr codec_client;
    codec_client.Bind(std::move(channel_local));

    audio_fidl::PlugState out_plug_state;
    ASSERT_OK(codec_client->WatchPlugState(&out_plug_state));
    ASSERT_EQ(out_plug_state.plugged(), true);
    ASSERT_GT(out_plug_state.plug_state_time(), 0);
  }
}

TEST_F(SimpleCodecTest, WatchPlugStateWhilePending) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();

  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);

  zx::channel channel_remote, channel_local;
  ASSERT_OK(zx::channel::create(0, &channel_local, &channel_remote));
  ASSERT_OK(codec->CodecConnect(std::move(channel_remote)));
  audio_fidl::CodecPtr codec_client;
  codec_client.Bind(std::move(channel_local), loop.dispatcher());

  std::atomic<bool> closed = false;
  codec_client.set_error_handler([&closed](zx_status_t status) {
    EXPECT_EQ(ZX_ERR_BAD_STATE, status);
    closed = true;
  });

  codec_client->WatchPlugState([](audio_fidl::PlugState plug_state) {});
  codec_client->WatchPlugState([](audio_fidl::PlugState plug_state) {});
  codec_client->WatchPlugState([](audio_fidl::PlugState plug_state) {});

  zx::time deadline = zx::deadline_after(zx::sec(5));
  while (!closed.load() && zx::clock::get_monotonic() < deadline) {
    loop.RunUntilIdle();
    zx::nanosleep(zx::deadline_after(zx::msec(10)));
  }
  EXPECT_TRUE(closed.load());
}

TEST_F(SimpleCodecTest, GainWithClientViaSignalProcessingApi) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();

  zx::channel channel_remote, channel_local;
  ASSERT_OK(zx::channel::create(0, &channel_local, &channel_remote));
  ASSERT_OK(codec->CodecConnect(std::move(channel_remote)));
  audio_fidl::CodecSyncPtr codec_client;
  codec_client.Bind(std::move(channel_local));

  fidl::InterfaceHandle<signal_fidl::SignalProcessing> signal_processing_handle;
  fidl::InterfaceRequest<signal_fidl::SignalProcessing> signal_processing_request =
      signal_processing_handle.NewRequest();
  ASSERT_OK(codec_client->SignalProcessingConnect(std::move(signal_processing_request)));
  fidl::SynchronousInterfacePtr signal_processing_client = signal_processing_handle.BindSync();

  // We should get 3 PEs with gain, mute and AGC support.
  signal_fidl::Reader_GetElements_Result result_elements;
  ASSERT_OK(signal_processing_client->GetElements(&result_elements));
  ASSERT_FALSE(result_elements.is_err());
  ASSERT_EQ(result_elements.response().processing_elements.size(), 3);
  ASSERT_EQ(result_elements.response().processing_elements[0].type(),
            signal_fidl::ElementType::GAIN);
  ASSERT_EQ(result_elements.response().processing_elements[1].type(),
            signal_fidl::ElementType::MUTE);
  ASSERT_EQ(result_elements.response().processing_elements[2].type(),
            signal_fidl::ElementType::AUTOMATIC_GAIN_CONTROL);

  // Control with test gain.
  signal_fidl::SignalProcessing_SetElementState_Result result_gain;
  signal_fidl::SettableElementState state_gain;
  constexpr float kTestGain = 12.3f;
  signal_fidl::GainElementState gain_state;
  gain_state.set_gain(kTestGain);
  state_gain.set_bypassed(false).set_started(true).set_type_specific(
      signal_fidl::SettableTypeSpecificElementState::WithGain(std::move(gain_state)));
  ASSERT_OK(signal_processing_client->SetElementState(
      result_elements.response().processing_elements[0].id(), std::move(state_gain), &result_gain));
  ASSERT_FALSE(result_gain.is_err());

  // Do a synchronous FIDL call to flush messages on the channel and force the server to update the
  // gain state.
  ASSERT_OK(signal_processing_client->GetElements(&result_elements));

  ASSERT_EQ(codec->GetGainState().gain, kTestGain);

  // Control with no gain returns an error.
  signal_fidl::SignalProcessing_SetElementState_Result result_no_gain;
  signal_fidl::SettableElementState state_no_gain;
  state_no_gain.set_bypassed(false).set_started(true);
  ASSERT_OK(signal_processing_client->SetElementState(
      result_elements.response().processing_elements[0].id(), std::move(state_no_gain),
      &result_no_gain));
  ASSERT_TRUE(result_no_gain.is_err());
  ASSERT_EQ(result_no_gain.err(), ZX_ERR_INVALID_ARGS);

  // Do a synchronous FIDL call to flush messages on the channel and force the server to make
  // any possible updates to the gain state.
  ASSERT_OK(signal_processing_client->GetElements(&result_elements));

  ASSERT_EQ(codec->GetGainState().gain, kTestGain);  // Keeps old gain.
}

TEST_F(SimpleCodecTest, AglStateServerWithClientViaSignalProcessingApi) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodecWithSignalProcessing>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodecWithSignalProcessing>();

  zx::channel channel_remote, channel_local;
  ASSERT_OK(zx::channel::create(0, &channel_local, &channel_remote));
  ASSERT_OK(codec->CodecConnect(std::move(channel_remote)));
  audio_fidl::CodecSyncPtr codec_client;
  codec_client.Bind(std::move(channel_local));

  fidl::InterfaceHandle<signal_fidl::SignalProcessing> signal_processing_handle;
  fidl::InterfaceRequest<signal_fidl::SignalProcessing> signal_processing_request =
      signal_processing_handle.NewRequest();
  ASSERT_OK(codec_client->SignalProcessingConnect(std::move(signal_processing_request)));
  fidl::SynchronousInterfacePtr signal_processing_client = signal_processing_handle.BindSync();

  // We should get one PE with AGL support.
  signal_fidl::Reader_GetElements_Result result;
  ASSERT_OK(signal_processing_client->GetElements(&result));
  ASSERT_FALSE(result.is_err());
  ASSERT_EQ(result.response().processing_elements.size(), 1);
  ASSERT_EQ(result.response().processing_elements[0].type(),
            signal_fidl::ElementType::AUTOMATIC_GAIN_LIMITER);
  ASSERT_FALSE(codec->agl_mode());

  // Control with enabled = true.
  signal_fidl::SignalProcessing_SetElementState_Result result_enable;
  signal_fidl::SettableElementState state_enable;
  state_enable.set_bypassed(false).set_started(true);
  ASSERT_OK(signal_processing_client->SetElementState(result.response().processing_elements[0].id(),
                                                      std::move(state_enable), &result_enable));
  ASSERT_FALSE(result_enable.is_err());
  ASSERT_TRUE(codec->agl_mode());
}

TEST_F(SimpleCodecTest, InspectDefaultState) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();

  zx::channel channel_remote, channel_local;
  ASSERT_OK(zx::channel::create(0, &channel_local, &channel_remote));
  ASSERT_OK(codec->CodecConnect(std::move(channel_remote)));
  audio_fidl::CodecSyncPtr codec_client;
  codec_client.Bind(std::move(channel_local));

  // Check inspect state.
  ASSERT_NO_FATAL_FAILURE(ReadInspect(codec->inspect().DuplicateVmo()));
  auto* simple_codec = hierarchy().GetByPath({"simple_codec"});
  ASSERT_TRUE(simple_codec);
  ASSERT_NO_FATAL_FAILURE(
      CheckProperty(simple_codec->node(), "state", inspect::StringPropertyValue("created")));
  ASSERT_NO_FATAL_FAILURE(
      CheckProperty(simple_codec->node(), "start_time", inspect::IntPropertyValue(0)));
  ASSERT_NO_FATAL_FAILURE(CheckProperty(simple_codec->node(), "unique_id",
                                        inspect::StringPropertyValue(kExpectedUniqueIdStr)));
}

TEST_F(SimpleCodecTest, InspectNoUniqueId) {
  struct TestCodecNoUniqueId : public TestCodec {
    explicit TestCodecNoUniqueId(zx_device_t* parent) : TestCodec(parent) {}
    zx::result<DriverIds> Initialize() override {
      return zx::ok(
          DriverIds{.vendor_id = 0, .device_id = 0, .instance_count = kTestInstanceCount});
    }
    Info GetInfo() override { return {}; }
  };
  auto fake_parent = MockDevice::FakeRootParent();

  SimpleCodecServer::CreateAndAddToDdk<TestCodecNoUniqueId>(fake_parent.get());
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();

  zx::channel channel_remote, channel_local;
  ASSERT_OK(zx::channel::create(0, &channel_local, &channel_remote));
  ASSERT_OK(codec->CodecConnect(std::move(channel_remote)));
  audio_fidl::CodecSyncPtr codec_client;
  codec_client.Bind(std::move(channel_local));

  // Check inspect state.
  ASSERT_NO_FATAL_FAILURE(ReadInspect(codec->inspect().DuplicateVmo()));
  auto* simple_codec = hierarchy().GetByPath({"simple_codec"});
  ASSERT_TRUE(simple_codec);
  ASSERT_NO_FATAL_FAILURE(
      CheckProperty(simple_codec->node(), "state", inspect::StringPropertyValue("created")));
  ASSERT_NO_FATAL_FAILURE(
      CheckProperty(simple_codec->node(), "start_time", inspect::IntPropertyValue(0)));
  ASSERT_NO_FATAL_FAILURE(
      CheckProperty(simple_codec->node(), "unique_id",
                    inspect::StringPropertyValue(std::to_string(kTestInstanceCount))));
}

TEST_F(SimpleCodecTest, MultipleClients) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();

  SimpleCodecClient codec_clients[3];
  for (auto& codec_client : codec_clients) {
    zx::result<fidl::ClientEnd<fuchsia_hardware_audio::Codec>> codec_client_end =
        codec->GetClient();
    ASSERT_OK(codec_client_end.status_value());
    ASSERT_OK(codec_client.SetCodec(std::move(*codec_client_end)));
  }

  {
    auto state = codec_clients[0].GetGainState();
    ASSERT_TRUE(state.is_ok());
    EXPECT_EQ(state->muted, false);
    EXPECT_EQ(state->agc_enabled, false);
    EXPECT_EQ(state->gain, 0.f);
  }

  codec_clients[1].SetGainState({.gain = 1.23f, .muted = true, .agc_enabled = false});

  // Wait for client 0 to be notified of the new gain state.
  for (;;) {
    auto state = codec_clients[0].GetGainState();
    ASSERT_TRUE(state.is_ok());
    if (state->muted) {
      break;
    }
  }

  {
    auto state = codec_clients[0].GetGainState();
    ASSERT_TRUE(state.is_ok());
    EXPECT_EQ(state->muted, true);
    EXPECT_EQ(state->agc_enabled, false);
    EXPECT_EQ(state->gain, 1.23f);
  }

  codec_clients[0].SetGainState({.gain = 5.67f, .muted = true, .agc_enabled = true});

  for (;;) {
    auto state = codec_clients[2].GetGainState();
    ASSERT_TRUE(state.is_ok());
    if (state->agc_enabled) {
      break;
    }
  }

  {
    auto state = codec_clients[2].GetGainState();
    ASSERT_TRUE(state.is_ok());
    EXPECT_EQ(state->muted, true);
    EXPECT_EQ(state->agc_enabled, true);
    EXPECT_EQ(state->gain, 5.67f);
  }
}

TEST_F(SimpleCodecTest, CloseChannel) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();
  zx::result<fidl::ClientEnd<fuchsia_hardware_audio::Codec>> codec_client_end = codec->GetClient();
  ASSERT_OK(codec_client_end.status_value());

  SimpleCodecClient codec_client;
  ASSERT_OK(codec_client.SetCodec(std::move(*codec_client_end)));

  codec_client.SetGainState({.gain = 1.23f, .muted = true, .agc_enabled = false});

  // Values updated eventually.
  for (;;) {
    auto state = codec_client.GetGainState();
    ASSERT_TRUE(state.is_ok());
    if (state->muted && !state->agc_enabled && state->gain == 1.23f) {
      break;
    }
  }

  EXPECT_OK(codec_client.Start());

  // TestCodec doesn't support this, so calling it should cause the server to unbind.
  EXPECT_NOT_OK(codec_client.Stop());

  // This should fail now that our channel has been closed.
  EXPECT_NOT_OK(codec_client.Start());
}

TEST_F(SimpleCodecTest, RebindClient) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();
  zx::result<fidl::ClientEnd<fuchsia_hardware_audio::Codec>> codec_client_end = codec->GetClient();
  ASSERT_OK(codec_client_end.status_value());

  SimpleCodecClient codec_client;
  ASSERT_OK(codec_client.SetCodec(std::move(*codec_client_end)));

  codec_client.SetGainState({.gain = 1.23f, .muted = true, .agc_enabled = false});

  // Values updated eventually.
  for (;;) {
    auto state = codec_client.GetGainState();
    ASSERT_TRUE(state.is_ok());
    if (state->muted && !state->agc_enabled && state->gain == 1.23f) {
      break;
    }
  }

  // Do a synchronous FIDL call to flush messages on the channel and force the server to update the
  // gain state.
  EXPECT_OK(codec_client.Start());

  zx::result<fidl::ClientEnd<fuchsia_hardware_audio::Codec>> codec_client_end2 = codec->GetClient();
  ASSERT_OK(codec_client_end2.status_value());
  ASSERT_OK(codec_client.SetCodec(std::move(*codec_client_end2)));

  // Values updated eventually.
  for (;;) {
    auto state = codec_client.GetGainState();
    ASSERT_TRUE(state.is_ok());
    if (state->muted && !state->agc_enabled && state->gain == 1.23f) {
      break;
    }
  }
}

// Verifies pipelined requests are not served until binding is set up, then all get replies.
TEST_F(SimpleCodecTest, PipelinedRequestsWaitForBinding) {
  // The connection is bound through a dispatcher that pauses right after the binding starts
  // waiting on its channel. Declared first: the codec's binding may refer to it until release.
  PausingDispatcher pausing;
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();
  pausing.set_target(codec->dispatcher());

  async::Loop client_loop(&kAsyncLoopConfigNeverAttachToThread);
  ASSERT_OK(client_loop.StartThread("simple-codec-test-client"));
  PipelinedClient client(client_loop.dispatcher(), *codec);
  ASSERT_OK(client.connect_status());

  // Accept the connection from a thread other than the codec's loop thread.
  std::thread connector([codec, &pausing, channel = client.TakeServerChannel()]() mutable {
    EXPECT_OK(codec->BindClientOn(std::move(channel), &pausing));
  });
  pausing.WaitUntilPaused();
  // No request may be served while the binding is still being set up. Probe the first request,
  // GetElements, whose handling takes no lock and does not depend on per-instance state. This is
  // the test's only timeout, since it checks that something does not happen.
  bool served_while_binding = client.replies().elements.Completed(zx::msec(500));
  pausing.Resume();
  connector.join();
  EXPECT_FALSE(served_while_binding);

  // Every pipelined request, including each first WatchElementState, must be answered.
  EXPECT_TRUE(client.replies().elements.Succeeded());
  EXPECT_TRUE(client.replies().gain.Succeeded());
  EXPECT_TRUE(client.replies().mute.Succeeded());
  EXPECT_TRUE(client.replies().agc.Succeeded());
  EXPECT_TRUE(client.replies().topologies.Succeeded());
}

// Verifies requests pipelined before an off-loop CodecConnect() all get replies.
TEST_F(SimpleCodecTest, PipelinedRequestsBeforeCodecConnect) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();

  async::Loop client_loop(&kAsyncLoopConfigNeverAttachToThread);
  ASSERT_OK(client_loop.StartThread("simple-codec-test-client"));
  PipelinedClient client(client_loop.dispatcher(), *codec);
  ASSERT_OK(client.connect_status());

  // Accept the connection from a thread other than the codec's loop thread, as drivers do.
  std::thread connector([codec, channel = client.TakeServerChannel()]() mutable {
    EXPECT_OK(codec->CodecConnect(std::move(channel)));
  });
  connector.join();

  // Every pipelined request, including each first WatchElementState, must be answered.
  EXPECT_TRUE(client.replies().elements.Succeeded());
  EXPECT_TRUE(client.replies().gain.Succeeded());
  EXPECT_TRUE(client.replies().mute.Succeeded());
  EXPECT_TRUE(client.replies().agc.Succeeded());
  EXPECT_TRUE(client.replies().topologies.Succeeded());
}

// Verifies that connecting an invalid channel is rejected instead of leaving an unbound instance.
TEST_F(SimpleCodecTest, CodecConnectRejectsInvalidChannel) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();

  EXPECT_EQ(codec->CodecConnect(zx::channel()), ZX_ERR_INVALID_ARGS);
}

// Verifies CodecConnect() fails and closes the channel once the codec's loop is shut down.
TEST_F(SimpleCodecTest, CodecConnectAfterLoopShutdownClosesChannel) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();

  // DdkRelease() shuts the loop down again when the device is released, which is then a no-op.
  async_loop_shutdown(async_loop_from_dispatcher(codec->dispatcher()));

  zx::channel channel_remote, channel_local;
  ASSERT_OK(zx::channel::create(0, &channel_local, &channel_remote));
  EXPECT_EQ(codec->CodecConnect(std::move(channel_remote)), ZX_ERR_BAD_STATE);
  // The task that owned the channel was destroyed before CodecConnect() returned.
  zx_signals_t observed = 0;
  EXPECT_OK(channel_local.wait_one(ZX_CHANNEL_PEER_CLOSED, zx::time::infinite_past(), &observed));
  EXPECT_TRUE(observed & ZX_CHANNEL_PEER_CLOSED);
}

// Verifies a connection whose binding fails is closed and the codec still serves new clients.
TEST_F(SimpleCodecTest, CodecConnectBindFailureClosesChannel) {
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();

  // Without ZX_RIGHT_WAIT the binding cannot wait on the channel, so Bind() fails on the loop.
  zx::channel channel_remote, channel_local, channel_no_wait;
  ASSERT_OK(zx::channel::create(0, &channel_local, &channel_remote));
  ASSERT_OK(channel_remote.replace(ZX_RIGHTS_IO | ZX_RIGHT_TRANSFER, &channel_no_wait));
  ASSERT_OK(codec->CodecConnect(std::move(channel_no_wait)));
  zx_signals_t observed = 0;
  EXPECT_OK(channel_local.wait_one(ZX_CHANNEL_PEER_CLOSED, zx::time::infinite(), &observed));
  EXPECT_TRUE(observed & ZX_CHANNEL_PEER_CLOSED);

  // The failed instance was erased without breaking the codec: a new client gets replies.
  async::Loop client_loop(&kAsyncLoopConfigNeverAttachToThread);
  ASSERT_OK(client_loop.StartThread("simple-codec-test-client"));
  PipelinedClient client(client_loop.dispatcher(), *codec);
  ASSERT_OK(client.connect_status());
  ASSERT_OK(codec->CodecConnect(client.TakeServerChannel()));
  EXPECT_TRUE(client.replies().elements.Succeeded());
  EXPECT_TRUE(client.replies().gain.Succeeded());
  EXPECT_TRUE(client.replies().topologies.Succeeded());
}

// Verifies a connection still pending at release is closed, so its client sees PEER_CLOSED.
TEST_F(SimpleCodecTest, PendingConnectionClosedOnRelease) {
  // Declared first, so that it outlives the codec's loop and the task below that signals it.
  sync_completion_t loop_busy;
  auto fake_parent = MockDevice::FakeRootParent();

  ASSERT_OK(SimpleCodecServer::CreateAndAddToDdk<TestCodec>(fake_parent.get()));
  auto* child_dev = fake_parent->GetLatestChild();
  ASSERT_NOT_NULL(child_dev);
  auto codec = child_dev->GetDeviceContext<TestCodec>();

  // Keep the codec's loop busy until its shutdown starts, so that work queued behind this task is
  // still pending when the loop shuts down.
  async_loop_t* codec_loop = async_loop_from_dispatcher(codec->dispatcher());
  ASSERT_OK(async::PostTask(codec->dispatcher(), [codec_loop, &loop_busy] {
    sync_completion_signal(&loop_busy);
    while (async_loop_get_state(codec_loop) != ASYNC_LOOP_SHUTDOWN) {
      zx::nanosleep(zx::deadline_after(zx::msec(1)));
    }
  }));
  sync_completion_wait(&loop_busy, ZX_TIME_INFINITE);

  zx::channel channel_remote, channel_local;
  ASSERT_OK(zx::channel::create(0, &channel_local, &channel_remote));
  ASSERT_OK(codec->CodecConnect(std::move(channel_remote)));

  // Shuts the codec's loop down and deletes the codec.
  child_dev->ReleaseOp();

  // The task that owned the channel was destroyed before ReleaseOp() returned.
  zx_signals_t observed = 0;
  EXPECT_OK(channel_local.wait_one(ZX_CHANNEL_PEER_CLOSED, zx::time::infinite_past(), &observed));
  EXPECT_TRUE(observed & ZX_CHANNEL_PEER_CLOSED);
}

}  // namespace

}  // namespace audio
