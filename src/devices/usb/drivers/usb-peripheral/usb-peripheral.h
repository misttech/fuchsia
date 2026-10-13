// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_USB_PERIPHERAL_USB_PERIPHERAL_H_
#define SRC_DEVICES_USB_DRIVERS_USB_PERIPHERAL_USB_PERIPHERAL_H_

#include <fidl/fuchsia.hardware.usb.dci/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.function/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.peripheral/cpp/wire.h>
#include <lib/async/cpp/executor.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/fit/function.h>
#include <lib/fpromise/bridge.h>
#include <lib/fpromise/promise.h>
#include <lib/fpromise/scope.h>
#include <lib/trace/event.h>
#include <lib/zx/channel.h>
#include <zircon/errors.h>

#include <format>
#include <optional>
#include <set>
#include <string_view>
#include <utility>

#include <fbl/auto_lock.h>
#include <fbl/mutex.h>
#include <usb-inspect/usb-inspect.h>
#include <usb-monitor-util/usb-monitor-util.h>
#include <usb/request-cpp.h>

#include "src/devices/usb/drivers/usb-peripheral/usb-dci-interface-server.h"
#include "src/devices/usb/drivers/usb-peripheral/usb-function.h"
#include "src/devices/usb/drivers/usb-peripheral/usb_peripheral_config.h"

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

/*
    THEORY OF OPERATION

    This driver is responsible for USB in the peripheral role, that is,
    acting as a USB device to a USB host.
    It serves as the central point of coordination for the peripheral role.
    It is configured via the fuchsia.hardware.usb.peripheral.Device FIDL protocol
    (or via structured configuration in Start()). Based on this configuration,
    it creates child nodes that serve fuchsia.hardware.usb.function.UsbFunction.
    These child nodes are bind points for USB function drivers, which implement
    USB interfaces for particular functions (such as USB ethernet, ADB, or VSOCK).

    This driver binds to a parent Device Controller Interface (DCI) driver via the
    fuchsia.hardware.usb.dci.UsbDci FIDL service and registers a
    fuchsia.hardware.usb.dci.UsbDciInterface server (hosted on a dedicated
    synchronized dispatcher by UsbDciInterfaceServer) to receive host connection,
    speed, and control transfer events.

    Lifecycle and state coordination are governed by two explicit state machines:
    - UsbPeripheral::DeviceState tracks the controller and configuration lifecycle
      (from kNoConfiguration and kWaitForFunctionBind through kStarting,
      kPeripheralReady, and kHostConnected, as well as the teardown states
      kUnregisteringFunction, kClearingFunctions, and kStoppingDriver).
    - UsbFunction::FunctionState tracks each function's binding and
      UsbFunctionInterface::SetConfigured RPC state, coalescing concurrent
      unconfiguration requests across host disconnects, function unregistrations,
      and configuration teardowns before stopping the DCI controller or removing
      child nodes.
*/

namespace usb_peripheral {

class UsbFunction;

using ConfigurationDescriptor =
    ::fidl::VectorView<fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor>;
using fuchsia_hardware_usb_peripheral::wire::DeviceDescriptor;
using fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor;

struct UsbConfiguration {
  explicit UsbConfiguration(uint8_t index) : index(index) {}

  static constexpr uint8_t MAX_INTERFACES = 32;
  // Indices of the functions associated with this configuration
  std::vector<size_t> functions;
  // USB configuration descriptor, synthesized from our functions' descriptors.
  std::vector<uint8_t> config_desc;

