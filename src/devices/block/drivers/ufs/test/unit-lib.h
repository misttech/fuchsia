// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_UFS_TEST_UNIT_LIB_H_
#define SRC_DEVICES_BLOCK_DRIVERS_UFS_TEST_UNIT_LIB_H_

#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <fidl/fuchsia.power.system/cpp/fidl.h>
#include <fidl/fuchsia.power.system/cpp/test_base.h>
#include <lib/driver/power/cpp/testing/fake_element_control.h>
#include <lib/driver/testing/cpp/driver_test.h>
#include <lib/inspect/testing/cpp/inspect.h>

#include <gtest/gtest.h>

#include "mock-device/ufs-mock-device.h"
#include "src/devices/block/drivers/ufs/ufs.h"
#include "src/devices/block/drivers/ufs/ufs_pci.h"
#include "src/devices/pci/testing/pci_protocol_fake.h"
#include "src/lib/testing/predicates/status.h"

namespace ufs {

using fdf_power::testing::FakeElementControl;

// Implement all the WireServer handlers of fuchsia_hardware_pci::Device as protocol as required by
// FIDL.
class FakePci : public fidl::WireServer<fuchsia_hardware_pci::Device> {
 public:
  fuchsia_hardware_pci::Service::InstanceHandler GetInstanceHandler() {
    return fuchsia_hardware_pci::Service::InstanceHandler({
        .device = binding_group_.CreateHandler(
            this, fdf::Dispatcher::GetCurrent()->async_dispatcher(), fidl::kIgnoreBindingClosure),
    });
  }

  void SetDeviceInfo(uint16_t vendor_id, uint16_t device_id) {
    vendor_id_ = vendor_id;
    device_id_ = device_id;
  }

  void GetDeviceInfo(GetDeviceInfoCompleter::Sync& completer) override {
    fuchsia_hardware_pci::wire::DeviceInfo info;
    info.vendor_id = vendor_id_;
    info.device_id = device_id_;
    completer.Reply(info);
  }
  void GetBar(GetBarRequestView request, GetBarCompleter::Sync& completer) override {
    fuchsia_hardware_pci::wire::Bar bar = {
        .bar_id = 0,
        .size = RegisterMap::kRegisterSize,
        .result = fuchsia_hardware_pci::wire::BarResult::WithVmo(mock_device_->GetVmo()),
    };
    completer.ReplySuccess(std::move(bar));
  }
  void SetBusMastering(SetBusMasteringRequestView request,
                       SetBusMasteringCompleter::Sync& completer) override {
    completer.ReplySuccess();
  }
  void ResetDevice(ResetDeviceCompleter::Sync& completer) override { completer.ReplySuccess(); }
  void AckInterrupt(AckInterruptCompleter::Sync& completer) override { completer.ReplySuccess(); }
  void MapInterrupt(MapInterruptRequestView request,
                    MapInterruptCompleter::Sync& completer) override {
    completer.ReplySuccess(mock_device_->GetIrq());
  }
  void GetInterruptModes(GetInterruptModesCompleter::Sync& completer) override {
    fuchsia_hardware_pci::wire::InterruptModes modes;
    modes.has_legacy = true;
    modes.msix_count = 0;
    modes.msi_count = 0;
    completer.Reply(modes);
  }
  void SetInterruptMode(SetInterruptModeRequestView request,
                        SetInterruptModeCompleter::Sync& completer) override {
    completer.ReplySuccess();
  }
  void ReadConfig8(ReadConfig8RequestView request, ReadConfig8Completer::Sync& completer) override {
    completer.ReplySuccess(0);
  }
  void ReadConfig16(ReadConfig16RequestView request,
                    ReadConfig16Completer::Sync& completer) override {
    completer.ReplySuccess(0);
  }
  void ReadConfig32(ReadConfig32RequestView request,
                    ReadConfig32Completer::Sync& completer) override {
    completer.ReplySuccess(0);
  }
  void WriteConfig8(WriteConfig8RequestView request,
                    WriteConfig8Completer::Sync& completer) override {
    completer.ReplySuccess();
  }
  void WriteConfig16(WriteConfig16RequestView request,
                     WriteConfig16Completer::Sync& completer) override {
    completer.ReplySuccess();
  }
  void WriteConfig32(WriteConfig32RequestView request,
                     WriteConfig32Completer::Sync& completer) override {
    completer.ReplySuccess();
  }
  void GetCapabilities(GetCapabilitiesRequestView request,
                       GetCapabilitiesCompleter::Sync& completer) override {
    std::vector<uint8_t> empty_vec;
    auto empty_vec_view = fidl::VectorView<uint8_t>::FromExternal(empty_vec);
    completer.Reply(empty_vec_view);
  }
  void GetExtendedCapabilities(GetExtendedCapabilitiesRequestView request,
                               GetExtendedCapabilitiesCompleter::Sync& completer) override {
    std::vector<uint16_t> empty_vec;
    auto empty_vec_view = fidl::VectorView<uint16_t>::FromExternal(empty_vec);
    completer.Reply(empty_vec_view);
  }
  void GetBti(GetBtiRequestView request, GetBtiCompleter::Sync& completer) override {
    completer.ReplySuccess(mock_device_->GetFakeBti());
  }

