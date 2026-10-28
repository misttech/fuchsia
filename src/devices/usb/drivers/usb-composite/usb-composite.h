// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_USB_DRIVERS_USB_COMPOSITE_USB_COMPOSITE_H_
#define SRC_DEVICES_USB_DRIVERS_USB_COMPOSITE_USB_COMPOSITE_H_

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fuchsia/hardware/usb/cpp/banjo.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/driver/incoming/cpp/namespace.h>

#include <memory>
#include <vector>

#include <fbl/array.h>
#include <fbl/mutex.h>
#include <usb/usb.h>

namespace usb_composite {

class UsbInterface;

// This class represents a USB composite device and manages creating child nodes for multiple USB
// interfaces.
class UsbComposite : public fdf::DriverBase2 {
 public:
  static constexpr std::string_view kDriverName = "usb-composite";

  // Defined out of line because UsbInterface is incomplete here.
  UsbComposite();
  ~UsbComposite() override;

  // fdf::DriverBase2 implementation.
  zx::result<> Start(fdf::DriverContext context) override;
  void Stop(fdf::StopCompleter completer) override;

  zx_status_t ClaimInterface(uint8_t interface_id);
  zx_status_t SetInterface(uint8_t interface_id, uint8_t alt_setting);
  zx_status_t GetAdditionalDescriptorList(uint8_t last_interface_id, uint8_t* out_desc_list,
                                          size_t desc_count, size_t* out_desc_actual);

  inline const usb_device_descriptor_t* device_descriptor() const { return &device_desc_; }
  inline const usb_configuration_descriptor_t* GetConfigurationDescriptor() const {
    return reinterpret_cast<usb_configuration_descriptor_t*>(config_desc_.data());
  }

  // Called by |interface| once its child node has been removed. Destroys |interface|. Must be
  // called on the driver dispatcher.
  void RemoveInterface(UsbInterface* interface);

 private:
  template <auto* descriptors>
  friend class UsbInterfaceTest;
  template <auto* descriptors>
  friend class UsbCompositeTest;

  enum class InterfaceStatus : uint8_t {
    // The interface has not been claimed and no device has been created for it.
    AVAILABLE,
    // Another interface has claimed the interface.
    CLAIMED,
    // A child device has been created for the interface.
    CHILD_DEVICE,
  };

  zx_status_t Init();

  // Publishes |interface| as a child node named |name| with the given bind |properties|.
  zx_status_t PublishInterface(std::unique_ptr<UsbInterface> interface, const char* name,
                               std::vector<fuchsia_driver_framework::NodeProperty2> properties);
  zx_status_t AddInterface(const usb_interface_descriptor_t* interface_desc, size_t length);
  zx_status_t AddInterfaceAssoc(const usb_interface_assoc_descriptor_t* assoc_desc, size_t length);
  zx_status_t AddInterfaces();
  UsbInterface* GetInterfaceById(uint8_t interface_id) __TA_REQUIRES(lock_);
  bool RemoveInterfaceById(uint8_t interface_id) __TA_REQUIRES(lock_);

  // Removes |interface| from |interfaces_| or |removed_interfaces_| and returns ownership of it.
  std::unique_ptr<UsbInterface> TakeInterface(UsbInterface* interface) __TA_REQUIRES(lock_);

  // Destroys all interfaces, which removes their child nodes and services. Must be called on the
  // driver dispatcher.
  void RemoveAllInterfaces() __TA_EXCLUDES(lock_);

  // The driver's incoming namespace. Used to connect each interface to the parent's
  // fuchsia.hardware.usb/Usb protocol.
  std::shared_ptr<fdf::Namespace> incoming_;

  // Our parent's USB protocol.
  ddk::UsbProtocolClient usb_;

  fidl::ClientEnd<fuchsia_driver_framework::Node> composite_node_;
  fidl::ClientEnd<fuchsia_driver_framework::NodeController> composite_node_controller_;

  // All of our published USB interfaces.
  std::vector<std::unique_ptr<UsbInterface>> interfaces_ __TA_GUARDED(lock_);

  // Interfaces whose node removal has been requested (e.g. because another interface claimed
  // them) but whose child driver may still be using them. They are destroyed in
  // RemoveInterface() once the node is gone.
  std::vector<std::unique_ptr<UsbInterface>> removed_interfaces_ __TA_GUARDED(lock_);

  InterfaceStatus interface_statuses_[UINT8_MAX] __TA_GUARDED(lock_) = {};

  usb_device_descriptor_t device_desc_;
  fbl::Array<uint8_t> config_desc_;
  fbl::Mutex lock_;
};

}  // namespace usb_composite

#endif  // SRC_DEVICES_USB_DRIVERS_USB_COMPOSITE_USB_COMPOSITE_H_
