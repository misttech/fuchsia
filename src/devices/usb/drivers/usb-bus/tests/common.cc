// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-bus/tests/common.h"

#include <usb/descriptors.h>

namespace fdescriptor = fuchsia_hardware_usb_descriptor;

namespace usb_bus {

const char16_t* kStringDescriptors[][2] = {{u"Fuchsia", u"Fucsia"}, {u"Device", u"Dispositivo"}};

FakeHci::FakeHci(async_dispatcher_t* dispatcher) : dispatcher_(dispatcher) {
  proto_.ops = &usb_hci_protocol_ops_;
  proto_.ctx = this;
}

zx_status_t FakeHci::UsbHciResetEndpoint(uint32_t device_id, uint8_t ep_address) {
  if (device_id == kDeviceId) {
    reset_endpoint_ = ep_address;
  }
  return ZX_OK;
}

zx_status_t FakeHci::UsbHciResetDevice(uint32_t hub_address, uint32_t device_id) {
  if (device_id == kDeviceId) {
    device_reset_ = true;
  }
  return ZX_OK;
}

zx_status_t FakeHci::UsbHciCancelAll(uint32_t device_id, uint8_t ep_address) {
  auto requests = pending_requests();
  requests.CompleteAll(ZX_ERR_CANCELED, 0);
  return ZX_OK;
}

void FakeHci::UsbHciRequestQueue(usb_request_t* usb_request_,
                                 const usb_request_complete_callback_t* complete_cb_) {
  usb::BorrowedRequest<void> request(usb_request_, *complete_cb_, sizeof(usb_request_t));
  if (error_state_ != ZX_OK) {
    request.Complete(error_state_, 0);
    return;
  }
  if (should_return_empty_) {
    request.Complete(ZX_OK, 0);
    return;
  }
  if ((request.request()->header.ep_address == 0) && !custom_control_) {
    if (request.request()->setup.bm_request_type == kStandardDeviceIn &&
        request.request()->setup.b_request == fdescriptor::StandardRequest::kGetDescriptor) {
      auto type = usb_descriptor_type_from_w_value(request.request()->setup.w_value);
      uint8_t index = usb_descriptor_index_from_w_value(request.request()->setup.w_value);
      switch (type) {
        case fdescriptor::DescriptorType::kDevice: {
          usb_device_descriptor_t* descriptor;
          request.Mmap(reinterpret_cast<void**>(&descriptor));
          descriptor->b_length = sizeof(*descriptor);
          descriptor->b_descriptor_type =
              static_cast<uint8_t>(fdescriptor::DescriptorType::kDevice);
          descriptor->b_num_configurations = 2;
          descriptor->id_vendor = kVendorId;
          descriptor->id_product = kProductId;
          descriptor->b_device_class = kDeviceClass;
          descriptor->b_device_sub_class = kDeviceSubclass;
          descriptor->b_device_protocol = kDeviceProtocol;
          request.Complete(ZX_OK, sizeof(*descriptor));
        }
          return;
        case fdescriptor::DescriptorType::kConfiguration: {
          if (include_endpoints_in_config_) {
            // Full USB Configuration descriptor bundle (USB 2.0 §9.4.3 & §9.6.3).
            // A USB configuration response concatenates the Configuration descriptor
            // followed by all Interface and Endpoint descriptors.
            //
            // This mock configuration defines:
            // - Interface 0, Alt-Setting 0:
            //     EP 0x81 (IN):  Interrupt, max packet size = 64 bytes, interval = 5ms
            //     EP 0x01 (OUT): Interrupt, max packet size = 64 bytes, interval = 5ms
            // - Interface 0, Alt-Setting 1:
            //     EP 0x81 (IN):  Bulk, max packet size = 512 bytes (0x0200), interval = 0
            //     EP 0x01 (OUT): Bulk, max packet size = 512 bytes (0x0200), interval = 0
            //
            // This allows testing dynamic endpoint reconfiguration when switching
            // alternate settings via SetInterface.
            static const uint8_t kFullConfigDesc[55] = {
                // --- Configuration Descriptor (9 bytes) ---
                9,  // bLength
                2,  // bDescriptorType (Configuration = 2)
                55,
                0,     // wTotalLength (55 bytes total, little-endian)
                1,     // bNumInterfaces (1 interface)
                1,     // bConfigurationValue (overridden with index + 1 below)
                0,     // iConfiguration (no string index)
                0x80,  // bmAttributes (Bus-powered, bit 7 reserved = 1)
                50,    // bMaxPower (50 * 2mA = 100mA)

                // --- Interface 0, Alternate Setting 0 (9 bytes) ---
                9,     // bLength
                4,     // bDescriptorType (Interface = 4)
                0,     // bInterfaceNumber (0)
                0,     // bAlternateSetting (0)
                2,     // bNumEndpoints (2 endpoints)
                0xff,  // bInterfaceClass (Vendor specific)
                0,     // bInterfaceSubClass
                0,     // bInterfaceProtocol
                0,     // iInterface (no string index)

                // Endpoint 1 IN (Interrupt, 64-byte max packet) (7 bytes)
                7,     // bLength
                5,     // bDescriptorType (Endpoint = 5)
                0x81,  // bEndpointAddress (EP 1, IN: 0x80 | 1)
                3,     // bmAttributes (Interrupt = 3)
                64,
                0,  // wMaxPacketSize (64 bytes, little-endian)
                5,  // bInterval (5ms)

                // Endpoint 1 OUT (Interrupt, 64-byte max packet) (7 bytes)
                7,     // bLength
                5,     // bDescriptorType (Endpoint = 5)
                0x01,  // bEndpointAddress (EP 1, OUT: 0x00 | 1)
                3,     // bmAttributes (Interrupt = 3)
                64,
                0,  // wMaxPacketSize (64 bytes, little-endian)
                5,  // bInterval (5ms)

                // --- Interface 0, Alternate Setting 1 (9 bytes) ---
                9,     // bLength
                4,     // bDescriptorType (Interface = 4)
                0,     // bInterfaceNumber (0)
                1,     // bAlternateSetting (1)
                2,     // bNumEndpoints (2 endpoints)
                0xff,  // bInterfaceClass (Vendor specific)
                0,     // bInterfaceSubClass
                0,     // bInterfaceProtocol
                0,     // iInterface (no string index)

                // Endpoint 1 IN (Bulk, 512-byte max packet) (7 bytes)
                7,     // bLength
                5,     // bDescriptorType (Endpoint = 5)
                0x81,  // bEndpointAddress (EP 1, IN: 0x80 | 1)
                2,     // bmAttributes (Bulk = 2)
                0x00,
                0x02,  // wMaxPacketSize (512 bytes = 0x0200, little-endian)
                0,     // bInterval (ignored for bulk)

                // Endpoint 1 OUT (Bulk, 512-byte max packet) (7 bytes)
                7,     // bLength
                5,     // bDescriptorType (Endpoint = 5)
                0x01,  // bEndpointAddress (EP 1, OUT: 0x00 | 1)
                2,     // bmAttributes (Bulk = 2)
                0x00,
                0x02,  // wMaxPacketSize (512 bytes = 0x0200, little-endian)
                0,     // bInterval (ignored for bulk)
            };
            void* mapped;
            request.Mmap(&mapped);
            size_t copy_len =
                std::min<size_t>(request.request()->header.length, sizeof(kFullConfigDesc));
            memcpy(mapped, kFullConfigDesc, copy_len);
            auto* descriptor = static_cast<usb_configuration_descriptor_t*>(mapped);
            descriptor->b_configuration_value = static_cast<uint8_t>(index + 1);
            request.Complete(ZX_OK, copy_len);
            return;
          }
          usb_configuration_descriptor_t* descriptor;
          request.Mmap(reinterpret_cast<void**>(&descriptor));
          descriptor->b_length = sizeof(*descriptor);
          descriptor->b_descriptor_type =
              static_cast<uint8_t>(fdescriptor::DescriptorType::kConfiguration);
          descriptor->w_total_length = sizeof(*descriptor);
          descriptor->b_configuration_value = static_cast<uint8_t>(index + 1);
          request.Complete(ZX_OK, sizeof(*descriptor));
        }
          return;
        case fdescriptor::DescriptorType::kString: {
          if (index == 0) {
            // Fetch language table
            usb_langid_desc_t* languages;
            request.Mmap(reinterpret_cast<void**>(&languages));
            languages->b_length = 2 + (2 * 2);
            languages->w_lang_ids[0] = MakeConstant<uint16_t, 2>("EN");
            languages->w_lang_ids[1] = MakeConstant<uint16_t, 2>("ES");
            request.Complete(ZX_OK, languages->b_length);
            return;
          }
          index--;
          uint16_t lang = request.request()->setup.w_index;
          switch (lang) {
            case MakeConstant<uint16_t, 2>("EN"):
              lang = 0;
              break;
            case MakeConstant<uint16_t, 2>("ES"):
              lang = 1;
              break;
          }
          if ((index < 2) && (lang < 2)) {
            usb_string_desc_t* descriptor;
            request.Mmap(reinterpret_cast<void**>(&descriptor));
            descriptor->b_length = static_cast<uint8_t>(
                2 + (2 * std::char_traits<char16_t>::length(kStringDescriptors[index][lang])));
            memcpy(descriptor->code_points, kStringDescriptors[index][lang],
                   descriptor->b_length - 2);
            request.Complete(ZX_OK, descriptor->b_length);
            return;
          }
          break;
        }
        default:
          break;
      }
    }
    if ((request.request()->setup.bm_request_type == kStandardDeviceOut) &&
        (request.request()->setup.b_request ==
         fidl::ToUnderlying(fdescriptor::StandardRequest::kSetConfiguration))) {
      selected_configuration_ = static_cast<uint8_t>(request.request()->setup.w_value);
      request.Complete(ZX_OK, 0);
      return;
    }
    request.Complete(ZX_ERR_INVALID_ARGS, 0);
    return;
  }
  pending_requests_.push(std::move(request));
}

zx_status_t FakeHci::UsbHciEnableEndpoint(uint32_t device_id,
                                          const usb_endpoint_descriptor_t* ep_desc,
                                          const usb_ss_ep_comp_descriptor_t* ss_com_desc,
                                          bool enable) {
  if (!enable_endpoint_hook_) {
    return ZX_ERR_BAD_STATE;
  }
  return enable_endpoint_hook_(device_id, ep_desc, ss_com_desc, enable);
}

void FakeHci::SetInterface(SetInterfaceRequest& request, SetInterfaceCompleter::Sync& completer) {
  hci_interface_client_ = fidl::WireSharedClient<fuchsia_hardware_usb_hci::UsbHciInterface>(
      std::move(request.interface()), dispatcher_);
  completer.Reply(zx::ok());
}

void FakeHci::ConnectToEndpoint(ConnectToEndpointRequest& request,
                                ConnectToEndpointCompleter::Sync& completer) {
  completer.Reply(zx::ok());
}

void FakeHci::GetMaxDeviceCount(GetMaxDeviceCountCompleter::Sync& completer) {
  completer.Reply(UsbHciGetMaxDeviceCount());
}

void FakeHci::EnableEndpoint(EnableEndpointRequest& request,
                             EnableEndpointCompleter::Sync& completer) {
  completer.Reply(zx::ok());
}

void FakeHci::GetCurrentFrame(GetCurrentFrameCompleter::Sync& completer) {
  completer.Reply(UsbHciGetCurrentFrame());
}

void FakeHci::ConfigureHub(ConfigureHubRequest& request, ConfigureHubCompleter::Sync& completer) {
  completer.Reply(zx::ok());
}

void FakeHci::HubDeviceAdded(HubDeviceAddedRequest& request,
                             HubDeviceAddedCompleter::Sync& completer) {
  completer.Reply(zx::ok());
}

void FakeHci::HubDeviceRemoved(HubDeviceRemovedRequest& request,
                               HubDeviceRemovedCompleter::Sync& completer) {
  completer.Reply(zx::ok());
}

void FakeHci::HubDeviceReset(HubDeviceResetRequest& request,
                             HubDeviceResetCompleter::Sync& completer) {
  completer.Reply(zx::ok());
}

void FakeHci::ResetEndpoint(ResetEndpointRequest& request,
                            ResetEndpointCompleter::Sync& completer) {
  completer.Reply(zx::ok());
}

void FakeHci::ResetDevice(ResetDeviceRequest& request, ResetDeviceCompleter::Sync& completer) {
  completer.Reply(zx::ok());
}

void FakeHci::GetMaxTransferSize(GetMaxTransferSizeRequest& request,
                                 GetMaxTransferSizeCompleter::Sync& completer) {
  completer.Reply(zx::ok(UsbHciGetMaxTransferSize(request.device_id(), request.ep_address())));
}

}  // namespace usb_bus
