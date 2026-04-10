
// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/block/drivers/ums-function/ums-function.h"

#include <assert.h>
#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fuchsia/hardware/usb/function/cpp/banjo.h>
#include <lib/driver/compat/cpp/banjo_client.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/scsi/controller.h>
#include <lib/zx/vmar.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <zircon/process.h>
#include <zircon/syscalls.h>

#include <vector>

#include <fbl/alloc_checker.h>
#include <fbl/auto_lock.h>
#include <usb/peripheral.h>
#include <usb/request-cpp.h>
#include <usb/ums.h>
#include <usb/usb-request.h>

namespace ums {

namespace ffdf = fuchsia_driver_framework;

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

void UmsFunction::RequestQueue(usb::Request<>* req,
                               const usb_request_complete_callback_t* completion) {
  const char* name = "unknown";
  bool* in_flight_ptr = nullptr;
  if (req == &cbw_req_.value()) {
    name = "cbw";
    in_flight_ptr = &cbw_in_flight_;
  } else if (req == &data_req_.value()) {
    name = "data";
    in_flight_ptr = &data_in_flight_;
  } else if (req == &csw_req_.value()) {
    name = "csw";
    in_flight_ptr = &csw_in_flight_;
  }

  {
    fbl::AutoLock l(&mtx_);
    if (in_flight_ptr && *in_flight_ptr) {
      fdf::error("UmsFunction: Request {} ({}) already in flight! Skipping re-queue.", name,
                 static_cast<void*>(req->request()));
      return;
    }
    if (in_flight_ptr) {
      *in_flight_ptr = true;
    }
  }

  atomic_fetch_add(&pending_request_count_, 1);
  function_.RequestQueue(req->request(), completion);
}

void UmsFunction::CompletionCallback(void* ctx, usb_request_t* req) {
  auto ums = reinterpret_cast<UmsFunction*>(ctx);
  ums->mtx_.Acquire();
  if (req == ums->cbw_req_->request()) {
    ums->cbw_req_complete_ = true;
    ums->cbw_in_flight_ = false;
  } else {
    if (req == ums->data_req_->request()) {
      ums->data_req_complete_ = true;
      ums->data_in_flight_ = false;
    } else {
      ums->csw_req_complete_ = true;
      ums->csw_in_flight_ = false;
    }
  }
  ums->condvar_.Signal();
  ums->mtx_.Release();
}

void UmsFunction::QueueData(usb::Request<>* req) {
  data_length_ += req->request()->header.length;
  req->request()->header.ep_address =
      current_cbw_.bmCBWFlags & USB_DIR_IN ? bulk_in_addr_ : bulk_out_addr_;
  RequestQueue(req, &request_complete_);
}

void UmsFunction::QueueCsw(uint8_t status) {
  // first queue next cbw so it is ready to go
  RequestQueue(&cbw_req_.value(), &request_complete_);

  usb::Request<>* req = &csw_req_.value();
  ums_csw_t* csw;
  zx_status_t mmap_status = req->Mmap(reinterpret_cast<void**>(&csw));
  if (mmap_status != ZX_OK) {
    fdf::error("QueueCsw: Mmap failed: {}", zx_status_get_string(mmap_status));
    return;
  }

  csw->dCSWSignature = htole32(CSW_SIGNATURE);
  csw->dCSWTag = current_cbw_.dCBWTag;
  csw->dCSWDataResidue = htole32(le32toh(current_cbw_.dCBWDataTransferLength) - data_length_);
  csw->bmCSWStatus = status;

  req->request()->header.length = sizeof(ums_csw_t);
  RequestQueue(&csw_req_.value(), &request_complete_);
}

void UmsFunction::ContinueTransfer() {
  usb::Request<>* req = &data_req_.value();

  size_t length = std::min(data_remaining_, kDataReqSize);
  req->request()->header.length = length;

  if (data_state_ == DATA_STATE_READ) {
    size_t result = req->CopyTo(static_cast<char*>(storage_) + data_offset_, length, 0);
    ZX_ASSERT(result == length);
    QueueData(req);
  } else if (data_state_ == DATA_STATE_WRITE || data_state_ == DATA_STATE_UNMAP) {
    QueueData(req);
  } else {
    fdf::error("ContinueTransfer: bad data state {}", data_state_);
  }
}

void UmsFunction::StartTransfer(DataState state, uint32_t transfer_bytes, uint64_t lba) {
  zx_off_t offset = lba * kBlockSize;
  if (offset + transfer_bytes > kStorageSize) {
    fdf::error("StartTransfer: transfer out of range state: {}, lba: {} transfer_bytes: {}",
               static_cast<uint32_t>(state), lba, transfer_bytes);
    QueueCsw(CSW_FAILED);
    return;
  }

  data_state_ = state;
  data_offset_ = offset;  // Not applicable for the DATA_STATE_UNMAP case.
  data_remaining_ = transfer_bytes;

  ContinueTransfer();
}

void UmsFunction::HandleInquiry(ums_cbw_t* cbw) {
  scsi::InquiryCDB cmd;
  memcpy(&cmd, cbw->CBWCB, sizeof(cmd));

  usb::Request<>* req = &data_req_.value();
  void* data;
  zx_status_t mmap_status = req->Mmap(&data);
  if (mmap_status != ZX_OK) {
    fdf::error("HandleInquiry: Mmap failed: {}", zx_status_get_string(mmap_status));
    QueueCsw(CSW_FAILED);
    return;
  }
  uint32_t length = std::min(static_cast<uint32_t>(UMS_INQUIRY_TRANSFER_LENGTH),
                             le32toh(cbw->dCBWDataTransferLength));
  memset(data, 0, length);
  req->request()->header.length = length;

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

  QueueData(req);
}

void UmsFunction::HandleTestUnitReady(ums_cbw_t* cbw) {
  // no data phase here. Just return status OK
  QueueCsw(CSW_SUCCESS);
}

void UmsFunction::HandleRequestSense(ums_cbw_t* cbw) {
  usb::Request<>* req = &data_req_.value();
  scsi::SenseDataHeader* data;
  zx_status_t mmap_status = req->Mmap(reinterpret_cast<void**>(&data));
  if (mmap_status != ZX_OK) {
    fdf::error("HandleRequestSense: Mmap failed: {}", zx_status_get_string(mmap_status));
    QueueCsw(CSW_FAILED);
    return;
  }
  uint32_t length = std::min(static_cast<uint32_t>(UMS_REQUEST_SENSE_TRANSFER_LENGTH),
                             le32toh(cbw->dCBWDataTransferLength));
  memset(data, 0, length);
  req->request()->header.length = length;

  data->response_code =
      static_cast<uint8_t>(scsi::SenseDataResponseCodes::kFixedCurrentInformation);
  data->additional_sense_code = 0x20;
  data->additional_sense_length = 10;

  QueueData(req);
}

void UmsFunction::HandleReadCapacity10(ums_cbw_t* cbw) {
  usb::Request<>* req = &data_req_.value();
  scsi::ReadCapacity10ParameterData* data;
  zx_status_t mmap_status = req->Mmap(reinterpret_cast<void**>(&data));
  if (mmap_status != ZX_OK) {
    fdf::error("HandleReadCapacity10: Mmap failed: {}", zx_status_get_string(mmap_status));
    QueueCsw(CSW_FAILED);
    return;
  }

  uint64_t lba = kBlockCount - 1;
  if (lba > UINT32_MAX) {
    data->returned_logical_block_address = htobe32(UINT32_MAX);
  } else {
    data->returned_logical_block_address = htobe32(lba);
  }
  data->block_length_in_bytes = htobe32(kBlockSize);

  uint32_t length =
      std::min(static_cast<uint32_t>(sizeof(*data)), le32toh(cbw->dCBWDataTransferLength));
  req->request()->header.length = length;
  QueueData(req);
}

void UmsFunction::HandleReadCapacity16(ums_cbw_t* cbw) {
  usb::Request<>* req = &data_req_.value();
  scsi::ReadCapacity16ParameterData* data;
  zx_status_t mmap_status = req->Mmap(reinterpret_cast<void**>(&data));
  if (mmap_status != ZX_OK) {
    fdf::error("HandleReadCapacity16: Mmap failed: {}", zx_status_get_string(mmap_status));
    QueueCsw(CSW_FAILED);
    return;
  }
  memset(data, 0, sizeof(*data));

  data->returned_logical_block_address = htobe64(kBlockCount - 1);
  data->block_length_in_bytes = htobe32(kBlockSize);

  uint32_t length =
      std::min(static_cast<uint32_t>(sizeof(*data)), le32toh(cbw->dCBWDataTransferLength));
  req->request()->header.length = length;
  QueueData(req);
}

void UmsFunction::HandleModeSense6(ums_cbw_t* cbw) {
  usb::Request<>* req = &data_req_.value();
  scsi::Mode6ParameterHeader* data;
  zx_status_t mmap_status = req->Mmap(reinterpret_cast<void**>(&data));
  if (mmap_status != ZX_OK) {
    fdf::error("HandleModeSense6: Mmap failed: {}", zx_status_get_string(mmap_status));
    QueueCsw(CSW_FAILED);
    return;
  }

  if (le32toh(cbw->dCBWDataTransferLength) < sizeof(scsi::Mode6ParameterHeader)) {
    fdf::error("HandleModeSense6: buffer too small");
    QueueCsw(CSW_FAILED);
    return;
  }

  auto* cdb = reinterpret_cast<scsi::ModeSense6CDB*>(cbw->CBWCB);
  uint8_t page_code = static_cast<uint8_t>(cdb->page_code());
  uint32_t allocation_length = le32toh(cbw->dCBWDataTransferLength);
  memset(data, 0, sizeof(*data));

  req->request()->header.length = sizeof(scsi::Mode6ParameterHeader);

  if ((page_code == static_cast<uint8_t>(scsi::PageCode::kCachingPageCode) ||
       page_code == static_cast<uint8_t>(scsi::PageCode::kAllPageCode)) &&
      allocation_length >= sizeof(scsi::Mode6ParameterHeader) + sizeof(scsi::CachingModePage)) {
    auto caching_page = reinterpret_cast<scsi::CachingModePage*>(
        reinterpret_cast<uint8_t*>(data) + sizeof(scsi::Mode6ParameterHeader));
    memset(caching_page, 0, sizeof(*caching_page));
    caching_page->ps_spf_and_page_code = static_cast<uint8_t>(scsi::PageCode::kCachingPageCode);
    caching_page->page_length = sizeof(*caching_page) - 2;
    caching_page->set_write_cache_enabled(true);
    req->request()->header.length =
        sizeof(scsi::Mode6ParameterHeader) + sizeof(scsi::CachingModePage);
  }
  data->mode_data_length = static_cast<uint8_t>(req->request()->header.length - 1);
  QueueData(req);
}

void UmsFunction::HandleRead10(ums_cbw_t* cbw) {
  scsi::Read10CDB* command = reinterpret_cast<scsi::Read10CDB*>(cbw->CBWCB);
  uint64_t lba = be32toh(command->logical_block_address);
  uint32_t blocks = be16toh(command->transfer_length);
  StartTransfer(DATA_STATE_READ, blocks * kBlockSize, lba);
}

void UmsFunction::HandleRead12(ums_cbw_t* cbw) {
  scsi::Read12CDB* command = reinterpret_cast<scsi::Read12CDB*>(cbw->CBWCB);
  uint64_t lba = be32toh(command->logical_block_address);
  uint32_t blocks = be32toh(command->transfer_length);
  StartTransfer(DATA_STATE_READ, blocks * kBlockSize, lba);
}

void UmsFunction::HandleRead16(ums_cbw_t* cbw) {
  scsi::Read16CDB* command = reinterpret_cast<scsi::Read16CDB*>(cbw->CBWCB);
  uint64_t lba = be64toh(command->logical_block_address);
  uint32_t blocks = be32toh(command->transfer_length);
  StartTransfer(DATA_STATE_READ, blocks * kBlockSize, lba);
}

void UmsFunction::HandleWrite10(ums_cbw_t* cbw) {
  scsi::Write10CDB* command = reinterpret_cast<scsi::Write10CDB*>(cbw->CBWCB);
  uint64_t lba = be32toh(command->logical_block_address);
  uint32_t blocks = be16toh(command->transfer_length);
  StartTransfer(DATA_STATE_WRITE, blocks * kBlockSize, lba);
}

void UmsFunction::HandleWrite12(ums_cbw_t* cbw) {
  scsi::Write12CDB* command = reinterpret_cast<scsi::Write12CDB*>(cbw->CBWCB);
  uint64_t lba = be32toh(command->logical_block_address);
  uint32_t blocks = be32toh(command->transfer_length);
  StartTransfer(DATA_STATE_WRITE, blocks * kBlockSize, lba);
}

void UmsFunction::HandleWrite16(ums_cbw_t* cbw) {
  scsi::Write16CDB* command = reinterpret_cast<scsi::Write16CDB*>(cbw->CBWCB);
  uint64_t lba = be64toh(command->logical_block_address);
  uint32_t blocks = be32toh(command->transfer_length);
  StartTransfer(DATA_STATE_WRITE, blocks * kBlockSize, lba);
}

void UmsFunction::HandleUnmap(ums_cbw_t* cbw) {
  scsi::UnmapCDB* command = reinterpret_cast<scsi::UnmapCDB*>(cbw->CBWCB);
  uint16_t unmap_data_length =
      sizeof(scsi::UnmapParameterListHeader) + sizeof(scsi::UnmapBlockDescriptor);
  if (betoh16(command->parameter_list_length) != unmap_data_length) {
    fdf::error("Command parameter list length is invalid: {} != {}",
               betoh16(command->parameter_list_length), unmap_data_length);
    QueueCsw(CSW_FAILED);
    return;
  }

  StartTransfer(DATA_STATE_UNMAP, unmap_data_length);
}

void UmsFunction::HandleCbw(ums_cbw_t* cbw) {
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
      HandleInquiry(cbw);
      break;
    case scsi::Opcode::TEST_UNIT_READY:
      HandleTestUnitReady(cbw);
      break;
    case scsi::Opcode::REQUEST_SENSE:
      HandleRequestSense(cbw);
      break;
    case scsi::Opcode::READ_CAPACITY_10:
      HandleReadCapacity10(cbw);
      break;
    case scsi::Opcode::READ_CAPACITY_16:
      HandleReadCapacity16(cbw);
      break;
    case scsi::Opcode::MODE_SENSE_6:
      HandleModeSense6(cbw);
      break;
    case scsi::Opcode::READ_10:
      HandleRead10(cbw);
      break;
    case scsi::Opcode::READ_12:
      HandleRead12(cbw);
      break;
    case scsi::Opcode::READ_16:
      HandleRead16(cbw);
      break;
    case scsi::Opcode::WRITE_10:
      HandleWrite10(cbw);
      break;
    case scsi::Opcode::WRITE_12:
      HandleWrite12(cbw);
      break;
    case scsi::Opcode::WRITE_16:
      HandleWrite16(cbw);
      break;
    case scsi::Opcode::SYNCHRONIZE_CACHE_10:
      // TODO: This is presently untestable.
      // Implement this once we have a means of testing this.
      QueueCsw(CSW_SUCCESS);
      break;
    case scsi::Opcode::UNMAP:
      HandleUnmap(cbw);
      break;
    default:
      fdf::error("HandleCbw: unsupported opcode {:02X}h", cbw->CBWCB[0]);
      if (cbw->dCBWDataTransferLength) {
        // queue zero length packet to satisfy data phase
        usb::Request<>* req = &data_req_.value();
        req->request()->header.length = 0;
        data_state_ = DATA_STATE_FAILED;
        QueueData(req);
      }
      QueueCsw(CSW_FAILED);
      break;
  }
}

