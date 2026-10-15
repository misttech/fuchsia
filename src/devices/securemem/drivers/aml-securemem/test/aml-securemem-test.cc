// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.sysmem/cpp/wire_test_base.h>
#include <fidl/fuchsia.hardware.tee/cpp/wire.h>
#include <fidl/fuchsia.sysmem2/cpp/wire.h>
#include <fidl/fuchsia.tee/cpp/wire.h>
#include <lib/async-loop/default.h>
#include <lib/async/cpp/task.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/fake-platform-device/cpp/fake-pdev.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/fdf/env.h>
#include <lib/fpromise/result.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/resource.h>
#include <zircon/limits.h>

#include <bind/fuchsia/amlogic/platform/sysmem/heap/cpp/bind.h>
#include <fbl/array.h>
#include <zxtest/zxtest.h>

#include "device.h"
#include "src/devices/testing/mock-ddk/mock-device.h"

class FakeSysmem : public fidl::testing::WireTestBase<fuchsia_hardware_sysmem::Sysmem> {
 public:
  void RegisterHeap(RegisterHeapRequestView request,
                    RegisterHeapCompleter::Sync& completer) override {
    // Currently, do nothing
  }

  void RegisterSecureMem(RegisterSecureMemRequestView request,
                         RegisterSecureMemCompleter::Sync& completer) override {
    // Stash the tee_connection_ so the channel can stay open long enough to avoid a potentially
    // confusing error message during the test.
    tee_connection_ = std::move(request->secure_mem_connection);
  }

  void UnregisterSecureMem(UnregisterSecureMemCompleter::Sync& completer) override {
    // Currently, do nothing
    completer.ReplySuccess();
  }

  void NotImplemented_(const std::string& name, ::fidl::CompleterBase& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  fidl::ClientEnd<fuchsia_sysmem2::SecureMem> take_tee_connection() {
    return std::move(tee_connection_);
  }

  void Connect(fidl::ServerEnd<fuchsia_hardware_sysmem::Sysmem> request) {
    sysmem_bindings_.AddBinding(async_get_default_dispatcher(), std::move(request), this,
                                fidl::kIgnoreBindingClosure);
  }

 private:
  fidl::ClientEnd<fuchsia_sysmem2::SecureMem> tee_connection_;
  fidl::ServerBindingGroup<fuchsia_hardware_sysmem::Sysmem> sysmem_bindings_;
};

class FakeTeeApplication : public fidl::WireServer<fuchsia_tee::Application> {
 public:
  void OpenSession2(OpenSession2RequestView request,
                    OpenSession2Completer::Sync& completer) override {
    fidl::Arena arena;
    auto res = fuchsia_tee::wire::OpResult::Builder(arena);
    res.return_code(TEEC_SUCCESS);
    res.return_origin(fuchsia_tee::wire::ReturnOrigin::kTrustedApplication);
    completer.Reply(1 /* session_id */, res.Build());
  }

  void CloseSession(CloseSessionRequestView request,
                    CloseSessionCompleter::Sync& completer) override {
    completer.Reply();
  }

