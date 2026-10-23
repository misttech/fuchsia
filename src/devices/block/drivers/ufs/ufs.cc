// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "ufs.h"

#include <endian.h>
#include <fidl/fuchsia.hardware.ufs/cpp/wire_types.h>
#include <fidl/fuchsia.power.system/cpp/fidl.h>
#include <lib/arch/intrin.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/logging/cpp/structured_logger.h>
#include <lib/driver/platform-device/cpp/pdev.h>
#include <lib/fit/defer.h>
#include <lib/sync/cpp/completion.h>
#include <lib/trace/event.h>
#include <zircon/errors.h>

#include <algorithm>
#include <array>
#include <atomic>
#include <mutex>
#include <vector>

#include <safemath/safe_conversions.h>

#include "rpmb.h"
#include "src/devices/block/drivers/ufs/server.h"

namespace ufs {
namespace {

constexpr std::string_view kIrqDispatcherRole = "fuchsia.devices.storage.ufs.irq";
constexpr std::string_view kIoDispatcherRole = "fuchsia.devices.storage.ufs.worker";

}  // namespace

zx::result<> Ufs::NotifyEventCallback(NotifyEvent event, uint64_t data) {
  switch (event) {
    // This should all be done by the bootloader at start up and not reperformed.
    case NotifyEvent::kInit:
    // This is normally done at init, but isn't necessary.
    case NotifyEvent::kReset:
    case NotifyEvent::kPreLinkStartup:
    case NotifyEvent::kPostLinkStartup:
    case NotifyEvent::kDeviceInitDone:
    case NotifyEvent::kSetupTransferRequestList:
    case NotifyEvent::kSetupTaskManagementRequestList:
    case NotifyEvent::kPrePowerModeChange:
    case NotifyEvent::kPostPowerModeChange:
    case NotifyEvent::kPreSuspend:
    case NotifyEvent::kPostSuspend:
    case NotifyEvent::kPreResume:
    case NotifyEvent::kPostResume:
      return zx::ok();
    default:
      return zx::error(ZX_ERR_INVALID_ARGS);
  };
}

zx::result<> Ufs::Notify(NotifyEvent event, uint64_t data) {
  if (!host_controller_callback_) {
    return zx::error(ZX_ERR_BAD_HANDLE);
  }
  return host_controller_callback_(event, data);
}

zx_status_t Ufs::WaitWithTimeout(fit::function<bool()> wait_for, zx::duration timeout,
                                 const fbl::String& timeout_message, zx::duration granularity) {
  if (wait_for()) {
    return ZX_OK;
  }
  const zx::time deadline = zx::deadline_after(timeout);
  // Most conditions polled here (UIC command completion, controller ready) are satisfied within
  // ~2-15us in hardware; spin for up to 25us before paying the kernel timer slack of nanosleep.
  const zx::time spin_deadline = zx::deadline_after(std::min(timeout, zx::usec(25)));
  while (zx::clock::get_monotonic() < spin_deadline) {
    if (wait_for()) {
      return ZX_OK;
    }
    arch::Yield();
  }
  while (true) {
    if (wait_for()) {
      return ZX_OK;
    }
    if (zx::clock::get_monotonic() >= deadline) {
      if (wait_for()) {
        return ZX_OK;
      }
      fdf::error("{} after {} usecs", timeout_message.begin(), timeout.to_usecs());
      return ZX_ERR_TIMED_OUT;
    }
    zx::nanosleep(zx::deadline_after(granularity));
  }
}

zx::result<> Ufs::AllocatePages(zx::vmo& vmo, fzl::VmoMapper& mapper, size_t size) {
  const uint32_t data_size =
      fbl::round_up(safemath::checked_cast<uint32_t>(size), zx_system_get_page_size());
  if (zx_status_t status = zx::vmo::create(data_size, 0, &vmo); status != ZX_OK) {
    return zx::error(status);
  }

  if (zx_status_t status = mapper.Map(vmo, 0, data_size); status != ZX_OK) {
    fdf::error("Failed to map IO buffer: {}", zx_status_get_string(status));
    return zx::error(status);
  }
  return zx::ok();
}

zx::result<uint16_t> Ufs::TranslateUfsLunToScsiLun(uint8_t ufs_lun) {
  // Logical unit
  if (!(ufs_lun & kUfsWellKnownlunId)) {
    if (ufs_lun > kMaxLunIndex) {
      return zx::error(ZX_ERR_OUT_OF_RANGE);
    }
    return zx::ok(ufs_lun);
  }

  // Well known logical unit
  return zx::ok(static_cast<uint16_t>((static_cast<uint16_t>(ufs_lun) & ~kUfsWellKnownlunId) |
                                      kScsiWellKnownLunId));
}

zx::result<uint8_t> Ufs::TranslateScsiLunToUfsLun(uint16_t scsi_lun) {
  // Well known logical unit
  if ((scsi_lun & kScsiWellKnownLunIdMask) == kScsiWellKnownLunId) {
    return zx::ok(static_cast<uint8_t>((scsi_lun & kMaxLunId) | kUfsWellKnownlunId));
  }

  // Logical unit
  if ((scsi_lun & kScsiWellKnownLunIdMask) != 0) {
    fdf::error("Invalid scsi lun: 0x{:x}", scsi_lun);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  if ((scsi_lun & kMaxLunId) > kMaxLunIndex) {
    return zx::error(ZX_ERR_OUT_OF_RANGE);
  }
  return zx::ok(static_cast<uint8_t>(scsi_lun & kMaxLunId));
}

void Ufs::ProcessIoSubmissions() {
  while (true) {
    std::optional<IoCommand> maybe_cmd;
    {
      std::lock_guard<std::mutex> lock(lock_);
      if (shutdown_ || pending_commands_.empty()) {
        return;
      }
      maybe_cmd.emplace(std::move(pending_commands_.front()));
      pending_commands_.pop_front();
    }

    IoCommand io_cmd = std::move(*maybe_cmd);

    DataDirection data_direction = DataDirection::kNone;
    if (io_cmd.request.is_write()) {
      data_direction = DataDirection::kHostToDevice;
    } else if (io_cmd.request.data_vmo()->is_valid()) {
      data_direction = DataDirection::kDeviceToHost;
    }

    uint32_t transfer_bytes = 0;
    uint64_t dma_offset = 0;
    uint64_t dma_length = 0;
    zx::vmo unmap_vmo;

    if (data_direction != DataDirection::kNone) {
      if (io_cmd.request.immediate_data().size() > 0) {
        // For the UNMAP command, a data buffer is required for the parameter list.
        const size_t immediate_size = io_cmd.request.immediate_data().size();
        fzl::VmoMapper mapper;
        if (zx::result<> result = AllocatePages(unmap_vmo, mapper, immediate_size);
            result.is_error()) {
          fdf::error("Failed to allocate data buffer: {}", result);
          io_cmd.request.Complete(result.status_value());
          continue;
        }
        memcpy(mapper.start(), io_cmd.request.immediate_data().data(), immediate_size);
        transfer_bytes = safemath::checked_cast<uint32_t>(immediate_size);
        dma_offset = 0;
        dma_length = zx_system_get_page_size();
      } else {
        transfer_bytes =
            safemath::checked_cast<uint32_t>(io_cmd.request.transfer_length() * io_cmd.block_size);
        dma_offset = io_cmd.request.vmo_offset();
        dma_length = transfer_bytes;
      }
    }

    if (transfer_bytes > max_transfer_bytes_) {
      fdf::error("Request exceeding max transfer size. transfer_bytes={}, max_transfer_bytes_={}",
                 transfer_bytes, max_transfer_bytes_);
      io_cmd.request.Complete(ZX_ERR_INVALID_ARGS);
      continue;
    }

    std::span<const uint8_t> cdb = io_cmd.request.cdb();
    ScsiCommandUpiu upiu(cdb.data(), safemath::checked_cast<uint8_t>(cdb.size()), data_direction,
                         transfer_bytes);

    zx::result<uint8_t> slot = transfer_request_processor_->ReserveSlot();
    if (slot.is_error()) {
      // ZX_ERR_NO_RESOURCES: the request list is full. ZX_ERR_BAD_STATE: slot allocation is
      // blocked for a power transition (see RequestProcessor::BlockSlotAllocation()). Either way
      // the command stays queued and is retried on the next TriggerIoWork().
      if (slot.error_value() == ZX_ERR_NO_RESOURCES || slot.error_value() == ZX_ERR_BAD_STATE) {
        std::lock_guard<std::mutex> lock(lock_);
        if (shutdown_) {
          io_cmd.request.Complete(ZX_ERR_IO_NOT_PRESENT);
          return;
        }
        pending_commands_.push_front(std::move(io_cmd));
        return;
      }
      io_cmd.request.Complete(slot.error_value());
      continue;
    }

    zx::unowned_vmo data_vmo;
    if (data_direction != DataDirection::kNone) {
      data_vmo = unmap_vmo.is_valid() ? unmap_vmo.borrow() : io_cmd.request.data_vmo();
    }
    const uint8_t lun = io_cmd.lun;
    const auto inline_crypto = io_cmd.request.inline_crypto();
    const scsi::Opcode opcode = io_cmd.request.opcode();
    uint64_t io_bytes = static_cast<uint64_t>(io_cmd.request.transfer_length()) * io_cmd.block_size;
    if (opcode == scsi::Opcode::UNMAP &&
        io_cmd.request.immediate_data().size() >=
            sizeof(scsi::UnmapParameterListHeader) + sizeof(scsi::UnmapBlockDescriptor)) {
      const auto* desc = reinterpret_cast<const scsi::UnmapBlockDescriptor*>(
          io_cmd.request.immediate_data().data() + sizeof(scsi::UnmapParameterListHeader));
      io_bytes = static_cast<uint64_t>(betoh32(desc->blocks)) * io_cmd.block_size;
    }

    auto cb = [this, req = std::move(io_cmd.request), vmo = std::move(unmap_vmo), opcode,
               io_bytes](zx_status_t status) mutable {
      vmo.reset();
      if (status == ZX_OK) {
        switch (opcode) {
          case scsi::Opcode::READ_10:
          case scsi::Opcode::READ_12:
          case scsi::Opcode::READ_16:
            properties_.read_ops.Add(1);
            properties_.read_bytes.Add(io_bytes);
            break;
          case scsi::Opcode::WRITE_10:
          case scsi::Opcode::WRITE_12:
          case scsi::Opcode::WRITE_16:
            properties_.write_ops.Add(1);
            properties_.write_bytes.Add(io_bytes);
            break;
          case scsi::Opcode::SYNCHRONIZE_CACHE_10:
          case scsi::Opcode::SYNCHRONIZE_CACHE_16:
            properties_.flush_ops.Add(1);
            break;
          case scsi::Opcode::UNMAP:
            properties_.trim_ops.Add(1);
            properties_.trim_bytes.Add(io_bytes);
            break;
          default:
            break;
        }
      }
      req.Complete(status);
    };

    transfer_request_processor_->SendIoScsiCmd(upiu, lun, slot.value(), std::move(data_vmo),
                                               dma_offset, dma_length, std::move(cb),
                                               inline_crypto);
  }
}

void Ufs::ProcessAdminCompletions() {
  transfer_request_processor_->ProcessCompletionOfAdminRequests();
}

void Ufs::ProcessIoCompletions() { transfer_request_processor_->ProcessCompletionOfIoRequests(); }

void Ufs::ProcessErrors() {
  // Wait for all commands in the request list to complete before recovering if an error occurs in
  // the request.(The LUN may be reset during the recovery process)
  if (transfer_request_processor_->GetInflightIoCount() == 0) {
    // TODO: Implement ErrorHandler.
  }
}

zx::result<> Ufs::Isr() {
  if (IsHardwarePoweredOff()) {
    // The UFSHCI power domains/clocks are gated, so the register space must not be accessed. This
    // can only be a late delivery of an interrupt that was already pending when IE was cleared in
    // StopHostControllerForPowerDown(), and there is nothing to stash: in-flight I/O was drained
    // before the controller was stopped, HCE = 0 reset the controller (doorbells, IS and UIC state
    // included), and ReinitAfterPowerDown() re-initializes it from scratch, so whatever this
    // interrupt reported no longer exists.
    return zx::ok();
  }

  const fdf::MmioBuffer& mmio = mmio_.value();
  auto interrupt_status = InterruptStatusReg::Get().ReadFrom(&mmio);

  // TODO(https://fxbug.dev/42075643): implement error handlers
  if (interrupt_status.uic_error()) {
    fdf::error("UFS: UIC error on ISR");
    InterruptStatusReg::Get().FromValue(0).set_uic_error(true).WriteTo(&mmio);

    // UECPA for Host UIC Error Code within PHY Adapter Layer.
    if (HostUicErrorCodePhyAdapterLayerReg::Get().ReadFrom(&mmio).uic_phy_adapter_layer_error()) {
      fdf::error("UECPA error code: 0x{:x}", HostUicErrorCodePhyAdapterLayerReg::Get()
                                                 .ReadFrom(&mmio)
                                                 .uic_phy_adapter_layer_error_code());
    }
    // UECDL for Host UIC Error Code within Data Link Layer.
    if (HostUicErrorCodeDataLinkLayerReg::Get().ReadFrom(&mmio).uic_data_link_layer_error()) {
      fdf::error(
          "UECDL error code: 0x{:x}",
          HostUicErrorCodeDataLinkLayerReg::Get().ReadFrom(&mmio).uic_data_link_layer_error_code());
    }
    // UECN for Host UIC Error Code within Network Layer.
    if (HostUicErrorCodeNetworkLayerReg::Get().ReadFrom(&mmio).uic_network_layer_error()) {
      fdf::error(
          "UECN error code: 0x{:x}",
          HostUicErrorCodeNetworkLayerReg::Get().ReadFrom(&mmio).uic_network_layer_error_code());
    }
    // UECT for Host UIC Error Code within Transport Layer.
    if (HostUicErrorCodeTransportLayerReg::Get().ReadFrom(&mmio).uic_transport_layer_error()) {
      fdf::error("UECT error code: 0x{:x}", HostUicErrorCodeTransportLayerReg::Get()
                                                .ReadFrom(&mmio)
                                                .uic_transport_layer_error_code());
    }
    // UECDME for Host UIC Error Code within DME subcomponent.
    if (HostUicErrorCodeReg::Get().ReadFrom(&mmio).uic_dme_error()) {
      fdf::error("UECDME error code: 0x{:x}",
                 HostUicErrorCodeReg::Get().ReadFrom(&mmio).uic_dme_error_code());
    }
  }
  if (interrupt_status.device_fatal_error_status()) {
    fdf::error("UFS: Device fatal error on ISR");
    InterruptStatusReg::Get().FromValue(0).set_device_fatal_error_status(true).WriteTo(&mmio);
  }
  if (interrupt_status.host_controller_fatal_error_status()) {
    fdf::error("UFS: Host controller fatal error on ISR");
    InterruptStatusReg::Get().FromValue(0).set_host_controller_fatal_error_status(true).WriteTo(
        &mmio);
  }
  if (interrupt_status.system_bus_fatal_error_status()) {
    fdf::error("UFS: System bus fatal error on ISR");
    InterruptStatusReg::Get().FromValue(0).set_system_bus_fatal_error_status(true).WriteTo(&mmio);
  }
  if (interrupt_status.crypto_engine_fatal_error_status()) {
    fdf::error("UFS: Crypto engine fatal error on ISR");
    InterruptStatusReg::Get().FromValue(0).set_crypto_engine_fatal_error_status(true).WriteTo(
        &mmio);
  }
  // Handle command completion interrupts.
  if (interrupt_status.utp_transfer_request_completion_status()) {
    InterruptStatusReg::Get().FromValue(0).set_utp_transfer_request_completion_status(true).WriteTo(
        &mmio);
    TriggerIoWork();
  }

  // Check admin command completion if an admin command is active.
  uint32_t door_bell = UtrListDoorBellReg::Get().ReadFrom(&mmio).door_bell();
  if (transfer_request_processor_->CheckAdminCommandCompleted(door_bell)) {
    TriggerAdminWork();
  }
  if (interrupt_status.utp_task_management_request_completion_status()) {
    InterruptStatusReg::Get()
        .FromValue(0)
        .set_utp_task_management_request_completion_status(true)
        .WriteTo(&mmio);
    task_management_request_processor_->ProcessCompletionOfIoRequests();
  }
  if (interrupt_status.uic_command_completion_status()) {
    // TODO(https://fxbug.dev/42075643): Handle UIC completion
    fdf::error("UFS: UIC completion not yet implemented");
  }

  return zx::ok();
}

void Ufs::HandleIrq(async_dispatcher_t* dispatcher, async::IrqBase* irq, zx_status_t status,
                    const zx_packet_interrupt_t* interrupt) {
  if (status != ZX_OK) {
    if (status == ZX_ERR_CANCELED) {
      fdf::debug("Interrupt cancelled.");
    } else {
      fdf::error("Failed to wait for interrupt: {}", zx_status_get_string(status));
    }
    return;
  }

  if (zx::result<> result = Isr(); result.is_error()) {
    fdf::error("Failed to run interrupt service routine: {}", result);
  }
  OnIrqComplete();

  if (zx_status_t ack_status = irq_.ack(); ack_status != ZX_OK) {
    if (ack_status != ZX_ERR_CANCELED) {
      fdf::error("Failed to ack interrupt: {}", zx_status_get_string(ack_status));
    }
  }
}

void Ufs::HandleTimeout(async_dispatcher_t* dispatcher, async::TaskBase* task, zx_status_t status) {
  if (status != ZX_OK) {
    return;
  }
  ProcessIo();
}

void Ufs::TriggerAdminWork() {
  async::PostTask(admin_worker_dispatcher_.async_dispatcher(),
                  [this]() { ProcessAdminCompletions(); });
}

void Ufs::TriggerIoWork() {
  async::PostTask(io_worker_dispatcher_.async_dispatcher(), [this]() { ProcessIo(); });
}

void Ufs::ProcessIo() {
  {
    std::lock_guard<std::mutex> lock(lock_);
    if (shutdown_) {
      return;
    }
  }

  // Always process in-flight completions before checking power state so pending query/init UPIUs
  // can complete even during power transitions.
  ProcessIoCompletions();

  {
    std::lock_guard<std::mutex> lock(lock_);
    if (!device_manager_->IsResumed()) {
      return;
    }
  }

  ProcessIoSubmissions();

  ProcessErrors();

  ScheduleTimeoutTask();
}

void Ufs::ScheduleTimeoutTask() {
  if (timeout_task_.is_pending()) {
    timeout_task_.Cancel();
  }

  zx_time_t deadline = transfer_request_processor_->GetEarliestTimeoutDeadline();
  if (deadline == ZX_TIME_INFINITE) {
    return;
  }

  timeout_task_.PostForTime(io_worker_dispatcher_.async_dispatcher(), zx::time(deadline));
}

void Ufs::SetHardwarePoweredOff(bool powered_off) {
  hardware_powered_off_.store(powered_off, std::memory_order_release);
  if (!powered_off) {
    return;
  }
  // Interrupts are already masked (IE = 0), but an ISR that started just before may still be
  // running on the IRQ worker, and an admin completion task that already passed its
  // IsHardwarePoweredOff() check may still be about to read the doorbell register on the admin
  // worker. Wait for both to drain before the PHY driver gates power/clocks. The I/O worker must
  // NOT be barriered here: it may be parked in IsResumed() on power_lock_, which the caller holds.
  // It stays off the registers because no I/O slot is allocated (SuspendPower() blocked slot
  // allocation and drained the request list before stopping the controller), so neither the
  // completion path nor ReserveSlot() reads the doorbell.
  for (fdf::Dispatcher* worker : {&irq_worker_dispatcher_, &admin_worker_dispatcher_}) {
    if (!worker->get()) {
      continue;
    }
    libsync::Completion barrier;
    zx_status_t status =
        async::PostTask(worker->async_dispatcher(), [&barrier]() { barrier.Signal(); });
    if (status == ZX_OK) {
      barrier.Wait();
    } else {
      fdf::warn("Failed to post power-down worker barrier: {}", zx_status_get_string(status));
    }
  }
}

zx_status_t Ufs::StopHostControllerForPowerDown() {
  // RequestProcessor::BlockSlotAllocation() must have run: the I/O completion path only stays away
  // from the registers because no I/O slot is allocated once the controller is powered off.
  ZX_DEBUG_ASSERT(transfer_request_processor_->GetInflightIoCount() == 0);

  // Serialise against UIC commands / key-slot writes that are already past their
  // IsHardwarePoweredOff() check (see uic_lock()).
  std::lock_guard<std::mutex> lock(uic_lock_);

  // Mask interrupts and disable the host controller before powering down the PHY and clocks
  // (matching ufshcd_hba_stop in Linux ufshcd) so that link-loss events during PHY power-down do
  // not trigger interrupt handling while UFSHCI clocks/power domains are off.
  InterruptEnableReg::Get().FromValue(0).WriteTo(&GetMmio());
  zx_status_t status = DisableHostController();
  if (status != ZX_OK) {
    fdf::error("Failed to disable host controller before PowerDown: {}",
               zx_status_get_string(status));
  }
  // From here on the ISR / I/O paths must not touch UFSHCI registers: the PHY driver is about to
  // gate the UFSHCI power domains and clocks.
  SetHardwarePoweredOff(true);
  return status;
}

void Ufs::ExecuteCommandsAsync(uint8_t target, uint16_t lun, std::span<scsi::ScsiRequest> batch) {
  {
    std::lock_guard<std::mutex> lock(lock_);
    if (shutdown_) {
      for (auto& req : batch) {
        req.Complete(ZX_ERR_IO_NOT_PRESENT);
      }
      return;
    }

    auto target_it = block_devs_.find(target);
    if (target_it == block_devs_.end()) {
      for (auto& req : batch) {
        req.Complete(ZX_ERR_NOT_FOUND);
      }
      return;
    }

    auto lun_it = target_it->second.find(lun);
    if (lun_it == target_it->second.end() || !lun_it->second) {
      for (auto& req : batch) {
        req.Complete(ZX_ERR_NOT_FOUND);
      }
      return;
    }

    auto lun_id = TranslateScsiLunToUfsLun(lun);
    if (lun_id.is_error()) {
      for (auto& req : batch) {
        req.Complete(lun_id.status_value());
      }
      return;
    }

    const uint32_t block_size = lun_it->second->block_size_bytes();

    for (auto& req : batch) {
      pending_commands_.push_back(IoCommand{
          .request = std::move(req),
          .lun = lun_id.value(),
          .block_size = block_size,
      });
    }
  }
  TriggerIoWork();
}

zx_status_t Ufs::ExecuteCommandSync(uint8_t target, uint16_t lun, iovec cdb, bool is_write,
                                    iovec data) {
  auto lun_id = TranslateScsiLunToUfsLun(lun);
  if (lun_id.is_error()) {
    return lun_id.status_value();
  }

  if (data.iov_len > max_transfer_bytes_) {
    fdf::error("Request exceeding max transfer size. transfer_bytes={}, max_transfer_bytes_={}",
               data.iov_len, max_transfer_bytes_);
    return ZX_ERR_INVALID_ARGS;
  }

  DataDirection data_direction = DataDirection::kNone;
  if (is_write) {
    data_direction = DataDirection::kHostToDevice;
  } else if (data.iov_base != nullptr) {
    data_direction = DataDirection::kDeviceToHost;
  }

  zx::vmo data_vmo;
  fzl::VmoMapper mapper;

  if (data_direction != DataDirection::kNone) {
    // Allocate a response data buffer.
    // TODO(https://fxbug.dev/42075643): We need to pre-allocate a data buffer that will be used in
    // the Sync command.
    if (zx::result<> result = AllocatePages(data_vmo, mapper, data.iov_len); result.is_error()) {
      return result.error_value();
    }
  }

  if (data_direction == DataDirection::kHostToDevice) {
    memcpy(mapper.start(), data.iov_base, data.iov_len);
  }

  ScsiCommandUpiu upiu(static_cast<uint8_t*>(cdb.iov_base),
                       safemath::checked_cast<uint8_t>(cdb.iov_len), data_direction,
                       safemath::checked_cast<uint32_t>(data.iov_len));

  auto response = transfer_request_processor_->SendAdminScsiCmd(upiu, lun_id.value(),
                                                                zx::unowned_vmo(data_vmo));
  if (response.is_error()) {
    return response.error_value();
  }

  auto header_response = response.value()->GetHeader().response;
  uint8_t scsi_status = response.value()->GetHeader().status;

  if (header_response != UpiuHeaderResponseCode::kTargetSuccess &&
      header_response != UpiuHeaderResponseCode::kTargetFailure) {
    fdf::error("ExecuteCommandSync failed: header_response=0x{:x}, scsi_status=0x{:x}",
               static_cast<uint32_t>(header_response), static_cast<uint32_t>(scsi_status));
    return ZX_ERR_BAD_STATE;
  }

  if (scsi_status != static_cast<uint8_t>(scsi::StatusCode::GOOD)) {
    auto* sense_data =
        reinterpret_cast<scsi::FixedFormatSenseDataHeader*>(response.value()->GetSenseData());
    // UNIT ATTENTION (e.g. POWER ON after a PowerDown suspend) and NOT READY are transient and
    // retried by the callers, so they are not worth an error log.
    const bool retryable = scsi_status == static_cast<uint8_t>(scsi::StatusCode::CHECK_CONDITION) &&
                           (sense_data->sense_key() == scsi::SenseKey::UNIT_ATTENTION ||
                            sense_data->sense_key() == scsi::SenseKey::NOT_READY);
    if (retryable) {
      fdf::debug("ExecuteCommandSync scsi_status=0x{:x}, sense_key=0x{:x}, asc=0x{:x}, ascq=0x{:x}",
                 scsi_status, static_cast<uint8_t>(sense_data->sense_key()),
                 sense_data->additional_sense_code, sense_data->additional_sense_code_qualifier);
      return ZX_ERR_UNAVAILABLE;
    }
    fdf::error("ExecuteCommandSync scsi_status=0x{:x}, sense_key=0x{:x}, asc=0x{:x}, ascq=0x{:x}",
               scsi_status, static_cast<uint8_t>(sense_data->sense_key()),
               sense_data->additional_sense_code, sense_data->additional_sense_code_qualifier);
    return ZX_ERR_BAD_STATE;
  }

  if (data_direction == DataDirection::kDeviceToHost) {
    memcpy(data.iov_base, mapper.start(), data.iov_len);
  }
  return ZX_OK;
}

// Record the constant inspects.
void Ufs::PopulateVersionInspect(inspect::Node* inspect_node) {
  const fdf::MmioBuffer& mmio = mmio_.value();
  VersionReg version_reg = VersionReg::Get().ReadFrom(&mmio);

  auto version = inspect_node->CreateChild("version");
  properties_.major_version_number =
      version.CreateUint("major_version_number", version_reg.major_version_number());
  properties_.minor_version_number =
      version.CreateUint("minor_version_number", version_reg.minor_version_number());
  properties_.version_suffix = version.CreateUint("version_suffix", version_reg.version_suffix());
  if (component_inspector_) {
    component_inspector_->inspector().emplace(std::move(version));
  }

  fdf::info("Controller version {}.{} found", version_reg.major_version_number(),
            version_reg.minor_version_number());
}

// Record the constant inspects.
void Ufs::PopulateCapabilitiesInspect(inspect::Node* inspect_node) {
  const fdf::MmioBuffer& mmio = mmio_.value();
  CapabilityReg caps_reg = CapabilityReg::Get().ReadFrom(&mmio);

  auto caps = inspect_node->CreateChild("capabilities");
  properties_.crypto_support = caps.CreateBool("crypto_support", caps_reg.crypto_support());
  properties_.uic_dme_test_mode_command_supported = caps.CreateBool(
      "uic_dme_test_mode_command_supported", caps_reg.uic_dme_test_mode_command_supported());
  properties_.out_of_order_data_delivery_supported = caps.CreateBool(
      "out_of_order_data_delivery_supported", caps_reg.out_of_order_data_delivery_supported());
  properties_._64_bit_addressing_supported =
      caps.CreateBool("64_bit_addressing_supported", caps_reg._64_bit_addressing_supported());
  properties_.auto_hibernation_support =
      caps.CreateBool("auto_hibernation_support", caps_reg.auto_hibernation_support());
  properties_.number_of_utp_task_management_request_slots =
      caps.CreateUint("number_of_utp_task_management_request_slots",
                      caps_reg.number_of_utp_task_management_request_slots());
  properties_.number_of_outstanding_rtt_requests_supported =
      caps.CreateUint("number_of_outstanding_rtt_requests_supported",
                      caps_reg.number_of_outstanding_rtt_requests_supported());
  properties_.number_of_utp_transfer_request_slots = caps.CreateUint(
      "number_of_utp_transfer_request_slots", caps_reg.number_of_utp_transfer_request_slots());
  if (component_inspector_) {
    component_inspector_->inspector().emplace(std::move(caps));
  }
}

zx::result<> Ufs::InitMmioBuffer() {
  auto mmio = CreateMmioBuffer(0, mmio_buffer_size_, std::move(mmio_buffer_vmo_));
  if (mmio.is_error()) {
    return zx::error(mmio.status_value());
  }
  mmio_ = std::move(mmio.value());
  return zx::ok();
}

Ufs::Ufs() : fdf::DriverBase2(kDriverName) {}

zx_status_t Ufs::Init() {
  if (zx::result<> result = InitMmioBuffer(); result.is_error()) {
    fdf::error("Failed to initialize MMIO buffer: {}", result);
    return result.error_value();
  }

  if (component_inspector_) {
    inspect_node_ = component_inspector_->root().CreateChild("ufs");
    properties_.read_ops = inspect_node_.CreateUint("block.read.count", 0);
    properties_.write_ops = inspect_node_.CreateUint("block.write.count", 0);
    properties_.flush_ops = inspect_node_.CreateUint("block.flush.count", 0);
    properties_.trim_ops = inspect_node_.CreateUint("block.trim.count", 0);
    properties_.read_bytes = inspect_node_.CreateUint("block.read.bytes", 0);
    properties_.write_bytes = inspect_node_.CreateUint("block.write.bytes", 0);
    properties_.flush_bytes = inspect_node_.CreateUint("block.flush.bytes", 0);
    properties_.trim_bytes = inspect_node_.CreateUint("block.trim.bytes", 0);
    PopulateVersionInspect(&inspect_node_);
    PopulateCapabilitiesInspect(&inspect_node_);
  }

  auto controller_node = inspect_node_.CreateChild("controller");
  auto wp_node = controller_node.CreateChild("write_protect");
  auto wb_node = controller_node.CreateChild("writebooster");
  auto bkop_node = controller_node.CreateChild("background_operations");

  if (zx::result<> result = InitQuirk(); result.is_error()) {
    fdf::error("Failed to initialize quirk: {}", result);
    return result.error_value();
  }
  if (zx::result<> result = InitController(); result.is_error()) {
    fdf::error("Failed to initialize UFS controller: {}", result);
    return result.error_value();
  }

  if (zx::result<> result = InitDeviceInterface(controller_node); result.is_error()) {
    fdf::error("Failed to initialize device interface: {}", result);
    return result.error_value();
  }

  if (zx::result<> result = device_manager_->GetControllerDescriptor(); result.is_error()) {
    fdf::error("Failed to get controller descriptor: {}", result);
    return result.error_value();
  }

  if (zx::result<> result = device_manager_->ConfigureWriteProtect(wp_node); result.is_error()) {
    fdf::error("Failed to configure Write Protect {}", result);
    return result.error_value();
  }

  if (zx::result<> result = device_manager_->ConfigureBackgroundOp(bkop_node); result.is_error()) {
    fdf::error("Failed to configure Background Operations {}", result);
    return result.error_value();
  }

  if (zx::result<> result = device_manager_->ConfigureWriteBooster(wb_node); result.is_error()) {
    if (result.status_value() == ZX_ERR_NOT_SUPPORTED) {
      fdf::warn("This device does not support WriteBooster");
    } else {
      fdf::error("Failed to configure WriteBooster {}", result);
      return result.error_value();
    }
  }

  // The maximum transfer size supported by UFSHCI spec is 65535 * 256 KiB. However, we limit the
  // maximum transfer size to 1MiB for performance reason.
  max_transfer_bytes_ = kMaxTransferSize1MiB;
  properties_.max_transfer_bytes =
      controller_node.CreateUint("max_transfer_bytes", max_transfer_bytes_);

  zx::result<uint32_t> lun_count;
  if (lun_count = AddLogicalUnits(); lun_count.is_error()) {
    fdf::error("Failed to scan logical units: {}", lun_count);
    return lun_count.error_value();
  }

  if (lun_count.value() == 0) {
    fdf::error("Bind Error. There is no available LUN(lun_count = 0).");
    return ZX_ERR_BAD_STATE;
  }
  logical_unit_count_ = lun_count.value();
  properties_.logical_unit_count =
      controller_node.CreateUint("logical_unit_count", logical_unit_count_);
  // 14 buckets spanning from 1us to ~8ms.
  properties_.wake_latency_us = inspect_node_.CreateExponentialUintHistogram(
      "wake_latency_us", /*floor=*/1, /*initial_step=*/1, /*step_multiplier=*/2,
      /*buckets=*/14);

  if (component_inspector_) {
    component_inspector_->inspector().emplace(std::move(controller_node));
    component_inspector_->inspector().emplace(std::move(wp_node));
    component_inspector_->inspector().emplace(std::move(wb_node));
    component_inspector_->inspector().emplace(std::move(bkop_node));
  }

  if (HasWellKnownLun(WellKnownLuns::kRpmb)) {
    fdf::info("Initializing RPMB LUN");
    const auto& geometry = GetDeviceManager().GetGeometryDescriptor();
    fdf::debug("Ufs Bind: bRPMB_ReadWriteSize = {}", geometry.bRPMB_ReadWriteSize);
    rpmb_device_ = std::make_unique<UfsRpmbDevice>(this);
    if (zx_status_t status = rpmb_device_->AddDevice(); status != ZX_OK) {
      fdf::error("Failed to add RPMB device: {}. Continuing initialization.", status);
      rpmb_device_.reset();
    }
  }

  fdf::info("Bind Success");

  return ZX_OK;
}

zx::result<> Ufs::InitController() {
  const fdf::MmioBuffer& mmio = mmio_.value();
  // Disable all interrupts.
  InterruptEnableReg::Get().FromValue(0).WriteTo(&mmio);

  if (zx::result<> result = Notify(NotifyEvent::kReset, 0); result.is_error()) {
    return result.take_error();
  }
  // If UFS host controller is already enabled, disable it.
  if (HostControllerEnableReg::Get().ReadFrom(&mmio).host_controller_enable()) {
    DisableHostController();
  }
  if (zx_status_t status = EnableHostController(); status != ZX_OK) {
    fdf::error("Failed to enable host controller {}", zx_status_get_string(status));
    return zx::error(status);
  }

  if (CapabilityReg::Get().ReadFrom(&mmio).crypto_support()) {
    auto inline_crypto_client =
        incoming_->Connect<fuchsia_hardware_ufs_phy::Service::InlineCrypto>("phy");
    if (inline_crypto_client.is_error()) {
      fdf::error("UFS controller supports crypto, but failed to connect to PHY inline crypto: {}",
                 inline_crypto_client);
    } else {
      inline_encryption_client_ = fidl::WireSyncClient(std::move(*inline_crypto_client));
      HostControllerEnableReg::Get().ReadFrom(&mmio).set_crypto_general_enable(true).WriteTo(&mmio);
      crypto_supported_ = true;
      fdf::info("UFS host controller inline encryption enabled");
    }
  }

  // Create and post IRQ worker
  {
    auto irq_dispatcher = fdf::SynchronizedDispatcher::Create(
        fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "ufs-irq-worker",
        [this](fdf_dispatcher_t*) { OnDispatcherShutdown(); }, kIrqDispatcherRole);
    if (irq_dispatcher.is_error()) {
      fdf::error("Failed to create IRQ dispatcher: {}",
                 zx_status_get_string(irq_dispatcher.status_value()));
      return zx::error(irq_dispatcher.status_value());
    }
    irq_worker_dispatcher_ = *std::move(irq_dispatcher);

    irq_handler_.set_object(irq_.get());
    zx_status_t status = irq_handler_.Begin(irq_worker_dispatcher_.async_dispatcher());
    if (status != ZX_OK) {
      fdf::error("Failed to begin IRQ wait: {}", zx_status_get_string(status));
      return zx::error(status);
    }
  }

  auto cancel_irq = fit::defer([this] { irq_handler_.Cancel(); });

  // Notify platform UFS that we are going to init the UFS host controller.
  if (zx::result<> result = Notify(NotifyEvent::kInit, 0); result.is_error()) {
    return result.take_error();
  }

  // Create Task Management Request Processor
  uint8_t number_of_task_management_request_slots = safemath::checked_cast<uint8_t>(
      CapabilityReg::Get().ReadFrom(&mmio).number_of_utp_task_management_request_slots() + 1);
  fdf::debug("number_of_task_management_request_slots={}", number_of_task_management_request_slots);

  auto task_management_request_processor =
      TaskManagementRequestProcessor::Create(*this, bti_.borrow(), mmio.View(0, mmio_buffer_size_),
                                             number_of_task_management_request_slots);
  if (task_management_request_processor.is_error()) {
    fdf::error("Failed to create task management request processor {}",
               task_management_request_processor);
    return task_management_request_processor.take_error();
  }
  task_management_request_processor_ = std::move(*task_management_request_processor);

  // Create Transfer Request Processor
  uint8_t number_of_transfer_request_slots = safemath::checked_cast<uint8_t>(
      CapabilityReg::Get().ReadFrom(&mmio).number_of_utp_transfer_request_slots() + 1);
  fdf::debug("number_of_transfer_request_slots={}", number_of_transfer_request_slots);

  auto transfer_request_processor = TransferRequestProcessor::Create(
      *this, bti_.borrow(), mmio.View(0, mmio_buffer_size_), number_of_transfer_request_slots);
  if (transfer_request_processor.is_error()) {
    fdf::error("Failed to create transfer request processor {}", transfer_request_processor);
    return transfer_request_processor.take_error();
  }
  transfer_request_processor_ = std::move(*transfer_request_processor);

  auto device_manager = DeviceManager::Create(*this, *transfer_request_processor_, properties_);
  if (device_manager.is_error()) {
    fdf::error("Failed to create device manager {}", device_manager);
    return device_manager.take_error();
  }
  device_manager_ = std::move(*device_manager);

  // Create Admin worker dispatcher
  {
    auto admin_dispatcher = fdf::SynchronizedDispatcher::Create(
        fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "ufs-admin-worker",
        [this](fdf_dispatcher_t*) { OnDispatcherShutdown(); });
    if (admin_dispatcher.is_error()) {
      fdf::error("Failed to create Admin dispatcher: {}",
                 zx_status_get_string(admin_dispatcher.status_value()));
      return zx::error(admin_dispatcher.status_value());
    }
    admin_worker_dispatcher_ = *std::move(admin_dispatcher);
  }

  // Create IO worker dispatcher
  {
    auto io_dispatcher = fdf::SynchronizedDispatcher::Create(
        fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "ufs-io-worker",
        [this](fdf_dispatcher_t*) { OnDispatcherShutdown(); }, kIoDispatcherRole);
    if (io_dispatcher.is_error()) {
      fdf::error("Failed to create IO dispatcher: {}",
                 zx_status_get_string(io_dispatcher.status_value()));
      return zx::error(io_dispatcher.status_value());
    }
    io_worker_dispatcher_ = *std::move(io_dispatcher);
  }

  // Create Exception Event worker
  {
    auto ee_dispatcher = fdf::SynchronizedDispatcher::Create(
        fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "ufs-exception-event-worker",
        [this](fdf_dispatcher_t*) { OnDispatcherShutdown(); });
    if (ee_dispatcher.is_error()) {
      fdf::error("Failed to create Exception Event dispatcher: {}",
                 zx_status_get_string(ee_dispatcher.status_value()));
      return zx::error(ee_dispatcher.status_value());
    }
    exception_event_dispatcher_ = *std::move(ee_dispatcher);
  }

  cancel_irq.cancel();
  return zx::ok();
}

zx::result<> Ufs::InitDeviceInterface(inspect::Node& controller_node) {
  const fdf::MmioBuffer& mmio = mmio_.value();

  // Send Link Startup UIC command to start the link startup procedure.
  if (zx::result<> result = device_manager_->SendLinkStartUp(); result.is_error()) {
    fdf::error("Failed to send Link Startup UIC command {}", result);
    return result.take_error();
  }

  // The |device_present| bit becomes true if the host controller has successfully received a Link
  // Startup UIC command response and the UFS device has found a physical link to the controller.
  if (!HostControllerStatusReg::Get().ReadFrom(&mmio).device_present()) {
    fdf::error("UFS device not found");
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  fdf::info("UFS device found");

  // Clear any residual UIC error flags from link startup and configure operational interrupts.
  InterruptStatusReg::Get().FromValue(0).set_uic_error(true).WriteTo(&mmio);
  InterruptEnableReg::Get()
      .FromValue(0)
      .set_crypto_engine_fatal_error_enable(true)
      .set_system_bus_fatal_error_enable(true)
      .set_host_controller_fatal_error_enable(true)
      .set_utp_error_enable(true)
      .set_device_fatal_error_enable(true)
      .set_uic_command_completion_enable(false)  // The UIC command uses polling mode.
      .set_utp_task_management_request_completion_enable(true)
      .set_uic_link_startup_status_enable(false)  // Ignore link startup interrupt.
      .set_uic_link_lost_status_enable(true)
      .set_uic_hibernate_enter_status_enable(false)  // The hibernate commands use polling mode.
      .set_uic_hibernate_exit_status_enable(false)   // The hibernate commands use polling mode.
      .set_uic_power_mode_status_enable(false)       // The power mode uses polling mode.
      .set_uic_test_mode_status_enable(true)
      .set_uic_error_enable(true)
      .set_uic_dme_endpointreset(true)
      .set_utp_transfer_request_completion_enable(true)
      .WriteTo(&mmio);

  if (zx::result<> result = task_management_request_processor_->Init(); result.is_error()) {
    fdf::error("Failed to initialize task management request processor {}", result);
    return result.take_error();
  }

  if (zx::result<> result = transfer_request_processor_->Init(); result.is_error()) {
    fdf::error("Failed to initialize transfer request processor {}", result);
    return result.take_error();
  }

  // TODO(https://fxbug.dev/42075643): Configure interrupt aggregation. (default 0)

  NopOutUpiu nop_upiu;
  auto nop_response = transfer_request_processor_->SendRequestUpiu<NopOutUpiu, NopInUpiu>(nop_upiu);
  if (nop_response.is_error()) {
    fdf::error("Failed to send NopOutUpiu {}", nop_response);
    return nop_response.take_error();
  }

  if (zx::result<> result = device_manager_->DeviceInit(); result.is_error()) {
    fdf::error("Failed to initialize device {}", result);
    return result.take_error();
  }

  if (zx::result<> result = Notify(NotifyEvent::kDeviceInitDone, 0); result.is_error()) {
    return result.take_error();
  }

  if (zx::result<> result = device_manager_->InitReferenceClock(controller_node);
      result.is_error()) {
    fdf::error("Failed to initialize reference clock {}", result);
    return result.take_error();
  }

  auto unipro_node = controller_node.CreateChild("unipro");
  auto attributes_node = controller_node.CreateChild("attributes");
  if (qemu_quirk_) {
    // Currently, QEMU UFS devices do not support unipro and power mode.
    device_manager_->SetCurrentPowerMode(UfsPowerMode::kActive);
  } else {
    if (zx::result<> result = device_manager_->InitUniproAttributes(unipro_node);
        result.is_error()) {
      fdf::error("Failed to initialize Unipro attributes {}", result);
      return result.take_error();
    }

    if (zx::result<> result = device_manager_->InitUicPowerMode(unipro_node); result.is_error()) {
      fdf::error("Failed to initialize UIC power mode {}", result);
      return result.take_error();
    }

    if (zx::result<> result = device_manager_->InitUfsPowerMode(controller_node, attributes_node);
        result.is_error()) {
      fdf::error("Failed to initialize UFS power mode {}", result);
      return result.take_error();
    }
  }
  properties_.power_suspended = inspect_node_.CreateBool("power_suspended", false);
  properties_.suspend_failures = inspect_node_.CreateUint("suspend_failures", 0);
  properties_.resume_failures = inspect_node_.CreateUint("resume_failures", 0);

  zx::result<uint32_t> result = device_manager_->GetBootLunEnabled();
  if (result.is_error()) {
    fdf::error("Failed to check Boot LUN enabled {}", result);
    return result.take_error();
  }
  properties_.b_boot_lun_en = attributes_node.CreateUint("bBootLunEn", result.value());

  // Configure Auto-Hibernate (AH8) if supported by the host controller.
  if (zx::result<> result = ConfigureAutoHibernate(); result.is_error()) {
    fdf::error("Failed to configure Auto-Hibernate: {}", result);
    return result.take_error();
  }

  // TODO(https://fxbug.dev/42075643): Set bMaxNumOfRTT (Read-to-transfer)

  if (component_inspector_) {
    component_inspector_->inspector().emplace(std::move(unipro_node));
    component_inspector_->inspector().emplace(std::move(attributes_node));
  }

  return zx::ok();
}

zx::result<> Ufs::ConfigureAutoHibernate() {
  if (qemu_quirk_) {
    return zx::ok();
  }

  if (!config_.enable_auto_hibernate()) {
    fdf::info("UFS Auto-Hibernate disabled by configuration");
    return zx::ok();
  }

  const fdf::MmioBuffer& mmio = mmio_.value();
  if (!CapabilityReg::Get().ReadFrom(&mmio).auto_hibernation_support()) {
    fdf::info("UFS Auto-Hibernate not supported by controller");
    return zx::ok();
  }

  auto ahit_reg = AutoHibernateIdleTimerReg::FromTimeoutUs(config_.auto_hibernate_timer_us());
  ahit_reg.WriteTo(&mmio);
  fdf::info("UFS Auto-Hibernate enabled (scale: {}, value: {})",
            static_cast<uint32_t>(ahit_reg.timer_scale()), ahit_reg.timer_value());
  return zx::ok();
}

zx::result<> Ufs::DisableAutoHibernate() {
  if (qemu_quirk_) {
    return zx::ok();
  }
  const fdf::MmioBuffer& mmio = mmio_.value();
  if (!CapabilityReg::Get().ReadFrom(&mmio).auto_hibernation_support()) {
    return zx::ok();
  }
  AutoHibernateIdleTimerReg::Get().FromValue(0).WriteTo(&mmio);
  return zx::ok();
}

zx::result<> Ufs::ReinitAfterPowerDown() {
  const fdf::MmioBuffer& mmio = mmio_.value();
  InterruptEnableReg::Get().FromValue(0).WriteTo(&mmio);

  if (zx::result<> result = Notify(NotifyEvent::kReset, 0); result.is_error()) {
    return result.take_error();
  }
  if (HostControllerEnableReg::Get().ReadFrom(&mmio).host_controller_enable()) {
    DisableHostController();
  }
  if (zx_status_t status = EnableHostController(); status != ZX_OK) {
    fdf::error("Failed to re-enable host controller: {}", zx_status_get_string(status));
    return zx::error(status);
  }
  if (crypto_supported_) {
    // Only the global enable is restored here; the per-slot state is re-established by
    // RestoreCryptoState() below once the controller is back up.
    HostControllerEnableReg::Get().ReadFrom(&mmio).set_crypto_general_enable(true).WriteTo(&mmio);
  }
  if (zx::result<> result = Notify(NotifyEvent::kInit, 0); result.is_error()) {
    return result.take_error();
  }

  if (zx::result<> result = device_manager_->SendLinkStartUp(); result.is_error()) {
    fdf::error("Failed to send Link Startup UIC command on resume: {}", result);
    return result.take_error();
  }
  if (!HostControllerStatusReg::Get().ReadFrom(&mmio).device_present()) {
    fdf::error("UFS device not found on resume");
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  InterruptStatusReg::Get().FromValue(0).set_uic_error(true).WriteTo(&mmio);
  InterruptEnableReg::Get()
      .FromValue(0)
      .set_crypto_engine_fatal_error_enable(true)
      .set_system_bus_fatal_error_enable(true)
      .set_host_controller_fatal_error_enable(true)
      .set_utp_error_enable(true)
      .set_device_fatal_error_enable(true)
      .set_uic_command_completion_enable(false)
      .set_utp_task_management_request_completion_enable(true)
      .set_uic_link_startup_status_enable(false)
      .set_uic_link_lost_status_enable(true)
      .set_uic_hibernate_enter_status_enable(false)
      .set_uic_hibernate_exit_status_enable(false)
      .set_uic_power_mode_status_enable(false)
      .set_uic_test_mode_status_enable(true)
      .set_uic_error_enable(true)
      .set_uic_dme_endpointreset(true)
      .set_utp_transfer_request_completion_enable(true)
      .WriteTo(&mmio);

  if (zx::result<> result = task_management_request_processor_->Init(); result.is_error()) {
    fdf::error("Failed to re-initialize task management request processor: {}", result);
    return result.take_error();
  }
  if (zx::result<> result = transfer_request_processor_->Init(); result.is_error()) {
    fdf::error("Failed to re-initialize transfer request processor: {}", result);
    return result.take_error();
  }

  NopOutUpiu nop_upiu;
  auto nop_response = transfer_request_processor_->SendRequestUpiu<NopOutUpiu, NopInUpiu>(nop_upiu);
  if (nop_response.is_error()) {
    fdf::error("Failed to send NopOutUpiu on resume: {}", nop_response);
    return nop_response.take_error();
  }

  if (zx::result<> result = device_manager_->DeviceInit(); result.is_error()) {
    fdf::error("Failed to re-initialize device on resume: {}", result);
    return result.take_error();
  }
  if (zx::result<> result = Notify(NotifyEvent::kDeviceInitDone, 0); result.is_error()) {
    return result.take_error();
  }

  // Inline encryption state must be restored once the host controller is enabled and the crypto
  // engine's power-on self test has completed (handled by the PHY during kInit), but before
  // encrypted I/O can be issued: that is, before the link is brought up for I/O rather than after
  // the first command.
  if (zx::result<> result = RestoreCryptoState(); result.is_error()) {
    fdf::error("Failed to restore inline encryption state on resume: {}", result);
    return result.take_error();
  }

  if (zx::result<> result = device_manager_->RestoreDeviceAndLinkConfig(); result.is_error()) {
    fdf::error("Failed to restore device and link configuration on resume: {}", result);
    return result.take_error();
  }

  if (zx::result<> result = ClearUnitAttentions(); result.is_error()) {
    return result.take_error();
  }

  return zx::ok();
}

zx::result<> Ufs::ClearUnitAttentions() {
  // Well-known LUs use the extended addressing produced by TranslateUfsLunToScsiLun(). The UFS
  // device W-LUN is cleared first because ResumePower() sends START STOP UNIT to it next, and it
  // is the only LU whose readiness is required for the resume to proceed.
  zx::result<uint16_t> device_wlun =
      TranslateUfsLunToScsiLun(static_cast<uint8_t>(WellKnownLuns::kUfsDevice));
  if (device_wlun.is_error()) {
    return device_wlun.take_error();
  }

  std::vector<uint16_t> luns;
  for (WellKnownLuns wlun :
       {WellKnownLuns::kRpmb, WellKnownLuns::kBoot, WellKnownLuns::kReportLuns}) {
    if (!HasWellKnownLun(wlun)) {
      continue;
    }
    zx::result<uint16_t> scsi_lun = TranslateUfsLunToScsiLun(static_cast<uint8_t>(wlun));
    if (scsi_lun.is_error()) {
      return scsi_lun.take_error();
    }
    luns.push_back(scsi_lun.value());
  }
  // Regular LUs are whichever LUNs ScanAndBindLogicalUnits() bound; they are not necessarily
  // contiguous (a disabled LU leaves a gap), so iterate the bound set rather than
  // 0..logical_unit_count_-1.
  {
    std::lock_guard<std::mutex> lock(lock_);
    auto target_it = block_devs().find(kPlaceholderTarget);
    if (target_it != block_devs().end()) {
      std::vector<uint16_t> regular_luns;
      for (const auto& [lun, block_device] : target_it->second) {
        if (block_device) {
          regular_luns.push_back(lun);
        }
      }
      std::sort(regular_luns.begin(), regular_luns.end());
      luns.insert(luns.end(), regular_luns.begin(), regular_luns.end());
    }
  }

  auto clear = [this](uint16_t lun) -> zx_status_t {
    zx_status_t status = ZX_ERR_INTERNAL;
    for (int retry = 0; retry < kMaxRetries; ++retry) {
      // UFS devices return autosense in the response UPIU, so the TEST UNIT READY that reports the
      // UNIT ATTENTION also clears it; an immediate retry confirms the LU is ready. Only back off
      // if the LU is still NOT READY after that.
      status = TestUnitReady(kPlaceholderTarget, lun);
      if (status != ZX_ERR_UNAVAILABLE) {
        break;
      }
      if (retry > 0) {
        zx::nanosleep(zx::deadline_after(kUnavailableRetryDelay));
      }
    }
    return status;
  };

  if (HasWellKnownLun(WellKnownLuns::kUfsDevice)) {
    if (zx_status_t status = clear(device_wlun.value()); status != ZX_OK) {
      fdf::error("UFS device W-LUN not ready after resume: {}", zx_status_get_string(status));
      return zx::error(status);
    }
  }
  for (uint16_t lun : luns) {
    // Best effort: if a LU is still not ready here, its first command will report the condition
    // to the caller; failing the whole resume instead would cost a full PHY power cycle per
    // attempt without making the LU any more likely to recover.
    if (zx_status_t status = clear(lun); status != ZX_OK) {
      fdf::warn("LUN 0x{:x} not ready after resume: {}", lun, zx_status_get_string(status));
    }
  }
  return zx::ok();
}

zx::result<uint32_t> Ufs::AddLogicalUnits() {
  uint8_t max_luns = device_manager_->GetMaxLunCount();
  ZX_ASSERT(max_luns <= kMaxLunCount);

  auto read_unit_descriptor = [this](uint16_t lun, size_t block_size,
                                     uint64_t block_count) -> zx::result<> {
    if (lun > UINT8_MAX) {
      return zx::error(ZX_ERR_OUT_OF_RANGE);
    }

    zx::result<UnitDescriptor> unit_descriptor =
        device_manager_->ReadUnitDescriptor(static_cast<uint8_t>(lun));
    if (unit_descriptor.is_error()) {
      return unit_descriptor.take_error();
    }

    if (unit_descriptor->bLUEnable != 1) {
      return zx::error(ZX_ERR_INTERNAL);
    }

    if (unit_descriptor->bLogicalBlockSize >= sizeof(size_t) * 8) {
      fdf::error("Cannot handle the unit descriptor bLogicalBlockSize = {}.",
                 unit_descriptor->bLogicalBlockSize);
      return zx::error(ZX_ERR_OUT_OF_RANGE);
    }

    size_t desc_block_size = 1 << unit_descriptor->bLogicalBlockSize;
    uint64_t desc_block_count = betoh64(unit_descriptor->qLogicalBlockCount);

    if (desc_block_size < kBlockSize ||
        desc_block_size <
            static_cast<size_t>(device_manager_->GetGeometryDescriptor().bMinAddrBlockSize) *
                kSectorSize ||
        desc_block_size >
            static_cast<size_t>(device_manager_->GetGeometryDescriptor().bMaxInBufferSize) *
                kSectorSize ||
        desc_block_size >
            static_cast<size_t>(device_manager_->GetGeometryDescriptor().bMaxOutBufferSize) *
                kSectorSize) {
      fdf::error("Cannot handle logical block size of {}.", desc_block_size);
      return zx::error(ZX_ERR_OUT_OF_RANGE);
    }
    ZX_ASSERT_MSG(desc_block_size == kBlockSize, "Currently, it only supports a 4KB block size.");

    if (desc_block_size != block_size || desc_block_count != block_count) {
      fdf::info("Failed to check for disk consistency. (block_size={}/{}, block_count={}/{})",
                desc_block_size, block_size, desc_block_count, block_count);
      return zx::error(ZX_ERR_BAD_STATE);
    }
    fdf::info("LUN-{} block_size={}, block_count={}", lun, desc_block_size, desc_block_count);

    // Currently, we only support kPowerOnWriteProtect.
    if (device_manager_->IsPowerOnWritePotectEnabled() &&
        unit_descriptor->bLUWriteProtect == LUWriteProtect::kPowerOnWriteProtect &&
        !device_manager_->IsLogicalLunPowerOnWriteProtect()) {
      device_manager_->SetLogicalLunPowerOnWriteProtect(true);
    }

    return zx::ok();
  };

  // UFS does not support the MODE SENSE(6) command. We should use the MODE SENSE(10) command.
  // UFS does not support the READ(12)/WRITE(12) commands.
  scsi::DeviceOptions options(/*check_unmap_support*/ true, /*use_mode_sense_6*/ false,
                              /*use_read_write_12*/ false);

  zx::result<uint32_t> lun_count = ScanAndBindLogicalUnits(kPlaceholderTarget, max_transfer_bytes_,
                                                           max_luns, read_unit_descriptor, options);
  if (lun_count.is_error()) {
    fdf::error("Failed to scan logical units: {}", lun_count);
    return lun_count.take_error();
  }

  // Find well known logical units.
  std::array<WellKnownLuns, static_cast<uint8_t>(WellKnownLuns::kCount)> well_known_luns = {
      WellKnownLuns::kReportLuns, WellKnownLuns::kUfsDevice, WellKnownLuns::kBoot,
      WellKnownLuns::kRpmb};

  for (auto& lun : well_known_luns) {
    auto scsi_lun = TranslateUfsLunToScsiLun(static_cast<uint8_t>(lun));
    if (scsi_lun.is_error()) {
      return scsi_lun.take_error();
    }
    zx_status_t status = ZX_OK;
    for (int retry = 0; retry < kMaxRetries; ++retry) {
      status = TestUnitReady(kPlaceholderTarget, scsi_lun.value());
      if (status == ZX_OK) {
        break;
      }
      fdf::warn("TestUnitReady for LUN 0x{:x} failed (attempt {}/{}): {}. Retrying...",
                static_cast<uint8_t>(lun), retry + 1, kMaxRetries, zx_status_get_string(status));
    }
    if (status != ZX_OK) {
      continue;
    }
    well_known_lun_set_.insert(lun);
    fdf::info("Well known LUN-0x{:x}", static_cast<uint8_t>(lun));
  }

  return zx::ok(lun_count.value());
}

void Ufs::DumpRegisters() {
  const fdf::MmioBuffer& mmio = mmio_.value();
  CapabilityReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("CapabilityReg::{}", arg); });
  VersionReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("VersionReg::{}", arg); });

  InterruptStatusReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("InterruptStatusReg::{}", arg); });
  InterruptEnableReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("InterruptEnableReg::{}", arg); });

  HostControllerStatusReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("HostControllerStatusReg::{}", arg); });
  HostControllerEnableReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("HostControllerEnableReg::{}", arg); });

  UtrListBaseAddressReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("UtrListBaseAddressReg::{}", arg); });
  UtrListBaseAddressUpperReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("UtrListBaseAddressUpperReg::{}", arg); });
  UtrListDoorBellReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("UtrListDoorBellReg::{}", arg); });
  UtrListClearReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("UtrListClearReg::{}", arg); });
  UtrListRunStopReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("UtrListRunStopReg::{}", arg); });
  UtrListCompletionNotificationReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("UtrListCompletionNotificationReg::{}", arg); });

  UtmrListBaseAddressReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("UtmrListBaseAddressReg::{}", arg); });
  UtmrListBaseAddressUpperReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("UtmrListBaseAddressUpperReg::{}", arg); });
  UtmrListDoorBellReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("UtmrListDoorBellReg::{}", arg); });
  UtmrListRunStopReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("UtmrListRunStopReg::{}", arg); });

  UicCommandReg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("UicCommandReg::{}", arg); });
  UicCommandArgument1Reg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("UicCommandArgument1Reg::{}", arg); });
  UicCommandArgument2Reg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("UicCommandArgument2Reg::{}", arg); });
  UicCommandArgument3Reg::Get().ReadFrom(&mmio).Print(
      [](const char* arg) { fdf::debug("UicCommandArgument3Reg::{}", arg); });
}