void UmsFunction::CbwComplete(usb::Request<>* req) {
  bool online = configured_ && active_.load();
  if (!online) {
    fdf::error("UmsFunction: Not online, dropping CBW");
    return;
  }

  if (req->request()->response.status != ZX_OK) {
    fdf::error("UmsFunction: CBW receive failed: {}",
               zx_status_get_string(req->request()->response.status));
    return;
  }

  if (req->request()->response.actual != sizeof(ums_cbw_t)) {
    return;
  }

  ums_cbw_t* cbw = &current_cbw_;
  memset(cbw, 0, sizeof(*cbw));
  [[maybe_unused]] size_t result = req->CopyFrom(cbw, sizeof(*cbw), 0);
  HandleCbw(cbw);
}

void UmsFunction::DataComplete(usb::Request<>* req) {
  bool online = configured_ && active_.load();
  if (!online) {
    return;
  }

  if (req->request()->response.status != ZX_OK) {
    fdf::error("UmsFunction: Data transfer failed: {}",
               zx_status_get_string(req->request()->response.status));
    data_state_ = DATA_STATE_NONE;
    QueueCsw(CSW_FAILED);
    return;
  }

  if (data_state_ == DATA_STATE_WRITE) {
    size_t result = req->CopyFrom(static_cast<char*>(storage_) + data_offset_,
                                  req->request()->response.actual, 0);
    ZX_ASSERT(result == req->request()->response.actual);
  } else if (data_state_ == DATA_STATE_UNMAP) {
    // Overwrite the unmapped blocks with zeros.
    usb::Request<>* req = &data_req_.value();
    uint8_t* data;
    zx_status_t mmap_status = req->Mmap(reinterpret_cast<void**>(&data));
    if (mmap_status != ZX_OK) {
      fdf::error("DataComplete: Mmap failed: {}", zx_status_get_string(mmap_status));
      QueueCsw(CSW_FAILED);
      return;
    }
    scsi::UnmapBlockDescriptor* block_descriptor = reinterpret_cast<scsi::UnmapBlockDescriptor*>(
        data + sizeof(scsi::UnmapParameterListHeader));
    size_t block_count = betoh32(block_descriptor->blocks);
    uint64_t start_lba = betoh64(block_descriptor->logical_block_address);
    memset(static_cast<char*>(storage_) + (start_lba * kBlockSize), 0, block_count * kBlockSize);
  } else if (data_state_ == DATA_STATE_FAILED) {
    data_state_ = DATA_STATE_NONE;
    QueueCsw(CSW_FAILED);
    return;
  } else {
    data_state_ = DATA_STATE_NONE;
    QueueCsw(CSW_SUCCESS);
    return;
  }

  data_offset_ += req->request()->response.actual;
  if (data_remaining_ > req->request()->response.actual) {
    data_remaining_ -= req->request()->response.actual;
  } else {
    data_remaining_ = 0;
  }

  if (data_remaining_ > 0) {
    ContinueTransfer();
  } else {
    data_state_ = DATA_STATE_NONE;
    QueueCsw(CSW_SUCCESS);
  }
}

