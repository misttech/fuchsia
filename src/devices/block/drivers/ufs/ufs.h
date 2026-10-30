// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_H_
#define SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_H_

#include <fidl/fuchsia.hardware.inlineencryption/cpp/wire.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/wire.h>
#include <fidl/fuchsia.hardware.ufs.phy/cpp/fidl.h>
#include <fidl/fuchsia.hardware.ufs/cpp/fidl.h>
#include <fidl/fuchsia.power.broker/cpp/fidl.h>
#include <lib/async/cpp/irq.h>
#include <lib/async/cpp/task.h>
#include <lib/async/cpp/wait.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/power/cpp/suspend.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/fzl/vmo-mapper.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/scsi/block-device.h>
#include <lib/zircon-internal/thread_annotations.h>

#include <array>
#include <atomic>
#include <condition_variable>
#include <deque>
#include <mutex>
#include <span>
#include <unordered_set>

#include <fbl/array.h>
#include <fbl/condition_variable.h>
#include <fbl/intrusive_double_list.h>
#include <fbl/string_printf.h>

#include "src/devices/block/drivers/ufs/device_manager.h"
#include "src/devices/block/drivers/ufs/registers.h"
#include "src/devices/block/drivers/ufs/request_processor.h"
#include "src/devices/block/drivers/ufs/task_management_request_processor.h"
#include "src/devices/block/drivers/ufs/transfer_request_processor.h"
#include "src/devices/block/drivers/ufs/ufs_config.h"

namespace ufs {

class UfsRpmbDevice;

constexpr uint32_t kMaxLunCount = 32;
constexpr uint32_t kMaxLunIndex = kMaxLunCount - 1;
constexpr uint32_t kDeviceInitTimeoutUs = 2000000;
constexpr uint32_t kHostControllerTimeoutUs = 1000;
// Bound on the HCS DP/UTRLRDY wait after a DME_HIBERNATE_EXIT issued to leave Auto-Hibern8 (M-PHY
// T_ACTIVATE is in the order of hundreds of microseconds).
constexpr uint32_t kAutoHibernateExitTimeoutUs = 10000;
// How many times the runtime link-idle timer re-arms itself when it fires while HCS still shows
// the link active (not yet parked in Auto-Hibern8) before waiting for the next activity.
constexpr uint32_t kMaxLinkIdleRearms = 2;
constexpr int kMaxRetries = 3;
constexpr uint32_t kMaxTransferSize1MiB = 1024 * 1024;
constexpr uint8_t kPlaceholderTarget = 0;
// Backoff for the driver-side retry of a failed resume (see Ufs::Resume): starts at
// kResumeRetryInitialDelay and doubles up to kResumeRetryMaxDelay.
constexpr zx::duration kResumeRetryInitialDelay = zx::sec(1);
constexpr zx::duration kResumeRetryMaxDelay = zx::sec(30);
// Heuristic upper bound on how long a synchronous caller waits for a scheduled RecoverLink() to
// finish (see Ufs::WaitForLinkRecovery()); on expiry WakeFromRuntimeSleep() reports the original
// wake error while the recovery continues in the background.
constexpr zx::duration kLinkRecoveryWaitTimeout = kCommandTimeout * kMaxPowerDownResumeAttempts;

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
  kPreSuspend,
  kPostSuspend,
  kPreResume,
  kPostResume,
  kLinkIdle,
  kLinkActive,
};

// struct representing a pending block request for a logical unit
struct IoCommand {
  scsi::ScsiRequest request;
  uint8_t lun;
  uint32_t block_size;
};

