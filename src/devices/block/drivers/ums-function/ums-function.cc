
// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/drivers/ums-function/ums-function.h"

#include <endian.h>
#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.endpoint/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.function/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.request/cpp/fidl.h>
#include <fuchsia/hardware/usb/function/cpp/banjo.h>
#include <lib/driver/compat/cpp/banjo_client.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/scsi/controller.h>
#include <lib/zx/result.h>
#include <lib/zx/vmar.h>
#include <stdint.h>
#include <string.h>
#include <threads.h>
#include <zircon/assert.h>
#include <zircon/process.h>
#include <zircon/status.h>
#include <zircon/types.h>

#include <algorithm>
#include <atomic>
#include <optional>
#include <vector>

#include <fbl/auto_lock.h>
#include <usb/request-fidl.h>
#include <usb/ums.h>

namespace ums {

namespace fendpoint = fuchsia_hardware_usb_endpoint;
namespace ffdf = fuchsia_driver_framework;
namespace frequest = fuchsia_hardware_usb_request;

static struct {
  usb_interface_descriptor_t intf;
  usb_endpoint_descriptor_t out_ep;
  usb_endpoint_descriptor_t in_ep;
} descriptors = {
    .intf =
        {
            .b_length = sizeof(usb_interface_descriptor_t),
            .b_descriptor_type = USB_DT_INTERFACE,
            //      .b_interface_number set later
            .b_alternate_setting = 0,
            .b_num_endpoints = 2,
            .b_interface_class = USB_CLASS_MSC,
            .b_interface_sub_class = USB_SUBCLASS_MSC_SCSI,
            .b_interface_protocol = USB_PROTOCOL_MSC_BULK_ONLY,
            .i_interface = 0,
        },
    .out_ep =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = USB_DT_ENDPOINT,
            //      .b_endpoint_address set later
            .bm_attributes = USB_ENDPOINT_BULK,
            .w_max_packet_size = htole16(UmsFunction::kBulkMaxPacket),
            .b_interval = 0,
        },
    .in_ep =
        {
            .b_length = sizeof(usb_endpoint_descriptor_t),
            .b_descriptor_type = USB_DT_ENDPOINT,
            //      .b_endpoint_address set later
            .bm_attributes = USB_ENDPOINT_BULK,
            .w_max_packet_size = htole16(UmsFunction::kBulkMaxPacket),
            .b_interval = 0,
        },
};

void UmsFunction::RequestQueueLocked(usb::FidlRequest* req) {
  const char* name = "unknown";
  bool* in_flight_ptr = nullptr;
  usb::EndpointClient<UmsFunction>* ep = nullptr;

  if (req == &cbw_req_.value()) {
    name = "cbw";
    in_flight_ptr = &cbw_in_flight_;
    ep = &out_ep_;
  } else if (req == &csw_req_.value()) {
    name = "csw";
    in_flight_ptr = &csw_in_flight_;
    ep = &in_ep_;
  } else if (req == &data_in_req_.value()) {
    name = "data";
    in_flight_ptr = &data_in_flight_;
    ep = &in_ep_;
  } else if (req == &data_out_req_.value()) {
    name = "data";
    in_flight_ptr = &data_in_flight_;
    ep = &out_ep_;
  }

  if (in_flight_ptr && *in_flight_ptr) {
    fdf::error("UmsFunction: Request {} (0x{:08x}) already in flight! Skipping re-queue.", name,
        reinterpret_cast<uintptr_t>(&req->request()));
    return;
  }
  if (in_flight_ptr) {
    *in_flight_ptr = true;
  }

  atomic_fetch_add(&pending_request_count_, 1);
  std::vector<frequest::Request> reqs;
  reqs.emplace_back(req->take_request());

  ZX_ASSERT(ep->client()->QueueRequests(std::move(reqs)).is_ok());
}

void UmsFunction::InEpCallback(std::vector<fendpoint::Completion> completion) {
  // CSW and in-data.
  ZX_ASSERT(completion.size() == 1);
  ZX_ASSERT(completion[0].transfer_size().has_value());
  ZX_ASSERT(completion[0].request().has_value());

  fbl::AutoLock lock(&mtx_);

  if (completion[0].request()->data()->at(0).buffer()->vmo_id().value() == csw_vmo_id_) {
    csw_req_.emplace(std::move(*completion[0].request()));
    csw_req_complete_.emplace(std::move(completion[0]));
    csw_in_flight_ = false;

    if (!csw_q_.empty()) {
      QueueCswLocked(csw_q_.front(), false);
      csw_q_.pop();
    }

  } else {
    data_in_req_.emplace(std::move(*completion[0].request()));
    data_req_complete_.emplace(std::move(completion[0]));
    data_in_flight_ = false;
  }

  condvar_.Signal();
}