static void CswComplete(usb::Request<>* req) {
  if (req->request()->response.status != ZX_OK) {
    fdf::error("UmsFunction: CSW send failed: {}",
               zx_status_get_string(req->request()->response.status));
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
    // Cancel all pending requests
    function_.CancelAll(bulk_out_addr_);
    function_.CancelAll(bulk_in_addr_);

    fbl::AutoLock l(&mtx_);
    cbw_in_flight_ = false;
    data_in_flight_ = false;
    csw_in_flight_ = false;
    cbw_req_complete_ = false;
    data_req_complete_ = false;
    csw_req_complete_ = false;
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
    cbw_req_complete_ = false;
    data_req_complete_ = false;
    csw_req_complete_ = false;
  }

  if (configured && status == ZX_OK) {
    // queue first read on OUT endpoint
    RequestQueue(&cbw_req_.value(), &request_complete_);
  }
  return status;
}

zx_status_t UmsFunction::UsbFunctionInterfaceSetInterface(uint8_t interface, uint8_t alt_setting) {
  return ZX_ERR_NOT_SUPPORTED;
}

void UmsFunction::PrepareStop(fdf::PrepareStopCompleter completer) {
  function_.CancelAll(bulk_out_addr_);
  function_.CancelAll(bulk_in_addr_);
  function_.CancelAll(descriptors.intf.b_interface_number);

  {
    fbl::AutoLock l(&mtx_);
    active_ = false;
    condvar_.Signal();
  }

  int retval;
  thrd_join(thread_, &retval);

  if (storage_) {
    zx_vmar_unmap(zx_vmar_root_self(), (uintptr_t)storage_, kStorageSize);
  }
  if (cbw_req_) {
    cbw_req_->Release();
  }
  if (data_req_) {
    data_req_->Release();
  }
  if (csw_req_) {
    csw_req_->Release();
  }

  completer(zx::ok());
}

