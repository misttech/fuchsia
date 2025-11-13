// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/usb/drivers/dwc3/dwc3.h"

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.hardware.clock/cpp/fidl.h>
#include <fidl/fuchsia.hardware.interconnect/cpp/fidl.h>
#include <fidl/fuchsia.hardware.platform.device/cpp/fidl.h>
#include <fidl/fuchsia.hardware.reset/cpp/fidl.h>
#include <fidl/fuchsia.hardware.usb.dci/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.descriptor/cpp/wire.h>
#include <fidl/fuchsia.hardware.usb.endpoint/cpp/wire.h>
#include <fidl/fuchsia.hardware.vreg/cpp/fidl.h>
#include <lib/ddk/metadata.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/component/cpp/node_add_args.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/platform-device/cpp/pdev.h>
#include <lib/fit/defer.h>
#include <lib/zx/clock.h>
#include <zircon/syscalls.h>
#include <zircon/threads.h>

#include <ranges>
#include <string>
#include <unordered_map>

#include <bind/fuchsia/cpp/bind.h>
#include <bind/fuchsia/designware/platform/cpp/bind.h>
#include <hwreg/bitfields.h>

#include "src/devices/usb/drivers/dwc3/dwc3-regs.h"

namespace dwc3 {

namespace fclock = fuchsia_hardware_clock;
namespace fdci = fuchsia_hardware_usb_dci;
namespace fdescriptor = fuchsia_hardware_usb_descriptor;
namespace fendpoint = fuchsia_hardware_usb_endpoint;
namespace fhi = fuchsia_hardware_interconnect;
namespace fpdev = fuchsia_hardware_platform_device;
namespace fphy = fuchsia_hardware_usb_phy;
namespace freset = fuchsia_hardware_reset;
namespace fvreg = fuchsia_hardware_vreg;

namespace {

class QualcommExtension final : public PlatformExtension {
  enum class BusPath : uint8_t { kUsbDdr, kUsbIpa, kDdrUsb };
  enum class State : uint8_t { kNone, kNominal, kSvs, kMin };

 public:
  class HsPhyCtrl : public hwreg::RegisterBase<HsPhyCtrl, uint32_t> {
   public:
    DEF_BIT(20, utmi_otg_vbus_valid);