void UmsFunction::OutEpCallback(std::vector<fendpoint::Completion> completion) {
  // CBW and out-data.
  ZX_ASSERT(completion.size() == 1);
  ZX_ASSERT(completion[0].transfer_size().has_value());
  ZX_ASSERT(completion[0].request().has_value());

  fbl::AutoLock lock(&mtx_);

  if (completion[0].request()->data()->at(0).buffer()->vmo_id().value() == cbw_vmo_id_) {
    cbw_req_.emplace(std::move(*completion[0].request()));
    cbw_req_complete_.emplace(std::move(completion[0]));
    cbw_in_flight_ = false;
  } else {
    data_out_req_.emplace(std::move(*completion[0].request()));
    data_req_complete_.emplace(std::move(completion[0]));
    data_in_flight_ = false;
  }

  condvar_.Signal();
}

void UmsFunction::QueueDataLocked(usb::FidlRequest* req) {
  data_length_ += (*req)->data()->at(0).size().value();
  RequestQueueLocked(req);
}

void UmsFunction::QueueCswLocked(uint8_t status, bool also_cbw) {
  if (also_cbw) {
    // first queue next cbw so it is ready to go
    RequestQueueLocked(&cbw_req_.value());
  }

  if (csw_in_flight_) {
    csw_q_.push(status);
    return;
  }

  ums_csw_t* csw;
  std::optional<zx_vaddr_t> vaddr = in_ep_.GetMappedAddr(csw_req_->request(), 0);
  if (!vaddr.has_value()) {
    fdf::error("QueueCsw: GetMappedAddr failed");
    return;
  }
  csw = reinterpret_cast<ums_csw_t*>(*vaddr);

  csw->dCSWSignature = htole32(CSW_SIGNATURE);
  csw->dCSWTag = current_cbw_.dCBWTag;
  csw->dCSWDataResidue = htole32(le32toh(current_cbw_.dCBWDataTransferLength) - data_length_);
  csw->bmCSWStatus = status;

  std::vector<size_t> actual = csw_req_->CopyTo(0, csw, sizeof(ums_csw_t), in_ep_.GetMapped());
  ZX_ASSERT(actual.size() == 1);
  ZX_ASSERT(actual[0] == sizeof(ums_csw_t));
  (*csw_req_)->data()->at(0).size(sizeof(ums_csw_t));

  csw_req_->CacheFlush(in_ep_.GetMapped());
  RequestQueueLocked(&csw_req_.value());
}

void UmsFunction::ContinueTransferLocked() {
  usb::FidlRequest* req = IsInData() ? &data_in_req_.value() : &data_out_req_.value();

  size_t length = std::min(data_remaining_, kDataReqSize);
  (*req)->data()->at(0).size(length);

  if (data_state_ == DATA_STATE_READ) {
    std::vector<size_t> result =
        req->CopyTo(0, static_cast<char*>(storage_) + data_offset_, length, in_ep_.GetMapped());
    ZX_ASSERT(result.size() == 1);
    ZX_ASSERT(result[0] == length);
    req->CacheFlush(in_ep_.GetMapped());
    QueueDataLocked(req);
  } else if (data_state_ == DATA_STATE_WRITE || data_state_ == DATA_STATE_UNMAP) {
    QueueDataLocked(req);
  } else {
    fdf::error("ContinueTransfer: bad data state {}", data_state_);
  }
}

void UmsFunction::StartTransferLocked(DataState state, uint32_t transfer_bytes, uint64_t lba) {
  zx_off_t offset = lba * kBlockSize;
  if (offset + transfer_bytes > kStorageSize) {
    fdf::error("StartTransfer: transfer out of range state: {}, lba: {} transfer_bytes: {}",
               static_cast<uint32_t>(state), lba, transfer_bytes);
    QueueCswLocked(CSW_FAILED);
    return;
  }

  data_state_ = state;
  data_offset_ = offset;  // Not applicable for the DATA_STATE_UNMAP case.
  data_remaining_ = transfer_bytes;

  ContinueTransferLocked();
}

zx::result<> UmsFunction::CancelAllLocked(usb::EndpointClient<UmsFunction>& ep) {
  // For convenience, wrap the various FIDL error cases with a zx::result.
  fidl::WireResult result = ep.client().wire_sync()->CancelAll();
  if (!result.ok()) {
    return zx::error(ZX_ERR_INTERNAL);
  }
  if (result->is_error()) {
    return result->take_error();
  }
  return zx::ok();
}