zx_status_t Ufs::EnableHostController() {
  const fdf::MmioBuffer& mmio = mmio_.value();
  HostControllerEnableReg::Get()
      .FromValue(0)
      .set_host_controller_enable(true)
      .set_crypto_general_enable(CapabilityReg::Get().ReadFrom(&mmio).crypto_support())
      .WriteTo(&mmio);

  auto wait_for = [&]() -> bool {
    return HostControllerEnableReg::Get().ReadFrom(&mmio).host_controller_enable();
  };
  fbl::String timeout_message = "Timeout waiting for EnableHostController";
  return WaitWithTimeout(wait_for, zx::usec(kHostControllerTimeoutUs), timeout_message);
}

zx_status_t Ufs::DisableHostController() {
  const fdf::MmioBuffer& mmio = mmio_.value();
  HostControllerEnableReg::Get().FromValue(0).set_host_controller_enable(false).WriteTo(&mmio);

  auto wait_for = [&]() -> bool {
    return !HostControllerEnableReg::Get().ReadFrom(&mmio).host_controller_enable();
  };
  fbl::String timeout_message = "Timeout waiting for DisableHostController";
  return WaitWithTimeout(wait_for, zx::usec(kHostControllerTimeoutUs), timeout_message);
}

zx::result<> Ufs::ConfigurePowerManagement() {
  // driver_manager creates a power element for this node (levels off/on, "on" depending on the CPU
  // element and on the parent nodes' elements) and hands us its ElementRunner and dependency
  // token through DriverStartArgs.power_element_args. Bind the runner so that Power Broker can
  // drive Suspend()/Resume().
  if (!power_element_token_.is_valid()) {
    fdf::error("No power element was provided by the driver framework");
    return zx::error(ZX_ERR_NOT_FOUND);
  }
  if (zx::result<> result = InitializeSuspend(dispatcher(), *driver_incoming(), name());
      result.is_error()) {
    fdf::error("Failed to initialize suspend: {}", result);
    return result.take_error();
  }

  // Register Execution State's dependency on our power element.
  zx::result connect_to_cpu_element_manager =
      driver_incoming()->Connect<fuchsia_power_system::CpuElementManager>();
  if (connect_to_cpu_element_manager.is_error()) {
    fdf::error("CpuElementManager unavailable: {}",
               zx_status_get_string(connect_to_cpu_element_manager.error_value()));
    return connect_to_cpu_element_manager.take_error();
  }

  fidl::SyncClient<fuchsia_power_system::CpuElementManager> cpu_element_manager(
      std::move(connect_to_cpu_element_manager.value()));
  zx::event clone;
  if (zx_status_t status = power_element_token_.duplicate(ZX_RIGHT_SAME_RIGHTS, &clone);
      status != ZX_OK) {
    fdf::error("Failed to duplicate power element token: {}", zx_status_get_string(status));
    return zx::error(status);
  }
  fidl::Result<fuchsia_power_system::CpuElementManager::AddExecutionStateDependency> result =
      cpu_element_manager->AddExecutionStateDependency(
          {{.dependency_token = std::move(clone), .power_level = kPowerLevelOn}});
  if (result.is_error()) {
    fdf::error("CpuElementManager token registration failed: {}",
               result.error_value().FormatDescription().c_str());
    if (result.error_value().is_framework_error()) {
      return zx::error(result.error_value().framework_error().status());
    }

    switch (result.error_value().domain_error()) {
      case fuchsia_power_system::AddExecutionStateDependencyError::kInvalidArgs:
        return zx::error(ZX_ERR_INVALID_ARGS);
      case fuchsia_power_system::AddExecutionStateDependencyError::kBadState:
        return zx::error(ZX_ERR_BAD_STATE);
      default:
        return zx::error(ZX_ERR_INTERNAL);
    }
  }

  return zx::success();
}