struct InspectProperties {
  inspect::BoolProperty power_suspended;              // Updated whenever power state changes.
  inspect::ExponentialUintHistogram wake_latency_us;  // Updated whenever the controller powers on.
  inspect::UintProperty suspend_failures;             // Incremented whenever SuspendPower() fails.
  inspect::UintProperty resume_failures;              // Incremented whenever ResumePower() fails.
  inspect::UintProperty auto_hibern8_error_count;     // Incremented on AH8 entry/exit errors.
  inspect::UintProperty uic_error_count;              // Incremented on every UIC error interrupt.
  inspect::UintProperty link_recovery_count;          // Incremented when RecoverLink() succeeds.
  inspect::UintProperty link_idle_count;              // Incremented when kLinkIdle is entered.
  inspect::UintProperty link_vote_error_count;        // Incremented when a PHY link vote fails.
  inspect::BoolProperty link_idle;  // True while runtime link-idle votes are dropped.
  // Latency histograms (microseconds) of successful runtime link power transitions.
  inspect::ExponentialUintHistogram link_active_latency_us;  // LINK_IDLE -> ACTIVE PHY restore.
  inspect::ExponentialUintHistogram link_idle_latency_us;    // ACTIVE -> LINK_IDLE PHY vote.
  inspect::ExponentialUintHistogram ah8_exit_latency_us;     // ExitAutoHibernate(), to DP/UTRLRDY.
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
  // Block I/O counters
  inspect::UintProperty read_ops;
  inspect::UintProperty write_ops;
  inspect::UintProperty flush_ops;
  inspect::UintProperty trim_ops;
  inspect::UintProperty read_kib;
  inspect::UintProperty write_kib;
  inspect::UintProperty flush_kib;
  inspect::UintProperty trim_kib;
  std::atomic<uint64_t> total_read_bytes = 0;
  std::atomic<uint64_t> total_write_bytes = 0;
  std::atomic<uint64_t> total_trim_bytes = 0;
};

using HostControllerCallback = fit::function<zx::result<>(NotifyEvent, uint64_t data)>;

// Base class for UFS drivers.
//
// This class is the parent class for both UfsPci and UfsPdev drivers, which bind the UFS
// controller via PCI and PDev respectively.
class Ufs : public fdf::DriverBase2, public fdf_power::Suspendable<Ufs>, public scsi::Controller {
 public:
  static constexpr char kDriverName[] = "ufs";
  // Levels of the framework-provided power element (fuchsia.hardware.power/FrameworkElementLevels).
  // TODO(https://fxbug.dev/42075643): We need to add sleep(low-power) level
  static constexpr fuchsia_power_broker::PowerLevel kPowerLevelOff = 0;
  static constexpr fuchsia_power_broker::PowerLevel kPowerLevelOn = 1;

  explicit Ufs();
  ~Ufs() override;

  zx::result<> Start(fdf::DriverContext context) override;

  void Stop(fdf::StopCompleter completer) override;

  // fdf_power::Suspendable. Suspend()/Resume() are driven by Power Broker through the
  // framework-provided ElementRunner (level "off" -> Suspend, "on" -> Resume). The mixin
  // acknowledges the first "off" before any "on" on its own (first_activation_occurred_), so the
  // hardware initialized by Init() is not powered down just because the element is created without
  // a lease; Power Broker raises us to "on" as soon as the Execution State dependency is
  // registered.
  void Suspend(fdf_power::SuspendCompleter completer) override;
  void Resume(fdf_power::ResumeCompleter completer) override;
  bool SuspendEnabled() override {
    return config().enable_suspend() && config().storage_power_management_enabled();
  }
  std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>> take_power_element_runner() {
    return std::move(power_element_runner_);
  }

  // scsi::Controller
  fidl::WireSyncClient<fuchsia_driver_framework::Node> &root_node() override { return root_node_; }
  std::string_view driver_name() const override { return name(); }
  const std::shared_ptr<fdf::Namespace> &driver_incoming() const override { return incoming_; }
  std::shared_ptr<fdf::OutgoingDirectory> &driver_outgoing() override { return outgoing(); }
  async_dispatcher_t *driver_async_dispatcher() const { return dispatcher(); }
  const std::optional<std::string> &driver_node_name() const override { return node_name_; }
  fdf::Logger &driver_logger() override { return logger(); }
  const ufs_config::Config &config() const { return config_; }

  zx_status_t ExecuteCommandSync(uint8_t target, uint16_t lun, iovec cdb, bool is_write,
                                 iovec data) override;
  void ExecuteCommandsAsync(uint8_t target, uint16_t lun,
                            std::span<scsi::ScsiRequest> batch) override;
  bool SupportsInlineEncryption() const override { return crypto_supported_; }
  zx::result<uint8_t> ProgramKeySlot(fidl::VectorView<uint8_t> wrapped_key,
                                     uint32_t data_unit_size) override;
  zx::result<std::vector<uint8_t>> DeriveRawSecret(fidl::VectorView<uint8_t> wrapped_key) override;

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

  fdf::UnownedDispatcher io_worker_dispatcher() const { return io_worker_dispatcher_.borrow(); }
  fdf::UnownedDispatcher admin_worker_dispatcher() const {
    return admin_worker_dispatcher_.borrow();
  }

