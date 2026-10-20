// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_USB_USB_CONNECTOR_USB_CONNECTOR_VISITOR_H_
#define LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_USB_USB_CONNECTOR_USB_CONNECTOR_VISITOR_H_

#include <lib/driver/devicetree/manager/node.h>
#include <lib/driver/devicetree/manager/visitor.h>
#include <lib/driver/devicetree/visitors/property-parser.h>

#include <memory>
#include <string_view>

namespace usb_connector_visitor_dt {

// Adds a USB connector parent spec (`fuchsia.hardware.usb.connector.Service`) to nodes
// referencing a connector via `usb-connector`.
// The provider's optional `usb-connector-output-name` selects the connector instance (defaulting
// to `"usb-connector"`), and the consumer's optional `usb-connector-names` names the bind parent
// (defaulting to `"usb-connector"` when a single connector is referenced).
class UsbConnectorVisitor : public fdf_devicetree::Visitor {
 public:
  // Properties on the consumer node.
  static constexpr char kUsbConnector[] = "usb-connector";
  static constexpr char kUsbConnectorNames[] = "usb-connector-names";
  // Property on the connector (provider) node.
  static constexpr char kUsbConnectorOutputName[] = "usb-connector-output-name";
  static constexpr char kDefaultConnectorName[] = "usb-connector";

  UsbConnectorVisitor();
  zx::result<> Visit(fdf_devicetree::Node& node,
                     const devicetree::PropertyDecoder& decoder) override;

 private:
  zx::result<> ParseReferenceChild(fdf_devicetree::Node& child,
                                   fdf_devicetree::ReferenceNode& connector,
                                   std::string_view parent_name);

  std::unique_ptr<fdf_devicetree::PropertyParser> parser_;
};

}  // namespace usb_connector_visitor_dt

#endif  // LIB_DRIVER_DEVICETREE_VISITORS_DRIVERS_USB_USB_CONNECTOR_USB_CONNECTOR_VISITOR_H_