void UmsFunction::HandleInquiryLocked(ums_cbw_t* cbw) {
  scsi::InquiryCDB cmd;
  memcpy(&cmd, cbw->CBWCB, sizeof(cmd));

  usb::FidlRequest* req = &data_in_req_.value();

  std::optional<zx_vaddr_t> vaddr = in_ep_.GetMappedAddr(req->request(), 0);
  if (!vaddr.has_value()) {
    fdf::error("{} GetMappedAddr failed", __func__);
    QueueCswLocked(CSW_FAILED);
    return;
  }
  auto* data = reinterpret_cast<void*>(*vaddr);

  uint32_t length = std::min(static_cast<uint32_t>(UMS_INQUIRY_TRANSFER_LENGTH),
                             le32toh(cbw->dCBWDataTransferLength));
  memset(data, 0, length);
  (*req)->data()->at(0).size(length);

  // fill in inquiry result
  if (!cmd.reserved_and_evpd) {
    scsi::InquiryData* inquiry_data = reinterpret_cast<scsi::InquiryData*>(data);
    inquiry_data->peripheral_device_type = 0;  // Peripheral Device Type: Direct access block device
    inquiry_data->removable = 0x80;            // Removable
    inquiry_data->version = 6;                 // Version SPC-4
    inquiry_data->response_data_format_and_control = 0x12;  // Response Data Format
    memcpy(inquiry_data->t10_vendor_id, "Google  ", 8);
    memcpy(inquiry_data->product_id, "Zircon UMS      ", 16);
    memcpy(inquiry_data->product_revision, "1.00", 4);
  } else {
    if (cmd.page_code == scsi::InquiryCDB::kPageListVpdPageCode) {
      auto vpd_page_list = reinterpret_cast<scsi::VPDPageList*>(data);
      vpd_page_list->peripheral_qualifier_device_type = 0;
      vpd_page_list->page_code = 0x00;
      vpd_page_list->page_length = 2;
      vpd_page_list->pages[0] = scsi::InquiryCDB::kBlockLimitsVpdPageCode;
      vpd_page_list->pages[1] = scsi::InquiryCDB::kLogicalBlockProvisioningVpdPageCode;
    } else if (cmd.page_code == scsi::InquiryCDB::kBlockLimitsVpdPageCode) {
      auto block_limits = reinterpret_cast<scsi::VPDBlockLimits*>(data);
      block_limits->peripheral_qualifier_device_type = 0;
      block_limits->page_code = scsi::InquiryCDB::kBlockLimitsVpdPageCode;
      block_limits->maximum_unmap_lba_count = htobe32(UINT32_MAX);
    } else if (cmd.page_code == scsi::InquiryCDB::kLogicalBlockProvisioningVpdPageCode) {
      auto provisioning = reinterpret_cast<scsi::VPDLogicalBlockProvisioning*>(data);
      provisioning->peripheral_qualifier_device_type = 0;
      provisioning->page_code = scsi::InquiryCDB::kLogicalBlockProvisioningVpdPageCode;
      provisioning->set_lbpu(true);
      provisioning->set_provisioning_type(0x02);  // The logical unit is thin provisioned
    } else {
      fdf::error("Unsupported Inquiry page code=0x{:x}", cmd.page_code);
      return;
    }
  }

  req->CacheFlush(in_ep_.GetMapped());
  QueueDataLocked(req);
}

void UmsFunction::HandleTestUnitReadyLocked(ums_cbw_t* cbw) {
  // no data phase here. Just return status OK
  QueueCswLocked(CSW_SUCCESS);
}

void UmsFunction::HandleRequestSenseLocked(ums_cbw_t* cbw) {
  usb::FidlRequest* req = &data_in_req_.value();

  std::optional<zx_vaddr_t> vaddr = in_ep_.GetMappedAddr(req->request(), 0);
  if (!vaddr.has_value()) {
    fdf::error("{} GetMappedAddr failed", __func__);
    QueueCswLocked(CSW_FAILED);
    return;
  }
  auto* data = reinterpret_cast<scsi::SenseDataHeader*>(*vaddr);

  uint32_t length = std::min(static_cast<uint32_t>(UMS_REQUEST_SENSE_TRANSFER_LENGTH),
                             le32toh(cbw->dCBWDataTransferLength));
  memset(data, 0, length);
  (*req)->data()->at(0).size(length);

  data->response_code =
      static_cast<uint8_t>(scsi::SenseDataResponseCodes::kFixedCurrentInformation);
  data->additional_sense_code = 0x20;
  data->additional_sense_length = 10;

  req->CacheFlush(in_ep_.GetMapped());
  QueueDataLocked(req);
}

void UmsFunction::HandleReadCapacity10Locked(ums_cbw_t* cbw) {
  usb::FidlRequest* req = &data_in_req_.value();

  std::optional<zx_vaddr_t> vaddr = in_ep_.GetMappedAddr(req->request(), 0);
  if (!vaddr.has_value()) {
    fdf::error("{} GetMappedAddr failed", __func__);
    QueueCswLocked(CSW_FAILED);
    return;
  }
  auto* data = reinterpret_cast<scsi::ReadCapacity10ParameterData*>(*vaddr);

  uint64_t lba = kBlockCount - 1;
  if (lba > UINT32_MAX) {
    data->returned_logical_block_address = htobe32(UINT32_MAX);
  } else {
    data->returned_logical_block_address = htobe32(lba);
  }
  data->block_length_in_bytes = htobe32(kBlockSize);

  uint32_t length =
      std::min(static_cast<uint32_t>(sizeof(*data)), le32toh(cbw->dCBWDataTransferLength));
  (*req)->data()->at(0).size(length);
  req->CacheFlush(in_ep_.GetMapped());
  QueueDataLocked(req);
}

