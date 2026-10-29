// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.component.runner/cpp/natural_types.h>
#include <fidl/fuchsia.hardware.pci/cpp/wire_test_base.h>
#include <fidl/fuchsia.io/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/async/default.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/device-protocol/pci.h>
#include <lib/driver/fake-bti/cpp/fake-bti.h>
#include <lib/driver/fake-resource/cpp/fake-resource.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/vmo.h>
#include <zircon/errors.h>
#include <zircon/syscalls.h>

#include <vector>

#include <gtest/gtest.h>

#include "src/lib/testing/predicates/status.h"

#if FUCHSIA_API_LEVEL_AT_LEAST(HEAD)

namespace {

namespace fpci = fuchsia_hardware_pci;

constexpr uint16_t kVendorId = 0x8086;
constexpr uint16_t kDeviceId = 0x1234;
constexpr uint8_t kRevisionId = 0x01;
constexpr uint32_t kConfig32Value = 0xdeadbeef;

class FakePciDeviceServer : public fidl::testing::WireTestBase<fpci::Device> {
 public:
  zx_status_t Bind(fidl::ServerEnd<fpci::Device> server_end) {
    bindings_.AddBinding(async_get_default_dispatcher(), std::move(server_end), this,
                         fidl::kIgnoreBindingClosure);
    return ZX_OK;
  }

  zx_status_t ServeOutgoing(fidl::ServerEnd<fuchsia_io::Directory> server_end) {
    async_dispatcher_t* dispatcher = async_get_default_dispatcher();
    outgoing_.emplace(dispatcher);
    auto make_handler = [this, dispatcher] {
      return fpci::Service::InstanceHandler({
          .device = bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure),
      });
    };
    if (zx::result result = outgoing_->AddService<fpci::Service>(make_handler(), "default");
        result.is_error()) {
      return result.status_value();
    }
    if (zx::result result = outgoing_->AddService<fpci::Service>(make_handler(), "pci");
        result.is_error()) {
      return result.status_value();
    }
    return outgoing_->Serve(std::move(server_end)).status_value();
  }

  void Reset() {
    outgoing_.reset();
    bindings_.RemoveAll();
  }

  void NotImplemented_(const std::string& name, fidl::CompleterBase& completer) override {
    completer.Close(ZX_ERR_NOT_SUPPORTED);
  }

  void GetDeviceInfo(GetDeviceInfoCompleter::Sync& completer) override {
    fpci::wire::DeviceInfo info = {};
    info.vendor_id = kVendorId;
    info.device_id = kDeviceId;
    info.revision_id = kRevisionId;
    completer.Reply(info);
  }

  void GetBar(GetBarRequestView request, GetBarCompleter::Sync& completer) override {
    if (request->bar_id == 0) {
      zx::vmo vmo;
      zx_status_t status = zx::vmo::create(4096, 0, &vmo);
      if (status != ZX_OK) {
        completer.ReplyError(status);
        return;
      }
      completer.ReplySuccess({
          .bar_id = 0,
          .size = 4096,
          .result = fpci::wire::BarResult::WithVmo(std::move(vmo)),
      });
      return;
    }
    if (request->bar_id == 1) {
      zx_handle_t rsrc_handle = ZX_HANDLE_INVALID;
      zx_status_t status = fake_resource_create(ZX_RSRC_KIND_IOPORT, &rsrc_handle);
      if (status != ZX_OK) {
        completer.ReplyError(status);
        return;
      }
      fpci::wire::IoBar io_bar{
          .address = 0x1000,
          .resource = zx::resource(rsrc_handle),
      };
      completer.ReplySuccess({
          .bar_id = 1,
          .size = 64,
          .result = fpci::wire::BarResult::WithIo(
              fidl::ObjectView<fpci::wire::IoBar>::FromExternal(&io_bar)),
      });
      return;
    }
    completer.ReplyError(ZX_ERR_NOT_FOUND);
  }

  void SetAvailableInterruptModes(fpci::wire::InterruptModes modes) { available_modes_ = modes; }
  void SetFailMode(fpci::InterruptMode mode) { fail_mode_ = mode; }

