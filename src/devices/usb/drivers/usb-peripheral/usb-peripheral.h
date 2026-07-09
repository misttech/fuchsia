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
#include <lib/driver/devfs/cpp/connector.h>
#include <lib/fit/function.h>
#include <lib/fpromise/bridge.h>
#include <lib/fpromise/promise.h>
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

/*
    THEORY OF OPERATION

    This driver is responsible for USB in the peripheral role, that is,
    acting as a USB device to a USB host.
    It serves as the central point of coordination for the peripheral role.
    It is configured via ioctls in the fuchsia.hardware.usb.peripheral FIDL interface
    (which is used by the usbctl command line program).
    Based on this configuration, it creates one or more devmgr devices with protocol
    ZX_PROTOCOL_USB_FUNCTION. These devices are bind points for USB function drivers,
    which implement USB interfaces for particular functions (like USB ethernet or mass storage).
    This driver also binds to a device with protocol ZX_PROTOCOL_USB_DCI
    (Device Controller Interface) which is implemented by a driver for the actual
    USB controller hardware for the peripheral role.

    The FIDL interface SetConfiguration() is used to initialize and start USB in the
    peripheral role. Internally this consists of several steps.
    The first step is setting up the USB device descriptor to be presented to the host
    during enumeration.
    Next, the descriptors for the USB functions are added to the configuration.
    Finally after all the functions have been added, the configuration is complete and
    it is now possible to build the configuration descriptor.
    Once we get to this point, UsbPeripheral.functions_bound_ is set to true.

    If the role is set to USB_MODE_PERIPHERAL and functions_bound_ is true,
    then we are ready to start USB in peripheral role.
    At this point, we create DDK devices for our list of functions.
    When the function drivers bind to these functions, they register an interface of type
    usb_function_interface_protocol_t with this driver via the usb_function_register() API.
    Once all of the function drivers have registered themselves this way,
    UsbPeripheral.functions_registered_ is set to true.

    if the usb mode is set to USB_MODE_PERIPHERAL and functions_registered_ is true,
    we are now finally ready to operate in the peripheral role.

    Teardown of the peripheral role:
    The FIDL ClearFunctions() message will reset this device's list of USB functions.
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
  //     -> kStopping: Occurs on ClearFunctions() or PrepareStop().
  //
  // kWaitForFunctionBind:
  //   Configuration committed. Child nodes are published. Waiting for function drivers to bind.
  //   Transitions:
  //     -> kStarting: Occurs when all functions have registered.
  //     -> kStopping: Occurs on ClearFunctions() or PrepareStop().
  //     -> kWaitForFunctionBind: Occurs when a function is unregistered (node unbound).
  //
  // kStarting:
  //   All functions have registered. Starting the DCI controller.
  //   Transitions:
  //     -> kPeripheralReady: Occurs when StartController() succeeds.
  //     -> kWaitForFunctionBind: Occurs if StartController() fails.
  //     -> kStopping: Occurs on ClearFunctions() or PrepareStop().
  //
  // kPeripheralReady:
  //   All functions have registered. DCI is active. Ready for a USB host to connect.
  //   Transitions:
  //     -> kHostConnected: Occurs when a USB host connects.
  //     -> kWaitForFunctionBind: Occurs if a function is unregistered.
  //     -> kStopping: Occurs on ClearFunctions() or PrepareStop().
  //
  // kHostConnected:
  //   USB host has performed enumeration and selected a configuration. Data paths are active.
  //   Transitions:
  //     -> kPeripheralReady: Occurs when the host disconnects.
  //     -> kWaitForFunctionBind: Occurs if a function is unregistered.
  //     -> kStopping: Occurs on ClearFunctions() or PrepareStop().
  //
  // kStopping:
  //   Teardown in progress (either a configuration clear or a full driver shutdown).
  //   Transitions:
  //     -> Terminates: When all functions are cleared and the driver is stopping.
  //     -> kNoConfiguration: When all functions are cleared and we are just clearing functions
  //        (not stopping the driver).
  enum class DeviceState : uint8_t {
    kNoConfiguration,
    kWaitForFunctionBind,
    kStarting,
    kPeripheralReady,
    kHostConnected,
    kStopping,
  };

  static constexpr std::string_view kDriverName = "usb_device";
  static constexpr std::string_view kChildNodeName = "usb-peripheral";

  UsbPeripheral() : fdf::DriverBase2(kDriverName) {}

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
  void SetStateChangeListener(SetStateChangeListenerRequestView request,
                              SetStateChangeListenerCompleter::Sync& completer) override;

  zx_status_t SetDeviceDescriptor(DeviceDescriptor desc);
  // Validates a function and returns the number of interfaces it uses on
  // success.
  zx::result<uint8_t> ValidateFunction(size_t function_index, void* descriptors, size_t length);
  zx_status_t FunctionRegistered();
  zx_status_t CheckAndStartController();
  zx_status_t StartController();
  zx_status_t StopController();
  zx_status_t FunctionUnregistered();
  void FunctionCleared(size_t function_index);

  DeviceState SnapshotState() const {
    fbl::AutoLock lock(&lock_);
    return state_;
  }

  void SetStateLocked(DeviceState state) __TA_REQUIRES(lock_);
  void SetState(DeviceState state) {
    fbl::AutoLock lock(&lock_);
    SetStateLocked(state);
  }

  usb_mode_t SnapshotUsbMode() const {
    fbl::AutoLock lock(&lock_);
    return cur_usb_mode_;
  }

  void SetUsbMode(usb_mode_t mode) {
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

  bool ValidateEndpoint(size_t function_index, uint8_t ep_address) const;

  void ReleaseResources(size_t function_index) __TA_EXCLUDES(lock_);
  void ReleaseResourcesLocked(size_t function_index) __TA_REQUIRES(lock_);

  // Returns currently allocated resources for the given function.
  // For testing purposes only.
  ResourceAllocations GetResourceAllocations(size_t function_index) __TA_EXCLUDES(lock_);

  inline const fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci>& dci() const { return dci_; }

  zx_status_t ConnectToEndpoint(uint8_t ep_address,
                                fidl::ServerEnd<fuchsia_hardware_usb_endpoint::Endpoint> ep);

  const usb_device_descriptor_t& device_desc() { return device_desc_; }
  void OnHostConnectionChanged(bool connected);
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

  // For the purposes of banjo->FIDL migration. Once banjo is ripped out of the driver, the logic
  // here can be folded into the FIDL endpoint implementation and calling code.
  void CommonControl(const fuchsia_hardware_usb_descriptor::wire::UsbSetup& setup,
                     cpp20::span<uint8_t> write_buffer,
                     fit::callback<void(zx::result<std::vector<uint8_t>>)> completer);

  // For mapping b_endpoint_address value to/from index in range 0 - 31.
  static inline uint8_t EpAddressToIndex(uint8_t addr) {
    return static_cast<uint8_t>(((addr) & 0xF) | (((addr) & 0x80) >> 3));
  }
  static inline uint8_t EpIndexToAddress(uint8_t index) {
    return static_cast<uint8_t>(((index) & 0xF) | (((index) & 0x10) << 3));
  }

  // Returns the index of the function that was added.
  zx::result<size_t> AddFunction(UsbConfiguration& config, FunctionDescriptor desc);
  // Begins the process of clearing the functions.
  void ClearFunctions(std::optional<fit::callback<void()>> callback = std::nullopt);
  void CheckAllFunctionsCleared();

  zx::result<std::string> GetSerialNumber();
  zx_status_t AddFunctionDevices() __TA_REQUIRES(lock_);
  zx_status_t GetDescriptor(uint8_t request_type, uint16_t value, uint16_t index, void* buffer,
                            size_t length, size_t* out_actual);
  void SetInterface(uint8_t interface, uint8_t alt_setting,
                    fit::callback<void(zx_status_t)> completer) __TA_EXCLUDES(lock_);
  void SetConfiguration(uint8_t configuration, fit::callback<void(zx_status_t)> completer)
      __TA_EXCLUDES(lock_);
  zx_status_t SetDefaultConfig(std::vector<FunctionDescriptor>& functions);

  zx_status_t AllocInterfaceLocked(size_t function_index, uint8_t* out_intf_num)
      __TA_REQUIRES(lock_);
  zx_status_t AllocEndpointLocked(size_t function_index,
                                  fuchsia_hardware_usb_descriptor::EndpointDirection direction,
                                  uint8_t* out_address) __TA_REQUIRES(lock_);
  zx_status_t AllocStringDescLocked(std::optional<size_t> function_index, std::string desc,
                                    uint8_t* out_index) __TA_REQUIRES(lock_);
  zx_status_t AllocStringDesc(std::optional<size_t> function_index, std::string desc,
                              uint8_t* out_index) __TA_EXCLUDES(lock_);

  bool AllFunctionsRegistered() const __TA_REQUIRES(lock_);

  UsbFunction& GetFunction(size_t index);
  const UsbFunction& GetFunction(size_t index) const;

  void Connect(fidl::ServerEnd<fuchsia_hardware_usb_peripheral::Device> request) {
    TRACE_DURATION("usb-peripheral", __func__);
    bindings_.AddBinding(dispatcher(), std::move(request), this, fidl::kIgnoreBindingClosure);
  }

  // `UsbFunction` wrapped in `shared_ptr` because `UsbFunction` instance may be
  // bound as a FIDL server which requires that the instance always be at the
  // same memory address and so we can generate weak pointers for lambda
  // captures for the function FIDL client.
  std::vector<std::shared_ptr<UsbFunction>> functions_;

  fidl::WireSyncClient<fuchsia_hardware_usb_dci::UsbDci> dci_;
  // USB device descriptor set via ioctl_usb_peripheral_set_device_desc()
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
  // Current USB mode set via ioctl_usb_peripheral_set_mode()
  usb_mode_t cur_usb_mode_ __TA_GUARDED(lock_) = USB_MODE_NONE;
  // Our parent's USB mode. Should not change after being set.
  usb_mode_t parent_usb_mode_ __TA_GUARDED(lock_) = USB_MODE_NONE;
  // True if we have added child devices for our functions.
  bool function_devs_added_ __TA_GUARDED(lock_) = false;
  // True if fuchsia_hardware_usb_dci::SetInterface performed in Init().
  bool set_interface_in_init_ __TA_GUARDED(lock_) = false;
  // True if we are connected to a host,
  bool connected_ __TA_GUARDED(lock_) = false;
  // True if we are under the PrepareStop() codepath.
  bool stopping_driver_ = false;
  // Current configuration number selected via USB_REQ_SET_CONFIGURATION
  // (will be 0 or 1 since we currently do not support multiple configurations).
  // 0 indicates that the device is unconfigured and should not accept USB requests
  // other than USB_REQ_SET_CONFIGURATION or requests targetting descriptors
  uint8_t configuration_ = 0;
  // USB connection speed.
  usb_speed_t speed_ = 0;

  // Registered listener
  fidl::WireSharedClient<fuchsia_hardware_usb_peripheral::Events> listener_;

  DeviceState state_ __TA_GUARDED(lock_) = DeviceState::kNoConfiguration;

  bool cache_enabled_ = true;
  bool cache_report_enabled_ = true;

  UsbMonitor usb_monitor_;

  // Wait for all functions to be cleared. Call the callback when all functions are gone.
  // If no functions are pending clearance, the callback is called immediately.
  void WaitForFunctionsCleared(fit::callback<void()> callback) __TA_EXCLUDES(lock_);

  std::vector<UnlockedCallback> on_all_functions_cleared_ __TA_GUARDED(lock_);
  std::set<uint8_t> stalled_eps_ __TA_GUARDED(lock_);

  UsbDciInterfaceServer intf_srv_{this};

  std::optional<async::Executor> executor_;

  fidl::ServerBindingGroup<fuchsia_hardware_usb_peripheral::Device> bindings_;
  fdf::OwnedChildNode child_;
  driver_devfs::Connector<fuchsia_hardware_usb_peripheral::Device> devfs_connector_{
      fit::bind_member<&UsbPeripheral::Connect>(this)};
  std::shared_ptr<fdf::Namespace> incoming_;

  std::optional<inspect::ComponentInspector> inspector_;
  inspect::Node usb_peripheral_node_;
  usb_inspect::DciInspect dci_inspect_;
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
      case usb_peripheral::UsbPeripheral::DeviceState::kStopping:
        name = "kStopping";
        break;
    }
    return std::formatter<std::string_view>::format(name, ctx);
  }
};

#endif  // SRC_DEVICES_USB_DRIVERS_USB_PERIPHERAL_USB_PERIPHERAL_H_
