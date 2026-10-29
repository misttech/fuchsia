// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fake-resource/resource.h>
#include <zircon/syscalls/resource.h>

#include "src/devices/pci/testing/pci_protocol_fake.h"

namespace pci {

void FakePciProtocol::GetBar(GetBarRequestView request, GetBarCompleter::Sync& completer) {
  if (request->bar_id >= PCI_DEVICE_BAR_COUNT) {
    completer.ReplyError(ZX_ERR_INVALID_ARGS);
    return;
  }

  auto& bar = bars()[request->bar_id];
  if (!bar.vmo.has_value()) {
    completer.ReplyError(ZX_ERR_NOT_FOUND);
    return;
  }

  if (bar.is_mmio) {
    zx::vmo dup;
    if (zx_status_t status = bar.vmo->duplicate(ZX_RIGHT_SAME_RIGHTS, &dup); status != ZX_OK) {
      completer.ReplyError(status);
      return;
    }
    completer.ReplySuccess({
        .bar_id = request->bar_id,
        .size = bar.size,
        .result = fuchsia_hardware_pci::wire::BarResult::WithVmo(std::move(dup)),
    });
  } else {
    zx::resource fake_ioport_resource;
    if (zx_status_t status =
            fake_resource_create(ZX_RSRC_KIND_IOPORT, fake_ioport_resource.reset_and_get_address());
        status != ZX_OK) {
      completer.ReplyError(status);
      return;
    }
    char name[] = "fake IO";
    zx::resource rsrc;
    if (zx_status_t status = zx::resource::create(fake_ioport_resource, ZX_RSRC_KIND_IOPORT, 0,
                                                  bar.size, name, sizeof(name), &rsrc);
        status != ZX_OK) {
      completer.ReplyError(status);
      return;
    }
    fidl::Arena arena;
    completer.ReplySuccess({
        .bar_id = request->bar_id,
        .size = bar.size,
        .result = fuchsia_hardware_pci::wire::BarResult::WithIo(arena,
                                                                fuchsia_hardware_pci::wire::IoBar{
                                                                    .address = 0,
                                                                    .resource = std::move(rsrc),
                                                                }),
    });
  }
}

void FakePciProtocol::GetDeviceInfo(GetDeviceInfoCompleter::Sync& completer) {
  fuchsia_hardware_pci::wire::DeviceInfo out_info;
  zx_status_t status = PciGetDeviceInfo(&out_info);
  if (status == ZX_OK) {
    completer.Reply(out_info);
  } else {
    completer.Close(status);
  }
}

void FakePciProtocol::GetBti(GetBtiRequestView request, GetBtiCompleter::Sync& completer) {
  zx::bti out_bti;
  zx_status_t status = PciGetBti(request->index, &out_bti);
  if (status == ZX_OK) {
    completer.ReplySuccess(std::move(out_bti));
  } else {
    completer.ReplyError(status);
  }
}

void FakePciProtocol::WriteConfig8(WriteConfig8RequestView request,
                                   WriteConfig8Completer::Sync& completer) {
  zx_status_t status = PciWriteConfig8(request->offset, request->value);
  if (status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(status);
  }
}

void FakePciProtocol::WriteConfig16(WriteConfig16RequestView request,
                                    WriteConfig16Completer::Sync& completer) {
  zx_status_t status = PciWriteConfig16(request->offset, request->value);
  if (status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(status);
  }
}

void FakePciProtocol::WriteConfig32(WriteConfig32RequestView request,
                                    WriteConfig32Completer::Sync& completer) {
  zx_status_t status = PciWriteConfig32(request->offset, request->value);
  if (status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(status);
  }
}

void FakePciProtocol::ReadConfig8(ReadConfig8RequestView request,
                                  ReadConfig8Completer::Sync& completer) {
  uint8_t out_value;
  zx_status_t status = PciReadConfig8(request->offset, &out_value);
  if (status == ZX_OK) {
    completer.ReplySuccess(out_value);
  } else {
    completer.ReplyError(status);
  }
}

void FakePciProtocol::ReadConfig16(ReadConfig16RequestView request,
                                   ReadConfig16Completer::Sync& completer) {
  uint16_t out_value;
  zx_status_t status = PciReadConfig16(request->offset, &out_value);
  if (status == ZX_OK) {
    completer.ReplySuccess(out_value);
  } else {
    completer.ReplyError(status);
  }
}

void FakePciProtocol::ReadConfig32(ReadConfig32RequestView request,
                                   ReadConfig32Completer::Sync& completer) {
  uint32_t out_value;
  zx_status_t status = PciReadConfig32(request->offset, &out_value);
  if (status == ZX_OK) {
    completer.ReplySuccess(out_value);
  } else {
    completer.ReplyError(status);
  }
}

void FakePciProtocol::GetInterruptModes(GetInterruptModesCompleter::Sync& completer) {
  fuchsia_hardware_pci::wire::InterruptModes out_modes;
  PciGetInterruptModes(&out_modes);
  completer.Reply(out_modes);
}

void FakePciProtocol::SetInterruptMode(SetInterruptModeRequestView request,
                                       SetInterruptModeCompleter::Sync& completer) {
  zx_status_t status = PciSetInterruptMode(request->mode, request->requested_irq_count);
  if (status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(status);
  }
}

void FakePciProtocol::MapInterrupt(MapInterruptRequestView request,
                                   MapInterruptCompleter::Sync& completer) {
  zx::interrupt out_interrupt;
  zx_status_t status = PciMapInterrupt(request->which_irq, &out_interrupt);
  if (status == ZX_OK) {
    completer.ReplySuccess(std::move(out_interrupt));
  } else {
    completer.ReplyError(status);
  }
}

void FakePciProtocol::GetCapabilities(GetCapabilitiesRequestView request,
                                      GetCapabilitiesCompleter::Sync& completer) {
  std::vector<uint8_t> capabilities;
  uint8_t offset;
  zx_status_t status = PciGetFirstCapability(fidl::ToUnderlying(request->id), &offset);
  if (status != ZX_OK) {
    completer.Close(status);
    return;
  }
  capabilities.push_back(offset);

  uint8_t out_offset;
  while (true) {
    status = PciGetNextCapability(fidl::ToUnderlying(request->id), offset, &out_offset);
    if (status == ZX_ERR_NOT_FOUND) {
      break;
    } else if (status != ZX_OK) {
      completer.Close(status);
      return;
    }

    capabilities.push_back(out_offset);
    offset = out_offset;
  }
  completer.Reply(fidl::VectorView<uint8_t>::FromExternal(capabilities));
}

void FakePciProtocol::SetBusMastering(SetBusMasteringRequestView request,
                                      SetBusMasteringCompleter::Sync& completer) {
  zx_status_t status = PciSetBusMastering(request->enabled);
  if (status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(status);
  }
}

void FakePciProtocol::ResetDevice(ResetDeviceCompleter::Sync& completer) {
  zx_status_t status = PciResetDevice();
  if (status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(status);
  }
}

void FakePciProtocol::AckInterrupt(AckInterruptCompleter::Sync& completer) {
  zx_status_t status = PciAckInterrupt();
  if (status == ZX_OK) {
    completer.ReplySuccess();
  } else {
    completer.ReplyError(status);
  }
}

fdf::Pci FakePciProtocol::SetUpFidlServer(async::Loop& loop) {
  auto endpoints = fidl::Endpoints<fuchsia_hardware_pci::Device>::Create();

  auto fake_pci = std::make_unique<FakePciProtocol>();
  std::optional<fidl::ServerBindingRef<fuchsia_hardware_pci::Device>> binding = fidl::BindServer(
      loop.dispatcher(),
      fidl::ServerEnd<fuchsia_hardware_pci::Device>(endpoints.server.TakeChannel()), this);
  ZX_ASSERT(binding.has_value());

  fdf::Pci pci = fdf::Pci(std::move(endpoints.client));
  ZX_ASSERT(pci.is_valid());
  return pci;
}

}  // namespace pci