    static auto Get() { return hwreg::RegisterAddr<HsPhyCtrl>(0xf'8810); }
  };

  static std::unique_ptr<QualcommExtension> Create(Dwc3* parent, const fdf::MmioView& mmio);

  QualcommExtension(const fdf::MmioView& mmio,
                    std::unordered_map<BusPath, fidl::ClientEnd<fhi::Path>> interconnect_clients,
                    std::unordered_map<std::string, fidl::ClientEnd<fclock::Clock>> clock_clients,
                    fidl::ClientEnd<freset::Reset> reset_client,
                    fidl::ClientEnd<fvreg::Vreg> regulator_client)
      : mmio_(mmio),
        interconnect_clients_{std::move(interconnect_clients)},
        clock_clients_{std::move(clock_clients)},
        reset_client_(std::move(reset_client)),
        regulator_client_(std::move(regulator_client)) {}

  // PlatformExtension interface implementation.
  zx::result<> Start() override { return PowerOn(true); }
  zx::result<> Suspend() override {
    HsPhyCtrl::Get().ReadFrom(&mmio_).set_utmi_otg_vbus_valid(false).WriteTo(&mmio_);
    return PowerOff();
  }
  zx::result<> Resume() override {
    if (zx::result<> result = PowerOn(false); result.is_error()) {
      return result;
    }
    HsPhyCtrl::Get().ReadFrom(&mmio_).set_utmi_otg_vbus_valid(true).WriteTo(&mmio_);
    return zx::ok();
  }

 private:
  zx::result<> PowerOn(bool driver_start) {
    if (power_on_) {
      return zx::ok();
    }

    if (zx::result<> result = VoteBandwidth(State::kSvs); result.is_error()) {
      return result.take_error();
    }

    if (zx::result<> result = VoteVoltage(true); result.is_error()) {
      return result.take_error();
    }

    // Only reset after being powered down, not during driver start.
    if (!driver_start) {
      if (fidl::Result result = fidl::Call(reset_client_)->ToggleWithTimeout(zx::usec(1500).get());
          result.is_error()) {
        FDF_LOG(ERROR, "Failed to toggle reset %s",
                result.error_value().FormatDescription().c_str());
        return zx::error(result.error_value().is_domain_error()
                             ? result.error_value().domain_error()
                             : result.error_value().framework_error().status());
      }
    }

    if (zx::result<> result = VoteClocks(true); result.is_error()) {
      return result;
    }

    fdf::info("Qualcomm extension: dwc3 core powered on");

    power_on_ = true;
    return zx::ok();
  }

  zx::result<> PowerOff() {
    if (!power_on_) {
      return zx::ok();
    }

    if (zx::result<> result = VoteClocks(false); result.is_error()) {
      return result.take_error();
    }

    if (zx::result<> result = VoteVoltage(false); result.is_error()) {
      return result.take_error();
    }

    if (zx::result<> result = VoteBandwidth(State::kNone); result.is_error()) {
      return result.take_error();
    }

    fdf::info("Qualcomm extension: dwc3 core powered off");

    power_on_ = false;
    return zx::ok();
  }

  zx::result<> VoteBandwidth(State state);
  zx::result<> VoteVoltage(bool on);
  zx::result<> VoteClocks(bool on);

  State state_ = State::kNone;
  fdf::MmioView mmio_;
  std::unordered_map<BusPath, fidl::ClientEnd<fhi::Path>> interconnect_clients_;
  std::unordered_map<std::string, fidl::ClientEnd<fvreg::Vreg>> regulator_clients_;
  std::unordered_map<std::string, fidl::ClientEnd<fclock::Clock>> clock_clients_;
  fidl::ClientEnd<freset::Reset> reset_client_;
  fidl::ClientEnd<fvreg::Vreg> regulator_client_;
  bool power_on_{false};
};

std::unique_ptr<QualcommExtension> QualcommExtension::Create(Dwc3* parent,
                                                             const fdf::MmioView& mmio) {
  // Get all resources.
  static const std::unordered_map<BusPath, const std::string> kBusPathNames{
      {BusPath::kUsbDdr, "interconnect-usb-ddr"},
      {BusPath::kUsbIpa, "interconnect-usb-ipa"},
      {BusPath::kDdrUsb, "interconnect-ddr-usb"}};

  static const std::vector<std::string> kClockNames{"core-clk", "iface-clk", "bus-aggr-clk",
                                                    "xo",       "sleep-clk", "utmi-clk"};

  if (mmio.get_size() < HsPhyCtrl::Get().addr() + sizeof(HsPhyCtrl::ValueType)) {
    fdf::info("MMIO is too small to be qualcomm chipset");
    return nullptr;
  }

  std::unordered_map<BusPath, fidl::ClientEnd<fhi::Path>> interconnect_clients;
  for (const auto& [path, node_name] : kBusPathNames) {
    zx::result client = parent->incoming()->Connect<fhi::PathService::Path>(node_name);
    if (client.is_error()) {
      fdf::info("Failed to get interconnect {}, assuming not qualcomm chipset", node_name);
      return nullptr;
    }
    interconnect_clients[path] = std::move(*client);
  }

  std::unordered_map<std::string, fidl::ClientEnd<fclock::Clock>> clock_clients;
  for (const auto& name : kClockNames) {
    zx::result client = parent->incoming()->Connect<fclock::Service::Clock>(name);
    if (client.is_error()) {
      fdf::info("Failed to get clock {}, assuming not qualcomm chipset", name);
      return nullptr;
    }
    clock_clients[name] = std::move(*client);
  }

  zx::result reset_client = parent->incoming()->Connect<freset::Service::Reset>("reset");
  if (reset_client.is_error()) {
    fdf::info("Failed to get reset, assuming not qualcomm chipset");
    return nullptr;
  }

  zx::result regulator_client = parent->incoming()->Connect<fvreg::Service::Vreg>("regulator");
  if (regulator_client.is_error()) {
    fdf::info("Failed to get regulator, assuming not qualcomm chipset");
    return nullptr;
  }

  return std::make_unique<QualcommExtension>(mmio, std::move(interconnect_clients),
                                             std::move(clock_clients), *std::move(reset_client),
                                             *std::move(regulator_client));
}

zx::result<> QualcommExtension::VoteBandwidth(State state) {
  static const std::unordered_map<State, std::unordered_map<BusPath, std::pair<uint32_t, uint32_t>>>
      kVoteMap = {
          {
              State::kNone,
              {
                  {BusPath::kUsbDdr, {0, 0}},
                  {BusPath::kUsbIpa, {0, 0}},
                  {BusPath::kDdrUsb, {0, 0}},
              },
          },
          {
              State::kNominal,
              {
                  {BusPath::kUsbDdr, {1'000'000, 1'250'000}},
                  {BusPath::kUsbIpa, {0, 2'400'000}},
                  {BusPath::kDdrUsb, {0, 40'000'000}},
              },
          },
          {
              State::kSvs,
              {
                  {BusPath::kUsbDdr, {240'000'000, 700'000'000}},
                  {BusPath::kUsbIpa, {0, 2'400'000}},
                  {BusPath::kDdrUsb, {0, 40'000'000}},
              },
          },
          {
              State::kMin,
              {
                  {BusPath::kUsbDdr, {1'000, 1'000}},
                  {BusPath::kUsbIpa, {1'000, 1'000}},
                  {BusPath::kDdrUsb, {1'000, 1'000}},
              },
          },
      };

  if (state_ == state) {
    // Already in the correct state
    return zx::ok();
  }
  state_ = state;

  for (const auto& [path, vote] : kVoteMap.at(state_)) {
    const auto& [average, peak] = vote;
    fidl::Result result = fidl::Call(interconnect_clients_.at(path))
                              ->SetBandwidth({{
                                  .average_bandwidth_bps = average,
                                  .peak_bandwidth_bps = peak,
                              }});
    if (result.is_error()) {
      fdf::error("Failed to set bandwidth: {}", result.error_value());
      return zx::error(ZX_ERR_NOT_SUPPORTED);
    }
  }

  return zx::ok();
}

zx::result<> QualcommExtension::VoteVoltage(bool on) {
  if (on) {
    fidl::Result enable = fidl::Call(regulator_client_)->Enable();
    if (enable.is_error()) {
      fdf::error("failed to enable regulator: {}", enable.error_value());
      return zx::error(enable.error_value().is_domain_error()
                           ? enable.error_value().domain_error()
                           : enable.error_value().framework_error().status());
    }
  } else {
    fidl::Result disable = fidl::Call(regulator_client_)->Disable();
    if (disable.is_error()) {
      fdf::error("failed to disable regulator: {}", disable.error_value());
      return zx::error(disable.error_value().is_domain_error()
                           ? disable.error_value().domain_error()
                           : disable.error_value().framework_error().status());
    }
  }

  return zx::ok();
}

zx::result<> QualcommExtension::VoteClocks(bool on) {
  constexpr std::array<std::string, 6> kClockNames{
      "xo", "sleep-clk", "iface-clk", "core-clk", "utmi-clk", "bus-aggr-clk",
  };

  if (on) {
    for (const std::string& name : kClockNames) {
      fidl::Result enable = fidl::Call(clock_clients_.at(name))->Enable();
      if (enable.is_error()) {
        fdf::error("could not enable clk {}: {}", name, enable.error_value());
        return zx::error(enable.error_value().is_domain_error()
                             ? enable.error_value().domain_error()
                             : enable.error_value().framework_error().status());
      }
    }
  } else {
    // Disable clocks in the opposite order.
    for (const std::string& name : kClockNames | std::views::reverse) {
      fidl::Result disable = fidl::Call(clock_clients_.at(name))->Disable();
      if (disable.is_error()) {
        fdf::error("could not disable clk {}: {}", name, disable.error_value());
        return zx::error(disable.error_value().is_domain_error()
                             ? disable.error_value().domain_error()
                             : disable.error_value().framework_error().status());
      }
    }
  }

  return zx::ok();
}

zx_status_t CacheFlushCommon(dma_buffer::ContiguousBuffer* buffer, zx_off_t offset, size_t length,
                             uint32_t flush_options) {
  if (offset + length < offset || offset + length > buffer->size()) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  auto virt{reinterpret_cast<const uint8_t*>(buffer->virt()) + offset};
  return zx_cache_flush(virt, length, flush_options);
}

}  // namespace

zx_status_t CacheFlush(dma_buffer::ContiguousBuffer* buffer, zx_off_t offset, size_t length) {
  return CacheFlushCommon(buffer, offset, length, ZX_CACHE_FLUSH_DATA);
}

zx_status_t CacheFlushInvalidate(dma_buffer::ContiguousBuffer* buffer, zx_off_t offset,
                                 size_t length) {
  return CacheFlushCommon(buffer, offset, length, ZX_CACHE_FLUSH_DATA | ZX_CACHE_FLUSH_INVALIDATE);
}

zx::result<> Dwc3::Start() {
  auto phy_client_end = incoming()->Connect<fphy::Service::Device>("dwc3-phy");
  if (phy_client_end.is_ok()) {
    phy_.Bind(*std::move(phy_client_end));
  }

  // Set up Inspect data.
  metrics_.Init();
  dwc3_root_ = inspector().root().CreateLazyNode(
      "dwc3", [this] { return fpromise::make_ok_promise(this->metrics_.RecordMetrics()); });

  if (zx_status_t status = AcquirePDevResources(); status != ZX_OK) {
    fdf::error("AcquirePDevResources: {}", zx_status_get_string(status));
    return zx::error(status);
  }

  if (std::unique_ptr extension = QualcommExtension::Create(this, get_mmio()->View(0)); extension) {
    if (zx::result result = extension->Start(); result.is_error()) {
      fdf::error("Failed platform extension start: {}", result);
      return result.take_error();
    }
    platform_extension_ = std::move(extension);

    auto connection_watcher_client_end =
        incoming()->Connect<fphy::ConnectionWatcherService::Watcher>("dwc3-phy");
    if (connection_watcher_client_end.is_ok()) {
      connection_watcher_.Bind(*std::move(connection_watcher_client_end),
                               fdf::Dispatcher::GetCurrent()->async_dispatcher());

      // Start the hanging-get call loop.
      connection_watcher_->WatchConnectStatusChanged().Then(
          fit::bind_member<&Dwc3::OnConnectStatusChanged>(this));
    }
  }

  if (zx_status_t status = Init(); status != ZX_OK) {
    return zx::error(status);
  }

  auto handler = bindings_.CreateHandler(this, dispatcher(), fidl::kIgnoreBindingClosure);

  auto serve_result =
      outgoing()->AddService<fdci::UsbDciService>(fdci::UsbDciService::InstanceHandler({
          .device = std::move(handler),
      }));

  if (serve_result.is_error()) {
    fdf::error("Failed to add service: {}", serve_result);
    return serve_result.take_error();
  }

  auto properties = std::vector{
      fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_VID,
                         bind_fuchsia_designware_platform::BIND_PLATFORM_DEV_VID_DESIGNWARE),
      fdf::MakeProperty2(bind_fuchsia::PLATFORM_DEV_DID,
                         bind_fuchsia_designware_platform::BIND_PLATFORM_DEV_DID_DWC3),
  };

