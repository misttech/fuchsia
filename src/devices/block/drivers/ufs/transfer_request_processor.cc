// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "transfer_request_processor.h"

#include <lib/driver/logging/cpp/logger.h>
#include <lib/trace/event.h>

#include <optional>

#include <safemath/checked_math.h>
#include <safemath/safe_conversions.h>

#include "src/devices/block/drivers/ufs/ufs.h"
#include "src/devices/block/drivers/ufs/upiu/upiu_transactions.h"

namespace ufs {

namespace {
void FillPrdt(PhysicalRegionDescriptionTableEntry *prdt,
              const std::vector<zx_paddr_t> &buffer_physical_addresses, uint32_t prdt_count,
              uint32_t data_length) {
  for (uint32_t i = 0; i < prdt_count; ++i) {
    // It only supports 4KB data buffers for each entry in the scatter-gather.
    ZX_ASSERT(buffer_physical_addresses[i] != 0);
    uint32_t byte_count = data_length < kPrdtEntryDataLength ? data_length : kPrdtEntryDataLength;
    prdt->set_data_base_address(static_cast<uint32_t>(buffer_physical_addresses[i] & 0xffffffff));
    prdt->set_data_base_address_upper(static_cast<uint32_t>(buffer_physical_addresses[i] >> 32));
    prdt->set_data_byte_count(byte_count - 1);

    ++prdt;
    data_length -= byte_count;
  }
  ZX_DEBUG_ASSERT(data_length == 0);
}
}  // namespace

template <>
std::tuple<uint16_t, uint32_t> TransferRequestProcessor::PreparePrdt<ScsiCommandUpiu>(
    ScsiCommandUpiu &request, const uint8_t lun, const uint8_t slot,
    const std::vector<zx_paddr_t> &buffer_phys, const uint16_t response_offset,
    const uint16_t response_length) {
  const uint32_t data_transfer_length = std::min(request.GetTransferBytes(), kMaxPrdtDataLength);

  request.GetHeader().lun = lun;
  request.SetExpectedDataTransferLength(data_transfer_length);

  // Prepare PRDT(physical region description table).
  const uint32_t prdt_entry_count =
      fbl::round_up(data_transfer_length, kPrdtEntryDataLength) / kPrdtEntryDataLength;
  ZX_DEBUG_ASSERT(prdt_entry_count <= kMaxPrdtEntryCount);

  uint16_t prdt_offset = response_offset + response_length;
  uint32_t prdt_length_in_bytes = prdt_entry_count * sizeof(PhysicalRegionDescriptionTableEntry);
  const size_t total_length = static_cast<size_t>(prdt_offset) + prdt_length_in_bytes;

  ZX_DEBUG_ASSERT_MSG(total_length <= request_list_.GetDescriptorBufferSize(slot),
                      "Invalid UPIU size for prdt");
  auto prdt =
      request_list_.GetDescriptorBuffer<PhysicalRegionDescriptionTableEntry>(slot, prdt_offset);
  CustomMemSet(prdt, 0, prdt_length_in_bytes);

  FillPrdt(prdt, buffer_phys, prdt_entry_count, data_transfer_length);

  // TODO(https://fxbug.dev/42075643): Enable unmmap and write buffer command. Umap and writebuffer
  // must set the xfer->count value differently.

  return {prdt_offset, prdt_entry_count};
}

zx::result<> TransferRequestProcessor::Init() {
  zx_paddr_t paddr =
      request_list_.GetRequestDescriptorPhysicalAddress<TransferRequestDescriptor>(0);
  UtrListBaseAddressReg::Get().FromValue(paddr & 0xffffffff).WriteTo(&register_);
  UtrListBaseAddressUpperReg::Get().FromValue(paddr >> 32).WriteTo(&register_);

  if (!HostControllerStatusReg::Get().ReadFrom(&register_).utp_transfer_request_list_ready()) {
    fdf::error("UTP transfer request list is not ready\n");
    return zx::error(ZX_ERR_INTERNAL);
  }

  if (UtrListDoorBellReg::Get().ReadFrom(&register_).door_bell() != 0) {
    fdf::error("UTP transfer request list door bell is not ready\n");
    return zx::error(ZX_ERR_INTERNAL);
  }

  if (UtrListCompletionNotificationReg::Get().ReadFrom(&register_).notification() != 0) {
    fdf::error("UTP transfer request list notification is not ready\n");
    return zx::error(ZX_ERR_INTERNAL);
  }

  // Start Utp Transfer Request list.
  UtrListRunStopReg::Get().FromValue(0).set_value(true).WriteTo(&register_);

  return zx::ok();
}

zx::result<uint8_t> TransferRequestProcessor::ReserveAdminSlot() {
  RequestSlot &slot = request_list_.GetSlot(kAdminCommandSlotNumber);
  if (slot.state == SlotState::kFree) {
    slot.state = SlotState::kReserved;
    return zx::ok(kAdminCommandSlotNumber);
  }
  fdf::debug("Failed to reserve a admin request slot");
  return zx::error(ZX_ERR_NO_RESOURCES);
}

zx::result<std::unique_ptr<ResponseUpiu>> TransferRequestProcessor::SendScsiUpiuUsingSlot(
    ScsiCommandUpiu &request, uint8_t lun, uint8_t slot, zx::unowned_vmo data_vmo,
    IoCommand *io_cmd, bool synchronous) {
  uint32_t block_offset = 0;
  uint32_t block_length = 0;
  uint64_t dma_offset = 0;
  uint64_t dma_length = 0;
  if (io_cmd != nullptr) {
    block_offset =
        safemath::checked_cast<uint32_t>(io_cmd->device_op.op.command.opcode == BLOCK_OPCODE_TRIM
                                             ? io_cmd->device_op.op.trim.offset_dev
                                             : io_cmd->device_op.op.rw.offset_dev);
    block_length = io_cmd->device_op.op.command.opcode == BLOCK_OPCODE_TRIM
                       ? io_cmd->device_op.op.trim.length
                       : io_cmd->device_op.op.rw.length;
    if (data_vmo->is_valid()) {
      if (io_cmd->device_op.op.command.opcode == BLOCK_OPCODE_TRIM) {
        dma_offset = 0;
        dma_length = zx_system_get_page_size();
      } else {
        dma_offset = io_cmd->device_op.op.rw.offset_vmo * io_cmd->block_size_bytes;
        dma_length = static_cast<uint64_t>(io_cmd->device_op.op.rw.length) *
                     io_cmd->block_size_bytes;
      }
    }
  } else if (data_vmo->is_valid()) {
    dma_offset = 0;
    dma_length = fbl::round_up(request.GetTransferBytes(), zx_system_get_page_size());
  }
  TRACE_DURATION("ufs", "SendScsiUpiu", "slot", slot, "offset", block_offset, "length",
                 block_length);

  zx::result<void *> response = SendRequestUsingSlot<ScsiCommandUpiu>(
      request, lun, slot, std::move(data_vmo), dma_offset, dma_length, io_cmd, synchronous);

  if (response.is_error()) {
    return response.take_error();
  }
  auto response_upiu = std::make_unique<ResponseUpiu>(response.value());
  return zx::ok(std::move(response_upiu));
}

zx::result<std::unique_ptr<ResponseUpiu>> TransferRequestProcessor::SendAdminScsiCmd(
    ScsiCommandUpiu &request, uint8_t lun, zx::unowned_vmo data_vmo) {
  if (request.GetTransferBytes() > 0 && !data_vmo->is_valid()) {
    return zx::error(ZX_ERR_BAD_HANDLE);
  }

  std::lock_guard<std::mutex> lock(admin_slot_lock_);
  zx::result<uint8_t> slot = ReserveAdminSlot();
  if (slot.is_error()) {
    return zx::error(ZX_ERR_NO_RESOURCES);
  }

  return SendScsiUpiuUsingSlot(request, lun, slot.value(), std::move(data_vmo), nullptr,
                               /*synchronous*/ true);
}

zx::result<std::unique_ptr<ResponseUpiu>> TransferRequestProcessor::SendIoScsiCmd(
    ScsiCommandUpiu &request, uint8_t lun, IoCommand *io_cmd) {
  if (io_cmd == nullptr) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  bool has_data_vmo = io_cmd->vmo()->is_valid();
  if (request.GetTransferBytes() > 0 && !has_data_vmo) {
    return zx::error(ZX_ERR_BAD_HANDLE);
  }

  const bool synchronous = io_cmd->device_op.completion_cb == nullptr;

  zx::result<uint8_t> slot = ReserveSlot();
  if (slot.is_error()) {
    return zx::error(ZX_ERR_NO_RESOURCES);
  }

  return SendScsiUpiuUsingSlot(request, lun, slot.value(), io_cmd->vmo(), io_cmd, synchronous);
}

zx::result<std::unique_ptr<QueryResponseUpiu>> TransferRequestProcessor::SendQueryRequestUpiu(
    QueryRequestUpiu &request) {
  auto response = SendRequestUpiu<QueryRequestUpiu, QueryResponseUpiu>(request);
  if (response.is_error()) {
    QueryOpcode query_opcode =
        static_cast<QueryOpcode>(request.GetData<QueryRequestUpiuData>()->opcode);
    uint8_t type = request.GetData<QueryRequestUpiuData>()->idn;
    fdf::error("Failed {}(type:0x{:x}) query request UPIU: {}", QueryOpcodeToString(query_opcode),
               type, response.status_string());
  }
  return response;
}

template <class RequestType>
zx::result<void *> TransferRequestProcessor::SendRequestUsingSlot(
    RequestType &request, uint8_t lun, uint8_t slot, zx::unowned_vmo data_vmo, uint64_t dma_offset,
    uint64_t dma_length, IoCommand *io_cmd, bool synchronous) {
  if (synchronous) {
    // TODO(https://fxbug.dev/42075643): Needs to be changed to be compatible with DFv2's dispatcher
    // Since the completion is handled by the I/O thread, submitting a synchronous command from the
    // I/O thread will cause a deadlock.
    // ZX_DEBUG_ASSERT(controller_.GetIoThread() != thrd_current());
  }

  RequestSlot &request_slot = request_list_.GetSlot(slot);
  ZX_DEBUG_ASSERT_MSG(request_slot.state == SlotState::kReserved, "Invalid slot state");
  auto cleanup = fit::defer([&]() {
    if (zx::result<> result = ClearSlot(request_slot); result.is_error()) {
      fdf::error("Failed to clear slot: {}", result);
    }
  });

  const uint16_t response_offset = request.GetResponseOffset();
  const uint16_t response_length = request.GetResponseLength();

  request_slot.io_cmd = io_cmd;
  request_slot.data_vmo = data_vmo;
  request_slot.dma_offset = dma_offset;
  request_slot.dma_length = dma_length;
  request_slot.is_read = (request.GetDataDirection() == DataDirection::kDeviceToHost);
  request_slot.is_scsi_command = std::is_base_of<ScsiCommandUpiu, RequestType>::value;
  request_slot.is_sync = synchronous;
  request_slot.response_upiu_offset = response_offset;

  uint16_t prdt_offset = 0;
  uint32_t prdt_entry_count = 0;
  std::vector<zx_paddr_t> data_paddrs;

  if (request_slot.data_vmo->is_valid()) {
    // Assign physical addresses(pin) to data vmo. The return value is the physical address of
    // the pinned memory.
    const uint32_t kPageSize = zx_system_get_page_size();
    uint32_t option = request_slot.is_read ? ZX_BTI_PERM_WRITE : ZX_BTI_PERM_READ;

    ZX_DEBUG_ASSERT(dma_length > 0 && dma_length % kPageSize == 0);

    data_paddrs.resize(dma_length / kPageSize, 0);
    if (zx_status_t status =
            GetBti()->pin(option, *request_slot.data_vmo, dma_offset, dma_length,
                          data_paddrs.data(), dma_length / kPageSize, &request_slot.pmt);
        status != ZX_OK) {
      fdf::error("Failed to pin IO buffer: {}", zx_status_get_string(status));
      return zx::error(status);
    }

    // Ensure that any cached writes are written out to volatile memory before we issue the
    // request. Even for a read, we must pessimistically assume that there are pending writes to
    // the VMO which need to be flushed before we start doing the DMA.  If we didn't flush the
    // writes, they might get written out after we start to DMA, in which case they could stomp
    // the read bytes.
    uint32_t op = request_slot.is_read ? ZX_VMO_OP_CACHE_CLEAN_INVALIDATE : ZX_VMO_OP_CACHE_CLEAN;
    zx_status_t status = request_slot.data_vmo->op_range(op, dma_offset, dma_length, nullptr, 0);
    if (status != ZX_OK) {
      fdf::error("Failed to invalidate cache for data VMO: {}", zx_status_get_string(status));
      return zx::error(status);
    }
  }

  std::tie(prdt_offset, prdt_entry_count) =
      PreparePrdt<RequestType>(request, lun, slot, data_paddrs, response_offset, response_length);

  // Record the slot number to |task_tag| for debugging.
  request.GetHeader().task_tag = slot;

  // Copy request and prepare response.
  const size_t length = static_cast<size_t>(response_offset) + response_length;
  ZX_DEBUG_ASSERT_MSG(length <= request_list_.GetDescriptorBufferSize(slot), "Invalid UPIU size");

  CustomMemCpy(request_list_.GetDescriptorBuffer(slot), request.GetData(), response_offset);
  CustomMemSet(request_list_.GetDescriptorBuffer<uint8_t>(slot) + response_offset, 0,
               response_length);
  auto response = request_list_.GetDescriptorBuffer(slot, response_offset);

  if (zx::result<> result =
          FillDescriptorAndSendRequest(slot, request.GetDataDirection(), response_offset,
                                       response_length, prdt_offset, prdt_entry_count);
      result.is_error()) {
    fdf::error("Failed to send upiu: {}", result);
    return result.take_error();
  }
  cleanup.cancel();

  if (synchronous) {
    // Wait for completion.
    TRACE_DURATION("ufs", "SendRequestUsingSlot::sync_completion_wait", "slot", slot);
    zx_status_t status =
        sync_completion_wait_deadline(&request_slot.complete, request_slot.deadline);
    zx_status_t request_result = request_slot.result;
    if (zx::result<> result = ClearSlot(request_slot); result.is_error()) {
      return result.take_error();
    }
    if (status != ZX_OK) {
      fdf::error("SendRequestUsingSlot request timed out: {}", zx_status_get_string(status));
      return zx::error(status);
    }
    if (request_result != ZX_OK) {
      return zx::error(request_result);
    }

    UtrListCompletionNotificationReg::Get().FromValue(0).set_notification(1 << slot).WriteTo(
        &register_);
  }

  return zx::ok(response);
}

template zx::result<void *> TransferRequestProcessor::SendRequestUsingSlot<QueryRequestUpiu>(
    QueryRequestUpiu &request, uint8_t lun, uint8_t slot, zx::unowned_vmo data_vmo,
    uint64_t dma_offset, uint64_t dma_length, IoCommand *io_cmd, bool synchronous);
template zx::result<void *> TransferRequestProcessor::SendRequestUsingSlot<ScsiCommandUpiu>(
    ScsiCommandUpiu &request, uint8_t lun, uint8_t slot, zx::unowned_vmo data_vmo,
    uint64_t dma_offset, uint64_t dma_length, IoCommand *io_cmd, bool synchronous);
template zx::result<void *> TransferRequestProcessor::SendRequestUsingSlot<NopOutUpiu>(
    NopOutUpiu &request, uint8_t lun, uint8_t slot, zx::unowned_vmo data_vmo, uint64_t dma_offset,
    uint64_t dma_length, IoCommand *io_cmd, bool synchronous);

zx_status_t TransferRequestProcessor::UpiuCompletion(uint8_t slot_num, RequestSlot &request_slot,
                                                     bool is_timeout) {
  TRACE_DURATION("ufs", "UpiuCompletion", "slot", slot_num);

  scsi::StatusMessage status_message;
  std::optional<std::reference_wrapper<scsi::FixedFormatSenseDataHeader>> sense_data = std::nullopt;

  ResponseUpiu response(
      request_list_.GetDescriptorBuffer<ResponseUpiu>(slot_num, request_slot.response_upiu_offset));

  zx::result<> request_result = zx::ok();
  if (is_timeout) {
    status_message.host_status_code = scsi::HostStatusCode::kTimeout;
    status_message.scsi_status_code = scsi::StatusCode::GOOD;
  } else {
    request_result = CheckResponse(slot_num, response);

    if (request_slot.is_scsi_command) {
      status_message = CheckScsiAndGetStatusMessage(slot_num, response);
      sense_data = *reinterpret_cast<scsi::FixedFormatSenseDataHeader *>(response.GetSenseData());
    }
  }

  if (request_slot.is_scsi_command && request_result.is_ok()) {
    // Until native UFS IO commands are defined by the UFS specification, we assume that only SCSI
    // commands can be IO commands.
    request_result = controller_.ScsiComplete(status_message, sense_data);

    // Unpin data buffer before signalling request completion to the upper layer. This is
    // necessary because the filesystem is allowed to transfer pages directly out of this
    // buffer.
    if (request_slot.pmt.is_valid()) {
      if (zx_status_t status = request_slot.pmt.unpin(); status != ZX_OK) {
        fdf::error("Failed to unpin IO buffer: {}", zx_status_get_string(status));
        request_result = zx::error(status);
      }
    }

    IoCommand *io_cmd = request_slot.io_cmd;
    if (io_cmd) {
      io_cmd->data_vmo.reset();
      io_cmd->device_op.Complete(request_result.status_value());
    }
  }

  if (response.GetHeader().event_alert()) {
    if (zx::result result = controller_.GetDeviceManager().PostExceptionEventsTask();
        result.is_error()) {
      fdf::error("Failed to handle Exception Event slot[{}]: {}", slot_num, result.status_string());
    }
  }

  return request_result.status_value();
}

void TransferRequestProcessor::RequestCompletion(uint8_t slot_num, RequestSlot &request_slot,
                                                 bool is_timeout) {
  if (request_slot.data_vmo->is_valid() && request_slot.is_read) {
    // Invalidate the cache so the read data is visible to the CPU.
    zx_status_t status =
        request_slot.data_vmo->op_range(ZX_VMO_OP_CACHE_CLEAN_INVALIDATE, request_slot.dma_offset,
                                        request_slot.dma_length, nullptr, 0);
    if (status != ZX_OK) {
      fdf::error("Failed to invalidate cache for data VMO: {}", zx_status_get_string(status));
    }
  }
  // Check request response.
  zx_status_t status = UpiuCompletion(slot_num, request_slot, is_timeout);
  if (status == ZX_ERR_UNAVAILABLE) {
    fdf::warn("Unavailability reported for request, slot[{}] (Possibly a UNIT_ATTENTION condition)",
              slot_num);
  } else if (status != ZX_OK) {
    fdf::error("Failed to complete request, slot[{}]: {}", slot_num, zx_status_get_string(status));
  }
  request_slot.result = status;

  if (is_timeout) {
    request_slot.state = SlotState::kTimeout;
  } else if (request_slot.is_sync) {
    sync_completion_signal(&request_slot.complete);
  } else {
    UtrListCompletionNotificationReg::Get()
        .FromValue(0)
        .set_notification(1 << slot_num)
        .WriteTo(&register_);

    if (zx::result result = ClearSlot(request_slot); result.is_error()) {
      fdf::error("Failed to clear slot[{}]: {}", slot_num, result);
    }
  }

  --inflight_io_count_;
}

bool TransferRequestProcessor::ProcessSlotCompletion(uint8_t slot_num) {
  bool is_completed = false;

  RequestSlot &request_slot = request_list_.GetSlot(slot_num);
  if (request_slot.state == SlotState::kScheduled) {
    if (!(UtrListDoorBellReg::Get().ReadFrom(&register_).door_bell() & (1 << slot_num))) {
      RequestCompletion(slot_num, request_slot, /*timeout*/ false);
      is_completed = true;
    } else if (request_slot.deadline < zx_clock_get_monotonic()) {
      RequestCompletion(slot_num, request_slot, /*timeout*/ true);
      is_completed = true;
    }
  }
  return is_completed;
}

uint32_t TransferRequestProcessor::ProcessCompletionOfAdminRequests() {
  uint32_t completion_count = 0;

  if (disable_completion_) {
    return completion_count;
  }

  completion_count = ProcessSlotCompletion(kAdminCommandSlotNumber);
  return completion_count;
}

uint32_t TransferRequestProcessor::ProcessCompletionOfIoRequests() {
  uint32_t completion_count = 0;

  if (disable_completion_) {
    return completion_count;
  }

  // Search for all pending slots and signal the ones already done.
  request_list_.ForEachSlot([&](uint8_t slot_num, RequestSlot &request_slot) {
    if (slot_num != kAdminCommandSlotNumber) {
      completion_count += ProcessSlotCompletion(slot_num);
    }
  });
  return completion_count;
}

zx_time_t TransferRequestProcessor::GetEarliestTimeoutDeadline() {
  zx_time_t deadline = ZX_TIME_INFINITE;
  request_list_.ForEachSlot([&](uint8_t slot_num, RequestSlot &request_slot) {
    if (request_slot.state == SlotState::kScheduled) {
      deadline = std::min(deadline, request_slot.deadline);
    }
  });
  return deadline;
}

zx::result<> TransferRequestProcessor::FillDescriptorAndSendRequest(
    uint8_t slot, const DataDirection data_dir, const uint16_t response_offset,
    const uint16_t response_length, const uint16_t prdt_offset, const uint32_t prdt_entry_count) {
  auto descriptor = request_list_.GetRequestDescriptor<TransferRequestDescriptor>(slot);
  zx_paddr_t paddr = request_list_.GetSlot(slot).command_descriptor_io->phys();

  // Fill up UTP Transfer Request Descriptor.
  CustomMemSet(descriptor, 0, sizeof(TransferRequestDescriptor));
  descriptor->set_interrupt(true);
  descriptor->set_data_direction(data_dir);
  descriptor->set_command_type(kCommandTypeUfsStorage);
  // If the command was successful, overwrite |overall_command_status| field with |kSuccess|.
  descriptor->set_overall_command_status(OverallCommandStatus::kInvalid);
  descriptor->set_utp_command_descriptor_base_address(static_cast<uint32_t>(paddr & 0xffffffff));
  descriptor->set_utp_command_descriptor_base_address_upper(static_cast<uint32_t>(paddr >> 32));

  constexpr uint16_t kDwordSize = 4;
  descriptor->set_response_upiu_offset(response_offset / kDwordSize);
  descriptor->set_response_upiu_length(response_length / kDwordSize);
  descriptor->set_prdt_offset(prdt_offset / kDwordSize);
  descriptor->set_prdt_length(prdt_entry_count);

  TRACE_DURATION("ufs", "RingRequestDoorbell", "slot", slot);
  if (zx::result<> result = controller_.Notify(NotifyEvent::kSetupTransferRequestList, slot);
      result.is_error()) {
    return result.take_error();
  }
  if (zx::result<> result = RingRequestDoorbell(slot); result.is_error()) {
    fdf::error("Failed to send cmd {}", result);
    return result.take_error();
  }
  ++inflight_io_count_;

  return zx::ok();
}

scsi::HostStatusCode TransferRequestProcessor::ScsiStatusToHostStatus(
    scsi::StatusCode scsi_status) {
  scsi::HostStatusCode host_status;
  switch (scsi_status) {
    case scsi::StatusCode::GOOD:
    case scsi::StatusCode::CHECK_CONDITION:
      host_status = scsi::HostStatusCode::kOk;
      break;
    case scsi::StatusCode::BUSY:
    case scsi::StatusCode::TASK_SET_FULL:
      host_status = scsi::HostStatusCode::kRequeue;
      break;
    case scsi::StatusCode::RESERVATION_CONFILCT:  // optional
      host_status = scsi::HostStatusCode::kOk;
      break;
    default:
      host_status = scsi::HostStatusCode::kError;
      break;
  }
  return host_status;
}

scsi::HostStatusCode TransferRequestProcessor::GetScsiCommandHostStatus(
    OverallCommandStatus ocs, UpiuHeaderResponseCode header_response,
    scsi::StatusCode scsi_status) {
  scsi::HostStatusCode host_status;
  switch (ocs) {
    case kSuccess:
      if (header_response == UpiuHeaderResponseCode::kTargetSuccess) {
        host_status = ScsiStatusToHostStatus(static_cast<scsi::StatusCode>(scsi_status));
      } else {
        host_status = scsi::HostStatusCode::kError;
      }
      break;
    case kAborted:
      host_status = scsi::HostStatusCode::kAbort;
      break;
    case kInvalid:
      host_status = scsi::HostStatusCode::kRequeue;
      break;
    default:
      host_status = scsi::HostStatusCode::kError;
      break;
  }
  return host_status;
}

scsi::StatusMessage TransferRequestProcessor::CheckScsiAndGetStatusMessage(
    uint8_t slot_num, AbstractResponseUpiu &response) {
  auto descriptor = request_list_.GetRequestDescriptor<TransferRequestDescriptor>(slot_num);
  OverallCommandStatus ocs = descriptor->overall_command_status();
  auto header_response = static_cast<UpiuHeaderResponseCode>(response.GetHeader().response);
  auto scsi_status = static_cast<scsi::StatusCode>(response.GetHeader().status);

  scsi::StatusMessage message;
  message.host_status_code = GetScsiCommandHostStatus(ocs, header_response, scsi_status);
  message.scsi_status_code = scsi_status;
  return message;
}

zx::result<> TransferRequestProcessor::CheckResponse(uint8_t slot_num,
                                                     AbstractResponseUpiu &response) {
  auto transaction_type = static_cast<UpiuTransactionCodes>(response.GetHeader().trans_type);
  auto descriptor = request_list_.GetRequestDescriptor<TransferRequestDescriptor>(slot_num);
  OverallCommandStatus ocs = descriptor->overall_command_status();
  auto header_response = static_cast<UpiuHeaderResponseCode>(response.GetHeader().response);

  switch (transaction_type) {
    case UpiuTransactionCodes::kResponse:
      if (response.GetHeader().command_set_type() != UpiuCommandSetType::kScsi) {
        fdf::error(
            "Unknown command(set type = 0x{:x}) response: ocs=0x{:x}, header_response=0x{:x}",
            static_cast<uint32_t>(response.GetHeader().command_set_type()),
            static_cast<uint32_t>(ocs), static_cast<uint32_t>(header_response));
        return zx::error(ZX_ERR_BAD_STATE);
      }
      // For SCSI commands, check ocs and header_response in CheckScsiAndGetStatusMessage().
      break;
    case UpiuTransactionCodes::kQueryResponse:
      if (ocs != OverallCommandStatus::kSuccess ||
          header_response != static_cast<uint8_t>(QueryResponseCode::kSuccess)) {
        fdf::error("Query request failure: ocs=0x{:x}, header_response=0x{:x}",
                   static_cast<uint32_t>(ocs), static_cast<uint32_t>(header_response));
        return zx::error(ZX_ERR_BAD_STATE);
      }
      break;
    default:
      if (ocs != OverallCommandStatus::kSuccess ||
          header_response != UpiuHeaderResponseCode::kTargetSuccess) {
        fdf::error("Generic request(transaction type = 0x{:x}) failure: ocs=0x{:x}",
                   static_cast<uint32_t>(transaction_type), static_cast<uint32_t>(ocs));
        return zx::error(ZX_ERR_BAD_STATE);
      }
      break;
  }
  return zx::ok();
}

}  // namespace ufs