  // Map from interface number to function index.
  std::optional<size_t> interface_map[MAX_INTERFACES];
  // Map from interface number to current alternate setting (USB 2.0 § 9.4.4).
  uint8_t alternate_setting[MAX_INTERFACES] = {0};
  const uint8_t index;
};

// This is the main class for the USB peripheral role driver.
// It binds against the USB DCI driver device and manages a list of UsbFunction devices,
// one for each USB function in the peripheral role configuration.
class UsbPeripheral : public fdf::DriverBase2,
                      public fidl::WireServer<fuchsia_hardware_usb_peripheral::Device> {
 public:
  // The driver uses a formal state machine to manage the lifecycle of configurations
  // and host connections. This ensures that resources (child nodes, DCI mode) are
  // handled safely during asynchronous events like host (dis)connects or driver
  // teardown.
  //
  // State Machine:
  //
  // kNoConfiguration:
  //   Initial state. No active configuration. Functions can be added to the staging area.
  //   Transitions:
  //     -> kWaitForFunctionBind: Occurs when a configuration is committed (SetConfiguration()
  //        or SetDefaultConfig()). Child nodes are published.
  //     -> kClearingFunctions: Occurs on ClearFunctions().
  //     -> kStoppingDriver: Occurs on Stop().
  //
  // kWaitForFunctionBind:
  //   Configuration committed. Child nodes are published. Waiting for function drivers to bind.
  //   Transitions:
  //     -> kStarting: Occurs when all functions have registered.
  //     -> kClearingFunctions: Occurs on ClearFunctions().
  //     -> kStoppingDriver: Occurs on Stop().
  //
  // kStarting:
  //   All functions have registered. Starting the DCI controller.
  //   Transitions:
  //     -> kPeripheralReady: Occurs when StartController() succeeds.
  //     -> kWaitForFunctionBind: Occurs if StartController() fails.
  //     -> kClearingFunctions: Occurs on ClearFunctions().
  //     -> kStoppingDriver: Occurs on Stop().
  //
  // kPeripheralReady:
  //   All functions have registered. DCI is active. Ready for a USB host to connect.
  //   Transitions:
  //     -> kHostConnected: Occurs when a USB host connects.
  //     -> kUnregisteringFunction: Occurs if a function is unregistered (`Deconfigure` / unbind).
  //     -> kClearingFunctions: Occurs on ClearFunctions().
  //     -> kStoppingDriver: Occurs on Stop().
  //
  // kHostConnected:
  //   USB host has performed enumeration and selected a configuration. Data paths are active.
  //   Transitions:
  //     -> kPeripheralReady: Occurs when the host disconnects and all functions finish
  //     unconfiguring.
  //     -> kUnregisteringFunction: Occurs if a function is unregistered (`Deconfigure` / unbind).
  //     -> kClearingFunctions: Occurs on ClearFunctions().
  //     -> kStoppingDriver: Occurs on Stop().
  //
  // kUnregisteringFunction:
  //   A single function unregistered while the controller was active. Unconfiguring remaining
  //   functions and stopping the controller before returning to `kWaitForFunctionBind`.
  //   Transitions:
  //     -> kWaitForFunctionBind: Once remaining functions finish unconfiguring and DCI stops.
  //     -> kClearingFunctions: Occurs if ClearFunctions() is called during unregistration.
  //     -> kStoppingDriver: Occurs if Stop() is called during unregistration.
  //
  // kClearingFunctions:
  //   Configuration clear in progress (`ClearFunctions()`). Unconfiguring all functions, stopping
  //   the controller, and removing child nodes.
  //   Transitions:
  //     -> kNoConfiguration: When all functions are cleared.
  //     -> kStoppingDriver: Occurs if Stop() is called while clearing functions.
  //
  // kStoppingDriver:
  //   Terminal driver shutdown in progress (`Stop()`).
  enum class DeviceState : uint8_t {
    kNoConfiguration,
    kWaitForFunctionBind,
    kStarting,
    kPeripheralReady,
    kHostConnected,
    kUnregisteringFunction,
    kClearingFunctions,
    kStoppingDriver,
  };

  static constexpr std::string_view kDriverName = "usb_device";
  static constexpr std::string_view kChildNodeName = "usb-peripheral";

  UsbPeripheral() : fdf::DriverBase2(kDriverName) {}
  ~UsbPeripheral() override { intf_srv_.Stop(); }

  static constexpr uint8_t kMaxInterfaces = UsbConfiguration::MAX_INTERFACES;
  static constexpr uint8_t kMaxStrings = 255;
  static constexpr uint8_t kMaxStringLength = 126;

  // OUT endpoints are in range 1 - 15, IN endpoints are in range 17 - 31.
  static constexpr uint8_t kOutEpStart = 1;
  static constexpr uint8_t kOutEpEnd = 15;
  static constexpr uint8_t kInEpStart = 17;
  static constexpr uint8_t kInEpEnd = 31;

  // fdf::DriverBase2 implementation.
  zx::result<> Start(fdf::DriverContext context) override;
  void Stop(fdf::StopCompleter completer) override;

  zx_status_t UsbDciCancelAll(uint8_t ep_address);
  zx_status_t UsbDciEndpointSetStall(uint8_t ep_address);
  zx_status_t UsbDciEndpointClearStall(uint8_t ep_address);

  // fuchsia_hardware_usb_peripheral::Device protocol implementation.
  void SetConfiguration(SetConfigurationRequestView request,
                        SetConfigurationCompleter::Sync& completer) override;
  void ClearFunctions(ClearFunctionsCompleter::Sync& completer) override;
  void GetConfiguration(GetConfigurationCompleter::Sync& completer) override;
  void SetStateChangeListener(SetStateChangeListenerRequestView request,
                              SetStateChangeListenerCompleter::Sync& completer) override;
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_usb_peripheral::Device> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  zx_status_t SetDeviceDescriptor(DeviceDescriptor desc);
  // Validates a function and returns the number of interfaces it uses on
  // success.
  zx::result<uint8_t> ValidateFunction(size_t function_index, void* descriptors, size_t length);
  zx_status_t FunctionRegistered();
  zx_status_t CheckAndStartController();
  zx_status_t StartController();
  zx_status_t StopController();
  // Handles unregistration of a single USB function (`Deconfigure` or channel closure).
  //
  // Callback Semantics (`on_complete`, optional):
  // - Invocation: If non-null, invoked on `dispatcher()` exactly once after remaining functions
  //   have finished unconfiguring and the DCI controller has stopped (or synchronously if the
  //   controller was not active).
  zx_status_t FunctionUnregistered(fit::callback<void()> on_complete = nullptr);
  void FunctionCleared(size_t function_index, uint64_t config_generation);

  DeviceState SnapshotState() const {
    fbl::AutoLock lock(&lock_);
    return state_;
  }

  size_t active_functions_count() const {
    fbl::AutoLock lock(&lock_);
    return active_functions_count_;
  }

  void IncrementActiveFunctionsLocked() __TA_REQUIRES(lock_) { active_functions_count_++; }

  void DecrementActiveFunctions(uint64_t config_generation) {
    fbl::AutoLock lock(&lock_);
    if (config_generation < config_generation_) {
      fdf::info(
          "UsbPeripheral: Discarding DecrementActiveFunctions from old config generation {} (current {})",
          config_generation, config_generation_);
      return;
    }
    if (active_functions_count_ > 0) {
      active_functions_count_--;
    } else {
      fdf::warn("DecrementActiveFunctions called when active_functions_count_ is already 0.");
    }
  }

  bool IsPeripheralStopping() const __TA_EXCLUDES(lock_);

  void SetStateLocked(DeviceState state) __TA_REQUIRES(lock_);
  void SetState(DeviceState state) {
    fbl::AutoLock lock(&lock_);
    SetStateLocked(state);
  }

  bool HasPendingSetConfiguration() const {
    fbl::AutoLock lock(&lock_);
    return pending_set_configuration_.has_value();
  }

  fdescriptor::UsbMode SnapshotUsbMode() const {
    fbl::AutoLock lock(&lock_);
    return cur_usb_mode_;
  }

  void SetUsbMode(fdescriptor::UsbMode mode) {
    fbl::AutoLock lock(&lock_);
    cur_usb_mode_ = mode;
    dci_inspect_.UpdateUsbMode(mode);
  }

  struct StringDescriptor {
    std::string text;
    std::optional<size_t> function_index;
    bool allocated = false;
  };

  struct ResourceAllocations {
    std::vector<uint8_t> interface_nums;
    std::vector<uint8_t> endpoint_addrs;
    std::vector<uint8_t> string_indices;
  };

  zx::result<ResourceAllocations> AllocResources(
      size_t function_index, uint8_t interface_count,
      std::span<fuchsia_hardware_usb_function::EndpointResource> endpoints,
      std::span<std::string> strings);

  bool ValidateEndpoint(size_t function_index, uint8_t ep_address) const __TA_EXCLUDES(lock_);

  void ReleaseResources(size_t function_index) __TA_EXCLUDES(lock_);
  void ReleaseResourcesLocked(size_t function_index) __TA_REQUIRES(lock_);

  // Returns currently allocated resources for the given function.
  // For testing purposes only.
  ResourceAllocations GetResourceAllocations(size_t function_index) __TA_EXCLUDES(lock_);

  inline const fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci>& dci() const { return dci_; }

  zx_status_t ConnectToEndpoint(uint8_t ep_address,
                                fidl::ServerEnd<fuchsia_hardware_usb_endpoint::Endpoint> ep);

  const usb_device_descriptor_t& device_desc() { return device_desc_; }
  void SetBcdUsbForTesting(uint16_t bcd_usb) { device_desc_.bcd_usb = htole16(bcd_usb); }
  bool HasActiveFunctionsForTesting() const {
    fbl::AutoLock lock(&lock_);
    return HasActiveFunctionsLocked();
  }
  size_t functions_count_for_testing() const {
    fbl::AutoLock lock(&lock_);
    return functions_.size();
  }

  // Updates host connection status and, on disconnect, drains `UsbFunction::Unconfigure()` across
  // all functions before transitioning back to `kPeripheralReady`.
  //
  // Callback Semantics (`on_complete`, optional):
  // - Invocation: If non-null, invoked on `dispatcher()` exactly once: synchronously on connect
  //   (or if no functions need unconfiguring), or asynchronously after all functions finish
  //   unconfiguring (or clearing) on disconnect.
  void OnHostConnectionChanged(bool connected, fit::callback<void()> on_complete = nullptr);
  inspect::Node& inspect_node() { return usb_peripheral_node_; }
  const inspect::Inspector& inspector() const { return inspector_->inspector(); }

  inline usb_inspect::DciInspect& dci_inspect() { return dci_inspect_; }

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(UsbPeripheral);

  // Considered part of the private impl.
  friend class UsbDciInterfaceServer;

  // Wrapper for Callbacks that must be invoked without holding a specific lock to prevent
  // deadlocks.
  class UnlockedCallback {
   public:
    UnlockedCallback(fit::callback<void()> cb, fbl::Mutex& lock)
        : cb_(std::move(cb)), lock_(&lock) {}
    UnlockedCallback() = default;

    // Call the callback ensuring the associated lock is not held.
    void operator()() __TA_EXCLUDES(*lock_) {
      if (cb_) {
        cb_();
      }
    }

    explicit operator bool() const { return !!cb_; }

   private:
    fit::callback<void()> cb_;
    fbl::Mutex* lock_ = nullptr;
  };

  bool OnDispatcher() const;
  void RunOnDispatcher(fit::closure task);
  // Sequentially dispatches a control request to `functions` starting at `index`. When
  // `is_device_recipient` is true, advances to the next function if the current function does not
  // handle the request. `completer` is invoked once with the response data or an error status
  // (synchronously if no functions remain, or on `dispatcher()` otherwise).
  void DispatchControlToFunctions(const fdescriptor::wire::UsbSetup& setup,
                                  std::vector<uint8_t> write_data,
                                  std::vector<std::shared_ptr<UsbFunction>> functions, size_t index,
                                  bool is_device_recipient,
                                  fit::callback<void(zx::result<std::vector<uint8_t>>)> completer);

  // Handles a USB control transfer from the DCI driver. `completer` is invoked exactly once
  // (synchronously for immediate descriptor/status replies, or asynchronously on `dispatcher()`
  // when delegated to `SetConfiguration`, `SetInterface`, or function drivers).
  void CommonControl(const fdescriptor::wire::UsbSetup& setup,
                     std::span<const uint8_t> write_buffer,
                     fit::callback<void(zx::result<std::vector<uint8_t>>)> completer);
  zx_status_t InitializeDci(fidl::ClientEnd<fuchsia_hardware_usb_dci::UsbDci> dci_client_end);

  // For mapping b_endpoint_address value to/from index in range 0 - 31.
  static inline uint8_t EpAddressToIndex(uint8_t addr) {
    return static_cast<uint8_t>(((addr) & 0xF) | (((addr) & 0x80) >> 3));
  }
  static inline uint8_t EpIndexToAddress(uint8_t index) {
    return static_cast<uint8_t>(((index) & 0xF) | (((index) & 0x10) << 3));
  }

  // Returns the index of the function that was added.
  zx::result<size_t> AddFunction(UsbConfiguration& config, FunctionDescriptor desc);
  // Begins the process of clearing the functions. If provided, `callback` is invoked once all
  // function child nodes have been removed and `functions_` is empty. If `stopping_driver` is true,
  // transitions to `kStoppingDriver` atomically under `lock_`.
  void ClearFunctions(std::optional<fit::callback<void()>> callback = std::nullopt,
                      bool stopping_driver = false);
  void CheckAllFunctionsCleared();
  // Completes controller stop and child node removal after function teardown.
  void CompleteFunctionsTeardown(const std::vector<std::shared_ptr<UsbFunction>>& to_teardown)
      __TA_EXCLUDES(lock_);

  zx::result<std::string> GetSerialNumber();
  zx_status_t AddFunctionDevices() __TA_REQUIRES(lock_);
  zx_status_t GetDescriptor(uint8_t request_type, uint16_t value, uint16_t index, void* buffer,
                            size_t length, size_t* out_actual);
  // Selects `alt_setting` for `interface`. `completer` is invoked exactly once on `dispatcher()`
  // with ZX_OK or an error status once the target function's SetInterface() completes.
  void SetInterface(uint8_t interface, uint8_t alt_setting,
                    fit::callback<void(zx_status_t)> completer) __TA_EXCLUDES(lock_);
  // Applies `configuration` across all functions. `completer` is invoked exactly once (on
  // `dispatcher()`, or synchronously if rejected/canceled) with ZX_OK, ZX_ERR_CANCELED if
  // superseded or interrupted by disconnect/teardown, or the first function error encountered.
  void SetConfiguration(uint8_t configuration, fit::callback<void(zx_status_t)> completer)
      __TA_EXCLUDES(lock_);
  zx_status_t SetDefaultConfig(std::vector<FunctionDescriptor>& functions);

  zx_status_t AllocInterfaceLocked(size_t function_index, uint8_t* out_intf_num)
      __TA_REQUIRES(lock_);
  // Main entry point for endpoint allocation. Decides which allocation strategy to use
  // based on DCI capabilities:
  // 1. If DCI supports dynamic sizing, delegates to the DCI driver (AllocEndpointDynamicLocked).
  // 2. If DCI reports hardware info but not dynamic sizing, uses a local best-fit algorithm
  //    based on reported endpoint capabilities (AllocEndpointBestFitLocked).
  // 3. If DCI does not support GetHardwareInfo (legacy), falls back to static sequential
  //    allocation (AllocEndpointLegacyLocked).
  zx_status_t AllocEndpointLocked(size_t function_index, fdescriptor::EndpointDirection direction,
                                  fuchsia_hardware_usb_endpoint::wire::EndpointInfo ep_info,
                                  uint32_t max_packet_size, uint8_t* out_address)
      __TA_REQUIRES(lock_);

  // Allocates an endpoint by calling the DCI driver's AllocEndpoint FIDL method.
  // Used when DCI supports dynamic endpoint sizing.
  zx_status_t AllocEndpointDynamicLocked(size_t function_index,
                                         fdescriptor::EndpointDirection direction,
                                         fuchsia_hardware_usb_endpoint::wire::EndpointInfo ep_info,
                                         uint32_t max_packet_size, uint8_t* out_address)
      __TA_REQUIRES(lock_);

  // Allocates an endpoint by choosing the best fit from the static list of endpoints
  // reported by the DCI driver via GetHardwareInfo.
  // Used when DCI does not support dynamic sizing but provides hardware info.
  zx_status_t AllocEndpointBestFitLocked(size_t function_index,
                                         fdescriptor::EndpointDirection direction,
                                         fuchsia_hardware_usb_endpoint::wire::EndpointInfo ep_info,
                                         uint32_t max_packet_size, uint8_t* out_address)
      __TA_REQUIRES(lock_);

  // Legacy sequential allocation. Assigns the next available endpoint number.
  // Used when DCI does not support GetHardwareInfo.
  zx_status_t AllocEndpointLegacyLocked(size_t function_index,
                                        fdescriptor::EndpointDirection direction,
                                        uint8_t* out_address) __TA_REQUIRES(lock_);
  zx_status_t AllocStringDescLocked(std::optional<size_t> function_index, std::string desc,
                                    uint8_t* out_index) __TA_REQUIRES(lock_);
  void FreeEndpointInternalLocked(uint8_t index, std::string_view context) __TA_REQUIRES(lock_);

  zx_status_t AllocStringDesc(std::optional<size_t> function_index, std::string desc,
                              uint8_t* out_index) __TA_EXCLUDES(lock_);

  bool HasActiveFunctionsLocked() const __TA_REQUIRES(lock_);
  bool AllFunctionsRegistered() const __TA_REQUIRES(lock_);

  UsbFunction& GetFunction(size_t index) __TA_REQUIRES(lock_);
  const UsbFunction& GetFunction(size_t index) const __TA_REQUIRES(lock_);

  void Connect(fidl::ServerEnd<fuchsia_hardware_usb_peripheral::Device> request) {
    TRACE_DURATION("usb-peripheral", __func__);
    bindings_.AddBinding(driver_dispatcher()->async_dispatcher(), std::move(request), this,
                         fidl::kIgnoreBindingClosure);
  }

  // `UsbFunction` wrapped in `shared_ptr` because `UsbFunction` instance may be
  // bound as a FIDL server which requires that the instance always be at the
  // same memory address and so we can generate weak pointers for lambda
  // captures for the function FIDL client.
  std::vector<std::shared_ptr<UsbFunction>> functions_;

  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci_;
  struct DciSupportedEndpointInfo {
    fdescriptor::wire::EndpointType endpoint_type = fdescriptor::wire::EndpointType::kBulk;
    uint16_t max_packet_size_limit = 0;
    uint64_t min_lead_time = 0;
  };
  struct DciEndpointInfo {
    uint8_t ep_address = 0;
    std::vector<DciSupportedEndpointInfo> supported_types;
  };
  bool supports_dynamic_ep_sizing_ = false;
  std::vector<DciEndpointInfo> dci_endpoints_;
  bool has_dci_hardware_info_ = false;
  // USB device descriptor set via SetConfiguration()
  usb_device_descriptor_t device_desc_ = {};
  // Map from endpoint index to function index.
  std::optional<size_t> endpoint_map_[USB_MAX_EPS];
  // Strings for USB string descriptors.
  std::vector<StringDescriptor> strings_ __TA_GUARDED(lock_);
  // List of usb_function_t.
  std::vector<UsbConfiguration> configurations_;
  // mutex for protecting our state
  // mutable to allow locking in const methods (e.g. state())
  mutable fbl::Mutex lock_;
  // Current USB mode.
  fdescriptor::UsbMode cur_usb_mode_ __TA_GUARDED(lock_) = fdescriptor::UsbMode::kNone;
  // Our parent's USB mode. Should not change after being set.
  fdescriptor::UsbMode parent_usb_mode_ __TA_GUARDED(lock_) = fdescriptor::UsbMode::kNone;
  // True if we are connected to a host.
  bool connected_ __TA_GUARDED(lock_) = false;
  // Current configuration number selected via StandardRequest::SET_CONFIGURATION
  // (will be 0 or 1 since we currently do not support multiple configurations).
  // 0 indicates that the device is unconfigured and should not accept USB requests
  // other than StandardRequest::SET_CONFIGURATION or requests targeting descriptors
  uint8_t configuration_ = 0;
  // USB connection speed.
  usb_speed_t speed_ __TA_GUARDED(lock_) = 0;

  // Registered listener
  fidl::WireSharedClient<fuchsia_hardware_usb_peripheral::Events> listener_;

  DeviceState state_ __TA_GUARDED(lock_) = DeviceState::kNoConfiguration;

  UsbMonitor usb_monitor_;

  // Returns true if the peripheral is undergoing teardown or stopping.
  //
  // Includes `kUnregisteringFunction` because when a single function unbinds or calls
  // `Deconfigure()`, the peripheral takes the controller offline: it unconfigures remaining
  // functions cleanly while hardware is live, stops the DCI controller, and returns to
  // `kWaitForFunctionBind`. While this hardware teardown is in flight, the peripheral is actively
  // decommissioning endpoints and must reject incoming `SetConfiguration()` or `SetInterface()`
  // requests from the host.
  bool IsStoppingOrClearingLocked() const __TA_REQUIRES(lock_) {
    return state_ == DeviceState::kUnregisteringFunction ||
           state_ == DeviceState::kClearingFunctions || state_ == DeviceState::kStoppingDriver;
  }

  // Registers `on_complete` with the active or newly-started unconfigure batch under `lock_`.
  //
  // - If `prioritize_callback` is true (used by `OnHostConnectionChanged(false)`), `on_complete`
  //   is queued in `pending_disconnect_callbacks_` ahead of teardown callbacks so that incoming
  //   `UsbDciInterface::SetConnected(false)` FIDL calls are replied to before the peripheral
  //   issues an outgoing synchronous `UsbDci::StopController()` call to the DCI driver.
  // - If a batch is already in flight (`unconfiguring_functions_ == true`), returns an empty
  //   vector (`on_complete` will be invoked when the in-flight batch finishes).
  // - If no functions are present in `functions_`, moves all queued callbacks into
  //   `*out_immediate_callbacks` to be invoked immediately after releasing `lock_`, and returns
  //   an empty vector.
  // - Otherwise, sets `unconfiguring_functions_ = true` atomically under `lock_` and returns the
  //   active functions to pass to `ScheduleUnconfigureBatch()` after releasing `lock_`.
  std::vector<std::shared_ptr<UsbFunction>> BeginUnconfigureFunctionsLocked(
      fit::callback<void()> on_complete, bool prioritize_callback,
      std::vector<fit::callback<void()>>* out_immediate_callbacks) __TA_REQUIRES(lock_);

  // Dispatches `UsbFunction::Unconfigure()` across `functions_to_unconfigure` (if non-empty) or
  // invokes `immediate_callbacks` (if non-empty). Must be called after releasing `lock_` following
  // `BeginUnconfigureFunctionsLocked()`.
  void ScheduleUnconfigureBatch(std::vector<std::shared_ptr<UsbFunction>> functions_to_unconfigure,
                                std::vector<fit::callback<void()>> immediate_callbacks)
      __TA_EXCLUDES(lock_);

  // Takes all queued callbacks from `pending_disconnect_callbacks_` followed by
  // `pending_unconfigure_callbacks_`.
  std::vector<fit::callback<void()>> TakePendingUnconfigureCallbacksLocked() __TA_REQUIRES(lock_);
  // Finalizes disconnect state transitions once all functions finish unconfiguring and invokes
  // `on_complete` (which replies to the held `SetConnected(false)` FIDL completer).
  void OnDisconnectUnconfigureComplete(fit::callback<void()> on_complete) __TA_EXCLUDES(lock_);

  std::vector<UnlockedCallback> on_all_functions_cleared_ __TA_GUARDED(lock_);
  std::set<uint8_t> stalled_eps_ __TA_GUARDED(lock_);

  UsbDciInterfaceServer intf_srv_{this};

  fidl::ServerBindingGroup<fuchsia_hardware_usb_peripheral::Device> bindings_;
  fdf::OwnedChildNode child_;
  std::shared_ptr<fdf::Namespace> incoming_;

  struct InspectEndpoint {
    inspect::Node node;
    inspect::UintProperty max_packet_size_limit;
    inspect::StringProperty supported_types;
  };

  std::optional<inspect::ComponentInspector> inspector_;
  inspect::Node usb_peripheral_node_;
  usb_inspect::DciInspect dci_inspect_;
  inspect::Node hardware_info_node_;
  inspect::BoolProperty supports_dynamic_ep_sizing_property_;
  std::vector<InspectEndpoint> inspect_endpoints_;

  size_t active_functions_count_ __TA_GUARDED(lock_) = 0;
  uint64_t config_generation_ __TA_GUARDED(lock_) = 0;
  // True while a batch of parallel `UsbFunction::Unconfigure()` FIDL calls is in flight.
  // Prevents re-entrant unconfiguration batches during disconnect storms or concurrent teardowns.
  bool unconfiguring_functions_ __TA_GUARDED(lock_) = false;
  // True while a `SetConfiguration()` batch is actively configuring/unconfiguring functions.
  // Serializes overlapping `SET_CONFIGURATION` requests from the host.
  bool configuring_functions_ __TA_GUARDED(lock_) = false;
  // Disconnect completion callbacks from `OnHostConnectionChanged(false)` waiting for the
  // in-flight unconfiguration batch to finish. Drained before `pending_unconfigure_callbacks_` so
  // `UsbDciInterface::SetConnected(false)` replies before `StopController()` is called.
  std::vector<fit::callback<void()>> pending_disconnect_callbacks_ __TA_GUARDED(lock_);
  // Teardown completion callbacks from `FunctionUnregistered()` and `ClearFunctions()` waiting for
  // the in-flight unconfiguration batch to finish before stopping DCI or removing child nodes.
  std::vector<fit::callback<void()>> pending_unconfigure_callbacks_ __TA_GUARDED(lock_);

  struct PendingSetConfiguration {
    uint8_t configuration;
    fit::callback<void(zx_status_t)> completer;
  };
  std::optional<PendingSetConfiguration> pending_set_configuration_ __TA_GUARDED(lock_);
  bool clear_in_progress_ __TA_GUARDED(lock_) = false;

  // Must be declared last so that `scope_` and `executor_` are destroyed first, abandoning any
  // in-flight promises (and their `fit::defer` cleanups) while all other members remain valid.
  // `scope_` binds scheduled fpromise continuations to UsbPeripheral's lifetime so tasks cannot
  // dereference a dangling `this` pointer if UsbPeripheral is destroyed while tasks are queued.
  std::unique_ptr<async::Executor> executor_;
  fpromise::scope scope_;
};

}  // namespace usb_peripheral

