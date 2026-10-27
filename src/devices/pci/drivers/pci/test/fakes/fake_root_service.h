// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_PCI_DRIVERS_PCI_TEST_FAKES_FAKE_ROOT_SERVICE_H_
#define SRC_DEVICES_PCI_DRIVERS_PCI_TEST_FAKES_FAKE_ROOT_SERVICE_H_

#include <fidl/fuchsia.hardware.pci.root/cpp/fidl.h>
#include <fidl/fuchsia.hardware.pci.root/cpp/wire_test_base.h>
#include <lib/fake-bti/bti.h>
#include <lib/fake-resource/resource.h>
#include <lib/zx/bti.h>
#include <lib/zx/eventpair.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/msi.h>
#include <lib/zx/resource.h>
#include <lib/zx/vmo.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/syscalls.h>

#include <functional>
#include <map>
#include <string>
#include <utility>
#include <vector>

#include "src/devices/pci/drivers/pci/test/fakes/fake_ecam.h"

namespace pci {

class FakeConfigServer : public fidl::testing::WireTestBase<fuchsia_hardware_pci_root::Config> {
 public:
  explicit FakeConfigServer(FakeEcam& ecam) : ecam_(ecam) {}

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void Read8(Read8RequestView request, Read8Completer::Sync& completer) override {
    completer.ReplySuccess(ecam_.get_config_view(request->address).Read<uint8_t>(request->offset));
  }

  void Read16(Read16RequestView request, Read16Completer::Sync& completer) override {
    completer.ReplySuccess(ecam_.get_config_view(request->address).Read<uint16_t>(request->offset));
  }

  void Read32(Read32RequestView request, Read32Completer::Sync& completer) override {
    completer.ReplySuccess(ecam_.get_config_view(request->address).Read<uint32_t>(request->offset));
  }

  void Write8(Write8RequestView request, Write8Completer::Sync& completer) override {
    ecam_.get_config_view(request->address).Write<uint8_t>(request->value, request->offset);
    completer.ReplySuccess();
  }

  void Write16(Write16RequestView request, Write16Completer::Sync& completer) override {
    ecam_.get_config_view(request->address).Write<uint16_t>(request->value, request->offset);
    completer.ReplySuccess();
  }

  void Write32(Write32RequestView request, Write32Completer::Sync& completer) override {
    ecam_.get_config_view(request->address).Write<uint32_t>(request->value, request->offset);
    completer.ReplySuccess();
  }

  void Bind(async_dispatcher_t* dispatcher,
            fidl::ServerEnd<fuchsia_hardware_pci_root::Config> server_end) {
    fidl::BindServer(dispatcher, std::move(server_end), this);
  }

 private:
  FakeEcam& ecam_;
};

class FakeRootServer : public fidl::testing::WireTestBase<fuchsia_hardware_pci_root::Root> {
 public:
  static constexpr uint64_t kDefaultHighMemoryAddress = (1llu << 32u);
  static constexpr uint32_t kDefaultLowMemoryAddress = (1u << 10u);
  static constexpr uint16_t kDefaultIoAddress = 0x10;

