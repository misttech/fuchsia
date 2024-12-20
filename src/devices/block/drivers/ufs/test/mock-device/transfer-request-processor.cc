// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/ddk/debug.h>

#include "ufs-mock-device.h"

namespace ufs {
namespace ufs_mock_device {

void TransferRequestProcessor::HandleTransferRequest(TransferRequestDescriptor &descriptor) {
  zx_paddr_t command_desc_base_paddr =
      (static_cast<zx_paddr_t>(descriptor.utp_command_descriptor_base_address_upper()) << 32) |
      descriptor.utp_command_descriptor_base_address();

  zx::result<zx_vaddr_t> command_desc_base_addr = mock_device_.MapDmaPaddr(command_desc_base_paddr);
  ZX_ASSERT_MSG(command_desc_base_addr.is_ok(), "Failed to map address.");

  CommandDescriptorData command_descriptor_data;
  command_descriptor_data.command_upiu_base_addr = command_desc_base_addr.value();
  command_descriptor_data.response_upiu_base_addr =
      command_desc_base_addr.value() + descriptor.response_upiu_offset() * sizeof(uint32_t);
  command_descriptor_data.response_upiu_length =
      descriptor.response_upiu_length() * sizeof(uint32_t);
  command_descriptor_data.prdt_base_addr =
      command_desc_base_addr.value() + descriptor.prdt_offset() * sizeof(uint32_t);
  command_descriptor_data.prdt_entry_count = descriptor.prdt_length() * sizeof(uint32_t);

  UpiuHeader *command_upiu_header =
      reinterpret_cast<UpiuHeader *>(command_descriptor_data.command_upiu_base_addr);

  UpiuHeader *response_upiu_header =
      reinterpret_cast<UpiuHeader *>(command_descriptor_data.response_upiu_base_addr);
  std::memcpy(response_upiu_header, command_upiu_header, sizeof(UpiuHeader));
  response_upiu_header->set_trans_code(command_upiu_header->trans_code() | (1 << 5));

  if (mock_device_.GetExceptionEventAlert()) {
    response_upiu_header->set_event_alert(true);
    mock_device_.SetExceptionEventAlert(false);
  }

  UpiuTransactionCodes opcode =
      static_cast<UpiuTransactionCodes>(command_upiu_header->trans_code());
  zx_status_t status = ZX_OK;
  if (auto it = handlers_.find(opcode); it != handlers_.end()) {
    status = (it->second)(mock_device_, command_descriptor_data);
  } else {
    status = ZX_ERR_NOT_SUPPORTED;
    FDF_LOG(ERROR, "UFS MOCK: transfer request opcode: 0x%x is not supported", opcode);
  }

  if (status == ZX_OK) {
    descriptor.set_overall_command_status(OverallCommandStatus::kSuccess);
  } else {
    FDF_LOG(ERROR, "UFS MOCK: Failed to handle transfer request: %s", zx_status_get_string(status));
    descriptor.set_overall_command_status(OverallCommandStatus::kInvalid);
  }

  if ((descriptor.overall_command_status() == OverallCommandStatus::kSuccess &&
       descriptor.interrupt()) ||
      descriptor.overall_command_status() != OverallCommandStatus::kSuccess) {
    InterruptStatusReg::Get()
        .ReadFrom(mock_device_.GetRegisters())
        .set_utp_transfer_request_completion_status(true)
        .WriteTo(mock_device_.GetRegisters());
    if (InterruptEnableReg::Get()
            .ReadFrom(mock_device_.GetRegisters())
            .utp_transfer_request_completion_enable()) {
      mock_device_.TriggerInterrupt();
    }
  }
}

zx_status_t TransferRequestProcessor::DefaultNopOutHandler(
    UfsMockDevice &mock_device, CommandDescriptorData command_descriptor_data) {
  NopInUpiuData *nop_in_upiu =
      reinterpret_cast<NopInUpiuData *>(command_descriptor_data.response_upiu_base_addr);
  nop_in_upiu->header.data_segment_length = 0;
  nop_in_upiu->header.flags = 0;
  nop_in_upiu->header.response = 0;
  return ZX_OK;
}

zx_status_t TransferRequestProcessor::DefaultQueryHandler(
    UfsMockDevice &mock_device, CommandDescriptorData command_descriptor_data) {
  QueryRequestUpiuData *request_upiu =
      reinterpret_cast<QueryRequestUpiuData *>(command_descriptor_data.command_upiu_base_addr);
  QueryResponseUpiuData *response_upiu =
      reinterpret_cast<QueryResponseUpiuData *>(command_descriptor_data.response_upiu_base_addr);

  response_upiu->opcode = request_upiu->opcode;
  response_upiu->idn = request_upiu->idn;
  response_upiu->index = request_upiu->index;
  response_upiu->selector = request_upiu->selector;

  zx_status_t status =
      mock_device.GetQueryRequestProcessor().HandleQueryRequest(*request_upiu, *response_upiu);
  response_upiu->header.data_segment_length = response_upiu->length;
  return status;
}

zx_status_t TransferRequestProcessor::DefaultCommandHandler(
    UfsMockDevice &mock_device, CommandDescriptorData command_descriptor_data) {
  CommandUpiuData *command_upiu =
      reinterpret_cast<CommandUpiuData *>(command_descriptor_data.command_upiu_base_addr);
  ResponseUpiuData *response_upiu =
      reinterpret_cast<ResponseUpiuData *>(command_descriptor_data.response_upiu_base_addr);
  cpp20::span<PhysicalRegionDescriptionTableEntry> prdt_upius(
      reinterpret_cast<PhysicalRegionDescriptionTableEntry *>(
          command_descriptor_data.prdt_base_addr),
      command_descriptor_data.prdt_entry_count);

  return mock_device.GetScsiCommandProcessor().HandleScsiCommand(*command_upiu, *response_upiu,
                                                                 prdt_upius);
}

}  // namespace ufs_mock_device
}  // namespace ufs
