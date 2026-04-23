// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/testing/cpp/driver_runtime.h>

#include "src/devices/nand/drivers/aml-rawnand/aml-rawnand.h"

namespace amlrawnand {

AmlRawNand::AmlRawNand(fdf::MmioBuffer mmio_nandreg, fdf::MmioBuffer mmio_clockreg, zx::bti bti,
                       std::unique_ptr<Onfi> onfi)
    : fdf::DriverBase(
          "aml-rawnand",
          []() {
            auto endpoints = fidl::Endpoints<fuchsia_io::Directory>::Create();
            auto [client2, server2] = fidl::Endpoints<fuchsia_io::Directory>::Create();
            fuchsia_driver_framework::DriverStartArgs start_args;
            std::vector<fuchsia_component_runner::ComponentNamespaceEntry> entries;
            entries.emplace_back(fuchsia_component_runner::ComponentNamespaceEntry{{
                .path = "/",
                .directory = std::move(endpoints.client),
            }});
            start_args.incoming() = std::move(entries);
            start_args.outgoing_dir() = std::move(server2);
            return start_args;
          }(),
          fdf_testing::DriverRuntime::GetInstance()->GetForegroundDispatcher()) {
  mmio_nandreg_ = std::move(mmio_nandreg);
  mmio_clockreg_ = std::move(mmio_clockreg);
  bti_ = std::move(bti);
  onfi_ = std::move(onfi);
}

}  // namespace amlrawnand