// Power Broker treats a closed ElementRunner channel as "this element can never transition
// again": it logs the error and stops driving the element, so storage would stay unusable until
// reboot. A failed transition is therefore never answered by dropping the completer.
//
// A failed "off" is logged and acknowledged: SuspendPower() has already rolled the device back or
// recorded PowerDown, and the element genuinely no longer needs its dependencies. A failed "on" is
// different: the completer is the signal that the level was reached, and Execution State depends
// on it (ConfigurePowerManagement()), so acknowledging it early would let the rest of the system
// come up and then stall on page faults against a device that is still down. Instead the completer
// is held (pending_resume_completer_) while the driver keeps retrying ResumePower() in the
// background (RetryResume()); Power Broker's watchdog reports the stuck element every 10 s, which
// points at the real culprit. Power Broker does not send another SetLevel() while one is pending,
// so the retry loop is the only path back to "on". DeviceManager keeps the truthful hardware state,
// so every retry continues from where the failed attempt left off (see the comments in
// SuspendPower()/ResumePower()).
//
// The very first "off" that Power Broker issues when the element is created without a lease is
// swallowed by fdf_power::Suspendable (first_activation_occurred_) and never reaches Suspend(), so
// the hardware initialized by Init() stays active until the first "on" has been observed.
void Ufs::Resume(fdf_power::ResumeCompleter completer) {
  if (stopping_) {
    completer();
    return;
  }
  const zx::time start = zx::clock::get_monotonic();
  power_level_on_required_ = true;
  resume_retry_task_.Cancel();
  resume_retry_delay_ = kResumeRetryInitialDelay;

  // Actually raise the hardware's power level.
  zx::result result = device_manager_->ResumePower();
  const zx::duration duration = zx::clock::get_monotonic() - start;
  if (result.is_error()) {
    // The device stays in its suspended state and IsResumed() keeps reporting false, so I/O
    // remains queued until a retry succeeds. The SetLevel() stays unanswered until then.
    fdf::error("Failed to resume power after {} us: {}; retrying in {} ms", duration.to_usecs(),
               result, resume_retry_delay_.to_msecs());
    properties_.resume_failures.Add(1);
    pending_resume_completer_.emplace(std::move(completer));
    ScheduleResumeRetry();
    return;
  }
  properties_.wake_latency_us.Insert(duration.to_usecs());

  // Communicate to Power Broker that the hardware power level has been raised.
  completer();
  TriggerIoWork();
}

