// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#ifndef SRC_DEVICES_PCI_DRIVERS_PCI_BUS_H_
#define SRC_DEVICES_PCI_DRIVERS_PCI_BUS_H_

#include <fidl/fuchsia.hardware.pci.root/cpp/fidl.h>
#include <fidl/fuchsia.hardware.pci.root/cpp/wire.h>
#include <fidl/fuchsia.hardware.pci/cpp/natural_types.h>
#include <fidl/fuchsia.hardware.pci/cpp/wire.h>
#include <lib/async/cpp/irq.h>
#include <lib/async/cpp/wait.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/ddk/device.h>
#include <lib/driver/mmio/cpp/mmio.h>
#include <lib/fidl/cpp/wire/channel.h>
#include <lib/stdcompat/span.h>
#include <lib/zx/interrupt.h>
#include <lib/zx/msi.h>
#include <lib/zx/thread.h>
#include <zircon/compiler.h>
#include <zircon/syscalls/port.h>

#include <list>
#include <map>
#include <memory>
#include <optional>
#include <unordered_map>
#include <vector>

#include <ddktl/device.h>
#include <ddktl/fidl.h>
#include <ddktl/metadata_server.h>
#include <ddktl/protocol/empty-protocol.h>

#include "src/devices/pci/drivers/pci/bus_device_interface.h"
#include "src/devices/pci/drivers/pci/device.h"
#include "src/devices/pci/drivers/pci/root.h"
#include "src/devices/pci/lib/cpp/config.h"

