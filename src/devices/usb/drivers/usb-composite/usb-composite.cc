// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/usb-composite/usb-composite.h"

#include <endian.h>
#include <lib/driver/compat/cpp/banjo_client.h>
#include <lib/driver/component/cpp/driver_export2.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/logging/cpp/logger.h>
#include <stdio.h>
#include <string.h>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/usb/cpp/bind.h>
#include <fbl/alloc_checker.h>
#include <fbl/auto_lock.h>

#include "src/devices/usb/drivers/usb-composite/usb-interface.h"

namespace usb_composite {

UsbComposite::UsbComposite() : fdf::DriverBase2(kDriverName) {}

UsbComposite::~UsbComposite() = default;

zx::result<> UsbComposite::Start(fdf::DriverContext context) {
  incoming_ = std::shared_ptr<fdf::Namespace>(context.take_incoming());

  zx::result usb = compat::ConnectBanjo<ddk::UsbProtocolClient>(incoming_);
  if (usb.is_error()) {
    fdf::error("Failed to connect to parent USB protocol: {}", usb.status_string());
    return usb.take_error();
  }
  usb_ = *usb;

  if (zx_status_t status = Init(); status != ZX_OK) {
    return zx::error(status);
  }

  char name[16];
  snprintf(name, sizeof(name), "%03d", usb_.GetDeviceId());
  zx::result composite_node = AddOwnedChild(name);
  if (composite_node.is_error()) {
    fdf::error("Failed to add composite node {}: {}", name, composite_node.status_string());
    return composite_node.take_error();
  }
  composite_node_ = std::move(composite_node->node_);
  composite_node_controller_ = std::move(composite_node->node_controller_);

  if (zx_status_t status = AddInterfaces(); status != ZX_OK) {
    fdf::warn("AddInterfaces() = {}", zx_status_get_string(status));
    RemoveAllInterfaces();
    return zx::error(status);
  }

  return zx::ok();
}

void UsbComposite::Stop(fdf::StopCompleter completer) {
  RemoveAllInterfaces();
  completer(zx::ok());
}

void UsbComposite::RemoveAllInterfaces() {
  std::vector<std::unique_ptr<UsbInterface>> interfaces;
  std::vector<std::unique_ptr<UsbInterface>> removed_interfaces;
  {
    fbl::AutoLock lock(&lock_);
    interfaces = std::move(interfaces_);
    removed_interfaces = std::move(removed_interfaces_);
    interfaces_.clear();
    removed_interfaces_.clear();
  }

  interfaces.clear();
  removed_interfaces.clear();
}

std::unique_ptr<UsbInterface> UsbComposite::TakeInterface(UsbInterface* interface) {
  for (auto* list : {&interfaces_, &removed_interfaces_}) {
    for (auto it = list->begin(); it != list->end(); ++it) {
      if (it->get() == interface) {
        std::unique_ptr<UsbInterface> taken = std::move(*it);
        list->erase(it);
        return taken;
      }
    }
  }
  return nullptr;
}

void UsbComposite::RemoveInterface(UsbInterface* interface) {
  std::unique_ptr<UsbInterface> removed;
  {
    fbl::AutoLock lock(&lock_);
    removed = TakeInterface(interface);
  }
  removed.reset();
}

// returns whether the interface with the given id was removed.
bool UsbComposite::RemoveInterfaceById(uint8_t interface_id) {
  for (auto it = interfaces_.begin(); it != interfaces_.end(); ++it) {
    if ((*it)->ContainsInterface(interface_id)) {
      // The child driver may still be using the interface until its node is gone, so keep it alive
      // until RemoveInterface() is called for it.
      (*it)->AsyncRemove();
      removed_interfaces_.push_back(std::move(*it));
      interfaces_.erase(it);
      return true;
    }
  }
  return false;
}

zx_status_t UsbComposite::PublishInterface(
    std::unique_ptr<UsbInterface> interface, const char* name,
    std::vector<fuchsia_driver_framework::NodeProperty2> properties) {
  UsbInterface* raw_interface = interface.get();
  {
    fbl::AutoLock lock(&lock_);
    // We need to do this first before adding the child node.
    interfaces_.push_back(std::move(interface));
  }

  zx::result controller =
      raw_interface->AddChild(composite_node_.borrow(), outgoing(), name, std::move(properties));

  zx_status_t status;
  std::unique_ptr<UsbInterface> failed;
  {
    fbl::AutoLock lock(&lock_);
    // The controller is set under the lock because a child driver may concurrently call
    // ClaimInterface(), which calls AsyncRemove() on this interface.
    status = controller.is_ok() ? raw_interface->SetController(std::move(*controller))
                                : controller.status_value();
    if (status != ZX_OK) {
      failed = TakeInterface(raw_interface);
    }
  }

  failed.reset();
  return status;
}

zx_status_t UsbComposite::AddInterface(const usb_interface_descriptor_t* interface_desc,
                                       size_t length) {
  zx::result client = incoming_->Connect<fuchsia_hardware_usb::UsbService::Device>();
  if (client.is_error()) {
    fdf::error("Failed to connect fidl protocol: {}", client.status_string());
    return client.error_value();
  }

  zx::result interface =
      UsbInterface::Create(this, usb_, std::move(*client), interface_desc, length, dispatcher());
  if (interface.is_error()) {
    return interface.error_value();
  }

  char name[20];
  snprintf(name, sizeof(name), "ifc-%03d", interface_desc->b_interface_number);

  std::vector<fuchsia_driver_framework::NodeProperty2> props = {
      fdf::MakeProperty2(bind_fuchsia::PROTOCOL, bind_fuchsia_usb::BIND_PROTOCOL_INTERFACE),
      fdf::MakeProperty2(bind_fuchsia::USB_VID, static_cast<uint32_t>(device_desc_.id_vendor)),
      fdf::MakeProperty2(bind_fuchsia::USB_PID, static_cast<uint32_t>(device_desc_.id_product)),
      fdf::MakeProperty2(bind_fuchsia::USB_CLASS, static_cast<uint32_t>((*interface)->usb_class())),
      fdf::MakeProperty2(bind_fuchsia::USB_SUBCLASS,
                         static_cast<uint32_t>((*interface)->usb_subclass())),
      fdf::MakeProperty2(bind_fuchsia::USB_PROTOCOL,
                         static_cast<uint32_t>((*interface)->usb_protocol())),
      fdf::MakeProperty2(bind_fuchsia::USB_INTERFACE_NUMBER,
                         static_cast<uint32_t>(interface_desc->b_interface_number)),
  };

  return PublishInterface(std::move(*interface), name, std::move(props));
}

zx_status_t UsbComposite::AddInterfaceAssoc(const usb_interface_assoc_descriptor_t* assoc_desc,
                                            size_t length) {
  zx::result client = incoming_->Connect<fuchsia_hardware_usb::UsbService::Device>();
  if (client.is_error()) {
    fdf::error("Failed to connect fidl protocol: {}", client.status_string());
    return client.error_value();
  }

  zx::result interface =
      UsbInterface::Create(this, usb_, std::move(*client), assoc_desc, length, dispatcher());
  if (interface.is_error()) {
    return interface.error_value();
  }

  char name[20];
  snprintf(name, sizeof(name), "asc-%03d", assoc_desc->b_first_interface);

  std::vector<fuchsia_driver_framework::NodeProperty2> props = {
      fdf::MakeProperty2(bind_fuchsia::PROTOCOL,
                         bind_fuchsia_usb::BIND_PROTOCOL_INTERFACE_ASSOCIATION),
      fdf::MakeProperty2(bind_fuchsia::USB_VID, static_cast<uint32_t>(device_desc_.id_vendor)),
      fdf::MakeProperty2(bind_fuchsia::USB_PID, static_cast<uint32_t>(device_desc_.id_product)),
      fdf::MakeProperty2(bind_fuchsia::USB_CLASS, static_cast<uint32_t>((*interface)->usb_class())),
      fdf::MakeProperty2(bind_fuchsia::USB_SUBCLASS,
                         static_cast<uint32_t>((*interface)->usb_subclass())),
      fdf::MakeProperty2(bind_fuchsia::USB_PROTOCOL,
                         static_cast<uint32_t>((*interface)->usb_protocol())),
      fdf::MakeProperty2(bind_fuchsia::USB_INTERFACE_NUMBER,
                         static_cast<uint32_t>(assoc_desc->b_first_interface)),
  };

  return PublishInterface(std::move(*interface), name, std::move(props));
}

zx_status_t UsbComposite::AddInterfaces() {
  if (config_desc_.size() < sizeof(usb_configuration_descriptor_t)) {
    fdf::error("Malformed USB descriptor detected!");
    return ZX_ERR_INTERNAL;
  }
  auto* config = reinterpret_cast<usb_configuration_descriptor_t*>(config_desc_.data());
  const usb::UnownedInterfaceList interface_list(config, le16toh(config->w_total_length), true);
  zx_status_t assoc_status = ZX_OK;
  struct assoc_iterator_t {
    const usb_interface_assoc_descriptor_t* desc_;
    size_t length_;
    UsbComposite* dev_ptr_;
    zx_status_t* status_;

    assoc_iterator_t(const usb_interface_assoc_descriptor_t* desc, size_t length,
                     UsbComposite* dev_ptr, zx_status_t* status)
        : desc_(desc), length_(length), dev_ptr_(dev_ptr), status_(status) {}

    ~assoc_iterator_t() {
      auto status = dev_ptr_->AddInterfaceAssoc(desc_, length_);
      if (status_ && *status_ != ZX_OK) {
        *status_ = status;
      }
    }
  };
  std::optional<assoc_iterator_t> assoc = std::nullopt;
  for (const auto& interface : interface_list) {
    if (!interface.descriptor()) {
      fdf::error("Malformed USB descriptor detected!");
      return ZX_ERR_INTERNAL;
    }

    if (interface.association()) {
      if (!assoc || interface.association() != assoc->desc_) {
        assoc.emplace(interface.association(), interface.association()->b_length, this,
                      &assoc_status);
      }

      assoc->length_ += interface.length(true);
      continue;
    }

    assoc.reset();
    if (assoc_status != ZX_OK) {
      return assoc_status;
    }

    auto intf_num = interface.descriptor()->b_interface_number;
    InterfaceStatus intf_status;
    {
      fbl::AutoLock lock(&lock_);

      // Only create a child device if no child interface has claimed this interface.
      intf_status = interface_statuses_[intf_num];
    }

    if (intf_status == InterfaceStatus::AVAILABLE) {
      auto status = AddInterface(interface.descriptor(), interface.length(true));
      if (status != ZX_OK) {
        return status;
      }

      fbl::AutoLock lock(&lock_);

      // The interface may have been claimed in the meanwhile, so we need to
      // check the interface status again.
      if (interface_statuses_[intf_num] == InterfaceStatus::CLAIMED) {
        bool removed = RemoveInterfaceById(intf_num);
        if (!removed) {
          return ZX_ERR_BAD_STATE;
        }
      } else {
        interface_statuses_[intf_num] = InterfaceStatus::CHILD_DEVICE;
      }
    }
  }

  return assoc_status;
}

UsbInterface* UsbComposite::GetInterfaceById(uint8_t interface_id) {
  for (auto& intf : interfaces_) {
    if (intf->ContainsInterface(interface_id)) {
      return intf.get();
    }
  }
  return nullptr;
}

zx_status_t UsbComposite::ClaimInterface(uint8_t interface_id) {
  fbl::AutoLock lock(&lock_);

  auto intf = GetInterfaceById(interface_id);
  if (intf == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  if (interface_statuses_[interface_id] == InterfaceStatus::CLAIMED) {
    // The interface has already been claimed by a different interface.
    return ZX_ERR_ALREADY_BOUND;
  } else if (interface_statuses_[interface_id] == InterfaceStatus::CHILD_DEVICE) {
    bool removed = RemoveInterfaceById(interface_id);
    if (!removed) {
      return ZX_ERR_BAD_STATE;
    }
  }
  interface_statuses_[interface_id] = InterfaceStatus::CLAIMED;

  return ZX_OK;
}

zx_status_t UsbComposite::SetInterface(uint8_t interface_id, uint8_t alt_setting) {
  fbl::AutoLock lock(&lock_);

  for (auto& intf : interfaces_) {
    if (intf->ContainsInterface(interface_id)) {
      return intf->SetAltSetting(interface_id, alt_setting);
    }
  }
  return ZX_ERR_INVALID_ARGS;
}

zx_status_t UsbComposite::GetAdditionalDescriptorList(uint8_t last_interface_id,
                                                      uint8_t* out_desc_list, size_t desc_count,
                                                      size_t* out_desc_actual) {
  *out_desc_actual = 0;

  if (config_desc_.size() < sizeof(usb_configuration_descriptor_t)) {
    fdf::error("Malformed USB descriptor detected!");
    return ZX_ERR_INTERNAL;
  }
  auto* config = reinterpret_cast<usb_configuration_descriptor_t*>(config_desc_.data());
  usb::UnownedInterfaceList intf_list(config, le16toh(config->w_total_length), true);
  for (const auto& intf : intf_list) {
    if (!intf.descriptor()) {
      fdf::error("Malformed USB descriptor detected!");
      return ZX_ERR_INTERNAL;
    }
    // We are only interested in descriptors past the last stored descriptor
    // for the current interface.
    if (intf.descriptor()->b_interface_number > last_interface_id) {
      size_t length = config->w_total_length - (reinterpret_cast<uintptr_t>(intf.descriptor()) -
                                                reinterpret_cast<uintptr_t>(config));
      if (length > desc_count) {
        return ZX_ERR_BUFFER_TOO_SMALL;
      }
      memcpy(out_desc_list, intf.descriptor(), length);
      *out_desc_actual = length;
      return ZX_OK;
    }
  }
  return ZX_OK;
}

zx_status_t UsbComposite::Init() {
  // Parent must support USB protocol.
  if (!usb_.is_valid()) {
    return ZX_ERR_NOT_SUPPORTED;
  }

  usb_.GetDeviceDescriptor(&device_desc_);

  auto configuration = usb_.GetConfiguration();
  size_t desc_length;
  auto status = usb_.GetConfigurationDescriptorLength(configuration, &desc_length);
  if (status != ZX_OK) {
    return status;
  }

  fbl::AllocChecker ac;
  auto desc_bytes = new (&ac) uint8_t[desc_length];
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  config_desc_.reset(desc_bytes, desc_length);

  size_t actual;
  status = usb_.GetConfigurationDescriptor(configuration, desc_bytes, desc_length, &actual);
  if (status == ZX_OK && actual != desc_length) {
    status = ZX_ERR_IO;
  }
  return status;
}

}  // namespace usb_composite

FUCHSIA_DRIVER_EXPORT2(usb_composite::UsbComposite);