void UmsFunction::HandleReadCapacity16Locked(ums_cbw_t* cbw) {
  usb::FidlRequest* req = &data_in_req_.value();

  std::optional<zx_vaddr_t> vaddr = in_ep_.GetMappedAddr(req->request(), 0);
  if (!vaddr.has_value()) {
    fdf::error("{} GetMappedAddr failed", __func__);
    QueueCswLocked(CSW_FAILED);
    return;
  }
  auto* data = reinterpret_cast<scsi::ReadCapacity16ParameterData*>(*vaddr);

  memset(data, 0, sizeof(*data));
  data->returned_logical_block_address = htobe64(kBlockCount - 1);
  data->block_length_in_bytes = htobe32(kBlockSize);

  uint32_t length =
      std::min(static_cast<uint32_t>(sizeof(*data)), le32toh(cbw->dCBWDataTransferLength));
  (*req)->data()->at(0).size(length);

  req->CacheFlush(in_ep_.GetMapped());
  QueueDataLocked(req);
}

void UmsFunction::HandleModeSense6Locked(ums_cbw_t* cbw) {
  usb::FidlRequest* req = &data_in_req_.value();

  std::optional<zx_vaddr_t> vaddr = in_ep_.GetMappedAddr(req->request(), 0);
  if (!vaddr.has_value()) {
    fdf::error("{} GetMappedAddr failed", __func__);
    QueueCswLocked(CSW_FAILED);
    return;
  }
  auto* data = reinterpret_cast<scsi::Mode6ParameterHeader*>(*vaddr);

  if (le32toh(cbw->dCBWDataTransferLength) < sizeof(scsi::Mode6ParameterHeader)) {
    fdf::error("HandleModeSense6: buffer too small");
    QueueCswLocked(CSW_FAILED);
    return;
  }

  auto* cdb = reinterpret_cast<scsi::ModeSense6CDB*>(cbw->CBWCB);
  uint8_t page_code = static_cast<uint8_t>(cdb->page_code());
  uint32_t allocation_length = le32toh(cbw->dCBWDataTransferLength);
  memset(data, 0, sizeof(*data));

  (*req)->data()->at(0).size(sizeof(scsi::Mode6ParameterHeader));

  if ((page_code == static_cast<uint8_t>(scsi::PageCode::kCachingPageCode) ||
       page_code == static_cast<uint8_t>(scsi::PageCode::kAllPageCode)) &&
      allocation_length >= sizeof(scsi::Mode6ParameterHeader) + sizeof(scsi::CachingModePage)) {
    auto caching_page = reinterpret_cast<scsi::CachingModePage*>(
        reinterpret_cast<uint8_t*>(data) + sizeof(scsi::Mode6ParameterHeader));
    memset(caching_page, 0, sizeof(*caching_page));
    caching_page->ps_spf_and_page_code = static_cast<uint8_t>(scsi::PageCode::kCachingPageCode);
    caching_page->page_length = sizeof(*caching_page) - 2;
    caching_page->set_write_cache_enabled(true);
    (*req)->data()->at(0).size(sizeof(scsi::Mode6ParameterHeader) + sizeof(scsi::CachingModePage));
  }
  data->mode_data_length = static_cast<uint8_t>((*req)->data()->at(0).size().value() - 1);

  req->CacheFlush(in_ep_.GetMapped());
  QueueDataLocked(req);
}

void UmsFunction::HandleRead10Locked(ums_cbw_t* cbw) {
  scsi::Read10CDB* command = reinterpret_cast<scsi::Read10CDB*>(cbw->CBWCB);
  uint64_t lba = be32toh(command->logical_block_address);
  uint32_t blocks = be16toh(command->transfer_length);
  StartTransferLocked(DATA_STATE_READ, blocks * kBlockSize, lba);
}

void UmsFunction::HandleRead12Locked(ums_cbw_t* cbw) {
  scsi::Read12CDB* command = reinterpret_cast<scsi::Read12CDB*>(cbw->CBWCB);
  uint64_t lba = be32toh(command->logical_block_address);
  uint32_t blocks = be32toh(command->transfer_length);
  StartTransferLocked(DATA_STATE_READ, blocks * kBlockSize, lba);
}

void UmsFunction::HandleRead16Locked(ums_cbw_t* cbw) {
  scsi::Read16CDB* command = reinterpret_cast<scsi::Read16CDB*>(cbw->CBWCB);
  uint64_t lba = be64toh(command->logical_block_address);
  uint32_t blocks = be32toh(command->transfer_length);
  StartTransferLocked(DATA_STATE_READ, blocks * kBlockSize, lba);
}