  // for test
  uint32_t GetLogicalUnitCount() const { return logical_unit_count_; }
  size_t GetPendingCommandsCount() {
    std::lock_guard<std::mutex> lock(lock_);
    return pending_commands_.size();
  }

  void DumpRegisters();
  void DumpErrorRegisters();

  // Keeps the UFSHCI link and its platform clocks active for the lifetime of the guard: restores
  // the PHY's ACTIVE vote if the runtime idle timer had voted LINK_IDLE, and keeps the timer from
  // voting LINK_IDLE again until the last guard (and the last allocated request slot, which holds
  // one implicitly) is gone. Must be held across every register access that can start link
  // activity: doorbells, UIC commands, suspend/resume and recovery sequences, the ISR.
  class ScopedLinkActive {
   public:
    explicit ScopedLinkActive(Ufs &ufs) : ufs_(ufs) { ufs_.AcquireLinkActive(); }
    ~ScopedLinkActive() { ufs_.ReleaseLinkActive(); }
    ScopedLinkActive(const ScopedLinkActive &) = delete;
    ScopedLinkActive &operator=(const ScopedLinkActive &) = delete;

   private:
    Ufs &ufs_;
  };

  // Temporarily disables hardware Auto-Hibern8 (AHIT = 0) during DME/PHY accesses so the link
  // cannot enter Hibern8 mid-access. Ref-counted across nested scopes; implies ScopedLinkActive.
  class ScopedAutoHibernateDisable {
   public:
    explicit ScopedAutoHibernateDisable(Ufs &ufs) : link_active_(ufs), ufs_(ufs) {
      ufs_.AcquireAutoHibernateDisable();
    }
    ~ScopedAutoHibernateDisable() { ufs_.ReleaseAutoHibernateDisable(); }
    ScopedAutoHibernateDisable(const ScopedAutoHibernateDisable &) = delete;
    ScopedAutoHibernateDisable &operator=(const ScopedAutoHibernateDisable &) = delete;

   private:
    ScopedLinkActive link_active_;
    Ufs &ufs_;
  };

  void AcquireLinkActive() TA_EXCL(ah8_lock_);
  void ReleaseLinkActive() TA_EXCL(ah8_lock_);
  // Arms the runtime link-idle timer if nothing holds the link active. Called where Auto-Hibern8
  // is (re-)enabled outside of a guard; guard release re-arms it otherwise.
  void ScheduleRuntimeIdleTimers() TA_EXCL(ah8_lock_);
  bool IsLinkIdle() TA_EXCL(ah8_lock_) {
    std::scoped_lock lock(ah8_lock_);
    return link_idle_;
  }
  void SetLinkIdleTimeoutForTesting(zx::duration timeout) { link_idle_timeout_ = timeout; }

  void ScheduleLinkRecovery() TA_EXCL(link_recovery_lock_);
  void RecoverLink() TA_EXCL(link_recovery_lock_);
  // Blocks until the RecoverLink() that is scheduled or in progress has run to completion (success
  // or terminal failure); returns ZX_OK at once if none is pending and ZX_ERR_TIMED_OUT after
  // |timeout|. The caller re-checks IsResumed() for the outcome. Must not be called on
  // exception_event_dispatcher_, which runs RecoverLink() itself.
  zx_status_t WaitForLinkRecovery(zx::duration timeout) TA_EXCL(link_recovery_lock_);
  bool IsOnExceptionEventDispatcher() const {
    return exception_event_dispatcher_.get() != nullptr &&
           fdf::Dispatcher::GetCurrent()->get() == exception_event_dispatcher_.get();
  }

  bool HasWellKnownLun(WellKnownLuns lun) {
    return well_known_lun_set_.find(lun) != well_known_lun_set_.end();
  }

  // Host-controller quirks detected from the PCI vendor/device ID (see UfsPci::InitQuirk()).
  //
  // Intel (Lakefield) UFSHCI: uses a 19.2 MHz reference clock instead of 38.4 MHz, needs the
  // peer's PA_TActivate raised by 200 us, and needs a 1250 us settle followed by a DME_PEER_GET to
  // clear a spurious DME error after a UIC power mode change.
  bool intel_quirk() const { return intel_quirk_; }
  // QEMU's UFS host controller model does not implement UniPro attributes, UIC power mode
  // changes or Auto-Hibernate, so those steps are skipped and the link is assumed Active.
  bool qemu_quirk() const { return qemu_quirk_; }