  void InvokeCommand(InvokeCommandRequestView request,
                     InvokeCommandCompleter::Sync& completer) override {
    invoke_command_count_++;
    uint32_t secmem_return_code = 0;
    if (invoke_command_count_ == 1) {
      secmem_return_code = 0xFFFF0000;
    } else if (invoke_command_count_ == 7) {
      secmem_return_code = 0xFFFF0000;
    }

    fidl::Arena arena;
    auto val_builder = fuchsia_tee::wire::Value::Builder(arena);
    val_builder.direction(fuchsia_tee::wire::Direction::kOutput);
    val_builder.a(secmem_return_code);

    auto buf_builder = fuchsia_tee::wire::Buffer::Builder(arena);
    buf_builder.direction(fuchsia_tee::wire::Direction::kInout);
    buf_builder.offset(0);
    if (!request->parameter_set.empty() && request->parameter_set[0].is_buffer()) {
      auto& req_buf = request->parameter_set[0].buffer();
      if (req_buf.has_size() && req_buf.size() > 0) {
        zx::vmo vmo;
        zx::vmo::create(req_buf.size(), 0, &vmo);
        buf_builder.vmo(std::move(vmo));
        buf_builder.size(req_buf.size());
      } else {
        buf_builder.size(0);
      }
    } else {
      buf_builder.size(0);
    }

    fidl::VectorView<fuchsia_tee::wire::Parameter> out_params(arena, 4);
    out_params[0] = fuchsia_tee::wire::Parameter::WithBuffer(arena, buf_builder.Build());
    out_params[1] = fuchsia_tee::wire::Parameter::WithNone(fuchsia_tee::wire::None{});
    out_params[2] = fuchsia_tee::wire::Parameter::WithNone(fuchsia_tee::wire::None{});
    out_params[3] = fuchsia_tee::wire::Parameter::WithValue(arena, val_builder.Build());

    auto res = fuchsia_tee::wire::OpResult::Builder(arena);
    res.return_code(TEEC_SUCCESS);
    res.return_origin(fuchsia_tee::wire::ReturnOrigin::kTrustedApplication);
    res.parameter_set(out_params);

    completer.Reply(res.Build());
  }

  uint32_t invoke_command_count() const { return invoke_command_count_; }

 private:
  uint32_t invoke_command_count_ = 0;
};

// We cover the code involved in supporting non-VDEC secure memory and VDEC secure memory in
// sysmem-test, so this fake doesn't really need to do much yet.
class FakeTee : public fidl::WireServer<fuchsia_hardware_tee::DeviceConnector> {
 public:
  void ConnectToApplication(ConnectToApplicationRequestView request,
                            ConnectToApplicationCompleter::Sync& completer) override {
    app_bindings_.AddBinding(async_get_default_dispatcher(),
                             std::move(request->application_request), &app_,
                             fidl::kIgnoreBindingClosure);
  }

  void ConnectToDeviceInfo(ConnectToDeviceInfoRequestView request,
                           ConnectToDeviceInfoCompleter::Sync& completer) override {}

  fuchsia_hardware_tee::Service::InstanceHandler CreateInstanceHandler() {
    return fuchsia_hardware_tee::Service::InstanceHandler(
        {.device_connector = bindings_.CreateHandler(this, async_get_default_dispatcher(),
                                                     fidl::kIgnoreBindingClosure)});
  }

  FakeTeeApplication& app() { return app_; }

 private:
  FakeTeeApplication app_;
  fidl::ServerBindingGroup<fuchsia_hardware_tee::DeviceConnector> bindings_;
  fidl::ServerBindingGroup<fuchsia_tee::Application> app_bindings_;
};

class AmlogicSecureMemTest : public zxtest::Test {
 protected:
  AmlogicSecureMemTest() {
    ASSERT_OK(incoming_loop_.StartThread("incoming"));

    // Create pdev fragment
    fdf_fake::FakePDev::Config config{.use_fake_bti = true};
    pdev_.SyncCall(&fdf_fake::FakePDev::SetConfig, std::move(config));
    auto pdev_handler =
        pdev_.SyncCall(&fdf_fake::FakePDev::GetInstanceHandler, async_patterns::PassDispatcher);
    auto pdev_endpoints = fidl::Endpoints<fuchsia_io::Directory>::Create();
    root_->AddFidlService(fuchsia_hardware_platform_device::Service::Name,
                          std::move(pdev_endpoints.client), "pdev");

    // Create sysmem fragment
    root_->AddNsProtocol<fuchsia_hardware_sysmem::Sysmem>(
        [&](auto request) { sysmem_.SyncCall(&FakeSysmem::Connect, std::move(request)); });

    // Create tee fragment
    auto tee_handler = tee_.SyncCall(&FakeTee::CreateInstanceHandler);
    auto tee_endpoints = fidl::Endpoints<fuchsia_io::Directory>::Create();
    root_->AddFidlService(fuchsia_hardware_tee::Service::Name, std::move(tee_endpoints.client),
                          "tee");

    outgoing_.SyncCall(
        [pdev_server = std::move(pdev_endpoints.server),
         tee_server = std::move(tee_endpoints.server), pdev_handler = std::move(pdev_handler),
         tee_handler = std::move(tee_handler)](component::OutgoingDirectory* outgoing) mutable {
          ZX_ASSERT(outgoing->Serve(std::move(pdev_server)).is_ok());
          ZX_ASSERT(outgoing->Serve(std::move(tee_server)).is_ok());

          ZX_ASSERT(
              outgoing
                  ->AddService<fuchsia_hardware_platform_device::Service>(std::move(pdev_handler))
                  .is_ok());
          ZX_ASSERT(
              outgoing->AddService<fuchsia_hardware_tee::Service>(std::move(tee_handler)).is_ok());
        });

    libsync::Completion completion;
    async::PostTask(dispatcher_->async_dispatcher(), [&]() {
      ASSERT_OK(amlogic_secure_mem::AmlogicSecureMemDevice::Create(nullptr, parent()));
      completion.Signal();
    });
    completion.Wait();
    ASSERT_EQ(root_->child_count(), 1);
    auto child = root_->GetLatestChild();
    dev_ = child->GetDeviceContext<amlogic_secure_mem::AmlogicSecureMemDevice>();
  }