void UmsFunction::HandleWrite10Locked(ums_cbw_t* cbw) {
  scsi::Write10CDB* command = reinterpret_cast<scsi::Write10CDB*>(cbw->CBWCB);
  uint64_t lba = be32toh(command->logical_block_address);
  uint32_t blocks = be16toh(command->transfer_length);
  StartTransferLocked(DATA_STATE_WRITE, blocks * kBlockSize, lba);
}

void UmsFunction::HandleWrite12Locked(ums_cbw_t* cbw) {
  scsi::Write12CDB* command = reinterpret_cast<scsi::Write12CDB*>(cbw->CBWCB);
  uint64_t lba = be32toh(command->logical_block_address);
  uint32_t blocks = be32toh(command->transfer_length);
  StartTransferLocked(DATA_STATE_WRITE, blocks * kBlockSize, lba);
}

void UmsFunction::HandleWrite16Locked(ums_cbw_t* cbw) {
  scsi::Write16CDB* command = reinterpret_cast<scsi::Write16CDB*>(cbw->CBWCB);
  uint64_t lba = be64toh(command->logical_block_address);
  uint32_t blocks = be32toh(command->transfer_length);
  StartTransferLocked(DATA_STATE_WRITE, blocks * kBlockSize, lba);
}

void UmsFunction::HandleUnmapLocked(ums_cbw_t* cbw) {
  scsi::UnmapCDB* command = reinterpret_cast<scsi::UnmapCDB*>(cbw->CBWCB);
  uint16_t unmap_data_length =
      sizeof(scsi::UnmapParameterListHeader) + sizeof(scsi::UnmapBlockDescriptor);
  if (betoh16(command->parameter_list_length) != unmap_data_length) {
    fdf::error("Command parameter list length is invalid: {} != {}",
               betoh16(command->parameter_list_length), unmap_data_length);
    QueueCswLocked(CSW_FAILED);
    return;
  }

  StartTransferLocked(DATA_STATE_UNMAP, unmap_data_length);
}

void UmsFunction::HandleCbwLocked(ums_cbw_t* cbw) {
  if (le32toh(cbw->dCBWSignature) != CBW_SIGNATURE) {
    fdf::error("HandleCbw: bad dCBWSignature 0x{:x}", le32toh(cbw->dCBWSignature));
    return;
  }

  // reset data length and state for computing residue
  data_length_ = 0;
  data_state_ = DATA_STATE_NONE;

  // all SCSI commands have opcode in the first byte.
  auto opcode = static_cast<scsi::Opcode>(cbw->CBWCB[0]);
  switch (opcode) {
    case scsi::Opcode::INQUIRY:
      HandleInquiryLocked(cbw);
      break;
    case scsi::Opcode::TEST_UNIT_READY:
      HandleTestUnitReadyLocked(cbw);
      break;
    case scsi::Opcode::REQUEST_SENSE:
      HandleRequestSenseLocked(cbw);
      break;
    case scsi::Opcode::READ_CAPACITY_10:
      HandleReadCapacity10Locked(cbw);
      break;
    case scsi::Opcode::READ_CAPACITY_16:
      HandleReadCapacity16Locked(cbw);
      break;
    case scsi::Opcode::MODE_SENSE_6:
      HandleModeSense6Locked(cbw);
      break;
    case scsi::Opcode::READ_10:
      HandleRead10Locked(cbw);
      break;
    case scsi::Opcode::READ_12:
      HandleRead12Locked(cbw);
      break;
    case scsi::Opcode::READ_16:
      HandleRead16Locked(cbw);
      break;
    case scsi::Opcode::WRITE_10:
      HandleWrite10Locked(cbw);
      break;
    case scsi::Opcode::WRITE_12:
      HandleWrite12Locked(cbw);
      break;
    case scsi::Opcode::WRITE_16:
      HandleWrite16Locked(cbw);
      break;
    case scsi::Opcode::SYNCHRONIZE_CACHE_10:
      // TODO: This is presently untestable.
      // Implement this once we have a means of testing this.
      QueueCswLocked(CSW_SUCCESS);
      break;
    case scsi::Opcode::UNMAP:
      HandleUnmapLocked(cbw);
      break;
    default:
      fdf::error("HandleCbw: unsupported opcode {:02X}h", cbw->CBWCB[0]);
      if (cbw->dCBWDataTransferLength) {
        // queue zero length packet to satisfy data phase
        usb::FidlRequest* req = &data_in_req_.value();
        (*req)->data()->at(0).size(0);
        data_state_ = DATA_STATE_FAILED;
        req->CacheFlush(in_ep_.GetMapped());
        QueueDataLocked(req);
      }
      QueueCswLocked(CSW_FAILED);
      break;
  }
}

