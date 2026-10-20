// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_BOARD_DRIVERS_VIRT_VIRT_H_
#define SRC_DEVICES_BOARD_DRIVERS_VIRT_VIRT_H_

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/wire.h>
#include <lib/driver/compat/cpp/compat.h>
#include <lib/driver/compat/cpp/device_server.h>
#include <lib/driver/component/cpp/driver_base2.h>
#include <lib/inspect/component/cpp/component.h>
#include <lib/pci/root_host.h>
#include <lib/zx/resource.h>
#include <lib/zx/result.h>

#include <optional>
#include <string>

#include "pciroot.h"

namespace board_virt {

// From QEMU hw/riscv/virt.c's memmap[VIRT_RTC].
constexpr zx_paddr_t kGoldfishRtcMmioBase = 0x101000;
constexpr size_t kGoldfishRtcMmioSize = 0x1000;

class Virt : public fdf::DriverBase2 {
 public:
  explicit Virt() : fdf::DriverBase2("virt") {}
  ~Virt() = default;

  zx::result<> Start(fdf::DriverContext context) override;

 private:
  zx::result<> PcirootInit(fdf::DriverContext& context, const std::string& node_name);
  zx::result<> RtcInit();
  void SysinfoInit();

  fdf::WireSyncClient<fuchsia_hardware_platform_bus::PlatformBus> pbus_;
  std::optional<PciRootHost> pci_root_host_;
  std::optional<VirtPciroot> pciroot_;
  std::optional<inspect::ComponentInspector> inspector_;

  std::optional<compat::BanjoServer> banjo_server_;
  compat::SyncInitializedDeviceServer compat_server_;
  fidl::Client<fuchsia_driver_framework::NodeController> pciroot_controller_;

  zx::resource msi_resource_;
  zx::resource mmio_resource_;
  zx::resource ioport_resource_;
};

}  // namespace board_virt

#endif  // SRC_DEVICES_BOARD_DRIVERS_VIRT_VIRT_H_
