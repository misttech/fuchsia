// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_H_
#define SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_H_

#include <fidl/fuchsia.hardware.platform.device/cpp/wire.h>
#include <fidl/fuchsia.hardware.ufs/cpp/fidl.h>
#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <fuchsia/hardware/block/driver/cpp/banjo.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/fzl/vmo-mapper.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/scsi/block-device.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zircon-internal/thread_annotations.h>

#include <unordered_set>

#include <fbl/array.h>
#include <fbl/condition_variable.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/string_printf.h>

#include "src/devices/block/drivers/ufs/device_manager.h"
#include "src/devices/block/drivers/ufs/request_processor.h"
#include "src/devices/block/drivers/ufs/task_management_request_processor.h"
#include "src/devices/block/drivers/ufs/transfer_request_processor.h"
#include "src/devices/block/drivers/ufs/ufs_config.h"

namespace ufs {

constexpr uint32_t kMaxLunCount = 32;
constexpr uint32_t kMaxLunIndex = kMaxLunCount - 1;
constexpr uint32_t kDeviceInitTimeoutUs = 2000000;
constexpr uint32_t kHostControllerTimeoutUs = 1000;
constexpr uint32_t kMaxTransferSize1MiB = 1024 * 1024;
constexpr uint8_t kPlaceholderTarget = 0;

constexpr uint32_t kBlockSize = 4096;
constexpr uint32_t kSectorSize = 512;

constexpr uint32_t kMaxLunId = 0x7f;
constexpr uint16_t kUfsWellKnownlunId = 1 << 7;
constexpr uint16_t kScsiWellKnownLunId = 0xc100;
constexpr uint16_t kScsiWellKnownLunIdMask = 0xff00;

enum class WellKnownLuns : uint8_t {
  kReportLuns = 0x81,
  kBoot = 0xb0,
  kRpmb = 0xc4,
  kUfsDevice = 0xd0,
  kCount = 4,
};

enum NotifyEvent {
  kInit = 0,
  kReset,
  kPreLinkStartup,
  kPostLinkStartup,
  kSetupTransferRequestList,
  kSetupTaskManagementRequestList,
  kDeviceInitDone,
  kPrePowerModeChange,
  kPostPowerModeChange,
};

struct IoCommand {
  scsi::DeviceOp device_op;

  // Ufs::ExecuteCommandAsync() checks that the incoming CDB's size does not exceed
  // this buffer's.
  uint8_t cdb_buffer[16];
  uint8_t cdb_length;
  uint8_t lun;
  uint32_t block_size_bytes;
  bool is_write;

  // Currently, data_buffer is only used by the UNMAP command and has a maximum size of 24 byte.
  uint8_t data_buffer[24];
  uint8_t data_length;
  zx::vmo data_vmo;

  list_node_t node;