void Ufs::Suspend(fdf_power::SuspendCompleter completer) {
  power_level_on_required_ = false;
  resume_retry_task_.Cancel();
  // Power Broker does not issue a new SetLevel() while one is unanswered, so no resume can still be
  // pending here; answer it anyway rather than leave it dangling if that ever changes.
  CompletePendingResume();
  if (stopping_) {
    completer();
    return;
  }

  // Actually lower the hardware's power level.
  zx::result result = device_manager_->SuspendPower();
  if (result.is_error()) {
    // There is nothing further to do here: SuspendPower() has already either rolled the device
    // back to fully Active (the failure happened before the host controller was stopped, and the
    // I/O path has been re-opened) or recorded it as PowerDown (the controller was already
    // stopped, so the next ResumePower() reinitializes it). In both cases the element no longer
    // requires power from its dependencies, so the level is acknowledged regardless; refusing to
    // would leave Power Broker's SetLevel() pending and block the system suspend, and closing the
    // channel would disable the element for good (see above). The failure is counted in inspect.
    fdf::error("Failed to suspend power: {}", result);
    properties_.suspend_failures.Add(1);
  }

  // Communicate to Power Broker that the hardware power level has been lowered.
  completer();
}

void Ufs::ScheduleResumeRetry() {
  if (zx_status_t status = resume_retry_task_.PostDelayed(dispatcher(), resume_retry_delay_);
      status != ZX_OK) {
    fdf::error("Failed to schedule resume retry: {}", zx_status_get_string(status));
  }
}