template <>
struct std::formatter<usb_peripheral::UsbPeripheral::DeviceState>
    : std::formatter<std::string_view> {
  auto format(usb_peripheral::UsbPeripheral::DeviceState state, std::format_context& ctx) const {
    std::string_view name = "<unknown>";
    switch (state) {
      case usb_peripheral::UsbPeripheral::DeviceState::kNoConfiguration:
        name = "kNoConfiguration";
        break;
      case usb_peripheral::UsbPeripheral::DeviceState::kWaitForFunctionBind:
        name = "kWaitForFunctionBind";
        break;
      case usb_peripheral::UsbPeripheral::DeviceState::kStarting:
        name = "kStarting";
        break;
      case usb_peripheral::UsbPeripheral::DeviceState::kPeripheralReady:
        name = "kPeripheralReady";
        break;
      case usb_peripheral::UsbPeripheral::DeviceState::kHostConnected:
        name = "kHostConnected";
        break;
      case usb_peripheral::UsbPeripheral::DeviceState::kUnregisteringFunction:
        name = "kUnregisteringFunction";
        break;
      case usb_peripheral::UsbPeripheral::DeviceState::kClearingFunctions:
        name = "kClearingFunctions";
        break;
      case usb_peripheral::UsbPeripheral::DeviceState::kStoppingDriver:
        name = "kStoppingDriver";
        break;
    }
    return std::formatter<std::string_view>::format(name, ctx);
  }
};

#endif  // SRC_DEVICES_USB_DRIVERS_USB_PERIPHERAL_USB_PERIPHERAL_H_
