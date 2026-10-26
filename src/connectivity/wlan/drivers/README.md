# WLAN Drivers

This directory contains platform, vendor, and virtual drivers for wireless networking devices on Fuchsia.

## Architecture

Fuchsia WLAN drivers separate hardware-agnostic IEEE 802.11 stack logic from hardware-specific device control. Vendor drivers bind to device transport buses (PCIe, SDIO, USB) and implement `fuchsia.wlan.phy` for physical device control. They also implement either the FullMAC or SoftMAC protocols for data and control.

```
       +---------------------------------------------+
       |   Device Management (wlandevicemonitor)     |
       +---------------------------------------------+
                              | fuchsia.wlan.phy
                              v
       +---------------------------------------------+
       |      Vendor Driver (fuchsia.wlan.phy)        |
       +---------------------------------------------+
              |                               |
              | FullMAC                       | SoftMAC
              v                               v
    +-------------------+           +-------------------+
    |      wlanif       |           |    wlansoftmac    |
    | (Platform Driver) |           | (Platform Driver) |
    +-------------------+           +-------------------+
              |                               |
              v                               v
    Vendor FullMAC Device           Vendor SoftMAC Device
    (e.g., brcmfmac)                (e.g., iwlwifi)
```

## Platform Drivers

Platform drivers provide Fuchsia's shared bridge between higher-level stack components (SME/MLME) and vendor drivers:

- **`wlanif`**: Platform driver for FullMAC devices. Relies on device firmware to implement MLME functions and bridges `fuchsia.wlan.sme` to `fuchsia.wlan.fullmac`.
- **`wlansoftmac`**: Platform driver for SoftMAC devices. Hosts host-side MLME and SME state machines, managing frame transmission, reception, association, and authentication. Communicates with vendor SoftMAC drivers through a C++/Rust FFI bridge (`lib/ffi-transport`). For details on maintaining FFI bindings, see [`wlansoftmac/rust_driver/c-binding/README.md`](wlansoftmac/rust_driver/c-binding/README.md).

## Vendor Drivers (`third_party/`)

Vendor drivers communicate directly with physical hardware and implement `fuchsia.wlan.phy`:

- **`third_party/intel/iwlwifi`**: SoftMAC driver for supported Intel PCIe wireless chipsets. Implements `fuchsia.wlan.softmac`. See [`third_party/intel/iwlwifi/README.md`](third_party/intel/iwlwifi/README.md).
- **`third_party/broadcom/brcmfmac`**: FullMAC driver for Broadcom SDIO/PCIe wireless devices. Implements `fuchsia.wlan.fullmac`.
- **Other vendor implementations**: Additional vendor drivers (such as NXP `nxpfmac` or Atheros `ath10k`) follow the same protocol contracts.

Note: Vendor drivers expose `fuchsia.wlan.phy` directly to `wlandevicemonitor` without the legacy `wlanphy` platform driver.

## Virtual & Simulation Drivers

- **`wlan-virtio-hwsim`**: VirtIO-based WLAN hardware simulator driver for virtual environments.
- **`../testing/wlantap-driver`**: Virtual driver creating mock PHY and interface devices via `fuchsia.wlan.tap` for isolated `hw-sim` integration tests.

## Shared Driver Libraries

- **`lib/`**: Common C++ driver utilities and logging helpers.
- **`lib-iwlwifi/`**: Shared libraries and mocks for Intel driver testing, including `fake-bti` and `fake-object` for mocking syscalls and kernel handles in unit tests.