void UmsFunction::CbwCompleteLocked() {
  fendpoint::Completion completion = std::move(*cbw_req_complete_);
  cbw_req_complete_.reset();

  bool online = configured_ && active_.load();
  if (!online) {
    fdf::error("UmsFunction: Not online, dropping CBW");
    return;
  }

  if (*completion.status() != ZX_OK) {
    fdf::error("UmsFunction: CBW receive failed: {}", zx_status_get_string(*completion.status()));
    return;
  }

  if (*completion.transfer_size() != sizeof(ums_cbw_t)) {
    fdf::error("Malformed CBW, size {} != {}", *completion.transfer_size(), sizeof(ums_cbw_t));
    return;
  }

  usb::FidlRequest* req = &cbw_req_.value();

  ums_cbw_t* cbw = &current_cbw_;
  memset(cbw, 0, sizeof(*cbw));
  [[maybe_unused]] zx_status_t status = req->CacheFlushInvalidate(out_ep_.GetMapped());
  [[maybe_unused]] std::vector<size_t> result =
      req->CopyFrom(0, cbw, sizeof(*cbw), out_ep_.GetMapped());
  HandleCbwLocked(cbw);
}

void UmsFunction::DataCompleteLocked() {
  bool online = configured_ && active_.load();
  if (!online) {
    return;
  }

  fendpoint::Completion completion = std::move(*data_req_complete_);
  data_req_complete_.reset();
  size_t actual = *completion.transfer_size();

  if (*completion.status() != ZX_OK) {
    fdf::error("UmsFunction: Data transfer failed: {}", zx_status_get_string(*completion.status()));
    data_state_ = DATA_STATE_NONE;
    QueueCswLocked(CSW_FAILED);
    return;
  }

  usb::FidlRequest* req = IsInData() ? &data_in_req_.value() : &data_out_req_.value();

  if (data_state_ == DATA_STATE_WRITE) {
    req->CacheFlushInvalidate(out_ep_.GetMapped());
    std::vector<size_t> result =
        req->CopyFrom(0, static_cast<char*>(storage_) + data_offset_, actual, out_ep_.GetMapped());
    ZX_ASSERT(result.size() == 1);
    ZX_ASSERT(result[0] == *completion.transfer_size());
  } else if (data_state_ == DATA_STATE_UNMAP) {
    // Overwrite the unmapped blocks with zeros.
    req->CacheFlushInvalidate(out_ep_.GetMapped());

    std::optional<zx_vaddr_t> vaddr = out_ep_.GetMappedAddr(req->request(), 0);
    if (!vaddr.has_value()) {
      fdf::error("DataComplete: GetMappedAddr");
      QueueCswLocked(CSW_FAILED);
      return;
    }
    auto* data = reinterpret_cast<uint8_t*>(*vaddr);

    scsi::UnmapBlockDescriptor* block_descriptor = reinterpret_cast<scsi::UnmapBlockDescriptor*>(
        data + sizeof(scsi::UnmapParameterListHeader));
    size_t block_count = betoh32(block_descriptor->blocks);
    uint64_t start_lba = betoh64(block_descriptor->logical_block_address);
    memset(static_cast<char*>(storage_) + (start_lba * kBlockSize), 0, block_count * kBlockSize);
  } else if (data_state_ == DATA_STATE_FAILED) {
    data_state_ = DATA_STATE_NONE;
    QueueCswLocked(CSW_FAILED);
    return;
  } else {
    data_state_ = DATA_STATE_NONE;
    QueueCswLocked(CSW_SUCCESS);
    return;
  }

  data_offset_ += actual;
  if (data_remaining_ > actual) {
    data_remaining_ -= actual;
  } else {
    data_remaining_ = 0;
  }

  if (data_remaining_ > 0) {
    ContinueTransferLocked();
  } else {
    data_state_ = DATA_STATE_NONE;
    QueueCswLocked(CSW_SUCCESS);
  }
}

void UmsFunction::CswCompleteLocked() {
  zx_status_t status = *csw_req_complete_->status();
  csw_req_complete_.reset();
  if (status != ZX_OK) {
    fdf::error("UmsFunction: CSW send failed: {}", zx_status_get_string(status));
  }
}

size_t UmsFunction::UsbFunctionInterfaceGetDescriptorsSize() { return sizeof(descriptors); }

void UmsFunction::UsbFunctionInterfaceGetDescriptors(uint8_t* out_descriptors_buffer,
                                                     size_t descriptors_size,
                                                     size_t* out_descriptors_actual) {
  const size_t length = std::min(sizeof(descriptors), descriptors_size);
  memcpy(out_descriptors_buffer, &descriptors, length);
  *out_descriptors_actual = length;
}

