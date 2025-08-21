// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_NAND_DRIVERS_RAM_NAND_RAM_NAND_CTL_H_
#define SRC_DEVICES_NAND_DRIVERS_RAM_NAND_RAM_NAND_CTL_H_

#include <fidl/fuchsia.hardware.nand/cpp/fidl.h>
#include <lib/driver/component/cpp/driver_base.h>
#include <lib/driver/devfs/cpp/connector.h>
#include <zircon/types.h>

#include <unordered_map>

#include "ram-nand.h"

namespace ram_nand {

class RamNandCtl : public fdf::DriverBase,
                   public fidl::WireServer<fuchsia_hardware_nand::RamNandCtl> {
 public:
  static constexpr std::string_view kDriverName = "ram_nand";
  static constexpr std::string_view kChildNodeName = "nand-ctl";

  RamNandCtl(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
      : DriverBase(kDriverName, std::move(start_args), std::move(driver_dispatcher)) {}

  // fdf::DriverBase implementation.
  zx::result<> Start() override;

  // fidl::WireServer<fuchsia_hardware_nand::RamNandCtl> implementation.
  void CreateDevice(CreateDeviceRequestView request,
                    CreateDeviceCompleter::Sync& completer) override;

 private:
  void DevfsConnect(fidl::ServerEnd<fuchsia_hardware_nand::RamNandCtl> server);

  NandDevice::Id next_device_id_ = 0;
  std::unordered_map<NandDevice::Id, std::unique_ptr<NandDevice>> devices_;

  driver_devfs::Connector<fuchsia_hardware_nand::RamNandCtl> devfs_connector_{
      fit::bind_member<&RamNandCtl::DevfsConnect>(this)};
  fdf::OwnedChildNode child_;
  fidl::ServerBindingGroup<fuchsia_hardware_nand::RamNandCtl> bindings_;
};

}  // namespace ram_nand

#endif  // SRC_DEVICES_NAND_DRIVERS_RAM_NAND_RAM_NAND_CTL_H_