  zx::unowned_vmo vmo() const {
    if (device_op.op.command.opcode == BLOCK_OPCODE_READ ||
        device_op.op.command.opcode == BLOCK_OPCODE_WRITE) {
      ZX_DEBUG_ASSERT_MSG(!data_vmo.is_valid(), "Internal VMO set for external RW request");
      return zx::unowned_vmo(device_op.op.rw.vmo);
    }
    return zx::unowned_vmo(data_vmo);
  }
};

struct InspectProperties {
  inspect::BoolProperty power_suspended;              // Updated whenever power state changes.
  inspect::ExponentialUintHistogram wake_latency_us;  // Updated whenever the controller powers on.
  // Controller
  inspect::UintProperty max_transfer_bytes;  // Set once by the init thread.
  inspect::UintProperty logical_unit_count;  // Set once by the init thread.
  inspect::StringProperty reference_clock;   // Set once by the init thread.
  inspect::UintProperty power_condition;     // Updated whenever power state changes.
  inspect::UintProperty link_state;          // Updated whenever power state changes.
  // Version
  inspect::UintProperty major_version_number;  // Set once by the init thread.
  inspect::UintProperty minor_version_number;  // Set once by the init thread.
  inspect::UintProperty version_suffix;        // Set once by the init thread.
  // Capabilities
  inspect::BoolProperty crypto_support;                        // Set once by the init thread.
  inspect::BoolProperty uic_dme_test_mode_command_supported;   // Set once by the init thread.
  inspect::BoolProperty out_of_order_data_delivery_supported;  // Set once by the init thread.
  inspect::BoolProperty _64_bit_addressing_supported;          // Set once by the init thread.
  inspect::BoolProperty auto_hibernation_support;              // Set once by the init thread.
  inspect::UintProperty
      number_of_utp_task_management_request_slots;  // Set once by the init thread.
  inspect::UintProperty
      number_of_outstanding_rtt_requests_supported;            // Set once by the init thread.
  inspect::UintProperty number_of_utp_transfer_request_slots;  // Set once by the init thread.
  // Attribute
  inspect::UintProperty b_boot_lun_en;         // Set once by the init thread.
  inspect::UintProperty b_current_power_mode;  // Updated whenever power state changes.
  inspect::UintProperty b_active_icc_level;    // Updated whenever power state changes.
  // Unipro
  inspect::UintProperty remote_version;           // Set once by the init thread.
  inspect::UintProperty local_version;            // Set once by the init thread.
  inspect::UintProperty host_t_activate;          // Set once by the init thread.
  inspect::UintProperty device_t_activate;        // Set once by the init thread.
  inspect::UintProperty host_granularity;         // Set once by the init thread.
  inspect::UintProperty device_granularity;       // Set once by the init thread.
  inspect::UintProperty pa_active_tx_data_lanes;  // Set once by the init thread.
  inspect::UintProperty pa_active_rx_data_lanes;  // Set once by the init thread.
  inspect::UintProperty pa_max_rx_hs_gear;        // Set once by the init thread.
  inspect::UintProperty pa_tx_gear;               // Updated whenever gear changes.
  inspect::UintProperty pa_rx_gear;               // Updated whenever gear changes.
  inspect::BoolProperty tx_termination;           // Set once by the init thread.
  inspect::BoolProperty rx_termination;           // Set once by the init thread.
  inspect::UintProperty pa_hs_series;             // Set once by the init thread.
  inspect::UintProperty power_mode;               // Updated whenever power state changes.
  // Write Protect
  inspect::BoolProperty is_power_on_write_protect_enabled;   // Set once by the init thread.
  inspect::BoolProperty logical_lun_power_on_write_protect;  // Set once by the init thread.
  // Background Operations
  inspect::BoolProperty is_background_op_enabled;  // Updated whenever the power state changes or an
                                                   // exception event occurs.
  // WriteBooster
  inspect::BoolProperty is_write_booster_enabled;                    // Set once by the init thread.
  inspect::BoolProperty writebooster_buffer_flush_during_hibernate;  // Set once by the init thread.
  inspect::BoolProperty writebooster_buffer_flush_enabled;           // Set once by the init thread.
  inspect::UintProperty write_booster_buffer_type;                   // Set once by the init thread.
  inspect::UintProperty user_space_configuration_option;             // Set once by the init thread.
  inspect::UintProperty write_booster_dedicated_lu;                  // Set once by the init thread.
  inspect::UintProperty write_booster_buffer_size_in_bytes;          // Set once by the init thread.
};

using HostControllerCallback = fit::function<zx::result<>(NotifyEvent, uint64_t data)>;

// Base class for UFS drivers.
//
// This class is the parent class for both UfsPci and UfsPdev drivers, which bind the UFS
// controller via PCI and PDev respectively.
class Ufs : public fdf::DriverBase2, public scsi::Controller {
 public:
  static constexpr char kDriverName[] = "ufs";
  static constexpr char kHardwarePowerElementName[] = "ufs-hardware";
  // TODO(https://fxbug.dev/42075643): We need to add sleep(low-power) level
  static constexpr fuchsia_power_broker::PowerLevel kPowerLevelOff = 0;
  static constexpr fuchsia_power_broker::PowerLevel kPowerLevelOn = 1;
  static constexpr fuchsia_power_broker::PowerLevel kPowerLevelBoot = 2;

  explicit Ufs() : fdf::DriverBase2(kDriverName), hardware_power_element_runner_server_(*this) {}
  ~Ufs() override = default;

  zx::result<> Start(fdf::DriverContext context) override;

  void Stop(fdf::StopCompleter completer) override;

  // scsi::Controller
  fidl::WireSyncClient<fuchsia_driver_framework::Node> &root_node() override { return root_node_; }
  std::string_view driver_name() const override { return name(); }
  const std::shared_ptr<fdf::Namespace> &driver_incoming() const override { return incoming_; }
  std::shared_ptr<fdf::OutgoingDirectory> &driver_outgoing() override { return outgoing(); }
  async_dispatcher_t *driver_async_dispatcher() const { return dispatcher(); }
  const std::optional<std::string> &driver_node_name() const override { return node_name_; }
  fdf::Logger &driver_logger() override { return logger(); }
  const ufs_config::Config &config() const { return config_; }

  size_t BlockOpSize() override { return sizeof(IoCommand); }
  zx_status_t ExecuteCommandSync(uint8_t target, uint16_t lun, iovec cdb, bool is_write,
                                 iovec data) override;
  void ExecuteCommandAsync(uint8_t target, uint16_t lun, iovec cdb, bool is_write,
                           uint32_t block_size_bytes, scsi::DeviceOp *device_op,
                           iovec data) override;

  const fdf::MmioBuffer &GetMmio() const {
    ZX_ASSERT(mmio_.has_value());
    return mmio_.value();
  }