  std::vector offers = {
      fdf::MakeOffer2<fdci::UsbDciService>(),
      mac_address_metadata_server_.MakeOffer(),
      serial_number_metadata_server_.MakeOffer(),
      usb_phy_metadata_server_.MakeOffer(),
  };

  auto child = AddChild(name(), properties, offers);
  if (child.is_error()) {
    fdf::error("AddChild(): {}", child);
    return child.take_error();
  }
  child_.Bind(std::move(*child));

  return zx::ok();
}

zx_status_t Dwc3::AcquirePDevResources() {
  auto pdev_client_end = incoming()->Connect<fpdev::Service::Device>("pdev");
  if (pdev_client_end.is_error()) {
    fdf::error("fidl::CreateEndpoints<fpdev::Service>(): {}", pdev_client_end);
    return pdev_client_end.error_value();
  }
  pdev_ = fdf::PDev{std::move(pdev_client_end.value())};

  // Initialize usb-phy metadata server.
  if (zx::result result = usb_phy_metadata_server_.SetMetadataFromPDevIfExists(pdev_);
      result.is_error()) {
    fdf::error("Failed to forward usb-phy metadata: {}", result);
    return result.status_value();
  }
  if (zx::result result = usb_phy_metadata_server_.Serve(*outgoing(), dispatcher());
      result.is_error()) {
    fdf::error("Failed to serve usb-phy address metadata: {}", result);
    return result.status_value();
  }

  // Initialize mac address metadata server.
  if (zx::result result = mac_address_metadata_server_.ForwardMetadataIfExists(incoming(), "pdev");
      result.is_error()) {
    fdf::error("Failed to forward mac address metadata: {}", result);
    return result.status_value();
  }
  if (zx::result result = mac_address_metadata_server_.Serve(*outgoing(), dispatcher());
      result.is_error()) {
    fdf::error("Failed to serve mac address metadata: {}", result);
    return result.status_value();
  }

  // Initialize serial number metadata server.
  if (zx::result result =
          serial_number_metadata_server_.ForwardMetadataIfExists(incoming(), "pdev");
      result.is_error()) {
    fdf::error("Failed to forward serial number metadata: {}", result);
    return result.status_value();
  }
  if (zx::result result = serial_number_metadata_server_.Serve(*outgoing(), dispatcher());
      result.is_error()) {
    fdf::error("Failed to serve serial number metadata: {}", result);
    return result.status_value();
  }

  auto mmio = pdev_.MapMmio(0);
  if (mmio.is_error()) {
    fdf::error("MapMmio failed: {}", mmio);
    return mmio.error_value();
  }
  mmio_ = std::move(*mmio);

  auto bti = pdev_.GetBti(0);
  if (bti.is_error()) {
    fdf::error("GetBti failed: {}", bti);
    return bti.error_value();
  }
  bti_ = std::move(*bti);

  auto irq = pdev_.GetInterrupt(0);
  if (irq.is_error()) {
    fdf::error("GetInterrupt failed: {}", irq);
    return irq.error_value();
  }
  irq_ = std::move(*irq);

  return ZX_OK;
}

zx_status_t Dwc3::Init() {
  // Start by identifying our hardware and making sure that we recognize it, and
  // it is a version that we know we can support.  Then, reset the hardware so
  // that we know it is in a good state.
  // Now that we have our registers, check to make sure that we are running on
  // a version of the hardware that we support.
  if (zx_status_t status = CheckHwVersion(); status != ZX_OK) {
    fdf::error("CheckHwVersion failed: {}", zx_status_get_string(status));
    return status;
  }

  // Now that we have our registers, reset the hardware.  This will ensure that
  // we are starting from a known state moving forward.
  if (zx_status_t status = ResetHw(); status != ZX_OK) {
    fdf::error("HW Reset Failed: {}", zx_status_get_string(status));
    return status;
  }

  // Finally, figure out the number of endpoints that this version of the
  // controller supports.
  uint32_t ep_count = GHWPARAMS3::Get().ReadFrom(get_mmio()).DWC_USB31_NUM_EPS();
  if (ep_count < (kUserEndpointStartNum + 1)) {
    fdf::error("HW supports only {} physical endpoints, but at least {} are needed to operate.",
               ep_count, (kUserEndpointStartNum + 1));
    return ZX_ERR_NOT_SUPPORTED;
  }

  // Now go ahead and allocate the user endpoint storage and uep servers.
  user_endpoints_.Init(ep_count - kUserEndpointStartNum, bti_, this);

  // Now that we have our BTI, and have reset our hardware, we can go ahead and
  // release the quarantine on any pages which may have been previously pinned
  // by this BTI.
  if (zx_status_t status = bti_.release_quarantine(); status != ZX_OK) {
    fdf::error("Release quarantine failed: {}", zx_status_get_string(status));
    return status;
  }

  // If something goes wrong after this point, make sure to release any of our
  // allocated dma buffers.
  auto cleanup = fit::defer([this]() { ReleaseResources(); });

  zx::result result = event_fifo_.Init(bti_);
  if (result.is_error()) {
    fdf::error("Event FIFO init failed: {}", result);
    return result.error_value();
  }

  // Now that we have allocated our event buffer, we have at least one region
  // pinned.  We need to be sure to place the hardware into reset before
  // unpinning the memory during shutdown.
  has_pinned_memory_ = true;

  zx_status_t status = dma_buffer::CreateBufferFactory()->CreateContiguous(bti_, kEp0BufferSize, 12,
                                                                           true, &ep0_.buffer);
  if (status != ZX_OK) {
    fdf::error("ep0_buffer init failed: {}", zx_status_get_string(status));
    return status;
  }

  if (zx_status_t status = Ep0Init(); status != ZX_OK) {
    fdf::error("Ep0Init init failed: {}", zx_status_get_string(status));
    return status;
  }

  irq_handler_.set_object(irq_.get());
  irq_handler_.Begin(fdf::Dispatcher::GetCurrent()->async_dispatcher());
  // Ack IRQ in case previous ack was hanging
  irq_.ack();

  // Things went well.  Cancel our cleanup routine.
  cleanup.cancel();
  return ZX_OK;
}

void Dwc3::ReleaseResources() {
  // If we managed to get our registers mapped, place the device into reset so
  // we are certain that there is no DMA going on in the background.
  if (mmio_.has_value() && power_on_) {
    if (zx_status_t status = ResetHw(); status != ZX_OK) {
      // Deliberately panic and terminate this driver if we fail to place the
      // hardware into reset at this point and we have any pinned memory..  We do this
      // deliberately because, if we cannot put the hardware into reset, it may still be accessing
      // pages we previously pinned using DMA.  If we are on a system with no
      // IOMMU, deliberately terminating the process will ensure that our
      // pinned pages are quarantined instead of being returned to the page
      // pool.
      if (has_pinned_memory_) {
        fdf::error(
            "Failed to place HW into reset during shutdown ({}), self-terminating in order "
            "to ensure quarantine",
            zx_status_get_string(status));
        ZX_ASSERT(false);
      }
    }

    ResetEndpoints();
  }

  // Now go ahead and release any buffers we may have pinned.
  ep0_.buffer.reset();
  ep0_.shared_fifo.Release();

  for (UserEndpoint& uep : user_endpoints_) {
    uep.fifo.Release();
  }

  event_fifo_.Release();
  has_pinned_memory_ = false;
}

zx_status_t Dwc3::CheckHwVersion() {
  auto* mmio = get_mmio();
  const uint32_t core_id = GSNPSID::Get().ReadFrom(mmio).core_id();
  if (core_id == 0x5533) {
    return ZX_OK;
  }

  const uint32_t ip_version = USB31_VER_NUMBER::Get().ReadFrom(mmio).IPVERSION();

  auto is_ascii_digit = [](char val) -> bool { return (val >= '0') && (val <= '9'); };
  auto is_ascii_letter = [](char val) -> bool {
    return ((val >= 'A') && (val <= 'Z')) || ((val >= 'a') && (val <= 'z'));
  };

  const char c1 = static_cast<char>((ip_version >> 24) & 0xFF);
  const char c2 = static_cast<char>((ip_version >> 16) & 0xFF);
  const char c3 = static_cast<char>((ip_version >> 8) & 0xFF);
  const char c4 = static_cast<char>(ip_version & 0xFF);

  // Format defined by section 1.3.44 of the DWC3 Programming Guide
  if (!is_ascii_digit(c1) || !is_ascii_digit(c2) || !is_ascii_digit(c3) ||
      (!is_ascii_letter(c4) && (c4 != '*'))) {
    fdf::error("Unrecognized USB IP Version 0x{:08x}", ip_version);
    return ZX_ERR_NOT_SUPPORTED;
  }

  const int major = c1 - '0';
  const int minor = ((c2 - '0') * 10) + (c3 - '0');

  if (major != 1) {
    fdf::error("Unsupported USB IP Version {}.{:02}{:c}", major, minor, c4);
    return ZX_ERR_NOT_SUPPORTED;
  }

  fdf::info("Detected DWC3 IP version {}.{:02}{:c}", major, minor, c4);
  return ZX_OK;
}

zx_status_t Dwc3::ResetHw() {
  auto* mmio = get_mmio();

  // Clear the run/stop bit and request a software reset.
  DCTL::Get().ReadFrom(mmio).set_RUN_STOP(0).set_CSFTRST(1).WriteTo(mmio);

  // HW will clear the software reset bit when it is finished with the reset
  // process.
  zx::time start = zx::clock::get_monotonic();
  while (DCTL::Get().ReadFrom(mmio).CSFTRST()) {
    if ((zx::clock::get_monotonic() - start) >= kHwResetTimeout) {
      return ZX_ERR_TIMED_OUT;
    }
  }

  return ZX_OK;
}

void Dwc3::SetDeviceAddress(uint32_t address) {
  auto* mmio = get_mmio();
  DCFG::Get().ReadFrom(mmio).set_DEVADDR(address).WriteTo(mmio);
}

void Dwc3::StartPeripheralMode() {
  auto* mmio = get_mmio();

  // configure and enable PHYs
  GUSB2PHYCFG::Get(0)
      .ReadFrom(mmio)
      .set_USBTRDTIM(9)    // USB2.0 Turn-around time == 9 phy clocks
      .set_ULPIAUTORES(0)  // No auto resume
      .WriteTo(mmio);

  GUSB3PIPECTL::Get(0)
      .ReadFrom(mmio)
      .set_DELAYP1TRANS(0)
      .set_SUSPENDENABLE(0)
      .set_LFPSFILTER(1)
      .set_SS_TX_DE_EMPHASIS(1)
      .WriteTo(mmio);

  // TODO(johngro): This is the number of receive buffers.  Why do we set it to 16?
  constexpr uint32_t nump = 16;
  DCFG::Get()
      .ReadFrom(mmio)
      .set_NUMP(nump)                  // number of receive buffers
      .set_DEVSPD(DCFG::DEVSPD_SUPER)  // max speed is 5Gbps USB3.1
      .set_DEVADDR(0)                  // device address is 0
      .WriteTo(mmio);

  // Program the location of the event buffer, then enable event delivery.
  StartEvents();

  Ep0Start();

  // Set the run/stop bit to start the controller
  DCTL::Get().FromValue(0).set_RUN_STOP(1).WriteTo(mmio);
}

void Dwc3::ResetConfiguration() {
  auto* mmio = get_mmio();
  // disable all endpoints except EP0_OUT and EP0_IN
  DALEPENA::Get().FromValue(0).EnableEp(kEp0Out).EnableEp(kEp0In).WriteTo(mmio);

  for (UserEndpoint& uep : user_endpoints_) {
    // Disabled above.
    uep.ep.enabled = false;
    uep.ep.stalled = false;
    UserEpReset(uep);
  }

  if (dci_intf_.is_valid()) {
    fidl::Arena arena;
    dci_intf_.buffer(arena)->SetConnected(true).Then(
        [](fidl::WireUnownedResult<fuchsia_hardware_usb_dci::UsbDciInterface::SetConnected>&
               result) {
          if (!result.ok()) {
            fdf::error("(framework) SetConnected(): {}", result.status_string());
          } else if (result->is_error()) {
            fdf::error("SetConnected(): {}", zx_status_get_string(result->error_value()));
          }
        });
  }

  if (phy_.is_valid()) {
    if (fidl::Result result = phy_->ConnectStatusChanged(true); result.is_error()) {
      fdf::warn("Call to ConnectStatusChanged on USB phy failed: {}", result.error_value());
    }
  }
}

void Dwc3::HandleResetEvent() {
  fdf::debug("Dwc3::HandleResetEvent");

  ResetEndpoints();
  SetDeviceAddress(0);
  Ep0Start();

  if (dci_intf_.is_valid()) {
    fidl::Arena arena;
    dci_intf_.buffer(arena)->SetConnected(false).Then(
        [](fidl::WireUnownedResult<fuchsia_hardware_usb_dci::UsbDciInterface::SetConnected>&
               result) {
          if (!result.ok()) {
            fdf::error("(framework) SetConnected(): {}", result.status_string());
          } else if (result->is_error()) {
            fdf::error("SetConnected(): {}", zx_status_get_string(result->error_value()));
          }
        });
  }
}

void Dwc3::HandleConnectionDoneEvent() {
  uint16_t ep0_max_packet = 0;
  fdescriptor::wire::UsbSpeed new_speed{fdescriptor::UsbSpeed::kUndefined};

  auto* mmio = get_mmio();

  uint32_t speed = DSTS::Get().ReadFrom(mmio).CONNECTSPD();

  switch (speed) {
    case DSTS::CONNECTSPD_HIGH:
      new_speed = fdescriptor::UsbSpeed::kHigh;
      ep0_max_packet = 64;
      break;
    case DSTS::CONNECTSPD_FULL:
      new_speed = fdescriptor::UsbSpeed::kFull;
      ep0_max_packet = 64;
      break;
    case DSTS::CONNECTSPD_SUPER:
      new_speed = fdescriptor::UsbSpeed::kSuper;
      ep0_max_packet = 512;
      break;
    case DSTS::CONNECTSPD_ENHANCED_SUPER:
      new_speed = fdescriptor::UsbSpeed::kEnhancedSuper;
      ep0_max_packet = 512;
      break;
    default:
      fdf::error("unsupported speed {}", speed);
      break;
  }

  if (ep0_max_packet) {
    std::array eps{&ep0_.out, &ep0_.in};
    for (Endpoint* ep : eps) {
      ep->type = USB_ENDPOINT_CONTROL;
      ep->interval = 0;
      ep->max_packet_size = ep0_max_packet;
      CmdEpSetConfig(*ep, true);
    }
    ep0_.cur_speed = new_speed;
  }

  if (dci_intf_.is_valid()) {
    fidl::Arena arena;
    dci_intf_.buffer(arena)->SetSpeed(new_speed).Then(
        [](fidl::WireUnownedResult<fuchsia_hardware_usb_dci::UsbDciInterface::SetSpeed>& result) {
          if (!result.ok()) {
            fdf::error("(framework) SetSpeed(): {}", result.status_string());
          } else if (result->is_error()) {
            fdf::error("SetSpeed(): {}", zx_status_get_string(result->error_value()));
          }
        });
  }
}

void Dwc3::HandleDisconnectedEvent() {
  fdf::debug("Dwc3::HandleDisconnectedEvent");

  if (dci_intf_.is_valid()) {
    fidl::Arena arena;
    dci_intf_.buffer(arena)->SetConnected(false).Then(
        [](fidl::WireUnownedResult<fuchsia_hardware_usb_dci::UsbDciInterface::SetConnected>&
               result) {
          if (!result.ok()) {
            fdf::error("(framework) SetConnected(): {}", result.status_string());
          } else if (result->is_error()) {
            fdf::error("SetConnected(): {}", zx_status_get_string(result->error_value()));
          }
        });
  }

  ResetEndpoints();

  if (phy_.is_valid()) {
    if (fidl::Result result = phy_->ConnectStatusChanged(false); result.is_error()) {
      fdf::warn("Call to ConnectStatusChanged on USB phy failed: {}", result.error_value());
    }
  }
}

void Dwc3::Stop() {
  fdf::debug("Stop()");
  irq_handler_.Cancel();
  ReleaseResources();

  // The OnUnbound() handler for each endpoint server should have already called zx_bti_unpin() for
  // each registered VMO. To guard against a crashed or stalled dispatcher, release all page
  // quarantines. The hardware is stopped and no further DMA transactions are scheduled.
  zx_status_t status = bti_.release_quarantine();
  if (status != ZX_OK) {
    fdf::error("Failed to release page quarantine ({})", zx_status_get_string(status));
  }

  controller_started_ = false;
}

void Dwc3::Suspend(fdf_power::SuspendCompleter completer) {
  // no-op.
  completer();
}

void Dwc3::Resume(fdf_power::ResumeCompleter completer) {
  // no-op.
  completer();
}

bool Dwc3::SuspendEnabled() { return false; }

void Dwc3::ConnectToEndpoint(ConnectToEndpointRequest& request,
                             ConnectToEndpointCompleter::Sync& completer) {
  UserEndpoint* uep{get_user_endpoint(UsbAddressToEpNum(request.ep_addr()))};
  if (uep == nullptr || !uep->server.has_value()) {
    completer.Reply(fit::as_error(ZX_ERR_INVALID_ARGS));
    return;
  }

  uep->server->Connect(fdf::Dispatcher::GetCurrent()->async_dispatcher(), std::move(request.ep()));
  completer.Reply(fit::ok());
}

void Dwc3::SetInterface(SetInterfaceRequest& request, SetInterfaceCompleter::Sync& completer) {
  if (!request.interface().is_valid()) {
    fdf::error("Interface should be valid");
    completer.Reply(zx::error(ZX_ERR_INVALID_ARGS));
    return;
  }

  if (dci_intf_.is_valid()) {
    fdf::error("{}: DCI Interface already set", __func__);
    completer.Reply(zx::error(ZX_ERR_BAD_STATE));
    return;
  }

  dci_intf_.Bind(std::move(request.interface()), fdf::Dispatcher::GetCurrent()->async_dispatcher());
  completer.Reply(zx::ok());
}

void Dwc3::StartController(StartControllerCompleter::Sync& completer) {
  controller_started_ = true;

  if (power_on_) {
    StartPeripheralMode();
  }

  completer.Reply(zx::ok());
}

void Dwc3::StopController(StopControllerCompleter::Sync& completer) {
  controller_started_ = false;
  ResetEndpoints();

  if (!power_on_) {
    completer.Reply(zx::ok());
    return;
  }

  zx_status_t status = ResetHw();
  if (status != ZX_OK) {
    fdf::error("Failed to reset hardware {}", zx_status_get_string(status));
    completer.Reply(zx::error(status));
    return;
  }
  zx::nanosleep(zx::deadline_after(zx::msec(50)));
  completer.Reply(zx::ok());
}

void Dwc3::ConfigureEndpoint(ConfigureEndpointRequest& request,
                             ConfigureEndpointCompleter::Sync& completer) {
  if (!power_on_) {
    completer.Reply(zx::error(ZX_ERR_IO_NOT_PRESENT));
    return;
  }

  const uint8_t ep_num = UsbAddressToEpNum(request.ep_descriptor().b_endpoint_address());
  UserEndpoint* const uep = get_user_endpoint(ep_num);

  if (uep == nullptr) {
    completer.Reply(zx::error(ZX_ERR_INVALID_ARGS));
    return;
  }

  uint8_t ep_type = usb_ep_type2(request.ep_descriptor());

  if (ep_type == USB_ENDPOINT_ISOCHRONOUS) {
    fdf::error("isochronous endpoints are not supported");
    completer.Reply(zx::error(ZX_ERR_NOT_SUPPORTED));
    return;
  }

  if (uep->ep.enabled) {
    // Endpoint already configured, nothing to do.
    fdf::error("Endpoint({}) already configured!", uep->ep.ep_num);
    completer.Reply(zx::ok());
    return;
  }

  if (zx::result result = uep->fifo.Init(bti_); result.is_error()) {
    fdf::error("fifo init failed {}", result);
    completer.Reply(result.take_error());
    return;
  }

  uep->ep.max_packet_size = usb_ep_max_packet2(request.ep_descriptor());
  uep->ep.type = ep_type;
  uep->ep.interval = request.ep_descriptor().b_interval();
  // TODO(voydanoff) USB3 support

  EpSetConfig(uep->ep, true);
  UserEpQueueNext(*uep);

  completer.Reply(zx::ok());
}

void Dwc3::DisableEndpoint(DisableEndpointRequest& request,
                           DisableEndpointCompleter::Sync& completer) {
  if (!power_on_) {
    completer.Reply(zx::error(ZX_ERR_IO_NOT_PRESENT));
    return;
  }

  const uint8_t ep_num = UsbAddressToEpNum(request.ep_address());
  UserEndpoint* const uep = get_user_endpoint(ep_num);

  if (uep == nullptr) {
    completer.Reply(zx::error(ZX_ERR_INVALID_ARGS));
    return;
  }

  uep->server->CancelAll(ZX_ERR_IO_NOT_PRESENT);
  EpSetConfig(uep->ep, false);

  completer.Reply(zx::ok());
}

void Dwc3::EndpointSetStall(EndpointSetStallRequest& request,
                            EndpointSetStallCompleter::Sync& completer) {
  if (!power_on_) {
    completer.Reply(zx::error(ZX_ERR_IO_NOT_PRESENT));
    return;
  }

  const uint8_t ep_num = UsbAddressToEpNum(request.ep_address());
  UserEndpoint* const uep = get_user_endpoint(ep_num);

  if (uep == nullptr) {
    completer.Reply(zx::error(ZX_ERR_INVALID_ARGS));
    return;
  }

  if (zx_status_t status = EpSetStall(uep->ep, true); status != ZX_OK) {
    completer.Reply(zx::error(status));
  } else {
    completer.Reply(zx::ok());
  }
}

void Dwc3::EndpointClearStall(EndpointClearStallRequest& request,
                              EndpointClearStallCompleter::Sync& completer) {
  if (!power_on_) {
    completer.Reply(zx::error(ZX_ERR_IO_NOT_PRESENT));
    return;
  }

  const uint8_t ep_num = UsbAddressToEpNum(request.ep_address());
  UserEndpoint* const uep = get_user_endpoint(ep_num);

  if (uep == nullptr) {
    completer.Reply(zx::error(ZX_ERR_INVALID_ARGS));
    return;
  }

  if (zx_status_t status = EpSetStall(uep->ep, false); status != ZX_OK) {
    completer.Reply(zx::error(status));
  } else {
    completer.Reply(zx::ok());
  }
}

void Dwc3::CancelAll(CancelAllRequest& request, CancelAllCompleter::Sync& completer) {
  const uint8_t ep_num = UsbAddressToEpNum(request.ep_address());
  UserEndpoint* const uep = get_user_endpoint(ep_num);

  if (uep == nullptr) {
    completer.Reply(zx::error(ZX_ERR_INVALID_ARGS));
    return;
  }

  uep->server->CancelAll(ZX_ERR_IO_NOT_PRESENT);
  completer.Reply(zx::ok());
}

void Dwc3::EpServer::GetInfo(GetInfoCompleter::Sync& completer) {
  auto info{fendpoint::EndpointInfo::WithControl(fendpoint::ControlEndpointInfo{})};

  switch (uep_->ep.type) {
    case USB_ENDPOINT_CONTROL:
      // Set up above.
      break;
    case USB_ENDPOINT_ISOCHRONOUS: {
      fendpoint::IsochronousEndpointInfo isoc;
      isoc.lead_time(1);
      info.isochronous(std::move(isoc));
      break;
    }
    case USB_ENDPOINT_BULK:
      info.bulk(fendpoint::BulkEndpointInfo{});
      break;
    case USB_ENDPOINT_INTERRUPT:
      info.interrupt(fendpoint::InterruptEndpointInfo{});
      break;
    default:
      // In theory, this should never happen unless a new EP type is added to the spec.
      fdf::error("unknown usb endpoint type: 0x{:x}", uep_->ep.type);
      completer.Reply(zx::error(ZX_ERR_BAD_STATE));
  }

  completer.Reply(zx::ok(std::move(info)));
}

void Dwc3::EpServer::QueueRequests(QueueRequestsRequest& request,
                                   QueueRequestsCompleter::Sync& completer) {
  if (!uep_->ep.enabled) {
    fdf::error("Dwc3: ep({}) not enabled!", uep_->ep.ep_num);
  }
  if (!uep_->ep.enabled || !dwc3_->power_on()) {
    for (auto& req : request.req()) {
      RequestComplete(ZX_ERR_IO_NOT_PRESENT, 0, usb::FidlRequest{std::move(req)});
    }
    return;
  }

  for (auto& req : request.req()) {
    usb::FidlRequest freq{std::move(req)};

    if (freq->data()->size() != 1) {
      fdf::error("scatter-gather not implemented");
      RequestComplete(ZX_ERR_INVALID_ARGS, 0, std::move(freq));
      continue;
    }

    if (uep_->ep.IsOutput()) {
      // Dig the length out of the request data block.
      size_t length = freq->data()->at(0).size().value();

      if (length == 0 || (length % uep_->ep.max_packet_size) != 0) {
        fdf::error("Dwc3: OUT transfers must be multiple of max packet size (len {} mps {})",
                   length, uep_->ep.max_packet_size);
        RequestComplete(ZX_ERR_INVALID_ARGS, 0, std::move(freq));
        continue;
      }
    }

    queued_reqs.emplace(std::move(freq));
  }

  dwc3_->UserEpQueueNext(*uep_);
}

void Dwc3::EpServer::CancelAll(CancelAllCompleter::Sync& completer) {
  CancelAll(ZX_ERR_IO_NOT_PRESENT);
  completer.Reply(zx::ok());
}

void Dwc3::EpReset(Endpoint& ep) {
  if (!power_on_) {
    return;
  }

  EpSetStall(ep, false);
  EpSetConfig(ep, false);
  ep.got_not_ready = false;
}

void Dwc3::UserEpReset(UserEndpoint& uep) {
  uep.server->CancelAll(ZX_ERR_IO_NOT_PRESENT);
  EpReset(uep.ep);
}

void Dwc3::Ep0Reset() {
  if (ep0_.transfer_in_progress_) {
    bool is_out = (ep0_.cur_setup.bm_request_type & USB_DIR_MASK) == USB_DIR_OUT;
    if (ep0_.state == Ep0::State::Status) {
      // Flip direction for status.
      is_out = !is_out;
    }
    CmdEpEndTransfer(is_out ? ep0_.out : ep0_.in);
  }
  EpReset(ep0_.out);
  EpReset(ep0_.in);
  ep0_.cur_setup = {};
  ep0_.cur_speed = fuchsia_hardware_usb_descriptor::wire::UsbSpeed::kUndefined;
  ep0_.state = Ep0::State::None;
  ep0_.transfer_in_progress_ = false;
  ep0_.shared_fifo.Clear();
}

void Dwc3::ResetEndpoints() {
  Ep0Reset();
  for (UserEndpoint& uep : user_endpoints_) {
    UserEpReset(uep);
  }
}

void Dwc3::OnConnectStatusChanged(
    fidl::Result<fuchsia_hardware_usb_phy::ConnectionWatcher::WatchConnectStatusChanged>& result) {
  ZX_DEBUG_ASSERT(platform_extension_);

  if (result.is_error() && result.error_value().is_framework_error()) {
    // Something happened to the FIDL connection, so don't make repeated calls.
    return;
  }

  connection_watcher_->WatchConnectStatusChanged().Then(
      fit::bind_member<&Dwc3::OnConnectStatusChanged>(this));

  if (result.is_error()) {
    fdf::error("WatchConnectStatusChanged returned {}",
               zx_status_get_string(result.error_value().domain_error()));
    return;
  }

  if (result->connected()) {
    fdf::debug("OnConnectStatusChanged: now connected");

    if (zx::result result = platform_extension_->Resume(); result.is_error()) {
      return;
    }
    power_on_ = true;

    if (controller_started_) {
      StartPeripheralMode();
    }
  } else {
    fdf::debug("OnConnectStatusChanged: now disconnected");

    // Cancel all pending requests.
    ResetEndpoints();

    if (zx::result result = platform_extension_->Suspend(); result.is_error()) {
      return;
    }
    power_on_ = false;
  }

  if (dci_intf_.is_valid()) {
    fidl::Arena arena;
    dci_intf_.buffer(arena)
        ->SetConnected(result->connected())
        .Then([](fidl::WireUnownedResult<fuchsia_hardware_usb_dci::UsbDciInterface::SetConnected>&
                     result) {
          if (!result.ok()) {
            fdf::error("(framework) SetConnected(): {}", result.status_string());
          } else if (result->is_error()) {
            fdf::error("SetConnected(): {}", zx_status_get_string(result->error_value()));
          }
        });
  }
}

}  // namespace dwc3

FUCHSIA_DRIVER_EXPORT(dwc3::Dwc3);