  void GetInterruptModes(GetInterruptModesCompleter::Sync& completer) override {
    completer.Reply(available_modes_);
  }

  void SetInterruptMode(SetInterruptModeRequestView request,
                        SetInterruptModeCompleter::Sync& completer) override {
    if (request->mode == fail_mode_) {
      completer.ReplyError(ZX_ERR_NOT_SUPPORTED);
      return;
    }
    current_mode_ = request->mode;
    completer.ReplySuccess();
  }

  void MapInterrupt(MapInterruptRequestView request,
                    MapInterruptCompleter::Sync& completer) override {
    zx::interrupt interrupt;
    zx_status_t status = zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt);
    if (status != ZX_OK) {
      completer.ReplyError(status);
      return;
    }
    completer.ReplySuccess(std::move(interrupt));
  }

  void AckInterrupt(AckInterruptCompleter::Sync& completer) override {
    acked_ = true;
    completer.ReplySuccess();
  }

  void ReadConfig8(ReadConfig8RequestView request, ReadConfig8Completer::Sync& completer) override {
    last_read_offset_ = request->offset;
    completer.ReplySuccess(static_cast<uint8_t>(kConfig32Value & 0xffu));
  }

  void ReadConfig16(ReadConfig16RequestView request,
                    ReadConfig16Completer::Sync& completer) override {
    last_read_offset_ = request->offset;
    completer.ReplySuccess(static_cast<uint16_t>(kConfig32Value & 0xffffu));
  }

  void ReadConfig32(ReadConfig32RequestView request,
                    ReadConfig32Completer::Sync& completer) override {
    last_read_offset_ = request->offset;
    completer.ReplySuccess(kConfig32Value);
  }

  void WriteConfig8(WriteConfig8RequestView request,
                    WriteConfig8Completer::Sync& completer) override {
    last_write_offset_ = request->offset;
    written_config_ = request->value;
    completer.ReplySuccess();
  }

  void WriteConfig16(WriteConfig16RequestView request,
                     WriteConfig16Completer::Sync& completer) override {
    last_write_offset_ = request->offset;
    written_config_ = request->value;
    completer.ReplySuccess();
  }

  void WriteConfig32(WriteConfig32RequestView request,
                     WriteConfig32Completer::Sync& completer) override {
    last_write_offset_ = request->offset;
    written_config_ = request->value;
    completer.ReplySuccess();
  }

  void GetCapabilities(GetCapabilitiesRequestView request,
                       GetCapabilitiesCompleter::Sync& completer) override {
    fidl::Arena arena;
    if (request->id == fpci::CapabilityId::kVendor) {
      fidl::VectorView<uint8_t> offsets(arena, 2);
      offsets[0] = 0x40;
      offsets[1] = 0x60;
      completer.Reply(offsets);
    } else if (request->id == fpci::CapabilityId::kPciExpress) {
      fidl::VectorView<uint8_t> offsets(arena, 1);
      offsets[0] = 0x70;
      completer.Reply(offsets);
    } else if (request->id == fpci::CapabilityId::kMsi) {
      fidl::VectorView<uint8_t> offsets(arena, 1);
      offsets[0] = 0x50;
      completer.Reply(offsets);
    } else {
      fidl::VectorView<uint8_t> offsets(arena, 0);
      completer.Reply(offsets);
    }
  }

  void GetExtendedCapabilities(GetExtendedCapabilitiesRequestView request,
                               GetExtendedCapabilitiesCompleter::Sync& completer) override {
    fidl::Arena arena;
    if (request->id == fpci::ExtendedCapabilityId::kVendor) {
      fidl::VectorView<uint16_t> offsets(arena, 2);
      offsets[0] = 0x100;
      offsets[1] = 0x140;
      completer.Reply(offsets);
    } else if (request->id == fpci::ExtendedCapabilityId::kVirtualChannelNoMfvc) {
      fidl::VectorView<uint16_t> offsets(arena, 1);
      offsets[0] = 0x180;
      completer.Reply(offsets);
    } else {
      fidl::VectorView<uint16_t> offsets(arena, 0);
      completer.Reply(offsets);
    }
  }

  void SetBusMastering(SetBusMasteringRequestView request,
                       SetBusMasteringCompleter::Sync& completer) override {
    bus_mastering_ = request->enabled;
    completer.ReplySuccess();
  }

  void ResetDevice(ResetDeviceCompleter::Sync& completer) override {
    reset_called_ = true;
    completer.ReplySuccess();
  }

  void GetBti(GetBtiRequestView request, GetBtiCompleter::Sync& completer) override {
    zx::result<zx::bti> bti = fake_bti::CreateFakeBti();
    if (bti.is_error()) {
      completer.ReplyError(bti.status_value());
      return;
    }
    completer.ReplySuccess(std::move(*bti));
  }

  fpci::InterruptMode current_mode() const { return current_mode_; }
  bool acked() const { return acked_; }
  uint16_t last_read_offset() const { return last_read_offset_; }
  uint16_t last_write_offset() const { return last_write_offset_; }
  uint32_t written_config() const { return written_config_; }
  bool bus_mastering() const { return bus_mastering_; }
  bool reset_called() const { return reset_called_; }

 private:
  fpci::wire::InterruptModes available_modes_{
      .has_legacy = true,
      .msi_count = 4,
      .msix_count = 8,
  };
  fpci::InterruptMode fail_mode_ = fpci::InterruptMode::kDisabled;
  fpci::InterruptMode current_mode_ = fpci::InterruptMode::kDisabled;
  bool acked_ = false;
  uint16_t last_read_offset_ = 0;
  uint16_t last_write_offset_ = 0;
  uint32_t written_config_ = 0;
  bool bus_mastering_ = false;
  bool reset_called_ = false;
  fidl::ServerBindingGroup<fpci::Device> bindings_;
  std::optional<component::OutgoingDirectory> outgoing_;
};