void Ufs::CompletePendingResume() {
  if (pending_resume_completer_.has_value()) {
    (*pending_resume_completer_)();
    pending_resume_completer_.reset();
  }
}

// Runs on the default dispatcher, like Suspend()/Resume(), so they never overlap: Suspend() cancels
// a pending retry and clears power_level_on_required_.
void Ufs::RetryResume(async_dispatcher_t* dispatcher, async::TaskBase* task, zx_status_t status) {
  if (status != ZX_OK || !power_level_on_required_) {
    return;
  }
  const zx::time start = zx::clock::get_monotonic();
  zx::result result = device_manager_->ResumePower();
  const zx::duration duration = zx::clock::get_monotonic() - start;
  if (result.is_error()) {
    resume_retry_delay_ = std::min(resume_retry_delay_ * 2, kResumeRetryMaxDelay);
    fdf::error("Resume retry failed after {} us: {}; retrying in {} ms", duration.to_usecs(),
               result, resume_retry_delay_.to_msecs());
    properties_.resume_failures.Add(1);
    ScheduleResumeRetry();
    return;
  }
  fdf::info("Resume retry succeeded after {} us", duration.to_usecs());
  properties_.wake_latency_us.Insert(duration.to_usecs());
  resume_retry_delay_ = kResumeRetryInitialDelay;
  // Now that the device is actually back, tell Power Broker the level was reached.
  CompletePendingResume();
  TriggerIoWork();
}

