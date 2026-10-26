# WLANIX (Android WiFi HAL Implementation)

`wlanix` implements the Android-compatible wireless networking APIs (`fuchsia.wlan.wlanix`) on Fuchsia. It serves as the backend for Android user-space components inside the Starnix container.

On products with a Starnix Android container, `wlanix` runs instead of the `wlancfg` policy service.

## Architecture & Subsystems

`wlanix` bridges Android WiFi HAL interfaces to Fuchsia's native WLAN stack:

1. **`Wifi` (Chip and Interface Management)**:
   - Handles top-level lifecycle requests from the Android WiFi framework (e.g., initial start/stop, chip capabilities).
   - Coordinates interface creation and destruction with `wlandevicemonitor` over `fuchsia.wlan.device.service.DeviceMonitor`.

2. **`Supplicant` (Connection & Security)**:
   - Implements Android Supplicant HAL semantics for network connection, credential management (Open, WEP, WPA2, WPA3), and roaming.
   - Manages client connection state machines and forwards connection commands to the SME via `fuchsia.wlan.sme/GenericSme`.

3. **`Nl80211` (Netlink & Event Translation)**:
   - Translates Linux `nl80211` generic netlink commands and attributes into Fuchsia SME requests and vice versa.
   - Dispatches scan requests (`nl80211::cmd::CMD_TRIGGER_SCAN`) and broadcasts scan completion events (`nl80211::cmd::CMD_NEW_SCAN_RESULTS`).
   - Supports scheduled scans and reports RSSI threshold events.

## Interface Management (`ifaces.rs`)

The `IfaceManager` manages active WLAN interfaces:
- Discovers physical devices by querying `wlandevicemonitor`.
- Creates and destroys client (`STA`) and access point (`AP`) interfaces.
- Binds to the corresponding `GenericSme` protocol channel returned by `wlandevicemonitor` to drive scanning and association.

## Product Configuration

`wlanix` and `wlancfg` are mutually exclusive policy managers. Product assembly selects `wlanix` for Android Starnix products and `wlancfg` for native Fuchsia products.
