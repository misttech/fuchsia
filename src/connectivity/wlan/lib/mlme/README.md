# WLAN MLME

This library implements IEEE Std 802.11 MAC Sublayer Management Entity (MLME) functionality for both SoftMAC and FullMAC hardware architectures.

## Layout

This library provides two MLME implementations:

### 1. SoftMAC MLME (`rust/`)

The [`wlansoftmac`](../../drivers/wlansoftmac) platform driver hosts this implementation. It runs the IEEE 802.11 host-side state machines:

- **Client STA (`rust/src/client/`)**: Client station state machines that manage synchronization, authentication, association, and power states.
- **Access Point (`rust/src/ap/`)**: AP mode state machines that manage beaconing, client authentication tables, and association tracking.

### 2. FullMAC MLME (`fullmac/`)

This module runs as an in-process translation and dispatch layer between the Station Management Entity (SME) and FullMAC vendor drivers:

- **`src/mlme_main_loop.rs`**: Runs the main loop that processes incoming `MlmeRequest` messages from the SME.
- **`src/device.rs`**: Defines the `DeviceOps` trait and `FullmacDevice` wrapper to call driver operations over `fuchsia.wlan.fullmac`.
- **`src/convert/`**: Converts between standard `fuchsia.wlan.mlme` protocol types and vendor `fuchsia.wlan.fullmac` driver types:
  - `mlme_to_fullmac.rs`: Translates MLME requests into vendor driver calls.
  - `fullmac_to_mlme.rs`: Translates vendor driver events into MLME indications.