  virtual bool SupportsPowerDownSuspend() const { return false; }
  virtual bool SupportsRuntimePowerManagement() const { return SupportsPowerDownSuspend(); }

  // Returns true if any inline encryption key slot has been programmed. Key slot state (UFSHCI
  // CRYPTOCFG and, on platforms where the key table lives inside the host controller power domain,
  // the key material itself) does not survive a PowerDown suspend.
  bool HasProgrammedCryptoKeys() TA_EXCL(uic_lock_);

  // Host-controller side of the power sequence, driven by DeviceManager::SuspendPower() /
  // ResumePower() (the device/link side lives in DeviceManager).

  zx::result<> ReinitAfterPowerDown();
  // After the device has been power-cycled (PowerDown suspend), every LU and well-known LU
  // reports a UNIT ATTENTION (POWER ON / RESET) on its first SCSI command. Clear them with
  // TEST UNIT READY so that the START STOP UNIT issued by ResumePower() and the first I/O to each
  // LU do not fail with ZX_ERR_UNAVAILABLE.
  zx::result<> ClearUnitAttentions();
  zx::result<> ConfigureAutoHibernate();
  zx::result<> DisableAutoHibernate();

  // Marks the UFSHCI register space as inaccessible (power domains/clocks gated). While set, the
  // interrupt handler and I/O paths must not touch controller registers. Setting it also waits for
  // any in-progress interrupt handling to finish.
  void SetHardwarePoweredOff(bool powered_off);
  bool IsHardwarePoweredOff() const {
    return hardware_powered_off_.load(std::memory_order_acquire);
  }

  // Masks interrupts, disables the host controller and marks the register space as powered off,
  // ahead of the PHY driver gating the UFSHCI power domains/clocks. Requires that no I/O slot is
  // allocated (RequestProcessor::BlockSlotAllocation() has run). Holds uic_lock() so that a UIC
  // command or key-slot write that has already passed its IsHardwarePoweredOff() check finishes
  // before the controller is stopped. Returns the status of the host controller disable; the
  // register space is marked powered off regardless.
  zx_status_t StopHostControllerForPowerDown();

  // Serialises register accesses that happen outside the I/O worker / ISR paths (UIC commands,
  // whether issued by this driver, by the PHY driver through fuchsia.hardware.ufs.phy/Ufshci, or
  // through fuchsia.hardware.ufs, and inline-crypto key-slot programming) against
  // StopHostControllerForPowerDown(). Must be acquired after DeviceManager::power_lock_ when both
  // are held, and must never be held across a Notify() callback into the PHY driver (its
  // Ufshci::DmeSet/DmeGet calls take this lock).
  std::mutex &uic_lock() TA_RET_CAP(uic_lock_) { return uic_lock_; }

  void TriggerIoWork();

  bool IsResumed() const { return device_manager_->IsResumed(); }

  const inspect::Inspector &inspect() { return component_inspector_->inspector(); }

 protected:
  // Initialize the UFS controller and bind the logical units.
  // Declare this as virtual to delay driver initialization in tests.
  virtual zx_status_t Init();

  virtual zx::result<fdf::MmioBuffer> CreateMmioBuffer(zx_off_t offset, size_t size, zx::vmo vmo) {
    return fdf::MmioBuffer::Create(offset, size, std::move(vmo), ZX_CACHE_POLICY_UNCACHED_DEVICE);
  }

  // Writes the UFSHCI CRYPTOCFG entry for |slot| and records the slot as programmed with
  // |data_unit_size|. Called by ProgramKeySlot() after the inline encryption provider has loaded
  // the key material into the hardware slot. Fails with ZX_ERR_BAD_STATE while the host
  // controller is powered down, in which case the slot is not recorded.
  zx::result<> ConfigureCryptoSlot(uint8_t slot, uint32_t data_unit_size) TA_EXCL(uic_lock_);

 private:
  friend class UfsTest;
  friend class RequestProcessor;

  // Writes UFSHCI CRYPTOCFG DWORD16/17 for |slot| per UFSHCI 3.0 section 5.6.3. The caller holds
  // uic_lock_ (so that the register writes cannot race with the host controller being powered
  // down) and has checked IsHardwarePoweredOff().
  void WriteCryptoConfigLocked(uint8_t slot, uint32_t data_unit_size) TA_REQ(uic_lock_);
  // Re-establishes inline encryption state in the host controller after a PowerDown suspend.
  zx::result<> RestoreCryptoState() TA_EXCL(uic_lock_);