  void SetMockDevice(ufs_mock_device::UfsMockDevice* mock_device) { mock_device_ = mock_device; }

  fidl::ServerBindingGroup<fuchsia_hardware_pci::Device> binding_group_;

  zx::interrupt irq_;
  ufs_mock_device::UfsMockDevice* mock_device_;
  uint16_t vendor_id_ = 0;
  uint16_t device_id_ = 0;
};

class FakeCpuElementManager
    : public fidl::testing::TestBase<fuchsia_power_system::CpuElementManager> {
 public:
  fidl::ProtocolHandler<fuchsia_power_system::CpuElementManager> CreateHandler() {
    return bindings_.CreateHandler(this, fdf::Dispatcher::GetCurrent()->async_dispatcher(),
                                   fidl::kIgnoreBindingClosure);
  }

  bool execution_state_dependency_added() const { return dependency_token_.is_valid(); }

  void AddExecutionStateDependency(AddExecutionStateDependencyRequest& request,
                                   AddExecutionStateDependencyCompleter::Sync& completer) override {
    if (!request.power_level() || *request.power_level() != Ufs::kPowerLevelOn ||
        !request.dependency_token()) {
      completer.Reply(
          fit::error(fuchsia_power_system::AddExecutionStateDependencyError::kInvalidArgs));
      return;
    }
    if (dependency_token_.is_valid()) {
      completer.Reply(
          fit::error(fuchsia_power_system::AddExecutionStateDependencyError::kInvalidArgs));
      return;
    }

    dependency_token_ = *std::move(request.dependency_token());
    completer.Reply(fit::success());
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    ADD_FAILURE() << name << " is not implemented";
  }

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_power_system::CpuElementManager> md,
      fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  fidl::ServerBindingGroup<fuchsia_power_system::CpuElementManager> bindings_;
  zx::event dependency_token_;
};

class FakeLessor : public fidl::Server<fuchsia_power_broker::Lessor> {
 public:
  void AddSideEffect(fit::function<void()> side_effect) { side_effect_ = std::move(side_effect); }

  void Lease(fuchsia_power_broker::LessorLeaseRequest& req,
             LeaseCompleter::Sync& completer) override {
    if (side_effect_) {
      side_effect_();
    }

    auto [lease_control_client_end, lease_control_server_end] =
        fidl::Endpoints<fuchsia_power_broker::LeaseControl>::Create();
    completer.Reply(fit::success(std::move(lease_control_client_end)));
  }

  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_power_broker::Lessor> md,
                             fidl::UnknownMethodCompleter::Sync& completer) override {}

 private:
  fit::function<void()> side_effect_;
};

class PowerElement {
 public:
  explicit PowerElement(
      fidl::ServerBindingRef<fuchsia_power_broker::ElementControl> element_control,
      fidl::ServerBindingRef<fuchsia_power_broker::Lessor> lessor)
      : element_control_(std::move(element_control)), lessor_(std::move(lessor)) {}

  fidl::ServerBindingRef<fuchsia_power_broker::ElementControl> element_control_;
  fidl::ServerBindingRef<fuchsia_power_broker::Lessor> lessor_;
};