  void TearDown() override {
    // For now, we use DdkSuspend(reboot) partly to cover DdkSuspend() handling, and
    // partly because it's the only way of cleaning up safely that we've implemented so far, as
    // aml-securemem doesn't yet implement DdkUnbind() - and arguably it doesn't really need to
    // given what aml-securemem is.

    async::PostTask(dispatcher_->async_dispatcher(), [&]() {
      dev()->zxdev()->SuspendNewOp(DEV_POWER_STATE_D3COLD, false, DEVICE_SUSPEND_REASON_REBOOT);
    });

    ASSERT_OK(dev()->zxdev()->WaitUntilSuspendReplyCalled());

    // Destroy the driver object in the dispatcher context.
    libsync::Completion destroy_completion;
    async::PostTask(dispatcher_->async_dispatcher(), [&]() {
      EXPECT_EQ(1, root_->child_count());
      dev()->zxdev()->ReleaseOp();
      mock_ddk::ReleaseFlaggedDevices(root_.get());
      destroy_completion.Signal();
    });
    destroy_completion.Wait();
  }

  zx_device_t* parent() { return root_.get(); }

  amlogic_secure_mem::AmlogicSecureMemDevice* dev() { return dev_; }

  async_patterns::TestDispatcherBound<FakeSysmem>& sysmem() { return sysmem_; }
  async_patterns::TestDispatcherBound<FakeTee>& tee() { return tee_; }

 private:
  fdf_testing::DriverRuntime* runtime() { return fdf_testing::DriverRuntime::GetInstance(); }

  async::Loop incoming_loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
  std::shared_ptr<MockDevice> root_{MockDevice::FakeRootParent()};
  async_patterns::TestDispatcherBound<fdf_fake::FakePDev> pdev_{incoming_loop_.dispatcher(),
                                                                std::in_place};
  async_patterns::TestDispatcherBound<FakeSysmem> sysmem_{incoming_loop_.dispatcher(),
                                                          std::in_place};
  async_patterns::TestDispatcherBound<FakeTee> tee_{incoming_loop_.dispatcher(), std::in_place};
  async_patterns::TestDispatcherBound<component::OutgoingDirectory> outgoing_{
      incoming_loop_.dispatcher(), std::in_place, async_patterns::PassDispatcher};
  amlogic_secure_mem::AmlogicSecureMemDevice* dev_;
  fdf::UnownedSynchronizedDispatcher dispatcher_{runtime()->StartBackgroundDispatcher()};