  void HandleIrq(async_dispatcher_t *dispatcher, async::IrqBase *irq, zx_status_t status,
                 const zx_packet_interrupt_t *interrupt);
  void HandleTimeout(async_dispatcher_t *dispatcher, async::TaskBase *task, zx_status_t status);
  void TriggerAdminWork();
  // ProcessIo() cannot process SCSI commands when the UFS device is suspended. The SCSI
  // StartStopUnit admin command is required to resume UFS device, so ProcessAdminCompletions() is
  // required to handle the completion of the admin command even in a suspended state.
  void ProcessIo();
  void ScheduleTimeoutTask();

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

  // Binds the framework-provided power element runner (fdf_power::Suspendable::InitializeSuspend)
  // and registers the Execution State's dependency on that element with CpuElementManager so that
  // the system cannot suspend while the UFS device is still active. Fails if the driver framework
  // did not hand us a power element (not a suspend-enabled platform).
  zx::result<> ConfigurePowerManagement();

  void Serve(fidl::ServerEnd<fuchsia_hardware_ufs::Ufs> server_end);

  std::optional<fdf::MmioBuffer> mmio_;

  inspect::Node inspect_node_;

 protected:
  zx::vmo mmio_buffer_vmo_;
  uint64_t mmio_buffer_size_ = 0;
  zx::interrupt irq_;
  zx::bti bti_;

  // The pending list consists of commands that have been received via ExecuteCommandsAsync() and
  // are waiting for IO to start.
  std::deque<IoCommand> pending_commands_ TA_GUARDED(lock_);

  void OnExceptionDispatcherShutdown() TA_EXCL(lock_);
  void FinishStopWorkers() TA_EXCL(lock_);
  void OnDispatcherShutdown() TA_EXCL(lock_);
  void FinishStop(fdf::StopCompleter completer) TA_EXCL(lock_);

  // Dispatcher for processing queued block requests.
  fdf::Dispatcher irq_worker_dispatcher_;
  fdf::Dispatcher io_worker_dispatcher_;
  fdf::Dispatcher admin_worker_dispatcher_;
  fdf::Dispatcher exception_event_dispatcher_;

  async::IrqMethod<Ufs, &Ufs::HandleIrq> irq_handler_{this};
  async::TaskMethod<Ufs, &Ufs::HandleTimeout> timeout_task_{this};

  std::unique_ptr<DeviceManager> device_manager_;
  std::unique_ptr<TransferRequestProcessor> transfer_request_processor_;
  std::unique_ptr<TaskManagementRequestProcessor> task_management_request_processor_;
  std::unique_ptr<UfsRpmbDevice> rpmb_device_;

  // Controller internal information.
  uint32_t logical_unit_count_ = 0;

  // The luns of the well-known logical units that exist on the UFS device.
  std::unordered_set<WellKnownLuns> well_known_lun_set_;
  HostControllerCallback host_controller_callback_;

  // Note: lock_ is inherited from scsi::Controller, which also guards block_devs_.
  bool shutdown_ TA_GUARDED(lock_) = false;
  uint32_t pending_dispatcher_shutdowns_ TA_GUARDED(lock_) = 0;
  std::optional<fdf::StopCompleter> stop_completer_ TA_GUARDED(lock_);

  // The maximum transfer size supported by UFSHCI spec is 65535 * 256 KiB. However, we limit the
  // maximum transfer size to 1MiB for performance reason.
  uint32_t max_transfer_bytes_ = kMaxTransferSize1MiB;

  bool qemu_quirk_ = false;
  bool intel_quirk_ = false;

  ufs_config::Config config_;

  // Record the variable inspects.
  InspectProperties properties_;

  std::shared_ptr<fdf::Namespace> incoming_;
  std::optional<inspect::ComponentInspector> component_inspector_;
  std::optional<std::string> node_name_;

  fidl::WireSyncClient<fuchsia_driver_framework::Node> parent_node_;
  fidl::WireSyncClient<fuchsia_driver_framework::Node> root_node_;
  fidl::WireSyncClient<fuchsia_driver_framework::NodeController> node_controller_;