class PciClientTest : public ::testing::Test {
 public:
  void SetUp() override {
    loop_.StartThread("pci-fidl-server-thread");
    auto endpoints = fidl::Endpoints<fpci::Device>::Create();
    ASSERT_OK(server_.SyncCall(&FakePciDeviceServer::Bind, std::move(endpoints.server)));
    pci_ = fdf::Pci(std::move(endpoints.client));
    ASSERT_TRUE(pci_.is_valid());
  }

  void TearDown() override {
    server_.SyncCall(&FakePciDeviceServer::Reset);
    server_.reset();
    loop_.Quit();
    loop_.JoinThreads();
  }

 protected:
  async_patterns::TestDispatcherBound<FakePciDeviceServer>& server() { return server_; }
  fdf::Pci& pci() { return pci_; }

 private:
  async::Loop loop_{&kAsyncLoopConfigNoAttachToCurrentThread};
  async_patterns::TestDispatcherBound<FakePciDeviceServer> server_{loop_.dispatcher(),
                                                                   std::in_place};
  fdf::Pci pci_;
};

TEST_F(PciClientTest, DeviceInfo) {
  fdf::Pci default_pci;
  EXPECT_FALSE(default_pci.is_valid());
  EXPECT_FALSE(default_pci.borrow().is_valid());
  EXPECT_TRUE(pci().borrow().is_valid());

  EXPECT_STATUS(pci().GetDeviceInfo(nullptr), ZX_ERR_INVALID_ARGS);

  fpci::wire::DeviceInfo info;
  ASSERT_OK(pci().GetDeviceInfo(&info));
  EXPECT_EQ(info.vendor_id, kVendorId);
  EXPECT_EQ(info.device_id, kDeviceId);
  EXPECT_EQ(info.revision_id, kRevisionId);
}