  explicit FakeRootServer(FakeEcam* ecam = nullptr) : ecam_(ecam) {
    ZX_ASSERT(fake_resource_create(ZX_RSRC_KIND_MMIO, mmio_resource_.reset_and_get_address()) ==
              ZX_OK);
    ZX_ASSERT(fake_resource_create(ZX_RSRC_KIND_IOPORT, ioport_resource_.reset_and_get_address()) ==
              ZX_OK);
    ZX_ASSERT(fake_bti_create(bti_.reset_and_get_address()) == ZX_OK);
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void GetPlatformInfo(GetPlatformInfoCompleter::Sync& completer) override {
    if (!platform_info_fn_) {
      completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
      return;
    }
    fidl::Arena arena;
    completer.ReplySuccess(fidl::ToWire(arena, platform_info_fn_()));
  }

  void GetCam(GetCamCompleter::Sync& completer) override {
    if (!enable_cam_ || !ecam_) {
      completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
      return;
    }
    zx::vmo dup;
    if (zx_status_t status = zx_handle_duplicate(ecam_->mmio().get_vmo()->get(),
                                                 ZX_RIGHT_SAME_RIGHTS, dup.reset_and_get_address());
        status != ZX_OK) {
      completer.ReplyError(status);
      return;
    }
    completer.ReplySuccess(std::move(dup), ecam_->is_extended());
  }

  void GetLegacyInterrupt(GetLegacyInterruptRequestView request,
                          GetLegacyInterruptCompleter::Sync& completer) override {
    if (get_legacy_interrupt_status_.has_value()) {
      completer.ReplyError(*get_legacy_interrupt_status_);
      return;
    }
    auto it = legacy_irqs_.find(request->vector);
    if (it == legacy_irqs_.end()) {
      completer.ReplyError(ZX_ERR_NOT_FOUND);
      return;
    }
    zx::interrupt dup;
    if (zx_status_t status = it->second.duplicate(ZX_RIGHT_SAME_RIGHTS, &dup); status != ZX_OK) {
      completer.ReplyError(status);
      return;
    }
    completer.ReplySuccess(std::move(dup));
  }

  void GetAddressSpace(GetAddressSpaceRequestView request,
                       GetAddressSpaceCompleter::Sync& completer) override {
    if (request->size == 0) {
      completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
      return;
    }
    zx_rsrc_kind_t kind;
    if (request->type == fuchsia_hardware_pci_root::AddressSpace::kMemory) {
      kind = ZX_RSRC_KIND_MMIO;
    } else if (request->type == fuchsia_hardware_pci_root::AddressSpace::kIo) {
      kind = (io_type_ == fuchsia_hardware_pci_root::AddressSpace::kMemory) ? ZX_RSRC_KIND_MMIO
                                                                            : ZX_RSRC_KIND_IOPORT;
    } else {
      completer.ReplyError(ZX_ERR_INVALID_ARGS);
      return;
    }
    uint64_t base = 0;
    if (request->in_base) {
      base = request->in_base;
    } else {
      if (request->type == fuchsia_hardware_pci_root::AddressSpace::kMemory) {
        base = request->low ? kDefaultLowMemoryAddress : kDefaultHighMemoryAddress;
      } else {
        base = kDefaultIoAddress;
      }
    }
    const zx::resource& parent = (kind == ZX_RSRC_KIND_MMIO) ? mmio_resource_ : ioport_resource_;
    zx::resource resource;
    ZX_ASSERT(zx::resource::create(parent, kind, base, request->size, "fake", 5, &resource) ==
              ZX_OK);
    zx::eventpair token, local_ep;
    ZX_ASSERT(zx::eventpair::create(0, &local_ep, &token) == ZX_OK);
    allocation_eps_.push_back(std::move(local_ep));
    completer.ReplySuccess(base, std::move(resource), std::move(token));
  }

  void GetBti(GetBtiRequestView request, GetBtiCompleter::Sync& completer) override {
    zx::bti bti;
    if (zx_status_t status = bti_.duplicate(ZX_RIGHT_SAME_RIGHTS, &bti); status != ZX_OK) {
      completer.ReplyError(status);
      return;
    }
    completer.ReplySuccess(std::move(bti));
  }

  void AllocateMsi(AllocateMsiRequestView request, AllocateMsiCompleter::Sync& completer) override {
    zx::msi allocation;
    zx_status_t status =
        zx_msi_allocate(ZX_HANDLE_INVALID, request->msi_count, allocation.reset_and_get_address());
    if (status != ZX_OK) {
      completer.ReplyError(status);
      return;
    }
    fuchsia_hardware_pci_root::wire::MsiAllocationInfo info{
        .target_addr = 0xCAFE,
        .target_data = 0xC0FE,
        .irq_count = request->msi_count,
    };
    completer.ReplySuccess(zx::handle(allocation.release()), info);
  }

  void GetMsiHandle(GetMsiHandleRequestView request,
                    GetMsiHandleCompleter::Sync& completer) override {
    zx::interrupt interrupt;
    zx_status_t status =
        zx::msi::create(zx::msi(request->allocation.release()), request->options, request->msi_id,
                        request->cfg_vmo, request->cfg_offset, &interrupt);
    if (status != ZX_OK) {
      completer.ReplyError(status);
      return;
    }
    completer.ReplySuccess(std::move(interrupt));
  }

  void Bind(async_dispatcher_t* dispatcher,
            fidl::ServerEnd<fuchsia_hardware_pci_root::Root> server_end) {
    fidl::BindServer(dispatcher, std::move(server_end), this);
  }

  std::vector<zx::eventpair>& allocation_eps() { return allocation_eps_; }
  std::map<uint32_t, zx::interrupt>& legacy_irqs() { return legacy_irqs_; }
  const zx::bti& bti() const { return bti_; }

  void set_platform_info_fn(std::function<fuchsia_hardware_pci_root::PlatformInfo()> fn) {
    platform_info_fn_ = std::move(fn);
  }
  void set_enable_cam(bool enable) { enable_cam_ = enable; }
  void set_io_type(fuchsia_hardware_pci_root::AddressSpace io_type) { io_type_ = io_type; }
  void set_get_legacy_interrupt_status(std::optional<zx_status_t> status) {
    get_legacy_interrupt_status_ = status;
  }

 private:
  FakeEcam* ecam_ = nullptr;
  zx::resource mmio_resource_;
  zx::resource ioport_resource_;
  fuchsia_hardware_pci_root::AddressSpace io_type_ = fuchsia_hardware_pci_root::AddressSpace::kIo;
  zx::bti bti_;
  std::vector<zx::eventpair> allocation_eps_;
  std::map<uint32_t, zx::interrupt> legacy_irqs_;
  std::function<fuchsia_hardware_pci_root::PlatformInfo()> platform_info_fn_;
  std::optional<zx_status_t> get_legacy_interrupt_status_;

  bool enable_cam_ = true;
};

}  // namespace pci

#endif  // SRC_DEVICES_PCI_DRIVERS_PCI_TEST_FAKES_FAKE_ROOT_SERVICE_H_