namespace pci {
// An entry corresponding to a place in the topology to scan. Use to allow for
// DFS traversal of the bus topology while keeping track of nodes upstream.
struct BusScanEntry {
  static constexpr uint8_t kDefaultMaxFunctions = 1;
  fuchsia_hardware_pci::wire::Bdf bdf;
  UpstreamNode* upstream;
  uint8_t max_functions = kDefaultMaxFunctions;
};

// Tracks a device sharing a physical legacy IRQ vector along with the wait
// object that monitors virtual interrupt servicing (untriggering) by the client.
struct SharedDevice {
  pci::Device* device = nullptr;
  async::Wait wait;
};
using SharedDeviceList = std::vector<std::unique_ptr<SharedDevice>>;

struct SharedVector {
  zx::interrupt interrupt;
  async::Irq irq_handler;
  SharedDeviceList list;
};

// A map of vector -> SharedVector for use in handling interrupts.
using SharedIrqMap = std::unordered_map<uint32_t, std::unique_ptr<SharedVector>>;
namespace PciFidl = fuchsia_hardware_pci;

// A tree of all pci Device objects in the bus topology, keyed by BDF.
using DeviceTree =
    std::map<fuchsia_hardware_pci::wire::Bdf, fbl::RefPtr<pci::Device>, Device::BdfCompare>;

class Bus;
using PciBusType = ddk::Device<Bus, ddk::Messageable<PciFidl::Debug>::Mixin>;
class Bus : public PciBusType,
            public ddk::EmptyProtocol<ZX_PROTOCOL_PCI>,
            public BusDeviceInterface {
 public:
  static zx_status_t Create(zx_device_t* parent);
  Bus(zx_device_t* parent, fidl::ClientEnd<fuchsia_hardware_pci_root::Root> root_client,
      fuchsia_hardware_pci_root::PlatformInfo info, fdf::MmioBuffer ecam, bool is_extended)
      : PciBusType(parent),  // fulfills the DDK mixins
        root_client_(std::move(root_client)),
        info_(std::move(info)),
        ecam_(std::move(ecam)),
        is_extended_(is_extended) {}
  Bus(zx_device_t* parent, fidl::ClientEnd<fuchsia_hardware_pci_root::Root> root_client,
      fuchsia_hardware_pci_root::PlatformInfo info,
      fidl::ClientEnd<fuchsia_hardware_pci_root::Config> config_client)
      : PciBusType(parent),  // fulfills the DDK mixins
        root_client_(std::move(root_client)),
        info_(std::move(info)),
        config_client_(std::move(config_client)) {}
  ~Bus() override;
  // Map an ecam VMO for Bus and Config use.
  static zx::result<fdf::MmioBuffer> MapConfigRegion(zx::vmo cam_vmo);

  zx_status_t Initialize() __TA_EXCLUDES(devices_lock_);
  // Bus Device Interface implementation
  zx_status_t LinkDevice(fbl::RefPtr<pci::Device> device) __TA_EXCLUDES(devices_lock_) final;
  zx_status_t UnlinkDevice(pci::Device* device) __TA_EXCLUDES(devices_lock_) final;
  zx_status_t AllocateMsi(uint32_t count, zx::msi* out_allocation,
                          fuchsia_hardware_pci_root::wire::MsiAllocationInfo* out_info)
      __TA_EXCLUDES(devices_lock_) final;
  zx_status_t GetMsiHandle(const zx::msi& allocation, uint32_t options, uint16_t msi_id,
                           const zx::vmo& cfg_vmo, uint64_t cfg_offset,
                           zx::interrupt* out_interrupt) __TA_EXCLUDES(devices_lock_) final;
  zx_status_t GetBti(const pci::Device* device, uint32_t index, zx::bti* bti)
      __TA_EXCLUDES(devices_lock_) final;
  uint16_t GetSegmentGroup() final;
  zx_status_t AddToSharedIrqList(pci::Device* device, uint32_t vector,
                                 zx::unowned_interrupt irq_handle)
      __TA_EXCLUDES(devices_lock_) final;
  zx_status_t RemoveFromSharedIrqList(pci::Device* device, uint32_t vector)
      __TA_EXCLUDES(devices_lock_) final;

  // All methods related to the fuchsia.hardware.pci service and the DDK.
  void DdkRelease() { delete this; }
  void GetDevices(GetDevicesCompleter::Sync& completer) final;
  void GetHostBridgeInfo(GetHostBridgeInfoCompleter::Sync& completer) final;
  void ReadBar(ReadBarRequestView request, ReadBarCompleter::Sync& completer) final;

 protected:
  // These are used by the derived TestBus class.
  fbl::Mutex* devices_lock() __TA_RETURN_CAPABILITY(devices_lock_) { return &devices_lock_; }
  pci::DeviceTree& devices() { return devices_; }
  SharedIrqMap& shared_irqs() { return shared_irqs_; }
  const PciFidl::BoardConfiguration& board_config() { return board_config_; }
  void HandleLegacyIrq(async_dispatcher_t* dispatcher, async::Irq* irq, zx_status_t status,
                       const zx_packet_interrupt_t* interrupt, uint32_t vector);
  // Invoked on dispatcher_ when a device driver acknowledges its virtual interrupt.
  static void HandleDeviceLegacyIrqUntriggered(async_dispatcher_t* dispatcher, async::Wait* wait,
                                               zx_status_t status, const zx_packet_signal_t* signal,
                                               pci::Device* device);

 private:
  // Map an ecam VMO for Bus and Config use.
  zx_status_t MapConfigRegion();
  // Creates a Config object for accessing the config space of the device at |bdf|.
  zx::result<std::unique_ptr<Config>> MakeConfig(fuchsia_hardware_pci::wire::Bdf bdf);
  // Scan all buses downstream from the root within the start and end
  // bus values given to the Bus driver through RootService.
  zx_status_t ScanDownstream();
  // Scan a specific bus
  void ScanBus(BusScanEntry entry, std::list<BusScanEntry>* scan_list);
  // Returns true if a given BDF is present in the list of devices provided by
  // the platform to us that use the ACPI fragment.
  bool DeviceHasAcpi(fuchsia_hardware_pci::wire::Bdf bdf);

  // Returns true if a given BDF is present in the list of devices that the
  // platform told us are described by the devicetree. For these devices the bus
  // driver publishes only the fragment and leaves composite creation to the
  // devicetree.
  bool DeviceHasDevicetree(fuchsia_hardware_pci::wire::Bdf bdf);

  // Creates interrupts corresponding to legacy IRQ vectors and configures devices accordingly.
  zx_status_t ConfigureLegacyIrqs() __TA_EXCLUDES(devices_lock_);
  // Creates and binds interrupts to the async dispatcher and sets up Shared IRQ handler lists.
  zx_status_t SetUpLegacyIrqHandlers() __TA_REQUIRES(devices_lock_);

  // members
  fidl::WireSyncClient<fuchsia_hardware_pci_root::Root> root_client_;
  const fuchsia_hardware_pci_root::PlatformInfo info_;
  std::optional<fdf::MmioBuffer> ecam_;
  const bool is_extended_ = false;
  fidl::WireSyncClient<fuchsia_hardware_pci_root::Config> config_client_;
  PciFidl::BoardConfiguration board_config_;

  // All devices hang off of this Bus's root port.
  std::unique_ptr<PciRoot> root_;
  fbl::Mutex devices_lock_;
  async_dispatcher_t* dispatcher_{nullptr};

  // All devices downstream of this bus are held here. Devices are keyed by
  // BDF so they will not experience any collisions.
  pci::DeviceTree devices_ __TA_GUARDED(devices_lock_);
  SharedIrqMap shared_irqs_ __TA_GUARDED(devices_lock_);

  fidl::ServerBindingGroup<fuchsia_hardware_pci::Debug> bindings_;
};

zx_status_t pci_bus_bind(void* ctx, zx_device_t* parent);

}  // namespace pci

#endif  // SRC_DEVICES_PCI_DRIVERS_PCI_BUS_H_