TEST_F(PciClientTest, ConnectFactory) {
  std::shared_ptr<fdf::Namespace> null_ns;
  EXPECT_STATUS(fdf::Pci::Connect(null_ns).status_value(), ZX_ERR_INVALID_ARGS);

  auto [svc_client, svc_server] = fidl::Endpoints<fuchsia_io::Directory>::Create();
  ASSERT_OK(server().SyncCall(&FakePciDeviceServer::ServeOutgoing, std::move(svc_server)));

  std::vector<fuchsia_component_runner::ComponentNamespaceEntry> entries;
  entries.push_back(fuchsia_component_runner::ComponentNamespaceEntry({
      .path = "/",
      .directory = std::move(svc_client),
  }));
  zx::result<fdf::Namespace> ns = fdf::Namespace::Create(entries);
  ASSERT_OK(ns);

  fpci::wire::DeviceInfo info;

  zx::result<fdf::Pci> pci = fdf::Pci::Connect(*ns, "pci");
  ASSERT_OK(pci);
  EXPECT_TRUE(pci->is_valid());
  ASSERT_OK(pci->GetDeviceInfo(&info));
  EXPECT_EQ(info.vendor_id, kVendorId);
  EXPECT_EQ(info.device_id, kDeviceId);

  // Verify default instance argument ("pci").
  zx::result<fdf::Pci> pci_default = fdf::Pci::Connect(*ns);
  ASSERT_OK(pci_default);
  EXPECT_TRUE(pci_default->is_valid());
  ASSERT_OK(pci_default->GetDeviceInfo(&info));
  EXPECT_EQ(info.vendor_id, kVendorId);
  EXPECT_EQ(info.device_id, kDeviceId);

  zx::result<fdf::Pci> pci_instance_default = fdf::Pci::Connect(*ns, "default");
  ASSERT_OK(pci_instance_default);
  EXPECT_TRUE(pci_instance_default->is_valid());
  ASSERT_OK(pci_instance_default->GetDeviceInfo(&info));
  EXPECT_EQ(info.vendor_id, kVendorId);
  EXPECT_EQ(info.device_id, kDeviceId);

  auto shared_ns = std::make_shared<fdf::Namespace>(std::move(*ns));
  zx::result<fdf::Pci> pci_shared = fdf::Pci::Connect(shared_ns, "pci");
  ASSERT_OK(pci_shared);
  EXPECT_TRUE(pci_shared->is_valid());
  ASSERT_OK(pci_shared->GetDeviceInfo(&info));
  EXPECT_EQ(info.vendor_id, kVendorId);
  EXPECT_EQ(info.device_id, kDeviceId);

  // Verify error propagation when namespace has no /svc.
  fdf::Namespace empty_ns;
  EXPECT_TRUE(fdf::Pci::Connect(empty_ns).is_error());
}

