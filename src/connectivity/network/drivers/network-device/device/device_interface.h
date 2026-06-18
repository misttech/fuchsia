// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_DEVICE_INTERFACE_H_
#define SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_DEVICE_INTERFACE_H_

#include <fidl/fuchsia.hardware.network.driver/cpp/driver/fidl.h>
#include <lib/async/dispatcher.h>
#include <lib/fit/function.h>

#include "data_structs.h"
#include "definitions.h"
#include "device_port.h"
#include "diagnostics_service.h"
#include "event_hook.h"
#include "log.h"
#include "port_watcher.h"
#include "public/locks.h"
#include "public/network_device.h"
#include "zircon/errors.h"

namespace network::testing {
class NetworkDeviceTest;
class FakeNetworkDeviceImpl;
}  // namespace network::testing

namespace network::internal {
class RxQueue;
class RxSessionTransaction;

class TxQueue;

class Session;
class AttachedPort;

struct RefCountedFifo : public fbl::RefCounted<RefCountedFifo> {
  zx::fifo fifo;
};

// Contains information about a filled Rx descriptor.
//
// Used to convey fulfilled rx frames in terms of descriptor indices.
struct SessionRxBuffer {
  uint16_t descriptor;
  uint32_t offset;
  uint32_t length;
};

// Helper struct containing information on an incoming complete rx frame.
//
// Used to cache common calculation and reduce number of arguments in functions.
struct RxFrameInfo {
  const fuchsia_hardware_network_driver::wire::BufferMetadata& meta;
  uint8_t port_id_salt;
  cpp20::span<const SessionRxBuffer> buffers;
  uint32_t total_length;
  uint16_t full_csums_verified;
};

enum class DeviceStatus { STARTING, STARTED, STOPPING, STOPPED };

enum class PendingDeviceOperation { NONE, START, STOP };

class DeviceInterface;

class DeviceInterface : public fidl::WireServer<netdev::Device>,
                        public fdf::WireServer<netdriver::NetworkDeviceIfc>,
                        public ::network::NetworkDeviceInterface {
 public:
  static zx::result<std::unique_ptr<DeviceInterface>> Create(
      const DeviceInterfaceDispatchers& dispatchers,
      std::unique_ptr<NetworkDeviceImplBinder>&& binder);
  ~DeviceInterface() override;

  // Public NetworkDevice API.
  void Teardown(fit::callback<void()> callback) override;
  zx_status_t Bind(fidl::ServerEnd<netdev::Device> req) override;
  zx_status_t BindPort(uint8_t port_id, fidl::ServerEnd<netdev::Port> req) override;

  // NetworkDeviceIfc implementation.
  void PortStatusChanged(netdriver::wire::NetworkDeviceIfcPortStatusChangedRequest* request,
                         fdf::Arena& arena, PortStatusChangedCompleter::Sync& completer) override;
  void AddPort(netdriver::wire::NetworkDeviceIfcAddPortRequest* request, fdf::Arena& arena,
               AddPortCompleter::Sync& completer) override;
  void RemovePort(netdriver::wire::NetworkDeviceIfcRemovePortRequest* request, fdf::Arena& arena,
                  RemovePortCompleter::Sync& completer) override;
  void CompleteRx(netdriver::wire::NetworkDeviceIfcCompleteRxRequest* request, fdf::Arena& arena,
                  CompleteRxCompleter::Sync& completer) override;
  void CompleteTx(netdriver::wire::NetworkDeviceIfcCompleteTxRequest* request, fdf::Arena& arena,
                  CompleteTxCompleter::Sync& completer) override;
  void DelegateRxLease(netdriver::wire::NetworkDeviceIfcDelegateRxLeaseRequest* request,
                       fdf::Arena& arena, DelegateRxLeaseCompleter::Sync& completer) override;
  void UpdateRxBufferParams(netdriver::wire::NetworkDeviceIfcUpdateRxBufferParamsRequest* request,
                            fdf::Arena& arena,
                            UpdateRxBufferParamsCompleter::Sync& completer) override;
  void RequestRxSpace(netdriver::wire::NetworkDeviceIfcRequestRxSpaceRequest* request,
                      fdf::Arena& arena, RequestRxSpaceCompleter::Sync& completer) override;

  uint16_t rx_fifo_depth() const;
  uint16_t tx_fifo_depth() const;
  bool IsDataVmoPrepared(uint8_t vmo_id) __TA_REQUIRES_SHARED(control_lock_) {
    auto* stored_vmo = vmo_store_.GetVmo(vmo_id);
    if (!stored_vmo) {
      return false;
    }
    return fbl::InContainer<PreparedVmosTag>(stored_vmo->meta());
  }

  // Returns the device-owned buffer count threshold at which we should trigger RxQueue work. If the
  // number of buffers on device is less than or equal to the threshold, we should attempt to fetch
  // more buffers.
  uint16_t rx_notify_threshold() const { return device_info_.rx_threshold().value_or(0); }

  TxQueue& tx_queue() { return *tx_queue_; }

  SharedLock& control_lock() __TA_RETURN_CAPABILITY(control_lock_) { return control_lock_; }
  fbl::Mutex& rx_lock() __TA_RETURN_CAPABILITY(rx_lock_) { return rx_lock_; }
  fbl::Mutex& tx_lock() __TA_RETURN_CAPABILITY(tx_lock_) { return tx_lock_; }
  const netdriver::DeviceImplInfo& info() { return device_info_; }

  // Loads rx path descriptors from the session into a session transaction.
  zx_status_t LoadRxDescriptors(RxSessionTransaction& transact) __TA_REQUIRES_SHARED(control_lock_);

  // Operates workflow for when the session is started. The data path will be started.
  void SessionStarted() __TA_RELEASE(control_lock_);
  // Operates workflow for when the session is stopped. The data path will be stopped.
  void SessionStopped() __TA_RELEASE(control_lock_);

  // If a live session exists, rx_fifo returns a reference-counted pointer to the
  // session's Rx FIFO. Otherwise, the returned pointer is null.
  fbl::RefPtr<RefCountedFifo> rx_fifo();

  // Commits all pending rx buffers in the active session.
  void CommitSession() __TA_REQUIRES_SHARED(control_lock_) __TA_REQUIRES(rx_lock_);
  // Notifies that a batch of Tx frames has been returned.
  //
  // If was_full is true, all active sessions are notified that device tx space has freed up.
  // Checks if dead sessions are ready to be destroyed due to buffers returning.
  void NotifyTxReturned(bool was_full);
  // Sends the provided space buffers in `rx` to the device implementation.
  void QueueRxSpace(cpp20::span<netdriver::wire::RxSpaceBuffer> rx)
      __TA_EXCLUDES(control_lock_, rx_lock_, tx_lock_);
  // Sends the provided transmit buffers in `tx` to the device implementation.
  void QueueTx(cpp20::span<netdriver::wire::TxBuffer> tx)
      __TA_EXCLUDES(control_lock_, rx_lock_, tx_lock_);
  bool IsDataPlaneOpen() __TA_REQUIRES_SHARED(control_lock_);

  // Called by the session when it's no longer running. If the dead session has any outstanding
  // buffers with the device implementation, it'll be kept until all the buffers
  // are safely returned and we own all the buffers again.
  void NotifyDeadSession();

  // FIDL protocol implementation.
  void GetInfo(GetInfoCompleter::Sync& completer) override;
  void OpenSession(OpenSessionRequestView request, OpenSessionCompleter::Sync& completer) override;
  void GetPort(GetPortRequestView request, GetPortCompleter::Sync& _completer) override;
  void GetPortWatcher(GetPortWatcherRequestView request,
                      GetPortWatcherCompleter::Sync& _completer) override;
  void Clone(CloneRequestView request, CloneCompleter::Sync& _completer) override;

  // Returns the current port salt for the provided base port ID.
  //
  // If the port with |base_id| does not currently exist, returns the value of
  // the previously existing port with the same |base_id| or the initial salt
  // value.
  uint8_t GetPortSalt(uint8_t base_id) __TA_REQUIRES_SHARED(control_lock_) {
    return ports_[base_id].salt;
  }

  // Notifies of |frame_length| bytes received on port with |base_id|.
  void NotifyPortRxFrame(uint8_t base_id, uint64_t frame_length)
      __TA_REQUIRES_SHARED(control_lock_);

  // Acquires a port for use in a Session.
  //
  // Sessions are notified of ports that are no longer safe to use by the DeviceInterface through
  // Session::DetachPort.
  //
  // NB: The validity of the returned AttachedPort is not really guaranteed by the type system, but
  // by the fact that DeviceInterface will detach all ports from sessions before continuing.
  zx::result<AttachedPort> AcquirePort(netdev::wire::PortId port_id,
                                       cpp20::span<const netdev::wire::FrameType> rx_frame_types)
      __TA_REQUIRES(control_lock_);

  // Event observer hook for Rx queue packets.
  void NotifyRxQueuePacket(uint64_t key);
  // Event observer hook for Tx complete.
  void NotifyTxComplete();

  DiagnosticsService& diagnostics() { return diagnostics_; }

  static void DropDelegatedRxLease(netdev::DelegatedRxLease lease);

  // Delegates a pending lease to the session.
  //
  // The lease is delegated if |completed_frame_index| is larger than the lease's
  // hold_until_frame value.
  //
  // The session receives the lease if one exists _and_ the session is
  // opted in to receive leases. Drops the pending lease immediately otherwise.
  void TryDelegateRxLease(uint64_t completed_frame_index) __TA_REQUIRES_SHARED(control_lock_)
      __TA_REQUIRES(rx_lock_);

 private:
  friend testing::NetworkDeviceTest;
  friend testing::FakeNetworkDeviceImpl;

  // Helper class to keep track of clients bound to DeviceInterface.
  class Binding : public fbl::DoublyLinkedListable<std::unique_ptr<Binding>> {
   public:
    static zx_status_t Bind(DeviceInterface* interface, fidl::ServerEnd<netdev::Device> channel)
        __TA_REQUIRES(interface->control_lock_);
    void Unbind();

   private:
    Binding() = default;
    std::optional<fidl::ServerBindingRef<netdev::Device>> binding_;
  };
  using BindingList = fbl::SizedDoublyLinkedList<std::unique_ptr<Binding>>;

  enum class TeardownState {
    RUNNING,
    BINDINGS,
    PORT_WATCHERS,
    PORTS,
    SESSION,
    DEVICE_IMPL,
    IFC_BINDING,
    BINDER,
    FINISHED
  };

  zx_status_t Init(std::unique_ptr<NetworkDeviceImplBinder>&& binder);
  explicit DeviceInterface(const DeviceInterfaceDispatchers& dispatchers);

  // Starts the data path with the device implementation.
  void StartDevice() __TA_EXCLUDES(control_lock_, tx_lock_, rx_lock_);
  void StartDeviceLocked() __TA_RELEASE(control_lock_) __TA_EXCLUDES(tx_lock_, rx_lock_);
  // Stops the data path with the device implementation.
  //
  // If continue_teardown is provided, teardown continuation will be attempted before notifying the
  // underlying device of stoppage.
  void StopDevice(std::optional<TeardownState> continue_teardown = std::nullopt)
      __TA_RELEASE(control_lock_) __TA_EXCLUDES(tx_lock_, rx_lock_);
  // Starts the device implementation with `DeviceStarted` as its callback.
  void StartDeviceInner() __TA_EXCLUDES(control_lock_);
  // Stops the device implementation with `DeviceStopped` as its callback.
  void StopDeviceInner() __TA_EXCLUDES(control_lock_);

  // Callback given to the device implementation for the `Start` call. The data path is considered
  // open only once the device is started.
  void DeviceStarted() __TA_RELEASE(control_lock_);
  // Callback given to the device implementation for the `Stop` call. All outstanding buffers are
  // automatically reclaimed once the device is considered stopped. If a teardown is pending,
  // `DeviceStopped` will complete the teardown BEFORE all buffers are reclaimed and all the
  // sessions are destroyed.
  void DeviceStopped();

  PendingDeviceOperation SetDeviceStatus(DeviceStatus status) __TA_REQUIRES(control_lock_);

  template <DataVmoIter Iter>
  void PrepareVmos(Iter begin, Iter end, fdf::Arena arena, fit::callback<void(zx::result<>)>&& cb)
      __TA_REQUIRES(control_lock_) __TA_RELEASE(control_lock_) {
    if (begin == end) {
      control_lock_.Release();
      cb(zx::ok());
      return;
    }

    VmoId id = begin->id;
    if (fbl::InContainer<PreparedVmosTag>(*begin)) {
      LOGF_INFO("VMO %d already prepared, skip preparing", id);
      PrepareVmos(std::next(begin), end, std::move(arena), std::move(cb));
      return;
    }

    DataVmoStore::StoredVmo* stored_vmo = vmo_store_.GetVmo(id);
    ZX_ASSERT(stored_vmo != nullptr);
    control_lock_.Release();

    zx::vmo vmo_clone;
    if (zx_status_t status = stored_vmo->vmo()->duplicate(ZX_RIGHT_SAME_RIGHTS, &vmo_clone);
        status != ZX_OK) {
      cb(zx::error(status));
      return;
    }
    device_impl_.buffer(arena)
        ->PrepareVmo(id, std::move(vmo_clone))
        .Then(
            [this, cur = begin, end, arena = std::move(arena), cb = std::move(cb)](
                fdf::WireUnownedResult<netdriver::NetworkDeviceImpl::PrepareVmo>& result) mutable {
              if (!result.ok() || result.value().s != ZX_OK) {
                LOGF_ERROR("PrepareVmo failed: %s", result.ok()
                                                        ? zx_status_get_string(result.value().s)
                                                        : result.FormatDescription().c_str());
                cb(zx::error(ZX_ERR_INTERNAL));
                return;
              }
              control_lock_.Acquire();
              prepared_vmos_.push_back(std::addressof(*cur));
              PrepareVmos(std::next(cur), end, std::move(arena), std::move(cb));
            });
  }

  template <DataVmoIter Iter>
  void ReleaseVmos(Iter begin, Iter end, fdf::Arena arena, fit::callback<void()> cb)
      __TA_EXCLUDES(control_lock_) {
    if (begin == end) {
      cb();
      return;
    }
    VmoId id = begin->id;

    if (!fbl::InContainer<PreparedVmosTag>(*begin)) {
      LOGF_INFO("VMO %d not prepared, skip releasing", id);
      ReleaseVmos(std::next(begin), end, std::move(arena), std::move(cb));
      return;
    }

    device_impl_.buffer(arena)->ReleaseVmo(id).Then(
        [this, cur = begin, end, arena = std::move(arena), cb = std::move(cb)](
            fdf::WireUnownedResult<netdriver::NetworkDeviceImpl::ReleaseVmo>& result) mutable {
          // Even if the release failed at the vendor driver, we continue to release the next one.
          if (!result.ok()) {
            LOGF_ERROR("ReleaseVmo failed: %s", result.FormatDescription().c_str());
          }
          auto next = std::next(cur);
          {
            fbl::AutoLock lock(&control_lock_);
            prepared_vmos_.erase(*cur);
          }
          ReleaseVmos(next, end, std::move(arena), std::move(cb));
        });
  }

  // Continues a teardown process, if one is running.
  //
  // The provided state is the expected state that the teardown process is in. If the given state is
  // not the current teardown state, no processing will happen. Otherwise, the teardown process will
  // continue if the pre-conditions to move between teardown states are met.
  //
  // Returns true if the teardown is completed and execution should be stopped.
  // ContinueTeardown is marked with many thread analysis lock exclusions so it can acquire those
  // locks internally and evaluate the teardown progress.
  bool ContinueTeardown(TeardownState state) __TA_RELEASE(control_lock_)
      __TA_EXCLUDES(tx_lock_, rx_lock_);

  // Calls f with a const std::unique_ptr<DevicePort>& to the DevicePort referenced by port_id
  // or nullptr if no ports with that id are installed.
  //
  // Returns the value returned by the call to f.
  //
  // It is unsafe to use the provided DevicePort outside of the scope of the callback f.
  template <typename F>
  auto WithPort(uint8_t port_id, F f) __TA_REQUIRES_SHARED(control_lock_) {
    if (port_id >= ports_.size()) {
      const std::unique_ptr<DevicePort> null_port;
      return f(null_port);
    }
    return f(ports_[port_id].port);
  }
  void OnPortTeardownComplete(DevicePort& port);

  // Destroys the dead session if it reports it can be destroyed through `Session::CanDestroy`.
  void PruneDeadSession() __TA_REQUIRES_SHARED(control_lock_);
  // Notifies all sessions that the transmit queue has available spots to take in transmit frames.
  void NotifyTxQueueAvailable() __TA_REQUIRES_SHARED(control_lock_);

  zx_status_t CanCreatePortWithId(uint8_t port_id) __TA_REQUIRES(control_lock_);

  netdriver::DeviceImplInfo device_info_;
  DiagnosticsService diagnostics_;
  const DeviceInterfaceDispatchers dispatchers_;
  // Only used to keep a network device shim alive during the device's lifetime.
  std::unique_ptr<NetworkDeviceImplBinder> binder_;
  std::optional<fdf::ServerBindingRef<netdriver::NetworkDeviceIfc>> ifc_binding_;
  fdf::WireSharedClient<netdriver::NetworkDeviceImpl> device_impl_;

  std::unique_ptr<Session> session_ __TA_GUARDED(control_lock_);

  struct PortSlot {
    std::unique_ptr<DevicePort> port;
    uint8_t salt;
  };
  std::array<PortSlot, netdev::wire::kMaxPorts> ports_ __TA_GUARDED(control_lock_);

  DataVmoStore vmo_store_ __TA_GUARDED(control_lock_);
  PreparedDataVmos prepared_vmos_ __TA_GUARDED(control_lock_);
  // Note: This is needed since the underlying VmoStore storage does not expose
  // iterators or the iterators cannot be implemented to support iteration and
  // deletion at the same time.
  AllDataVmos all_vmos_ __TA_GUARDED(control_lock_);
  BindingList bindings_ __TA_GUARDED(control_lock_);

  PortWatcher::List port_watchers_ __TA_GUARDED(control_lock_);

  TeardownState teardown_state_ __TA_GUARDED(control_lock_) = TeardownState::RUNNING;
  fit::callback<void()> teardown_callback_ __TA_GUARDED(control_lock_);

  PendingDeviceOperation pending_device_op_ = PendingDeviceOperation::NONE;

  std::unique_ptr<TxQueue> tx_queue_;
  std::unique_ptr<RxQueue> rx_queue_;

  DeviceStatus device_status_ __TA_GUARDED(control_lock_) = DeviceStatus::STOPPED;

  std::optional<netdev::DelegatedRxLease> rx_lease_pending_ __TA_GUARDED(rx_lock_);

  fbl::Mutex rx_lock_;
  fbl::Mutex tx_lock_ __TA_ACQUIRED_AFTER(rx_lock_);
  SharedLock control_lock_ __TA_ACQUIRED_AFTER(tx_lock_, rx_lock_);

  // Event hooks used in tests:
  EventHook<fit::function<void(const char*)>> evt_session_started_;
  // NB: This will be called with control_lock_ held.
  EventHook<fit::function<void(const char*)>> evt_session_died_;
  EventHook<fit::function<void(uint64_t)>> evt_rx_queue_packet_;
  EventHook<fit::function<void()>> evt_tx_complete_;
};

}  // namespace network::internal

#endif  // SRC_CONNECTIVITY_NETWORK_DRIVERS_NETWORK_DEVICE_DEVICE_DEVICE_INTERFACE_H_