// Stands in for Power Broker on the far side of the power element that driver_manager creates for
// the node and passes to the driver through DriverStartArgs.power_element_args: it serves the
// ElementControl/Lessor ends and keeps the ElementRunner client so tests can issue SetLevel().
class FakePowerBroker {
 public:
  void AddHardwarePowerElement(
      fidl::ServerEnd<fuchsia_power_broker::ElementControl> element_control_server_end,
      fidl::ClientEnd<fuchsia_power_broker::ElementRunner> element_runner_client_end,
      fidl::ServerEnd<fuchsia_power_broker::Lessor> lessor_server_end) {
    // Instantiate (fake) element control implementation.
    auto element_control_impl = std::make_unique<FakeElementControl>();
    fidl::ServerBindingRef<fuchsia_power_broker::ElementControl> element_control_binding =
        fidl::BindServer<fuchsia_power_broker::ElementControl>(
            fdf::Dispatcher::GetCurrent()->async_dispatcher(),
            std::move(element_control_server_end), std::move(element_control_impl),
            [](FakeElementControl* impl, fidl::UnbindInfo info,
               fidl::ServerEnd<fuchsia_power_broker::ElementControl> server_end) mutable {});

    // Instantiate (fake) lessor implementation.
    auto lessor_impl = std::make_unique<FakeLessor>();
    hardware_power_lessor_ = lessor_impl.get();
    fidl::ServerBindingRef<fuchsia_power_broker::Lessor> lessor_binding =
        fidl::BindServer<fuchsia_power_broker::Lessor>(
            fdf::Dispatcher::GetCurrent()->async_dispatcher(), std::move(lessor_server_end),
            std::move(lessor_impl),
            [](FakeLessor* impl, fidl::UnbindInfo info,
               fidl::ServerEnd<fuchsia_power_broker::Lessor> server_end) mutable {});

    hardware_power_element_runner_client_ = fidl::Client<fuchsia_power_broker::ElementRunner>(
        std::move(element_runner_client_end), fdf::Dispatcher::GetCurrent()->async_dispatcher());

    // Power Broker issues SetLevel(off) as soon as the element exists because no lease is held
    // yet. fdf_power::Suspendable acknowledges it without calling Ufs::Suspend() (the first "off"
    // before any "on" is ignored); the tests verify that the hardware stays active.
    hardware_power_element_runner_client_->SetLevel(Ufs::kPowerLevelOff)
        .ThenExactlyOnce(
            [](fidl::Result<fuchsia_power_broker::ElementRunner::SetLevel>& result) mutable {
              if (!result.is_ok()) {
                ADD_FAILURE() << "SetLevel failed: " << result.error_value();
              }
            });

    servers_.emplace_back(std::move(element_control_binding), std::move(lessor_binding));
  }

  FakeLessor* hardware_power_lessor_ = nullptr;
  fidl::Client<fuchsia_power_broker::ElementRunner> hardware_power_element_runner_client_;

 private:
  std::vector<PowerElement> servers_;
};

class TestUfs : public UfsPci {
 public:
  explicit TestUfs() : UfsPci() {}
  ~TestUfs() override = default;

  inspect::ComponentInspector& GetInspector() { return *component_inspector_; }

  bool SupportsPowerDownSuspend() const override { return supports_power_down_suspend_; }
  bool SupportsRuntimePowerManagement() const override {
    return supports_runtime_power_management_;
  }

  static void SetSupportsPowerDownSuspend(bool supports) {
    supports_power_down_suspend_ = supports;
  }
  static void SetSupportsRuntimePowerManagement(bool supports) {
    supports_runtime_power_management_ = supports;
  }

  // Marks |slot| as holding a programmed inline encryption key, bypassing the inline encryption
  // provider FIDL call that ProgramKeySlot() would otherwise make.
  zx::result<> ProgramCryptoSlotForTest(uint8_t slot, uint32_t data_unit_size) {
    return ConfigureCryptoSlot(slot, data_unit_size);
  }

  static void SetMockDevice(ufs_mock_device::UfsMockDevice* mock_device) {
    TestUfs::mock_device_ = mock_device;
  }

 protected:
  zx::result<fdf::MmioBuffer> CreateMmioBuffer(zx_off_t offset, size_t size, zx::vmo vmo) override;

 private:
  // TODO(https://fxbug.dev/42075643): We can avoid the static pointer by moving the
  // RegisterMmioProcessor to the TestUfs class.
  static ufs_mock_device::UfsMockDevice* mock_device_;
  static bool supports_power_down_suspend_;
  static bool supports_runtime_power_management_;
};

class Environment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    // Serve device_server_
    device_server_.Initialize(component::kDefaultInstance);
    device_server_.Serve(fdf::Dispatcher::GetCurrent()->async_dispatcher(), &to_driver_vfs);

    // Serve (fake) pci_server_
    auto result = to_driver_vfs.AddService<fuchsia_hardware_pci::Service>(
        pci_server_.GetInstanceHandler(), "pci");
    EXPECT_EQ(ZX_OK, result.status_value());

    // Serve (fake) cpu_element_manager.
    auto result_cpu_manager =
        to_driver_vfs.component().AddUnmanagedProtocol<fuchsia_power_system::CpuElementManager>(
            cpu_element_manager_.CreateHandler());
    EXPECT_EQ(ZX_OK, result_cpu_manager.status_value());

    return zx::ok();
  }

  FakePci& pci_server() { return pci_server_; }
  FakeCpuElementManager& cpu_element_manager() { return cpu_element_manager_; }
  FakePowerBroker& power_broker() { return power_broker_; }

 private:
  FakePci pci_server_;
  compat::DeviceServer device_server_;
  FakeCpuElementManager cpu_element_manager_;
  FakePowerBroker power_broker_;
};

