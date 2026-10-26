# Intel WiFi Driver (`iwlwifi`)

## What is This?

Modern Intel chipsets integrate an on-chip WiFi feature accessed via the PCIe
bus. The `iwlwifi` driver is the vendor SoftMAC driver for supported Intel
wireless hardware on Fuchsia.

On some Intel chipsets, the WiFi feature coexists with Bluetooth on the same
physical module. A separate Bluetooth driver handles Bluetooth.

## Terminology

- **`iwl-drv`**: The core Intel WiFi driver. An `iwl-drv` instance maps to a
  single firmware operational mode (`opmode`).
- **Firmware (`ucode`)**: Operational code running inside the wireless chipset.
  Firmware variants include:
  - **MVM**: SoftMAC firmware used by 7000, 8000, 9000 and 22000-series devices.
  - **XVT, FMAC, DVM, TEST**: Other Intel opmodes (FMAC is FullMAC, DVM is the
    legacy one). Fuchsia only builds MVM.
- **`opmode`**: Operational mode mapped to loaded firmware. The opmode layer
  provides a unified interface for the MLME so that upper layers do not need to
  manage firmware-specific details.
- **`wlansoftmac`**: The platform SoftMAC driver that manages IEEE 802.11 state
  machines and interacts with `iwlwifi` over the `fuchsia.wlan.softmac`
  protocol.

## Driver Architecture (Driver Framework v2)

The `iwlwifi` driver is a Driver Framework v2 (DFv2) component:

```
               wlandevicemonitor / wlansoftmac
                             |
       +---------------------------------------------+
       |   fuchsia.wlan.phy / fuchsia.wlan.softmac   |
       +---------------------------------------------+
                             |
             platform/ (Fuchsia integration)
      PcieIwlwifiDriver (fdf::DriverBase2)
      WlanPhyDevice & WlanSoftmacDevice
                             |
                             v
               +---------------------------+      +-------------------------+
               |  MVM opmode (mvm/)        | <--- | iwl_drv (Core Driver)   |
               +---------------------------+      | Loads fw & starts MVM   |
                             |                    +-------------------------+
            ----------------------------------- iwl_trans (Transport Layer)
                             |
              +--------------+--------------+
              |                             |
              v                             v
      pcie/ (Real PCIe)              test/ (sim-trans.cc)
              |                             |
              v                             v
      Hardware Bus / DMA             Simulated Firmware
```

### Driver Initialization Procedure

1. **Binding**: `platform/iwlwifi.bind` matches the PCIe vendor/device ID and
   triggers driver loading by the driver manager.
2. **DFv2 Driver Start**: The driver framework instantiates `PcieIwlwifiDriver`
   (`platform/pcie-iwlwifi-driver.cc`), calling `DriverBase2::Start()`.
3. **PCI Discovery**: `PcieIwlwifiDriver` connects to `fuchsia.hardware.pci`,
   gets the BTI and the PCI device IDs, and finds the matching entry in the
   device table.
4. **Transport Setup**: `iwl_pci_probe()` sets up `struct iwl_trans` and the
   device configuration (`struct iwl_cfg`), maps the device registers, sets up
   the interrupt, and calls `iwl_drv_start()`.
5. **Firmware Loading**: `iwl_drv_start()` (in `iwl-drv.c`) starts firmware
   discovery via `iwl_load_firmware()`.
6. **Opmode Startup**: `iwl_req_fw_callback()` parses the firmware image and
   starts the MVM opmode. `iwl_op_mode_mvm_start()` runs the INIT ucode to read
   the NVM, then stops the device until an interface is created.
7. **PHY Exposure**: `PcieIwlwifiDriver` adds an `iwlwifi-wlanphy` child node
   and serves `fuchsia.wlan.phy/Service` itself, since it is a `WlanPhyDevice`.
   `wlandevicemonitor` uses it for PHY management.
8. **Interface Creation**: When `wlandevicemonitor` calls `CreateIface`, the
   driver creates a `WlanSoftmacDevice` (`platform/wlansoftmac-device.cc`),
   which loads the runtime ucode. The driver then adds an `iwlwifi-wlansoftmac`
   child node that offers `fuchsia.wlan.softmac/Service` to `wlansoftmac`.

## Directory Structure

### Root
- `iwl-*.*`: Core driver files, adapted for Fuchsia coding styles and
  concurrency models.

### `platform/` (Fuchsia integration)
- `iwlwifi.bind`: Composite bind rules with a PCI parent (Intel vendor ID and
  the supported device IDs) and an optional ACPI parent.
- `pcie-iwlwifi-driver.cc` / `.h`: `PcieIwlwifiDriver` implementing
  `fdf::DriverBase2`.
- `wlanphyimpl-device.cc` / `.h`: `WlanPhyDevice` implementing the
  `fuchsia.wlan.phy.WlanPhy` protocol.
- `wlansoftmac-device.cc` / `.h`: `WlanSoftmacDevice` implementing the
  `fuchsia.wlan.softmac.WlanSoftmac` protocol.
- `mvm-mlme.cc` / `.h`: Translates `WlanPhy` and `WlanSoftmac` calls into `mvm/`
  operations.
- `pci-fidl.cc` / `.h`: C wrappers around `fuchsia.hardware.pci` for `pcie/`.
- `driver-inspector.cc` / `.h`: Owns the driver's Inspect tree and publishes
  firmware core dumps to it.

### `pcie/`
- Implements low-level PCIe transport (`drv.c`, `trans.c`, `rx.c`, `tx.c`,
  `ctxt-info.c`). Manages PCIe RX/TX rings and DMA buffers.

### `fw/`
- Firmware loading routines, command definitions (`fw/api/commands.h`), firmware
  file headers (`fw/file.h`), runtime state (`fw/runtime.h`), and memory paging
  (`fw/paging.c`). `iwl_req_fw_callback()` parses the firmware image into host
  memory (`drv->fw`), `iwl_op_mode_mvm_start()` links `mvm->fwrt` to `drv->fw`,
  and `iwl_mvm_up()` loads the runtime ucode into the device with
  `iwl_mvm_load_ucode_wait_alive()`.

### `mvm/`
- Implements the MVM operational mode:
  - `mac80211.c`: Core interface for interface addition, scanning, and station
    management.
  - `fw.c`: Lifecycle and ucode execution management.
  - `ops.c`: Handler table for notifications and responses from the firmware.
  - `tx.c` / `rx.c`: Data-plane packet transmission and reception.

### `cfg/`
- Chipset configurations for supported hardware generations:
  - `cfg/7000.c`: 7000-series chipsets.
  - `cfg/8000.c`: 8000-series chipsets.
  - `cfg/9000.c`: 9000-series chipsets.
  - `cfg/22000.c`: 22000-series chipsets.

### `test/`
- Unit tests and simulation infrastructure:
  - `sim-trans.cc` / `sim-trans.h`: Simulated PCIe transport layer.
  - `sim-mvm.cc`: Simulated MVM firmware responding to commands.
  - `single-ap-test.cc`: Base test fixture that starts the simulated transport
    with a fake firmware image and defines one test AP (SSID and channel).

## Build & Testing System

Bazel builds the `iwlwifi` driver and its unit tests via `BUILD.bazel`:
- **Driver Target**: `platform/BUILD.bazel` defines the `fuchsia_cc_driver`
  target and packages it into `fuchsia_driver_component`.
- **Test Package**: `test/BUILD.bazel` defines the `fuchsia_test_package`
  target.