zx_status_t UmsFunction::UsbFunctionInterfaceControl(const usb_setup_t* setup,
                                                     const uint8_t* write_buffer, size_t write_size,
                                                     uint8_t* out_read_buffer, size_t read_size,
                                                     size_t* out_read_actual) {
  if (setup->bm_request_type == (USB_DIR_IN | USB_TYPE_CLASS | USB_RECIP_INTERFACE) &&
      setup->b_request == USB_REQ_GET_MAX_LUN && setup->w_value == 0 && setup->w_index == 0 &&
      setup->w_length >= sizeof(uint8_t)) {
    *((uint8_t*)out_read_buffer) = 0;
    *out_read_actual = sizeof(uint8_t);
    return ZX_OK;
  }

  if (setup->bm_request_type == (USB_DIR_OUT | USB_TYPE_CLASS | USB_RECIP_INTERFACE) &&
      setup->b_request == USB_REQ_RESET && setup->w_value == 0 && setup->w_index == 0 &&
      setup->w_length == 0) {
    fbl::AutoLock l(&mtx_);

    // Cancel all pending requests
    zx::result result = CancelAllLocked(in_ep_);
    if (result.is_error()) {
      fdf::error("Error canceling existing in-type requests: {}", result);
    }

    result = CancelAllLocked(out_ep_);
    if (result.is_error()) {
      fdf::error("Error canceling existing out-type requests: {}", result);
    }

    cbw_in_flight_ = false;
    data_in_flight_ = false;
    csw_in_flight_ = false;
    return ZX_OK;
  }

  return ZX_ERR_NOT_SUPPORTED;
}

zx_status_t UmsFunction::UsbFunctionInterfaceSetConfigured(bool configured, usb_speed_t speed) {
  zx_status_t status = ZX_OK;

  configured_ = configured;

  // TODO(voydanoff) fullspeed and superspeed support
  if (configured) {
    if ((status = function_.ConfigEp(&descriptors.out_ep, NULL)) != ZX_OK ||
        (status = function_.ConfigEp(&descriptors.in_ep, NULL)) != ZX_OK) {
      fdf::error("SetConfigured: ConfigEp failed");
    }
  } else {
    if ((status = function_.DisableEp(bulk_out_addr_)) != ZX_OK ||
        (status = function_.DisableEp(bulk_in_addr_)) != ZX_OK) {
      fdf::error("SetConfigured: DisableEp failed");
    }

    // Reset state flags on disconnect.
    fbl::AutoLock l(&mtx_);
    cbw_in_flight_ = false;
    data_in_flight_ = false;
    csw_in_flight_ = false;
  }

  if (configured && status == ZX_OK) {
    // queue first read on OUT endpoint
    fbl::AutoLock lock(&mtx_);
    RequestQueueLocked(&cbw_req_.value());
  }
  return status;
}

zx_status_t UmsFunction::UsbFunctionInterfaceSetInterface(uint8_t interface, uint8_t alt_setting) {
  return ZX_ERR_NOT_SUPPORTED;
}

void UmsFunction::PrepareStop(fdf::PrepareStopCompleter completer) {
  {
    fbl::AutoLock l(&mtx_);
    zx::result cancel = CancelAllLocked(in_ep_);
    if (cancel.is_error()) {
      fdf::error("Error canceling existing in-type requests: {}", cancel);
    }

    cancel = CancelAllLocked(out_ep_);
    if (cancel.is_error()) {
      fdf::error("Error canceling existing out-type requests: {}", cancel);
    }

    active_ = false;
    condvar_.Signal();
  }

  int retval;
  thrd_join(thread_, &retval);

  if (storage_) {
    zx_vmar_unmap(zx_vmar_root_self(), (uintptr_t)storage_, kStorageSize);
  }

  dispatcher_.ShutdownAsync();

  completer(zx::ok());
}

zx::vmo UmsFunction::vmo_ = zx::vmo();

int UmsFunction::WorkerLoop() {
  while (active_) {
    fbl::AutoLock l(&mtx_);
    // Wait until a request is complete (signaled in CompletionCallback),
    // unless the driver is shutting down (signaled in DdkUnbind).
    if (!(cbw_req_complete_.has_value() || csw_req_complete_.has_value() ||
          data_req_complete_.has_value() || IsReadyForShutdown())) {
      condvar_.Wait(&mtx_);
    }

    // Exit the thread if the driver is inactive and all pending requests have been processed.
    if (IsReadyForShutdown()) {
      return 0;
    }

    if (cbw_req_complete_.has_value()) {
      atomic_fetch_add(&pending_request_count_, -1);
      CbwCompleteLocked();
    }
    if (csw_req_complete_.has_value()) {
      atomic_fetch_add(&pending_request_count_, -1);
      CswCompleteLocked();
    }
    if (data_req_complete_.has_value()) {
      atomic_fetch_add(&pending_request_count_, -1);
      DataCompleteLocked();
    }
  }
  return 0;
}