class TestConfig final {
 public:
  using DriverType = TestUfs;
  using EnvironmentType = Environment;
};

class UfsTest : public ::testing::Test {
 public:
  void SetUp() override;
  void TearDown() override;

  void InitMockDevice();
  void StartDriver(bool supply_power_framework = false);

  fdf_testing::ForegroundDriverTest<TestConfig>& driver_test() { return driver_test_; }

  zx::result<fdf::MmioBuffer> GetMmioBuffer(zx::vmo vmo) {
    return zx::ok(mock_device_.GetMmioBuffer(std::move(vmo)));
  }

  zx_status_t DisableController();
  zx_status_t EnableController();

  // Helper functions for accessing private functions.
  zx::result<> TransferFillDescriptorAndSendRequest(uint8_t slot, DataDirection ddir,
                                                    uint16_t resp_offset, uint16_t resp_len,
                                                    uint16_t prdt_offset,
                                                    uint16_t prdt_entry_count);
  zx::result<> TaskManagementFillDescriptorAndSendRequest(uint8_t slot,
                                                          TaskManagementRequestUpiu& request);

  // Map the data vmo to the address space and assign physical addresses. Currently, it only
  // supports 8KB vmo. So, we get two physical addresses. The return value is the physical address
  // of the pinned memory.
  zx::result<> MapVmo(zx::unowned_vmo& vmo, fzl::VmoMapper& mapper, uint64_t offset_vmo,
                      uint64_t length);

  uint8_t GetSlotStateCount(SlotState slot_state);

  zx::result<uint32_t> ReadAttribute(Attributes attribute, uint8_t index = 0);
  zx::result<> WriteAttribute(Attributes attribute, uint32_t value, uint8_t index = 0);

  zx::result<> DisableBackgroundOp() { return dut_->GetDeviceManager().DisableBackgroundOp(); }

  // This function is a wrapper to avoid the thread annotation of ReserveAdminSlot().
  zx::result<uint8_t> ReserveAdminSlot() {
    std::lock_guard<std::mutex> lock(dut_->GetTransferRequestProcessor().admin_slot_lock_);
    return dut_->GetTransferRequestProcessor().ReserveAdminSlot();
  }

  // Releases a transfer request slot reserved with ReserveSlot<TransferRequestProcessor>().
  zx::result<> ClearSlot(uint8_t slot_num) {
    TransferRequestProcessor& processor = dut_->GetTransferRequestProcessor();
    std::lock_guard<std::mutex> lock(processor.GetSlotLock());
    return processor.ClearSlotLocked(processor.GetRequestListLocked().GetSlot(slot_num));
  }

  // Marks a reserved transfer request slot as timed out, as RequestCompletion() does when a
  // command's deadline passes while the controller still owns the slot.
  void SetSlotTimedOut(uint8_t slot_num) {
    TransferRequestProcessor& processor = dut_->GetTransferRequestProcessor();
    std::lock_guard<std::mutex> lock(processor.GetSlotLock());
    processor.GetRequestListLocked().GetSlot(slot_num).state = SlotState::kTimeout;
  }

  ufs_mock_device::UfsMockDevice mock_device_;

  template <class T>
  zx::result<uint8_t> ReserveSlot() {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  template <>
  zx::result<uint8_t> ReserveSlot<TransferRequestProcessor>() {
    return dut_->GetTransferRequestProcessor().ReserveSlot();
  }
  template <>
  zx::result<uint8_t> ReserveSlot<TaskManagementRequestProcessor>() {
    return dut_->GetTaskManagementRequestProcessor().ReserveSlot();
  }

  template <class T>
  void RingRequestDoorbell(uint8_t slot_num) {}
  template <>
  void RingRequestDoorbell<TransferRequestProcessor>(uint8_t slot_num) {
    dut_->GetTransferRequestProcessor().RingRequestDoorbell(slot_num);
  }
  template <>
  void RingRequestDoorbell<TaskManagementRequestProcessor>(uint8_t slot_num) {
    dut_->GetTaskManagementRequestProcessor().RingRequestDoorbell(slot_num);
  }

 protected:
  fdf_testing::ForegroundDriverTest<TestConfig> driver_test_;
  TestConfig::DriverType* dut_;
  bool driver_stopped_ = false;
};

}  // namespace ufs

#endif  // SRC_DEVICES_BLOCK_DRIVERS_UFS_TEST_UNIT_LIB_H_