zx::vmo UmsFunction::vmo_ = zx::vmo();

int UmsFunction::WorkerLoop() {
  while (active_) {
    bool cbw_req_complete = false;
    bool csw_req_complete = false;
    bool data_req_complete = false;

    {
      fbl::AutoLock l(&mtx_);
      // Wait until a request is complete (signaled in CompletionCallback),
      // unless the driver is shutting down (signaled in DdkUnbind).
      if (!(cbw_req_complete_ || csw_req_complete_ || data_req_complete_ || IsReadyForShutdown())) {
        condvar_.Wait(&mtx_);
      }

      // Exit the thread if the driver is inactive and all pending requests have been processed.
      if (IsReadyForShutdown()) {
        return 0;
      }

      cbw_req_complete = cbw_req_complete_;
      cbw_req_complete_ = false;
      csw_req_complete = csw_req_complete_;
      csw_req_complete_ = false;
      data_req_complete = data_req_complete_;
      data_req_complete_ = false;
    }

    if (cbw_req_complete) {
      atomic_fetch_add(&pending_request_count_, -1);
      CbwComplete(&cbw_req_.value());
    }
    if (csw_req_complete) {
      atomic_fetch_add(&pending_request_count_, -1);
      CswComplete(&csw_req_.value());
    }
    if (data_req_complete) {
      atomic_fetch_add(&pending_request_count_, -1);
      DataComplete(&data_req_.value());
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

  request_complete_ = {
      .callback = CompletionCallback,
      .ctx = this,
  };

  zx_status_t status = ZX_OK;

  parent_req_size_ = function_.GetRequestSize();
  ZX_DEBUG_ASSERT(parent_req_size_ != 0);

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

  status = usb::Request<>::Alloc(&cbw_req_, kBulkMaxPacket, bulk_out_addr_, parent_req_size_);
  if (status != ZX_OK) {
    return status;
  }
  // Endpoint for data_req depends on current_cbw.bmCBWFlags,
  // and will be set in QueueData.
  status = usb::Request<>::Alloc(&data_req_, kDataReqSize, 0, parent_req_size_);
  if (status != ZX_OK) {
    return status;
  }
  status = usb::Request<>::Alloc(&csw_req_, kBulkMaxPacket, bulk_in_addr_, parent_req_size_);
  if (status != ZX_OK) {
    return status;
  }
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

  csw_req_->request()->header.length = sizeof(ums_csw_t);

  function_.SetInterface(this, &usb_function_interface_protocol_ops_);
  thrd_create_with_name(
      &thread_, [](void* ctx) { return reinterpret_cast<UmsFunction*>(ctx)->WorkerLoop(); }, this,
      "ums_worker");
  return ZX_OK;
}

}  // namespace ums

// clang-format off
FUCHSIA_DRIVER_EXPORT(ums::UmsFunction);