zx::result<> UmsFunction::Start() {
  zx::result function = compat::ConnectBanjo<ddk::UsbFunctionProtocolClient>(incoming());
  if (function.is_error()) {
    return function.take_error();
  }
  function_ = *function;

  zx_status_t status = Init();
  if (status != ZX_OK) {
    return zx::error(status);
  }

  ffdf::DevfsAddArgs devfs_args{};
  std::vector<ffdf::NodeProperty> props{};
  std::vector<ffdf::Offer> offers{};

  zx::result start = AddChild(name(), devfs_args, props, offers);
  if (start.is_error()) {
    fdf::error("AddChild(): {}", start);
    return start.take_error();
  }

  return zx::ok();
}

zx_status_t UmsFunction::Init() {
  data_state_ = DATA_STATE_NONE;
  active_ = true;
  pending_request_count_ = 0;

  zx_status_t status = ZX_OK;

  status = function_.AllocInterface(&descriptors.intf.b_interface_number);
  if (status != ZX_OK) {
    fdf::error("Init: AllocInterface failed");
    return status;
  }
  status = function_.AllocEp(USB_DIR_OUT, &bulk_out_addr_);
  if (status != ZX_OK) {
    fdf::error("Init: AllocEp(USB_DIR_OUT, ...) failed");
    return status;
  }
  status = function_.AllocEp(USB_DIR_IN, &bulk_in_addr_);
  if (status != ZX_OK) {
    fdf::error("Init: AllocEp(USB_DIR_IN, ...) failed");
    return status;
  }
  descriptors.out_ep.b_endpoint_address = bulk_out_addr_;
  descriptors.in_ep.b_endpoint_address = bulk_in_addr_;

  zx::result func =
      incoming()->Connect<fuchsia_hardware_usb_function::UsbFunctionService::Device>();
  if (func.is_error()) {
    fdf::error("Could not connect to UsbFunction service: {}", func);
    return func.status_value();
  }

  auto dispatcher = fdf::SynchronizedDispatcher::Create(
      fdf::SynchronizedDispatcher::Options::kAllowSyncCalls, "ums-fidl-dispatcher",
      [](fdf_dispatcher_t*) {}, "");
  if (dispatcher.is_error()) {
    fdf::error("Failed to create dispatcher: {}", dispatcher.status_string());
    return dispatcher.error_value();
  }
  dispatcher_ = std::move(*dispatcher);

  status = in_ep_.Init(bulk_in_addr_, func.value(), dispatcher_.async_dispatcher());
  if (status != ZX_OK) {
    fdf::error("Failed to init IN endpoint: {}", zx_status_get_string(status));
    return status;
  }

  status = out_ep_.Init(bulk_out_addr_, func.value(), dispatcher_.async_dispatcher());
  if (status != ZX_OK) {
    fdf::error("Failed to init OUT endpoint: {}", zx_status_get_string(status));
    return status;
  }

  size_t actual = out_ep_.AddRequests(1, kBulkMaxPacket, frequest::Buffer::Tag::kVmoId);
  if (actual != 1) {
    fdf::error("Failed to allocate CBW request");
    return ZX_ERR_INTERNAL;
  }
  cbw_req_.emplace(*out_ep_.GetRequest());
  cbw_vmo_id_ = (*cbw_req_)->data()->at(0).buffer()->vmo_id().value();

  actual = in_ep_.AddRequests(1, kDataReqSize, frequest::Buffer::Tag::kVmoId);
  if (actual != 1) {
    fdf::error("Failed to allocate in-data request");
    return ZX_ERR_INTERNAL;
  }
  data_in_req_.emplace(*in_ep_.GetRequest());

  actual = out_ep_.AddRequests(1, kDataReqSize, frequest::Buffer::Tag::kVmoId);
  if (actual != 1) {
    fdf::error("Failed to allocate out-data request");
    return ZX_ERR_INTERNAL;
  }
  data_out_req_.emplace(*out_ep_.GetRequest());

  actual = in_ep_.AddRequests(1, kBulkMaxPacket, frequest::Buffer::Tag::kVmoId);
  if (actual != 1) {
    fdf::error("Failed to allocate CSW request");
    return ZX_ERR_INTERNAL;
  }
  csw_req_.emplace(*in_ep_.GetRequest());
  csw_vmo_id_ = (*csw_req_)->data()->at(0).buffer()->vmo_id().value();

  // create and map a VMO
  if (!vmo_.is_valid()) {
    status = vmo_.create(kStorageSize, 0, &vmo_);
    if (status != ZX_OK) {
      return status;
    }
  }
  status = zx::vmar::root_self()->map(ZX_VM_PERM_READ | ZX_VM_PERM_WRITE, 0, vmo_, 0, kStorageSize,
                                      (zx_vaddr_t*)&storage_);
  if (status != ZX_OK) {
    return status;
  }

  function_.SetInterface(this, &usb_function_interface_protocol_ops_);
  thrd_create_with_name(
      &thread_, [](void* ctx) { return reinterpret_cast<UmsFunction*>(ctx)->WorkerLoop(); }, this,
      "ums_worker");
  return ZX_OK;
}

}  // namespace ums

// clang-format off
FUCHSIA_DRIVER_EXPORT(ums::UmsFunction);
