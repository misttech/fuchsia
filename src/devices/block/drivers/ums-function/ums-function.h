// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BLOCK_DRIVERS_UMS_FUNCTION_UMS_FUNCTION_H_
#define SRC_DEVICES_BLOCK_DRIVERS_UMS_FUNCTION_UMS_FUNCTION_H_

#include <fidl/fuchsia.hardware.usb.function/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/zx/result.h>

#include <atomic>
#include <format>
#include <optional>
#include <queue>
#include <string>
#include <utility>

#include <fbl/condition_variable.h>
#include <fbl/mutex.h>
#include <usb-endpoint/usb-endpoint-client.h>
#include <usb/descriptors.h>
#include <usb/request-fidl.h>
#include <usb/ums.h>

namespace ums {

class UmsFunction : public fdf::DriverBase,
                    public fidl::Server<fuchsia_hardware_usb_function::UsbFunctionInterface> {
 public:
  static constexpr char kDriverName[] = "usb-ums-function";
  static constexpr uint32_t kBlockSize = 512;
  static constexpr size_t kStorageSize = 4L * 1024L * 1024L * 1024L;
  static constexpr uint64_t kBlockCount = kStorageSize / kBlockSize;
  static constexpr size_t kDataReqSize = 16384;
  static constexpr uint16_t kBulkMaxPacket = 512;

  UmsFunction(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher dispatcher)
      : DriverBase(kDriverName, std::move(start_args), std::move(dispatcher)) {}
  ~UmsFunction() = default;

  zx::result<> Start() override;
  void PrepareStop(fdf::PrepareStopCompleter completer) override;

  // fuchsia_hardware_usb_function::UsbFunctionInterface impl.
  void Control(ControlRequest& req, ControlCompleter::Sync& completer) override;
  void SetConfigured(SetConfiguredRequest& req, SetConfiguredCompleter::Sync& completer) override;
  void SetInterface(SetInterfaceRequest& req, SetInterfaceCompleter::Sync& completer) override;

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_hardware_usb_function::UsbFunctionInterface> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override {
    fdf::error("Unknown method %ld", metadata.method_ordinal);
  }

 private:
  enum DataState {
    DATA_STATE_NONE,
    DATA_STATE_READ,
    DATA_STATE_WRITE,
    DATA_STATE_UNMAP,
    DATA_STATE_FAILED
  };
  friend struct std::formatter<DataState>;

  void RequestQueueLocked(usb::FidlRequest* req) __TA_REQUIRES(mtx_);
  void InEpCallback(std::vector<fuchsia_hardware_usb_endpoint::Completion> completion);
  void OutEpCallback(std::vector<fuchsia_hardware_usb_endpoint::Completion> completion);
  bool IsReadyForShutdown() const { return !active_.load() && pending_request_count_.load() == 0; }

  zx::result<> ConfigureEndpoint(const usb_endpoint_info_descriptor_t& desc);

  // Main driver initialization.
  zx_status_t Init();

  void QueueDataLocked(usb::FidlRequest* req) __TA_REQUIRES(mtx_);
  void QueueCswLocked(uint8_t status, bool also_cbw = true) __TA_REQUIRES(mtx_);
  void ContinueTransferLocked() __TA_REQUIRES(mtx_);
  void StartTransferLocked(DataState state, uint32_t transfer_bytes, uint64_t lba = 0)
      __TA_REQUIRES(mtx_);
  zx::result<> CancelAllLocked(usb::EndpointClient<UmsFunction>& ep) __TA_REQUIRES(mtx_);

  void HandleInquiryLocked(ums_cbw_t* cbw) __TA_REQUIRES(mtx_);
  void HandleTestUnitReadyLocked(ums_cbw_t* cbw) __TA_REQUIRES(mtx_);
  void HandleRequestSenseLocked(ums_cbw_t* cbw) __TA_REQUIRES(mtx_);
  void HandleReadCapacity10Locked(ums_cbw_t* cbw) __TA_REQUIRES(mtx_);
  void HandleReadCapacity16Locked(ums_cbw_t* cbw) __TA_REQUIRES(mtx_);
  void HandleModeSense6Locked(ums_cbw_t* cbw) __TA_REQUIRES(mtx_);
  void HandleRead10Locked(ums_cbw_t* cbw) __TA_REQUIRES(mtx_);
  void HandleRead12Locked(ums_cbw_t* cbw) __TA_REQUIRES(mtx_);
  void HandleRead16Locked(ums_cbw_t* cbw) __TA_REQUIRES(mtx_);
  void HandleWrite10Locked(ums_cbw_t* cbw) __TA_REQUIRES(mtx_);
  void HandleWrite12Locked(ums_cbw_t* cbw) __TA_REQUIRES(mtx_);
  void HandleWrite16Locked(ums_cbw_t* cbw) __TA_REQUIRES(mtx_);
  void HandleUnmapLocked(ums_cbw_t* cbw) __TA_REQUIRES(mtx_);

  void HandleCbwLocked(ums_cbw_t* cbw) __TA_REQUIRES(mtx_);
  void CbwCompleteLocked() __TA_REQUIRES(mtx_);
  void CswCompleteLocked() __TA_REQUIRES(mtx_);
  void DataCompleteLocked() __TA_REQUIRES(mtx_);

  int WorkerLoop();

  bool IsInData() const { return current_cbw_.bmCBWFlags & USB_DIR_IN; }
  bool IsOutData() const { return !IsInData(); }

  fidl::SyncClient<fuchsia_hardware_usb_function::UsbFunction> function_;
  fidl::ServerBindingGroup<fuchsia_hardware_usb_function::UsbFunctionInterface> bindings_;
  bool configured_ = false;

  std::optional<usb::FidlRequest> cbw_req_;
  std::optional<fuchsia_hardware_usb_endpoint::Completion> cbw_req_complete_ __TA_GUARDED(mtx_);
  bool cbw_in_flight_ __TA_GUARDED(mtx_) = false;
  uint64_t cbw_vmo_id_;

  std::optional<usb::FidlRequest> data_in_req_;
  std::optional<usb::FidlRequest> data_out_req_;
  std::optional<fuchsia_hardware_usb_endpoint::Completion> data_req_complete_ __TA_GUARDED(mtx_);
  bool data_in_flight_ __TA_GUARDED(mtx_) = false;

  std::optional<usb::FidlRequest> csw_req_;
  std::optional<fuchsia_hardware_usb_endpoint::Completion> csw_req_complete_ __TA_GUARDED(mtx_);
  bool csw_in_flight_ __TA_GUARDED(mtx_) = false;
  uint64_t csw_vmo_id_;
  std::queue<uint8_t> csw_q_ __TA_GUARDED(mtx_);

  // Command status wrapper (csw) and in-data to host.
  usb::EndpointClient<UmsFunction> in_ep_{usb::EndpointType::BULK, this,
                                          std::mem_fn(&UmsFunction::InEpCallback)};

  // Command block wrapper (cbw) and out-data from host.
  usb::EndpointClient<UmsFunction> out_ep_{usb::EndpointType::BULK, this,
                                           std::mem_fn(&UmsFunction::OutEpCallback)};

  fdf::SynchronizedDispatcher dispatcher_;

  // vmo for backing storage
  static zx::vmo vmo_;
  void* storage_;

  // command we are currently handling
  ums_cbw_t current_cbw_ = {};
  // data transferred for the current command
  uint32_t data_length_ = 0;

  // state for data transfers
  DataState data_state_;
  // state for reads and writes
  zx_off_t data_offset_ = 0;
  size_t data_remaining_ = 0;

  uint8_t bulk_out_addr_;
  uint8_t bulk_in_addr_;
  thrd_t thread_;
  std::atomic_bool active_;
  fbl::Mutex mtx_;
  fbl::ConditionVariable condvar_ __TA_GUARDED(mtx_);
  std::atomic_int pending_request_count_ = 0;
};

}  // namespace ums

template <>
struct std::formatter<ums::UmsFunction::DataState> : std::formatter<std::string> {
  auto format(const ums::UmsFunction::DataState state, format_context& ctx) const {
    std::string fmt;
    switch (state) {
      case ums::UmsFunction::DataState::DATA_STATE_NONE:
        fmt = "None";
        break;
      case ums::UmsFunction::DataState::DATA_STATE_READ:
        fmt = "Read";
        break;
      case ums::UmsFunction::DataState::DATA_STATE_WRITE:
        fmt = "Write";
        break;
      case ums::UmsFunction::DataState::DATA_STATE_UNMAP:
        fmt = "Unmap";
        break;
      case ums::UmsFunction::DataState::DATA_STATE_FAILED:
        fmt = "Failed";
        break;
      default:
        fmt = "<unknown>";
        break;
    };
    return std::formatter<std::string>::format(fmt, ctx);
  }
};

#endif  // SRC_DEVICES_BLOCK_DRIVERS_UMS_FUNCTION_UMS_FUNCTION_H_
