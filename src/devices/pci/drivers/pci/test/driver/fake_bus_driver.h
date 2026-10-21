// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_PCI_DRIVERS_PCI_TEST_DRIVER_FAKE_BUS_DRIVER_H_
#define SRC_DEVICES_PCI_DRIVERS_PCI_TEST_DRIVER_FAKE_BUS_DRIVER_H_

#include <fidl/fuchsia.hardware.pci/cpp/wire.h>

#include <ddktl/device.h>

#include "src/devices/pci/drivers/pci/test/driver/driver_tests.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_bus.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_pciroot.h"
#include "src/devices/pci/drivers/pci/test/fakes/fake_upstream_node.h"

namespace pci {

class FakeBusDriver;
using FakeBusDriverType = ddk::Device<FakeBusDriver>;

// Fake bus driver loaded in DriverTestRealm by pci-driver-test.
// Configures a fake Quadro device and serves fuchsia.hardware.pci.Service
// for driver_tests.rs.
class FakeBusDriver : public FakeBusDriverType {
 public:
  static zx_status_t Create(zx_device_t* parent, const char* name, uint8_t start_bus,
                            uint8_t end_bus, bool is_extended);
  ~FakeBusDriver() {
    upstream_.DisableDownstream();
    upstream_.UnplugDownstream();
  }
  zx_status_t CreateDevice(fuchsia_hardware_pci::wire::Bdf bdf, uint8_t* base_cfg,
                           size_t base_cfg_size, uint16_t vid = PCI_TEST_DRIVER_VID,
                           uint16_t did = PCI_TEST_DRIVER_DID);

  FakePciType0Config* GetDevice(fuchsia_hardware_pci::wire::Bdf bdf) {
    return bus().pciroot().ecam().get_device(bdf);
  }
  FakePciType1Config* GetBridge(fuchsia_hardware_pci::wire::Bdf bdf) {
    return bus().pciroot().ecam().get_bridge(bdf);
  }

  FakeUpstreamNode& upstream() { return upstream_; }
  FakeBus& bus() { return bus_; }
  fuchsia_hardware_pci::wire::Bdf test_bdf() { return test_bdf_; }
  void DdkRelease() { delete this; }

 private:
  FakeBusDriver(zx_device_t* parent, uint8_t bus_start, uint8_t bus_end, bool is_extended)
      : FakeBusDriverType(parent),
        upstream_(UpstreamNode::Type::ROOT, 0),
        bus_(bus_start, bus_end, is_extended) {}

  FakeUpstreamNode upstream_;
  FakeBus bus_;
  const fuchsia_hardware_pci::wire::Bdf test_bdf_ = {
      .bus_id = PCI_TEST_BUS_ID,
      .device_id = PCI_TEST_DEV_ID,
      .function_id = PCI_TEST_FUNC_ID,
  };
};
}  // namespace pci

#endif  // SRC_DEVICES_PCI_DRIVERS_PCI_TEST_DRIVER_FAKE_BUS_DRIVER_H_