TEST_F(PciClientTest, GetBar) {
  fidl::Arena arena;
  fpci::wire::Bar bar;
  EXPECT_STATUS(pci().GetBar(arena, 0, nullptr), ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(pci().GetBar(arena, 99, &bar), ZX_ERR_NOT_FOUND);

  ASSERT_OK(pci().GetBar(arena, 0, &bar));
  EXPECT_EQ(bar.bar_id, 0u);
  EXPECT_EQ(bar.size, 4096u);
  EXPECT_TRUE(bar.result.is_vmo());

  ASSERT_OK(pci().GetBar(arena, 1, &bar));
  EXPECT_EQ(bar.bar_id, 1u);
  EXPECT_EQ(bar.size, 64u);
  EXPECT_TRUE(bar.result.is_io());
  EXPECT_EQ(bar.result.io().address, 0x1000u);

  // Calling MapMmio on an I/O BAR should return ZX_ERR_WRONG_TYPE.
  EXPECT_STATUS(pci().MapMmio(1).status_value(), ZX_ERR_WRONG_TYPE);
  std::optional<fdf::MmioBuffer> mmio;
  EXPECT_STATUS(pci().MapMmio(1, ZX_CACHE_POLICY_UNCACHED_DEVICE, &mmio), ZX_ERR_WRONG_TYPE);
}

TEST_F(PciClientTest, MapMmio) {
  std::optional<fdf::MmioBuffer> mmio;
  EXPECT_STATUS(pci().MapMmio(0, ZX_CACHE_POLICY_UNCACHED_DEVICE, nullptr), ZX_ERR_INVALID_ARGS);

  ASSERT_OK(pci().MapMmio(0, ZX_CACHE_POLICY_UNCACHED_DEVICE, &mmio));
  ASSERT_TRUE(mmio.has_value());
  EXPECT_EQ(mmio->get_size(), 4096u);

  zx::result<fdf::MmioBuffer> mmio_res = pci().MapMmio(0);
  ASSERT_OK(mmio_res);
  EXPECT_EQ(mmio_res->get_size(), 4096u);

  EXPECT_STATUS(pci().MapMmio(99).status_value(), ZX_ERR_NOT_FOUND);
}

TEST_F(PciClientTest, Interrupts) {
  // Verify passing nullptr does not fault.
  pci().GetInterruptModes(nullptr);

  fpci::wire::InterruptModes modes;
  pci().GetInterruptModes(&modes);
  EXPECT_TRUE(modes.has_legacy);
  EXPECT_EQ(modes.msi_count, 4u);
  EXPECT_EQ(modes.msix_count, 8u);

  EXPECT_STATUS(pci().ConfigureInterruptMode(0), ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(pci().ConfigureInterruptMode(100), ZX_ERR_NOT_SUPPORTED);

  // Preferred mode selection (MSI-X).
  fpci::InterruptMode configured_mode;
  ASSERT_OK(pci().ConfigureInterruptMode(4, &configured_mode));
  EXPECT_EQ(configured_mode, fpci::InterruptMode::kMsiX);

  // Null out_mode parameter.
  ASSERT_OK(pci().ConfigureInterruptMode(4, nullptr));

  // Fallback to MSI when MSI-X is unavailable.
  server().SyncCall([](FakePciDeviceServer* server) {
    server->SetAvailableInterruptModes({.has_legacy = true, .msi_count = 4, .msix_count = 0});
  });
  ASSERT_OK(pci().ConfigureInterruptMode(4, &configured_mode));
  EXPECT_EQ(configured_mode, fpci::InterruptMode::kMsi);

  // Fallback to Legacy when MSI-X and MSI are unavailable.
  server().SyncCall([](FakePciDeviceServer* server) {
    server->SetAvailableInterruptModes({.has_legacy = true, .msi_count = 0, .msix_count = 0});
  });
  ASSERT_OK(pci().ConfigureInterruptMode(1, &configured_mode));
  EXPECT_EQ(configured_mode, fpci::InterruptMode::kLegacy);

  // Fallback to MSI when MSI-X SetInterruptMode fails.
  server().SyncCall([](FakePciDeviceServer* server) {
    server->SetAvailableInterruptModes({.has_legacy = true, .msi_count = 4, .msix_count = 8});
    server->SetFailMode(fpci::InterruptMode::kMsiX);
  });
  ASSERT_OK(pci().ConfigureInterruptMode(4, &configured_mode));
  EXPECT_EQ(configured_mode, fpci::InterruptMode::kMsi);

  // Direct SetInterruptMode calls.
  server().SyncCall([](FakePciDeviceServer* server) {
    server->SetFailMode(fpci::InterruptMode::kDisabled);
    server->SetAvailableInterruptModes({.has_legacy = true, .msi_count = 4, .msix_count = 8});
  });
  ASSERT_OK(pci().SetInterruptMode(fpci::InterruptMode::kMsiX, 8));
  server().SyncCall([](FakePciDeviceServer* server) {
    EXPECT_EQ(server->current_mode(), fpci::InterruptMode::kMsiX);
  });

  server().SyncCall(
      [](FakePciDeviceServer* server) { server->SetFailMode(fpci::InterruptMode::kLegacy); });
  EXPECT_STATUS(pci().SetInterruptMode(fpci::InterruptMode::kLegacy, 1), ZX_ERR_NOT_SUPPORTED);

  // Reset fail mode and available modes.
  server().SyncCall([](FakePciDeviceServer* server) {
    server->SetFailMode(fpci::InterruptMode::kDisabled);
    server->SetAvailableInterruptModes({.has_legacy = true, .msi_count = 4, .msix_count = 8});
  });

  zx::interrupt interrupt;
  EXPECT_STATUS(pci().MapInterrupt(0, nullptr), ZX_ERR_INVALID_ARGS);
  ASSERT_OK(pci().MapInterrupt(0, &interrupt));
  ASSERT_TRUE(interrupt.is_valid());

  ASSERT_OK(pci().AckInterrupt());

  server().SyncCall([](FakePciDeviceServer* server) { EXPECT_TRUE(server->acked()); });
}

TEST_F(PciClientTest, ConfigSpace) {
  uint8_t val8 = 0;
  ASSERT_OK(pci().ReadConfig8(0x04, &val8));
  EXPECT_EQ(val8, static_cast<uint8_t>(kConfig32Value & 0xffu));
  server().SyncCall(
      [](FakePciDeviceServer* server) { EXPECT_EQ(server->last_read_offset(), 0x04u); });

  uint16_t val16 = 0;
  ASSERT_OK(pci().ReadConfig16(0x06, &val16));
  EXPECT_EQ(val16, static_cast<uint16_t>(kConfig32Value & 0xffffu));
  server().SyncCall(
      [](FakePciDeviceServer* server) { EXPECT_EQ(server->last_read_offset(), 0x06u); });

  uint32_t val32 = 0;
  ASSERT_OK(pci().ReadConfig32(0x08, &val32));
  EXPECT_EQ(val32, kConfig32Value);
  server().SyncCall(
      [](FakePciDeviceServer* server) { EXPECT_EQ(server->last_read_offset(), 0x08u); });

  EXPECT_STATUS(pci().ReadConfig8(0, nullptr), ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(pci().ReadConfig16(0, nullptr), ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(pci().ReadConfig32(0, nullptr), ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(pci().ReadConfig8(fpci::Config::kHeaderType, nullptr), ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(pci().ReadConfig16(fpci::Config::kVendorId, nullptr), ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(pci().ReadConfig32(fpci::Config::kBaseAddresses, nullptr), ZX_ERR_INVALID_ARGS);

  // Strongly-typed Config enum overloads.
  uint8_t cfg8 = 0;
  ASSERT_OK(pci().ReadConfig8(fpci::Config::kHeaderType, &cfg8));
  EXPECT_EQ(cfg8, static_cast<uint8_t>(kConfig32Value & 0xffu));
  server().SyncCall(
      [](FakePciDeviceServer* server) { EXPECT_EQ(server->last_read_offset(), 0x0Eu); });

  uint16_t cfg16 = 0;
  ASSERT_OK(pci().ReadConfig16(fpci::Config::kVendorId, &cfg16));
  EXPECT_EQ(cfg16, static_cast<uint16_t>(kConfig32Value & 0xffffu));
  server().SyncCall(
      [](FakePciDeviceServer* server) { EXPECT_EQ(server->last_read_offset(), 0x00u); });

  uint32_t cfg32 = 0;
  ASSERT_OK(pci().ReadConfig32(fpci::Config::kBaseAddresses, &cfg32));
  EXPECT_EQ(cfg32, kConfig32Value);
  server().SyncCall(
      [](FakePciDeviceServer* server) { EXPECT_EQ(server->last_read_offset(), 0x10u); });

  ASSERT_OK(pci().WriteConfig8(0x04, 0x12));
  server().SyncCall([](FakePciDeviceServer* server) {
    EXPECT_EQ(server->last_write_offset(), 0x04u);
    EXPECT_EQ(server->written_config(), 0x12u);
  });

  ASSERT_OK(pci().WriteConfig16(0x06, 0x1234));
  server().SyncCall([](FakePciDeviceServer* server) {
    EXPECT_EQ(server->last_write_offset(), 0x06u);
    EXPECT_EQ(server->written_config(), 0x1234u);
  });

  ASSERT_OK(pci().WriteConfig32(0x08, 0x12345678));
  server().SyncCall([](FakePciDeviceServer* server) {
    EXPECT_EQ(server->last_write_offset(), 0x08u);
    EXPECT_EQ(server->written_config(), 0x12345678u);
  });
}

TEST_F(PciClientTest, Capabilities) {
  EXPECT_STATUS(pci().GetFirstCapability(fpci::CapabilityId::kVendor, nullptr),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(pci().GetNextCapability(fpci::CapabilityId::kVendor, 0x40, nullptr),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(pci().GetFirstExtendedCapability(fpci::ExtendedCapabilityId::kVendor, nullptr),
                ZX_ERR_INVALID_ARGS);
  EXPECT_STATUS(
      pci().GetNextExtendedCapability(fpci::ExtendedCapabilityId::kVendor, 0x100, nullptr),
      ZX_ERR_INVALID_ARGS);

  uint8_t offset = 0;
  ASSERT_OK(pci().GetFirstCapability(fpci::CapabilityId::kVendor, &offset));
  EXPECT_EQ(offset, 0x40u);

  ASSERT_OK(pci().GetNextCapability(fpci::CapabilityId::kVendor, 0x40, &offset));
  EXPECT_EQ(offset, 0x60u);

  EXPECT_STATUS(pci().GetNextCapability(fpci::CapabilityId::kVendor, 0x60, &offset),
                ZX_ERR_NOT_FOUND);
  EXPECT_STATUS(pci().GetNextCapability(fpci::CapabilityId::kVendor, 0x99, &offset),
                ZX_ERR_NOT_FOUND);

  EXPECT_STATUS(pci().GetFirstCapability(fpci::CapabilityId::kNull, &offset), ZX_ERR_NOT_FOUND);
  EXPECT_STATUS(pci().GetNextCapability(fpci::CapabilityId::kNull, 0x00, &offset),
                ZX_ERR_NOT_FOUND);

  // Single-element capability vector (GetFirst succeeds, GetNext returns NOT_FOUND).
  uint8_t single_offset = 0;
  ASSERT_OK(pci().GetFirstCapability(fpci::CapabilityId::kPciExpress, &single_offset));
  EXPECT_EQ(single_offset, 0x70u);
  EXPECT_STATUS(pci().GetNextCapability(fpci::CapabilityId::kPciExpress, 0x70, &single_offset),
                ZX_ERR_NOT_FOUND);

  uint16_t ext_offset = 0;
  ASSERT_OK(pci().GetFirstExtendedCapability(fpci::ExtendedCapabilityId::kVendor, &ext_offset));
  EXPECT_EQ(ext_offset, 0x100u);

  ASSERT_OK(
      pci().GetNextExtendedCapability(fpci::ExtendedCapabilityId::kVendor, 0x100, &ext_offset));
  EXPECT_EQ(ext_offset, 0x140u);

  EXPECT_STATUS(
      pci().GetNextExtendedCapability(fpci::ExtendedCapabilityId::kVendor, 0x140, &ext_offset),
      ZX_ERR_NOT_FOUND);
  EXPECT_STATUS(
      pci().GetNextExtendedCapability(fpci::ExtendedCapabilityId::kVendor, 0x200, &ext_offset),
      ZX_ERR_NOT_FOUND);

  // Single-element extended capability vector.
  uint16_t single_ext_offset = 0;
  ASSERT_OK(pci().GetFirstExtendedCapability(fpci::ExtendedCapabilityId::kVirtualChannelNoMfvc,
                                             &single_ext_offset));
  EXPECT_EQ(single_ext_offset, 0x180u);
  EXPECT_STATUS(pci().GetNextExtendedCapability(fpci::ExtendedCapabilityId::kVirtualChannelNoMfvc,
                                                0x180, &single_ext_offset),
                ZX_ERR_NOT_FOUND);

  EXPECT_STATUS(pci().GetFirstExtendedCapability(fpci::ExtendedCapabilityId::kNull, &ext_offset),
                ZX_ERR_NOT_FOUND);
  EXPECT_STATUS(
      pci().GetNextExtendedCapability(fpci::ExtendedCapabilityId::kNull, 0x00, &ext_offset),
      ZX_ERR_NOT_FOUND);
}

TEST_F(PciClientTest, DeviceControlAndBti) {
  ASSERT_OK(pci().SetBusMastering(true));
  server().SyncCall([](FakePciDeviceServer* server) { EXPECT_TRUE(server->bus_mastering()); });

  ASSERT_OK(pci().SetBusMastering(false));
  server().SyncCall([](FakePciDeviceServer* server) { EXPECT_FALSE(server->bus_mastering()); });

  ASSERT_OK(pci().ResetDevice());
  server().SyncCall([](FakePciDeviceServer* server) { EXPECT_TRUE(server->reset_called()); });

  EXPECT_STATUS(pci().GetBti(0, nullptr), ZX_ERR_INVALID_ARGS);
  zx::bti bti;
  ASSERT_OK(pci().GetBti(0, &bti));
  ASSERT_TRUE(bti.is_valid());
}

TEST(PciClientStandaloneTest, DisconnectedClient) {
  auto endpoints = fidl::Endpoints<fpci::Device>::Create();
  endpoints.server.reset();
  fdf::Pci pci(std::move(endpoints.client));
  EXPECT_TRUE(pci.is_valid());

  fpci::wire::DeviceInfo info;
  EXPECT_STATUS(pci.GetDeviceInfo(&info), ZX_ERR_PEER_CLOSED);

  fpci::wire::InterruptModes modes{
      .has_legacy = true,
      .msi_count = 4,
      .msix_count = 8,
  };
  pci.GetInterruptModes(&modes);
  EXPECT_FALSE(modes.has_legacy);
  EXPECT_EQ(modes.msi_count, 0u);
  EXPECT_EQ(modes.msix_count, 0u);

  EXPECT_STATUS(pci.ConfigureInterruptMode(1), ZX_ERR_PEER_CLOSED);
  EXPECT_STATUS(pci.SetInterruptMode(fpci::InterruptMode::kLegacy, 1), ZX_ERR_PEER_CLOSED);

  zx::interrupt interrupt;
  EXPECT_STATUS(pci.MapInterrupt(0, &interrupt), ZX_ERR_PEER_CLOSED);
  EXPECT_STATUS(pci.AckInterrupt(), ZX_ERR_PEER_CLOSED);

  fidl::Arena arena;
  fpci::wire::Bar bar;
  EXPECT_STATUS(pci.GetBar(arena, 0, &bar), ZX_ERR_PEER_CLOSED);
  EXPECT_STATUS(pci.MapMmio(0).status_value(), ZX_ERR_PEER_CLOSED);

  uint8_t val8 = 0;
  EXPECT_STATUS(pci.ReadConfig8(0, &val8), ZX_ERR_PEER_CLOSED);
  uint16_t val16 = 0;
  EXPECT_STATUS(pci.ReadConfig16(0, &val16), ZX_ERR_PEER_CLOSED);
  uint32_t val32 = 0;
  EXPECT_STATUS(pci.ReadConfig32(0, &val32), ZX_ERR_PEER_CLOSED);

  EXPECT_STATUS(pci.WriteConfig8(0, 0), ZX_ERR_PEER_CLOSED);
  EXPECT_STATUS(pci.WriteConfig16(0, 0), ZX_ERR_PEER_CLOSED);
  EXPECT_STATUS(pci.WriteConfig32(0, 0), ZX_ERR_PEER_CLOSED);

  EXPECT_STATUS(pci.GetFirstCapability(fpci::CapabilityId::kPciExpress, &val8), ZX_ERR_PEER_CLOSED);
  EXPECT_STATUS(pci.GetNextCapability(fpci::CapabilityId::kPciExpress, 0, &val8),
                ZX_ERR_PEER_CLOSED);
  uint16_t ext_val16 = 0;
  EXPECT_STATUS(
      pci.GetFirstExtendedCapability(fpci::ExtendedCapabilityId::kVirtualChannelNoMfvc, &ext_val16),
      ZX_ERR_PEER_CLOSED);
  EXPECT_STATUS(pci.GetNextExtendedCapability(fpci::ExtendedCapabilityId::kVirtualChannelNoMfvc, 0,
                                              &ext_val16),
                ZX_ERR_PEER_CLOSED);

  EXPECT_STATUS(pci.SetBusMastering(true), ZX_ERR_PEER_CLOSED);
  EXPECT_STATUS(pci.ResetDevice(), ZX_ERR_PEER_CLOSED);

  zx::bti bti;
  EXPECT_STATUS(pci.GetBti(0, &bti), ZX_ERR_PEER_CLOSED);
}

}  // namespace

#endif  // FUCHSIA_API_LEVEL_AT_LEAST(HEAD)