zx::result<> Ufs::Start(fdf::DriverContext context) {
  incoming_ = std::shared_ptr<fdf::Namespace>(context.take_incoming());
  node_name_ = context.node_name();
  config_ = context.take_config<ufs_config::Config>();
  // Power element created by the driver framework (see ConfigurePowerManagement()).
  if (std::optional token = context.power_element_token(); token.has_value()) {
    power_element_token_ = std::move(token.value());
  }
  power_element_runner_ = context.take_power_element_runner();

  if (zx::result<> status = InitResources(); status.is_error()) {
    return status.take_error();
  }

  auto [controller_client_end, controller_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::NodeController>::Create();
  auto [node_client_end, node_server_end] =
      fidl::Endpoints<fuchsia_driver_framework::Node>::Create();

  node_controller_.Bind(std::move(controller_client_end));
  root_node_.Bind(std::move(node_client_end));

  fidl::Arena arena;

  const auto args =
      fuchsia_driver_framework::wire::NodeAddArgs::Builder(arena).name(arena, name()).Build();

  // Add root device, which will contain block devices for logical units
  auto result = fidl::WireCall(node().borrow())
                    ->AddChild(args, std::move(controller_server_end), std::move(node_server_end));
  if (!result.ok()) {
    fdf::error("Failed to add child: {}", result.status_string());
    return zx::error(result.status());
  }

  if (!host_controller_callback_) {
    SetHostControllerCallback(NotifyEventCallback);
  }

  auto connect_result = incoming_->Connect<fuchsia_inspect::InspectSink>();
  if (connect_result.is_ok()) {
    component_inspector_.emplace(dispatcher(), inspect::PublishOptions{
                                                   .tree_name = "ufs",
                                                   .client_end = std::move(connect_result.value()),
                                               });
  } else {
    fdf::warn("Failed to connect to InspectSink: {}", connect_result.status_string());
  }

  if (zx_status_t status = Init(); status != ZX_OK) {
    return zx::error(status);
  }

  if (SuspendEnabled()) {
    if (zx::result<> status = ConfigurePowerManagement(); status.is_error()) {
      return status.take_error();
    }
  }

  {
    fuchsia_hardware_ufs::Service::InstanceHandler handler({
        .device = fit::bind_member<&Ufs::Serve>(this),
    });
    zx::result result = outgoing()->AddService<fuchsia_hardware_ufs::Service>(std::move(handler));
    if (result.is_error()) {
      fdf::error("Failed to add service: {}", result);
      return result.take_error();
    }
  }

  return zx::ok();
}

zx::result<uint8_t> Ufs::ProgramKeySlot(fidl::VectorView<uint8_t> wrapped_key,
                                        uint32_t data_unit_size) {
  if (!inline_encryption_client_.is_valid()) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  // UFSHCI CRYPTOCFG DWORD 16 DUSIZE[7:0] encodes the data unit size as a one-hot
  // bitmask in multiples of 512 bytes (0x01 = 512B/0x200 .. 0x80 = 64KiB/0x10000).
  constexpr uint32_t kMinCryptoDataUnitSize = 0x200;    // 512 B (DUSIZE = 0x01)
  constexpr uint32_t kMaxCryptoDataUnitSize = 0x10000;  // 64 KiB (DUSIZE = 0x80)
  const uint32_t du = data_unit_size;
  if (du < kMinCryptoDataUnitSize || du > kMaxCryptoDataUnitSize || !std::has_single_bit(du)) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  auto result = inline_encryption_client_->ProgramKeySlot(wrapped_key, data_unit_size);
  if (!result.ok()) {
    return zx::error(result.status());
  }
  if (result->is_error()) {
    return zx::error(result->error_value());
  }

  const uint8_t slot = result->value()->slot;
  if (slot >= kMaxCryptoSlots) {
    return zx::error(ZX_ERR_OUT_OF_RANGE);
  }

  if (zx::result<> result = ConfigureCryptoSlot(slot, data_unit_size); result.is_error()) {
    return result.take_error();
  }
  return zx::ok(slot);
}

zx::result<> Ufs::ConfigureCryptoSlot(uint8_t slot, uint32_t data_unit_size) {
  ZX_DEBUG_ASSERT(slot < kMaxCryptoSlots);
  // CRYPTOCFG lives in the UFSHCI register space, which is gated during a PowerDown suspend.
  std::lock_guard<std::mutex> lock(uic_lock_);
  if (IsHardwarePoweredOff()) {
    return zx::error(ZX_ERR_BAD_STATE);
  }
  WriteCryptoConfigLocked(slot, data_unit_size);
  crypto_slot_data_unit_size_[slot] = data_unit_size;
  return zx::ok();
}

void Ufs::WriteCryptoConfigLocked(uint8_t slot, uint32_t data_unit_size) {
  // Configure UFSHCI CRYPTO_CFG per Section 5.6.3:
  // 1. Clear CFGE = 0 first.
  // 2. Program remaining configuration words (DWORD 17 = 0).
  // 3. Write CFGE = 1 | DUSIZE to DWORD 16 last.
  constexpr uint32_t kMinCryptoDataUnitSize = 0x200;
  constexpr uint32_t kCryptoCfgBase = 0x500;
  constexpr uint32_t kCryptoCfgStride = 0x80;
  constexpr uint32_t kCryptoCfgDword16Offset = 0x40;
  constexpr uint32_t kCryptoCfgDword17Offset = 0x44;
  constexpr uint32_t kCryptoCfgEnable = 1U << 31;
  const uint32_t dusize_flag = data_unit_size / kMinCryptoDataUnitSize;
  const uint32_t cfg_offset = kCryptoCfgBase + slot * kCryptoCfgStride;
  GetMmio().Write32(0, cfg_offset + kCryptoCfgDword16Offset);
  GetMmio().Write32(0, cfg_offset + kCryptoCfgDword17Offset);
  GetMmio().Write32(kCryptoCfgEnable | (dusize_flag & 0xffU), cfg_offset + kCryptoCfgDword16Offset);
}

bool Ufs::HasProgrammedCryptoKeys() {
  std::lock_guard<std::mutex> lock(uic_lock_);
  return std::ranges::any_of(crypto_slot_data_unit_size_, [](uint32_t du) { return du != 0; });
}

zx::result<> Ufs::RestoreCryptoState() {
  if (!crypto_supported_) {
    return zx::ok();
  }

  uint32_t restored_slots = 0;
  {
    std::lock_guard<std::mutex> lock(uic_lock_);
    if (IsHardwarePoweredOff()) {
      return zx::error(ZX_ERR_BAD_STATE);
    }
    for (uint8_t slot = 0; slot < kMaxCryptoSlots; ++slot) {
      const uint32_t data_unit_size = crypto_slot_data_unit_size_[slot];
      if (data_unit_size == 0) {
        continue;
      }
      WriteCryptoConfigLocked(slot, data_unit_size);
      ++restored_slots;
    }
  }
  if (restored_slots == 0) {
    return zx::ok();
  }

  // TODO(https://fxbug.dev/568465123): CRYPTOCFG has been rewritten above, but the key material in
  // the host controller's key table is also lost across PowerDown and must be re-pushed by the
  // inline encryption provider (e.g. GSA KDN RESTORE_KEYS on Iris) before encrypted I/O can be
  // resumed. Until that is plumbed through fuchsia.hardware.inlineencryption, fail resume rather
  // than let encrypted I/O silently proceed with an empty key table. DeviceManager::SuspendPower()
  // falls back to Sleep when keys are programmed, so this is not reachable in practice.
  fdf::error("Cannot restore {} inline encryption key(s) after PowerDown suspend", restored_slots);
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}

zx::result<std::vector<uint8_t>> Ufs::DeriveRawSecret(fidl::VectorView<uint8_t> wrapped_key) {
  if (!inline_encryption_client_.is_valid()) {
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
  auto result = inline_encryption_client_->DeriveRawSecret(wrapped_key);
  if (!result.ok()) {
    return zx::error(result.status());
  }
  if (result->is_error()) {
    return zx::error(result->error_value());
  }
  auto& secret = result->value()->secret;
  return zx::ok(std::vector<uint8_t>(secret.begin(), secret.end()));
}

void Ufs::OnDispatcherShutdown() {
  std::optional<fdf::StopCompleter> completer;
  {
    std::lock_guard<std::mutex> lock(lock_);
    if (pending_dispatcher_shutdowns_ > 0 && --pending_dispatcher_shutdowns_ == 0) {
      if (auto old = std::exchange(stop_completer_, std::nullopt); old.has_value()) {
        completer.emplace(std::move(*old));
      }
    }
  }
  if (completer.has_value()) {
    (*completer)(zx::ok());
  }
}

void Ufs::FinishStop(fdf::StopCompleter completer) {
  if (zx_status_t status = StopResources(); status != ZX_OK) {
    completer(zx::error(status));
    return;
  }

  if (irq_.is_valid()) {
    irq_.destroy();
  }

  uint32_t dispatchers_to_shutdown = 0;
  if (exception_event_dispatcher_.get())
    dispatchers_to_shutdown++;
  if (irq_worker_dispatcher_.get())
    dispatchers_to_shutdown++;
  if (io_worker_dispatcher_.get())
    dispatchers_to_shutdown++;
  if (admin_worker_dispatcher_.get())
    dispatchers_to_shutdown++;

  if (dispatchers_to_shutdown == 0) {
    completer(zx::ok());
    return;
  }

  {
    std::lock_guard<std::mutex> lock(lock_);
    stop_completer_.emplace(std::move(completer));
    pending_dispatcher_shutdowns_ = dispatchers_to_shutdown;
  }

  if (exception_event_dispatcher_.get()) {
    exception_event_dispatcher_.ShutdownAsync();
  }
  if (irq_worker_dispatcher_.get()) {
    irq_worker_dispatcher_.ShutdownAsync();
  }
  if (io_worker_dispatcher_.get()) {
    io_worker_dispatcher_.ShutdownAsync();
  }
  if (admin_worker_dispatcher_.get()) {
    admin_worker_dispatcher_.ShutdownAsync();
  }
}

void Ufs::Stop(fdf::StopCompleter completer) {
  // Stop acting on power-level transitions: a SetLevel(on) arriving mid-teardown would otherwise
  // start a full ResumePower()/re-initialization against dispatchers that are shutting down. The
  // ElementRunner binding is owned by fdf_power::Suspendable and stays open until the driver is
  // destroyed, so Suspend()/Resume() check stopping_ and just acknowledge the level.
  stopping_ = true;
  resume_retry_task_.Cancel();
  power_level_on_required_ = false;
  // A resume that was still being retried will never complete now; answer Power Broker so the
  // completer is not destroyed unanswered.
  CompletePendingResume();
  std::deque<IoCommand> pending;
  std::vector<scsi::BlockDevice*> devs;
  {
    std::lock_guard<std::mutex> lock(lock_);
    shutdown_ = true;
    pending.swap(pending_commands_);
    for (auto& [target, luns] : block_devs_) {
      for (auto& [lun, dev] : luns) {
        if (dev) {
          devs.push_back(dev.get());
        }
      }
    }

    if (devs.empty()) {
      block_devs_.clear();
    }
  }
  for (auto& cmd : pending) {
    cmd.request.Complete(ZX_ERR_IO_NOT_PRESENT);
  }

  if (devs.empty()) {
    FinishStop(std::move(completer));
    return;
  }

  auto shared_completer = std::make_shared<fdf::StopCompleter>(std::move(completer));
  auto remaining = std::make_shared<std::atomic<size_t>>(devs.size());

  for (auto* dev : devs) {
    dev->ShutdownAsync([this, shared_completer, remaining]() {
      if (remaining->fetch_sub(1) == 1) {
        async::PostTask(dispatcher(), [this, shared_completer]() {
          {
            std::lock_guard<std::mutex> lock(lock_);
            block_devs_.clear();
          }
          FinishStop(std::move(*shared_completer));
        });
      }
    });
  }
}

void Ufs::Serve(fidl::ServerEnd<fuchsia_hardware_ufs::Ufs> server_end) {
  auto server_impl = std::make_unique<UfsServer>(this);
  fidl::BindServer(dispatcher(), std::move(server_end), std::move(server_impl));
}

Ufs::~Ufs() {
  resume_retry_task_.Cancel();
  CompletePendingResume();
  irq_handler_.Cancel();
  if (irq_.is_valid()) {
    irq_.destroy();
  }
}

}  // namespace ufs
