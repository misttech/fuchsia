// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_USB_PERIPHERAL_USB_FUNCTION_H_
#define SRC_DEVICES_USB_DRIVERS_USB_PERIPHERAL_USB_FUNCTION_H_

#include <fidl/fuchsia.boot.metadata/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.function/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.peripheral/cpp/fidl.h>
#include <lib/async/cpp/wait.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/metadata/cpp/metadata_server.h>
#include <lib/trace/event.h>

#include <atomic>
#include <format>

#include <fbl/array.h>
#include <usb-inspect/usb-inspect.h>
#include <usb/usb.h>

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace usb_peripheral {

class UsbPeripheral;

// This class represents a USB function in the peripheral role configurations.
// USB function drivers bind to this.
class UsbFunction : public fidl::Server<fuchsia_hardware_usb_function::UsbFunction>,
                    public std::enable_shared_from_this<UsbFunction> {
 public:
  UsbFunction(size_t index, UsbPeripheral* peripheral,
              fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor desc, uint8_t configuration,
              uint64_t config_generation, async_dispatcher_t* dispatcher)
      : index_(index),
        config_generation_(config_generation),
        configuration_(configuration),
        peripheral_(peripheral),
        function_descriptor_(desc),
        dispatcher_(dispatcher),
        name_(std::format("function-{:03d}", index)) {}
  ~UsbFunction() override;
  uint64_t config_generation() const { return config_generation_; }

  // Configures the function for the given bus speed. If called from an already configured state
  // (`kConfiguring` or `kConfigured`), performs a deconfigure/reconfigure cycle per USB 2.0
  // § 9.1.1.5 to reset the function's endpoint state.
  //
  // Callback Semantics:
  // - Invocation: Invoked on `dispatcher_` exactly once: asynchronously when the function driver
  //   acknowledges the `UsbFunctionInterface.SetConfigured(true)` FIDL call (or an error/cancel
  //   occurs), or synchronously with `ZX_ERR_BAD_STATE` if the function interface is unbound or in
  //   `kWaitingForBind` / `kCleared`.
  // - Status codes:
  //   * `ZX_OK`: Function driver successfully enabled its endpoints and is active.
  //   * `ZX_ERR_BAD_STATE`: Function interface is unbound or in `kWaitingForBind` / `kCleared`.
  //   * `ZX_ERR_CANCELED`: Request was superseded by a subsequent `Configure()`, `Unconfigure()`,
  //     or disconnect before the configure RPC completed.
  //   * Other error codes returned by the function driver or FIDL transport.
  void Configure(usb_speed_t speed, fit::callback<void(zx_status_t)> completer);

  // Unconfigures the function (`UsbFunctionInterface::SetConfigured(false)`), coalescing
  // concurrent callers onto a single in-flight RPC and canceling any queued `Configure()` request.
  //
  // Callback Semantics:
  // - Invocation: Invoked on `dispatcher_` exactly once when the function driver finishes
  //   disabling its endpoints and canceling in-flight transfers. If the function is already
  //   unconfigured, unbound, or in `kCleared`, the callback is invoked synchronously with `ZX_OK`.
  // - Status codes:
  //   * `ZX_OK`: Function is deconfigured (or was already unconfigured/unbound/closed).
  //   * Unexpected error codes returned by the function driver or FIDL transport.
  void Unconfigure(fit::callback<void(zx_status_t)> completer);

  // Selects an alternate setting for an interface on the function driver.
  //
  // Callback Semantics:
  // - Invocation: Invoked on `dispatcher_` exactly once: asynchronously when the function driver
  //   acknowledges `UsbFunctionInterface.SetInterface()`, or synchronously with `ZX_ERR_BAD_STATE`
  //   if the function interface is invalid.
  // - Status codes:
  //   * `ZX_OK`: Interface alternate setting applied successfully.
  //   * `ZX_ERR_BAD_STATE`: Function interface is invalid.
  //   * Errors propagated from the function driver or FIDL transport.
  void SetInterface(uint8_t interface, uint8_t alt_setting,
                    fit::callback<void(zx_status_t)> completer);

  // Forwards a setup control request to the function driver via `UsbFunctionInterface.Control()`.
  //
  // Callback Semantics:
  // - Invocation: Invoked on `dispatcher_` exactly once with the read payload on success or an
  //   error status on failure.
  void Control(const fdescriptor::wire::UsbSetup& setup, std::span<const uint8_t> write_buffer,
               fit::callback<void(zx::result<std::vector<uint8_t>>)> completer);
  size_t function_index() const { return index_; }
  std::string name() const { return name_; }
  void RequestRemoval();
  uint8_t configuration() const { return configuration_; }

  void OnNodeControllerUnbound(fidl::UnbindInfo info);

  inline const usb_descriptor_header_t* GetDescriptors(size_t* out_length) const {
    *out_length = descriptors_.size();
    return reinterpret_cast<usb_descriptor_header_t*>(descriptors_.data());
  }

  inline const fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor& GetFunctionDescriptor()
      const {
    return function_descriptor_;
  }

  inline uint8_t GetNumInterfaces() const {
    return num_interfaces_.load(std::memory_order_relaxed);
  }

  // Returns all endpoint addresses associated with the given interface and alternate setting
  // by parsing the function's descriptors. Returns std::nullopt if the function has no descriptors
  // (e.g. mock test functions).
  std::optional<std::vector<uint8_t>> GetEndpointsForInterface(uint8_t interface_num,
                                                               uint8_t alt_setting) const;

  // fuchsia_hardware_usb_function.UsbFunction protocol implementation.
  void ConnectToEndpoint(ConnectToEndpointRequest& request,
                         ConnectToEndpointCompleter::Sync& completer) override;
  void Configure(ConfigureRequest& request, ConfigureCompleter::Sync& completer) override;
  void AllocResources(AllocResourcesRequest& request,
                      AllocResourcesCompleter::Sync& completer) override;
  void EndpointSetStall(EndpointSetStallRequest& request,
                        EndpointSetStallCompleter::Sync& completer) override;
  void EndpointClearStall(EndpointClearStallRequest& request,
                          EndpointClearStallCompleter::Sync& completer) override;
  void ConfigureEndpoint(ConfigureEndpointRequest& request,
                         ConfigureEndpointCompleter::Sync& completer) override;
  void DisableEndpoint(DisableEndpointRequest& request,
                       DisableEndpointCompleter::Sync& completer) override;
  void Deconfigure(DeconfigureCompleter::Sync& completer) override;

  zx::result<> AddChild(fidl::UnownedClientEnd<fuchsia_driver_framework::Node> parent,
                        const std::shared_ptr<fdf::Namespace>& incoming,
                        const std::shared_ptr<fdf::OutgoingDirectory>& outgoing);
  bool registered() const { return function_intf_.is_valid(); }
  bool is_registered_for_inspect() const { return registered_.load(std::memory_order_relaxed); }
  uint8_t num_interfaces() const { return num_interfaces_.load(std::memory_order_relaxed); }
  bool is_configured() const {
    return configured_.load(std::memory_order_relaxed) &&
           !unconfigure_in_flight_.load(std::memory_order_relaxed);
  }
  bool is_unconfigure_in_flight() const {
    return unconfigure_in_flight_.load(std::memory_order_relaxed);
  }
  int64_t last_unconfigure_duration_ns() const {
    return last_unconfigure_duration_ns_.load(std::memory_order_relaxed);
  }
  uint64_t unconfigure_error_count() const {
    return unconfigure_error_count_.load(std::memory_order_relaxed);
  }

 private:
  DISALLOW_COPY_ASSIGN_AND_MOVE(UsbFunction);

  // Tracks the lifecycle and `UsbFunctionInterface::SetConfigured` RPC state of this function:
  // - `kWaitingForBind`: Child node created (or interface unbound); waiting for the function
  //   driver to call `UsbFunction::Configure` and bind `function_intf_`.
  // - `kBound`: `function_intf_` is bound, prior to any `SetConfigured` RPC. An initial
  //   `Unconfigure()` sends `SetConfigured(false)` once to transition to `kUnconfigured`.
  // - `kConfiguring`: A `SetConfigured(true, speed)` FIDL call is in flight.
  // - `kConfigured`: The function driver has acknowledged `SetConfigured(true, speed)`.
  // - `kUnconfiguring`: A `SetConfigured(false, speed)` FIDL call is in flight. Concurrent
  //   `Unconfigure()` callers coalesce onto `pending_unconfigure_completers_`, and at most one
  //   `Configure()` call may queue in `pending_configure_`.
  // - `kUnconfigured`: `SetConfigured(false)` has completed; subsequent `Unconfigure()` calls
  //   complete immediately as idempotent no-ops.
  // - `kCleared`: Child node has been removed/unbound; terminal state for this instance.
  enum class FunctionState : uint8_t {
    kWaitingForBind,
    kBound,
    kConfiguring,
    kConfigured,
    kUnconfiguring,
    kUnconfigured,
    kCleared,
  };

  void CloseFunctionInterface();
  void StartUnconfigure(fuchsia_hardware_usb_descriptor::wire::UsbSpeed speed);
  void CompletePendingUnconfigures(zx_status_t status);
  void SendSetConfiguredTrue(fuchsia_hardware_usb_descriptor::wire::UsbSpeed speed,
                             fit::callback<void(zx_status_t)> completer);
  void SetDescriptors(uint8_t* descriptors, size_t length);
  void ClearDescriptors();

  // fidl::WireAsyncEventHandler<fuchsia_hardware_usb_function::UsbFunctionInterface>
  class FunctionEventHandler
      : public fidl::WireAsyncEventHandler<fuchsia_hardware_usb_function::UsbFunctionInterface> {
   public:
    FunctionEventHandler(UsbFunction* parent, uint64_t bind_generation)
        : parent_(parent->weak_from_this()), bind_generation_(bind_generation) {}
    ~FunctionEventHandler();
    void on_fidl_error(fidl::UnbindInfo info) override;
    void handle_unknown_event(
        fidl::UnknownEventMetadata<fuchsia_hardware_usb_function::UsbFunctionInterface> metadata)
        override;

   private:
    std::weak_ptr<UsbFunction> parent_;
    const uint64_t bind_generation_;
  };

  class NodeControllerEventHandler
      : public fidl::WireAsyncEventHandler<fuchsia_driver_framework::NodeController> {
   public:
    explicit NodeControllerEventHandler(UsbFunction* parent) : parent_(parent->weak_from_this()) {}
    void on_fidl_error(fidl::UnbindInfo info) override {
      if (std::shared_ptr parent = parent_.lock()) {
        parent->OnNodeControllerUnbound(info);
      }
    }
    void handle_unknown_event(
        fidl::UnknownEventMetadata<fuchsia_driver_framework::NodeController> metadata) override {}

   private:
    std::weak_ptr<UsbFunction> parent_;
  };

  const size_t index_;
  const uint64_t config_generation_;
  uint8_t configuration_;

  struct PendingConfigure {
    fuchsia_hardware_usb_descriptor::wire::UsbSpeed speed;
    fit::callback<void(zx_status_t)> completer;
  };
  FunctionState state_ = FunctionState::kWaitingForBind;
  uint64_t bind_generation_ = 0;
  uint64_t rpc_generation_ = 0;
  fit::callback<void(zx_status_t)> active_configure_completer_;
  // Holds at most one pending `Configure()` request that arrived while an unconfigure operation
  // (`kUnconfiguring`) was in flight per USB 2.0 § 9.1.1.5. Dispatched once the in-flight
  // `SetConfigured(false)` finishes, or canceled with `ZX_ERR_CANCELED` if a disconnect occurs.
  std::optional<PendingConfigure> pending_configure_;
  // Queue of completion callbacks from concurrent `Unconfigure()` callers coalesced onto the
  // single in-flight `SetConfigured(false)` RPC. All completers are drained with `ZX_OK`
  // (or error) once the RPC completes.
  std::vector<fit::callback<void(zx_status_t)>> pending_unconfigure_completers_;
  UsbPeripheral* peripheral_;

  fidl::WireSharedClient<fuchsia_hardware_usb_function::UsbFunctionInterface> function_intf_;
  const fuchsia_hardware_usb_peripheral::wire::FunctionDescriptor function_descriptor_;

  std::atomic<uint8_t> num_interfaces_{0};
  fbl::Array<uint8_t> descriptors_;

  async_dispatcher_t* dispatcher_;
  fidl::ServerBindingGroup<fuchsia_hardware_usb_function::UsbFunction> bindings_;
  fidl::WireSharedClient<fuchsia_driver_framework::NodeController> child_;
  compat::SyncInitializedDeviceServer compat_server_;
  std::optional<fdf_metadata::MetadataServer<fuchsia_boot_metadata::MacAddressMetadata>>
      mac_address_metadata_server_;
  std::optional<fdf_metadata::MetadataServer<fuchsia_boot_metadata::SerialNumberMetadata>>
      serial_number_metadata_server_;
  std::optional<DeconfigureCompleter::Async> deconfigure_completer_;
  std::shared_ptr<fdf::OutgoingDirectory> outgoing_;
  std::string name_;

  std::atomic<bool> registered_{false};
  std::atomic<bool> configured_{false};
  std::atomic<bool> unconfigure_in_flight_{false};
  std::atomic<zx_time_t> unconfigure_start_time_{ZX_TIME_INFINITE_PAST};
  std::atomic<int64_t> last_unconfigure_duration_ns_{0};
  std::atomic<uint64_t> unconfigure_error_count_{0};

  usb_inspect::FunctionInspect inspect_;
};

}  // namespace usb_peripheral

#endif  // SRC_DEVICES_USB_DRIVERS_USB_PERIPHERAL_USB_FUNCTION_H_