  DeviceManager &GetDeviceManager() const {
    ZX_DEBUG_ASSERT(device_manager_ != nullptr);
    return *device_manager_;
  }
  TransferRequestProcessor &GetTransferRequestProcessor() const {
    ZX_DEBUG_ASSERT(transfer_request_processor_ != nullptr);
    return *transfer_request_processor_;
  }
  TaskManagementRequestProcessor &GetTaskManagementRequestProcessor() const {
    ZX_DEBUG_ASSERT(task_management_request_processor_ != nullptr);
    return *task_management_request_processor_;
  }

  // Queue an IO command to be performed asynchronously.
  void QueueIoCommand(IoCommand *io_cmd);

  // Convert block operations to UPIU commands and submit them asynchronously.
  void ProcessIoSubmissions();
  // Find the completed Admin commands in the Request List and handle their completion.
  void ProcessAdminCompletions();
  // Find the completed IO commands in the Request List and handle their completion.
  void ProcessIoCompletions();
  // Recover errors in the request list.
  void ProcessErrors();

  // Used to register a platform-specific NotifyEventCallback, which handles variants and quirks for
  // each host interface platform.
  void SetHostControllerCallback(HostControllerCallback callback) {
    host_controller_callback_ = std::move(callback);
  }

  // Defines a callback function to perform when an |event| occurs.
  static zx::result<> NotifyEventCallback(NotifyEvent event, uint64_t data);
  // The controller notifies the host controller when it takes the action defined in |event|.
  zx::result<> Notify(NotifyEvent event, uint64_t data);

  zx_status_t WaitWithTimeout(fit::function<bool()> wait_for, zx::duration timeout,
                              const fbl::String &timeout_message,
                              zx::duration granularity = zx::usec(1));

  static zx::result<uint16_t> TranslateUfsLunToScsiLun(uint8_t ufs_lun);
  static zx::result<uint8_t> TranslateScsiLunToUfsLun(uint16_t scsi_lun);

  fdf::Dispatcher &exception_event_dispatcher() { return exception_event_dispatcher_; }
  libsync::Completion &exception_event_completion() { return exception_event_completion_; }

  // for test
  uint32_t GetLogicalUnitCount() const { return logical_unit_count_; }

  void DumpRegisters();

  bool HasWellKnownLun(WellKnownLuns lun) {
    return well_known_lun_set_.find(lun) != well_known_lun_set_.end();
  }

  bool intel_quirk() const { return intel_quirk_; }

  bool IsResumed() const { return device_manager_->IsResumed(); }

  const inspect::Inspector &inspect() { return component_inspector_->inspector(); }

 protected:
  // Initialize the UFS controller and bind the logical units.
  // Declare this as virtual to delay driver initialization in tests.
  virtual zx_status_t Init();

  virtual zx::result<fdf::MmioBuffer> CreateMmioBuffer(zx_off_t offset, size_t size, zx::vmo vmo) {
    return fdf::MmioBuffer::Create(offset, size, std::move(vmo), ZX_CACHE_POLICY_UNCACHED_DEVICE);
  }

 private:
  friend class UfsTest;
  int IrqLoop();
  // IoLoop() cannot process SCSI commands when the UFS device is suspended. The SCSI StartStopUnit
  // admin command is required to resume UFS device, so AdminLoop() is required to handle the
  // completion of the admin command even in a suspended state.
  int AdminLoop();
  int IoLoop();

  // Interrupt service routine. Check that the request is complete.
  zx::result<> Isr();

  // Initialize the UFS controller and bind the logical units.
  virtual zx::result<> InitResources() = 0;

  // Release the resources acquired in InitResources().
  virtual zx_status_t StopResources() { return ZX_OK; }
  // Perform any quirks required for the UFS controller.
  //
  // For example, Red Hat QEMU UFS host controller requires the device to be reset after
  // initialization.
  virtual zx::result<> InitQuirk() { return zx::ok(); }

  // Perform work required after an interrupt is received.
  //
  // For example, legacy PCI devices require the interrupt to be acknowledged.
  virtual void OnIrqComplete() {}

  void PopulateVersionInspect(inspect::Node *inspect_node);
  void PopulateCapabilitiesInspect(inspect::Node *inspect_node);

  zx::result<> InitMmioBuffer();
  zx::result<> InitController();
  zx::result<> InitDeviceInterface(inspect::Node &controller_node);
  zx::result<> GetControllerDescriptor();
  zx::result<uint32_t> AddLogicalUnits();

  zx_status_t EnableHostController();
  zx_status_t DisableHostController();

  zx::result<> AllocatePages(zx::vmo &vmo, fzl::VmoMapper &mapper, size_t size);