  // Power element created by the driver framework for this node
  // (DriverStartArgs.power_element_args, see `suspend_enabled` in the component manifest). The
  // token identifies the element to Power Broker; the runner is handed to fdf_power::Suspendable by
  // take_power_element_runner().
  zx::event power_element_token_;
  std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>> power_element_runner_;
  // The level Power Broker currently requires. Power Broker only calls SetLevel() when the
  // required level changes, so a failed ResumePower() is retried by the driver itself
  // (resume_retry_task_) for as long as "on" is still required.
  bool power_level_on_required_ = false;
  // The SetLevel(on) acknowledgement, held while ResumePower() is being retried: Power Broker
  // (and, through the Execution State dependency, everything that depends on storage) is only
  // told that the level was reached once the device is actually back. Completed by RetryResume()
  // on success, or by Stop() if the driver goes away first.
  std::optional<fdf_power::ResumeCompleter> pending_resume_completer_;

 private:
  // Set by Stop(): power-level transitions that arrive mid-teardown are acknowledged without
  // touching the hardware (the fdf_power::Suspendable binding lives until the driver is destroyed).
  std::atomic<bool> stopping_{false};
  void RetryResume(async_dispatcher_t *dispatcher, async::TaskBase *task, zx_status_t status);
  void ScheduleResumeRetry();
  void CompletePendingResume();
  async::TaskMethod<Ufs, &Ufs::RetryResume> resume_retry_task_{this};
  zx::duration resume_retry_delay_ = kResumeRetryInitialDelay;

  void AcquireAutoHibernateDisable() TA_EXCL(ah8_lock_);
  void ReleaseAutoHibernateDisable() TA_EXCL(ah8_lock_);
  // Issues DME_HIBERNATE_EXIT and waits for the link to report active again. Called by the
  // outermost AcquireAutoHibernateDisable() when HCS shows the link parked in Auto-Hibern8.
  void ExitAutoHibernate() TA_EXCL(ah8_lock_, uic_lock_);
  void ScheduleRuntimeIdleTimersLocked() TA_REQ(ah8_lock_);
  void ArmLinkIdleTimer(uint64_t generation) TA_EXCL(ah8_lock_);
  void HandleLinkIdleTimer(async_dispatcher_t *dispatcher, async::TaskBase *task,
                           zx_status_t status) TA_EXCL(ah8_lock_);
  // Runs on exception_event_dispatcher_ only, as do the two fields below.
  async::TaskMethod<Ufs, &Ufs::HandleLinkIdleTimer> link_idle_task_{this};
  uint64_t scheduled_link_idle_gen_ = 0;
  uint32_t link_idle_rearm_count_ = 0;
  zx::duration link_idle_timeout_ = zx::msec(4);