  libsync::Completion shutdown_completion_;
};

TEST_F(AmlogicSecureMemTest, GetSecureMemoryPhysicalAddressBadVmo) {
  zx::vmo vmo;
  ASSERT_OK(zx::vmo::create(zx_system_get_page_size(), 0, &vmo));

  ASSERT_TRUE(dev()->GetSecureMemoryPhysicalAddress(std::move(vmo)).is_error());
}

// Verifies that out-of-bounds physical range modifications are rejected by IsWithinAllowedHeap()
// and never forwarded to the TEE over SMC.
TEST_F(AmlogicSecureMemTest, OutOfBoundsSecureHeapRangeRejected) {
  // Retrieve the fuchsia.sysmem2/SecureMem client endpoint registered by the driver during setup.
  fidl::ClientEnd<fuchsia_sysmem2::SecureMem> client_end;
  sysmem().SyncCall([&](FakeSysmem* sysmem) { client_end = sysmem->take_tee_connection(); });
  ASSERT_TRUE(client_end.is_valid());
  fidl::WireSyncClient<fuchsia_sysmem2::SecureMem> client(std::move(client_end));

  fidl::Arena arena;
  // Helper lambda to construct a valid SecureHeapAndRange table for requests without reusing
  // builders.
  auto make_heap_and_range = [&](uint64_t phys_addr, uint64_t size_bytes) {
    auto heap_builder = fuchsia_sysmem2::wire::Heap::Builder(arena);
    heap_builder.heap_type(bind_fuchsia_amlogic_platform_sysmem_heap::HEAP_TYPE_SECURE);
    heap_builder.id(0);

    auto range_builder = fuchsia_sysmem2::wire::SecureHeapRange::Builder(arena);
    range_builder.physical_address(phys_addr);
    range_builder.size_bytes(size_bytes);

    auto heap_range_builder = fuchsia_sysmem2::wire::SecureHeapAndRange::Builder(arena);
    heap_range_builder.heap(heap_builder.Build());
    heap_range_builder.range(range_builder.Build());
    return heap_range_builder.Build();
  };

  // Initialize allowed_heap_ bounds.
  auto props_req_builder =
      fuchsia_sysmem2::wire::SecureMemGetPhysicalSecureHeapPropertiesRequest::Builder(arena);
  props_req_builder.entire_heap(make_heap_and_range(0x40000000, 0x00200000));

  auto props_res = client->GetPhysicalSecureHeapProperties(props_req_builder.Build());
  ASSERT_OK(props_res.status());
  ASSERT_TRUE(props_res->is_ok());

  // Record the baseline number of TEE commands dispatched during property detection and setup.
  uint32_t probe_cmd_count = 0;
  tee().SyncCall([&](FakeTee* tee) { probe_cmd_count = tee->app().invoke_command_count(); });
  EXPECT_GT(probe_cmd_count, 0);

  // Request an in-bounds physical range modification.
  auto add_req_builder1 =
      fuchsia_sysmem2::wire::SecureMemAddSecureHeapPhysicalRangeRequest::Builder(arena);
  add_req_builder1.heap_range(make_heap_and_range(0x40000000, 0x00010000));

  auto add_res1 = client->AddSecureHeapPhysicalRange(add_req_builder1.Build());
  ASSERT_OK(add_res1.status());
  ASSERT_TRUE(add_res1->is_ok());

  // Verify that an in-bounds request is allowed and forwards exactly one command to the TEE.
  uint32_t after_in_bounds_count = 0;
  tee().SyncCall([&](FakeTee* tee) { after_in_bounds_count = tee->app().invoke_command_count(); });
  EXPECT_EQ(after_in_bounds_count, probe_cmd_count + 1);

  // Request an out-of-bounds physical range modification.
  auto add_req_builder2 =
      fuchsia_sysmem2::wire::SecureMemAddSecureHeapPhysicalRangeRequest::Builder(arena);
  add_req_builder2.heap_range(make_heap_and_range(0x50000000, 0x00010000));

  auto add_res2 = client->AddSecureHeapPhysicalRange(add_req_builder2.Build());
  ASSERT_OK(add_res2.status());
  ASSERT_TRUE(add_res2->is_error());
  EXPECT_EQ(add_res2->error_value(), fuchsia_sysmem2::wire::Error::kProtocolDeviation);

  // Assert that the out-of-bounds request was blocked before reaching the TEE.
  uint32_t after_out_of_bounds_count = 0;
  tee().SyncCall(
      [&](FakeTee* tee) { after_out_of_bounds_count = tee->app().invoke_command_count(); });
  EXPECT_EQ(after_out_of_bounds_count, after_in_bounds_count);
}