  // TODO(b/309152899): Once fuchsia.power.SuspendEnabled config cap is available, have this method
  // return failure if power management could not be configured. Use fuchsia.power.SuspendEnabled to
  // ignore this failure when expected.
  // Register power configs from the board driver with Power Broker, and begin the continuous
  // power level adjustment of hardware. For boards/products that don't support the Power Framework,
  // this method simply returns success.
  zx::result<> ConfigurePowerManagement();

  // Acquires a lease during initialization that holds the power element at its "boot" level until
  // an external dependency pulls it to "on".
  zx::result<fuchsia_power_broker::LeaseToken> AcquireInitLease(
      const fidl::WireSyncClient<fuchsia_power_broker::Topology> &topology_client);

  // Adjusts the hardware power level in response to SetLevel calls from the Power Broker.
  class HardwareElementRunner : public fidl::Server<fuchsia_power_broker::ElementRunner> {
   public:
    explicit HardwareElementRunner(Ufs &p) : parent_(p) {}
    void SetLevel(fuchsia_power_broker::ElementRunnerSetLevelRequest &request,
                  SetLevelCompleter::Sync &completer) override;
    void handle_unknown_method(
        fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementRunner> metadata,
        fidl::UnknownMethodCompleter::Sync &completer) override;

   private:
    Ufs &parent_;
  };

  HardwareElementRunner hardware_power_element_runner_server_;
  std::optional<fidl::ServerBinding<fuchsia_power_broker::ElementRunner>>
      hardware_power_element_runner_server_binding_;

  void Serve(fidl::ServerEnd<fuchsia_hardware_ufs::Ufs> server_end);

  std::optional<fdf::MmioBuffer> mmio_;

  inspect::Node inspect_node_;

 protected:
  zx::vmo mmio_buffer_vmo_;
  uint64_t mmio_buffer_size_ = 0;
  zx::interrupt irq_;
  zx::bti bti_;

  std::mutex commands_lock_;
  // The pending list consists of commands that have been received via QueueIoCommand() and are
  // waiting for IO to start.
  list_node_t pending_commands_ TA_GUARDED(commands_lock_);

  // Notifies AdminThread() that it has work to do. Signaled from QueueIoCommand() or the IRQ
  // handler.
  sync_completion_t admin_signal_;
  // Notifies IoThread() that it has work to do. Signaled from QueueIoCommand() or the IRQ handler.
  sync_completion_t io_signal_;

  // Dispatcher for processing queued block requests.
  fdf::Dispatcher irq_worker_dispatcher_;
  fdf::Dispatcher io_worker_dispatcher_;
  fdf::Dispatcher admin_worker_dispatcher_;
  fdf::Dispatcher exception_event_dispatcher_;
  // Signaled when worker_dispatcher_ is shut down.
  libsync::Completion irq_worker_shutdown_completion_;
  libsync::Completion io_worker_shutdown_completion_;
  libsync::Completion admin_worker_shutdown_completion_;
  libsync::Completion exception_event_completion_;
  // Signaled when power has been resumed.
  libsync::Completion wait_for_power_resumed_;

  std::unique_ptr<DeviceManager> device_manager_;
  std::unique_ptr<TransferRequestProcessor> transfer_request_processor_;
  std::unique_ptr<TaskManagementRequestProcessor> task_management_request_processor_;

  // Controller internal information.
  uint32_t logical_unit_count_ = 0;

  // The luns of the well-known logical units that exist on the UFS device.
  std::unordered_set<WellKnownLuns> well_known_lun_set_;

  // Callback function to perform when the host controller is notified.
  HostControllerCallback host_controller_callback_;

  bool driver_shutdown_ TA_GUARDED(lock_) = false;

  // The maximum transfer size supported by UFSHCI spec is 65535 * 256 KiB. However, we limit the
  // maximum transfer size to 1MiB for performance reason.
  uint32_t max_transfer_bytes_ = kMaxTransferSize1MiB;

  bool qemu_quirk_ = false;
  bool intel_quirk_ = false;

  std::mutex lock_;

  ufs_config::Config config_;

  // Record the variable inspects.
  InspectProperties properties_;

  std::shared_ptr<fdf::Namespace> incoming_;
  std::optional<inspect::ComponentInspector> component_inspector_;
  std::optional<std::string> node_name_;

  fidl::WireSyncClient<fuchsia_driver_framework::Node> parent_node_;
  fidl::WireSyncClient<fuchsia_driver_framework::Node> root_node_;
  fidl::WireSyncClient<fuchsia_driver_framework::NodeController> node_controller_;

  fidl::WireSyncClient<fuchsia_power_broker::ElementControl> hardware_power_element_control_client_;
  zx::event hardware_power_assertive_token_;
  fuchsia_power_broker::LeaseToken hardware_power_lease_control_token_;
};

}  // namespace ufs

#endif  // SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_H_