  // Link power lock. Guards the Auto-Hibern8 state and the runtime link-idle state below. The PHY
  // LINK_IDLE / ACTIVE votes (Notify(kLinkIdle/kLinkActive)) are issued while it is held so that a
  // guard acquisition and the idle timer cannot interleave; fuchsia.hardware.ufs.phy requires that
  // SetPowerState() does not call back into Ufshci, whose DME commands take this lock.
  //
  // Lock order: power_lock_ -> admin_slot_lock_ -> slot_lock_ -> ah8_lock_,
  // uic_lock_ -> ah8_lock_ (StopHostControllerForPowerDown() / StopHostControllerForRecovery()),
  // and power_lock_ -> link_recovery_lock_ (leaf). Nothing may be acquired while ah8_lock_ is held
  // other than the PHY call above; in particular not lock_, slot_lock_, uic_lock_ or power_lock_.
  mutable std::mutex ah8_lock_ TA_ACQ_AFTER(uic_lock_);
  uint32_t ah8_disable_depth_ TA_GUARDED(ah8_lock_) = 0;
  uint32_t saved_ahit_reg_ TA_GUARDED(ah8_lock_) = 0;
  bool ah8_enabled_ TA_GUARDED(ah8_lock_) = false;
  bool ah8_logged_ TA_GUARDED(ah8_lock_) = false;
  // Number of link-active holders: ScopedLinkActive guards plus one per allocated request slot.
  // Slots cannot embed a guard (RequestSlot lives in request_list.h and has no Ufs reference), so
  // RequestProcessor::ReserveSlot() / ClearSlotLocked() call Acquire/ReleaseLinkActive() directly;
  // the release is keyed on the allocated_slots_mask_ bit so a double clear cannot under-count.
  // The idle timer only votes LINK_IDLE while this and ah8_disable_depth_ are zero.
  uint32_t link_active_depth_ TA_GUARDED(ah8_lock_) = 0;
  // True while the PHY holds our LINK_IDLE vote.
  bool link_idle_ TA_GUARDED(ah8_lock_) = false;
  // Bumped whenever the link becomes active or a new idle timer is armed; a timer whose
  // generation no longer matches is stale and does nothing.
  uint64_t idle_timer_generation_ TA_GUARDED(ah8_lock_) = 0;
  // Recovery bookkeeping: ScheduleLinkRecovery() bumps |link_recoveries_scheduled_| before it
  // posts RecoverLink() and the posted task's cleanup bumps |link_recoveries_finished_| when it
  // finishes (or if the dispatcher drops it on shutdown), so at most one recovery is outstanding
  // (scheduled != finished) and WaitForLinkRecovery() can wait for the counters to match again.
  // Leaf lock (acquired under power_lock_ by ScheduleLinkRecovery(); nothing is acquired under it).
  void MarkLinkRecoveryFinished() TA_EXCL(link_recovery_lock_);
  std::mutex link_recovery_lock_;
  std::condition_variable_any link_recovery_cv_;
  uint64_t link_recoveries_scheduled_ TA_GUARDED(link_recovery_lock_) = 0;
  uint64_t link_recoveries_finished_ TA_GUARDED(link_recovery_lock_) = 0;
  // Set once Init() has completed: UIC/fatal errors raised while the init thread still owns the
  // controller are left to the init path instead of scheduling RecoverLink() underneath it.
  std::atomic<bool> link_recovery_enabled_{false};
  // Set when RecoverLink() gives up: the controller is left stopped (as after a PowerDown suspend)
  // and new I/O fails immediately instead of queueing. Cleared by the next successful
  // ReinitAfterPowerDown(), which the next resume from PowerDown performs.
  std::atomic<bool> link_failed_{false};
  void FailPendingCommands(zx_status_t status) TA_EXCL(lock_);
  // Masks interrupts, resets the host controller (HCE = 0) and marks the register space powered
  // off; the recovery-path counterpart of StopHostControllerForPowerDown(). Returns the status of
  // the host controller disable: if HCE did not clear the controller may still DMA into the
  // in-flight buffers, which the caller must then quarantine rather than unpin.
  zx_status_t StopHostControllerForRecovery() TA_EXCL(uic_lock_, ah8_lock_);
  // True while the UFSHCI power domains/clocks are gated: set by StopHostControllerForPowerDown()
  // before kPostSuspend, cleared after a successful kPreResume.
  //
  // This is a publication flag rather than a mutex-guarded bool because it is read lock-free on
  // the IRQ worker (Isr()) and the completion paths, which must not block behind the writer:
  // StopHostControllerForPowerDown() holds uic_lock() (and is called under admin_slot_lock())
  // while it stops the controller. Readers that go on to touch registers are made safe by holding
  // one of those locks across the access (UIC commands, key-slot writes, admin UPIU submission),
  // by having no I/O slot allocated (completion paths), or by the worker barriers in
  // SetHardwarePoweredOff() (ISR / admin completion). The release/acquire pairing orders the
  // PHY power-up that precedes clearing the flag before the register accesses that follow a read.
  std::atomic<bool> hardware_powered_off_{false};
  // See uic_lock().
  std::mutex uic_lock_;

  bool crypto_supported_ = false;
  fidl::WireSyncClient<fuchsia_hardware_inlineencryption::Controller> inline_encryption_client_;

  // UFSHCI 3.0 section 5.6: CRYPTOCAP.CFGC is 8 bits, but the driver (and the block server's
  // session-scoped slot numbering) currently supports at most 32 slots.
  static constexpr uint8_t kMaxCryptoSlots = 32;
  // Data unit size programmed into each crypto slot, or 0 if the slot has not been programmed.
  // Shadow of the CRYPTOCFG registers, so it shares their lock (uic_lock_): every writer also
  // writes the registers, and RestoreCryptoState() replays the whole table in one critical section.
  std::array<uint32_t, kMaxCryptoSlots> crypto_slot_data_unit_size_ TA_GUARDED(uic_lock_) = {};
};

}  // namespace ufs

#endif  // SRC_DEVICES_BLOCK_DRIVERS_UFS_UFS_H_
