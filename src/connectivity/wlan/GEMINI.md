# Fuchsia WLAN Architecture

This directory contains the WLAN stack for Fuchsia. The stack follows a layered architecture, with components interconnected via FIDL.

## Layers

1. **Policy Layer (`wlancfg` or `wlanix`)**:
   - **Role**: High-level management, auto-connect logic, network selection, and saved network storage.
   - **Exclusivity**: Only one of these is present on a given product.
     - **`wlancfg`**: Standard Fuchsia WLAN policy. Implements `fuchsia.wlan.policy`.
     - **`wlanix`**: Provides Android-compatible WiFi APIs for Starnix. Implements `fuchsia.wlan.wlanix`.
   - **Interactions**: Both interact with `wlandevicemonitor` to manage interfaces and access the SME.

2. **Device Management Layer (`wlandevicemonitor`)**:
   - **Role**: Monitors hardware (PHYs), manages the lifecycle of WLAN interfaces, and bridges policy requests to the appropriate SME.
   - **Protocols**: Implements `fuchsia.wlan.device.service.DeviceMonitor`.

3. **Core / Stack Layer (SME & MLME)**:
   - **SME (`src/connectivity/wlan/lib/sme`)**: Shared logic across all hardware. Manages connection state, scanning, and security (RSN).
   - **MLME (`src/connectivity/wlan/lib/mlme`)**:
     - **SoftMAC**: Implemented in host code (`mlme/rust`). Manages the 802.11 state machine.
     - **FullMAC**: Implemented as a translation layer (`mlme/fullmac`). Converts standard MLME commands to vendor-specific `fuchsia.wlan.fullmac` calls.

4. **Driver Layer (`drivers/`)**:
   - **`wlanphy`**: Management of the physical device.
   - **`wlansoftmac`**: Driver that hosts the SoftMAC SME/MLME and implements lower MAC functions.
   - **`wlanif`**: Interface protocol between the stack and the driver.

## Command Flow: Adding a Feature (e.g., APF)

To plumb a new feature through the stack, follow this flow (using Android Packet Filter (APF) as an example):

### 1. FIDL Definitions

Define the new types and methods in the appropriate FIDL libraries:

- Shared types (e.g.,`ApfPacketFilterSupport`).
  - `sdk/fidl/fuchsia.wlan.common`: Shared types for the entire stack.
  - `sdk/fidl/fuchsia.wlan.ieee80211`: Shared types that match ieee802.11 types.
  - `sdk/fidl/fuchsia.wlan.internal`: Shared types which are only used between sme, mlme, and driver layers.
- `sdk/fidl/fuchsia.wlan.wlancfg`: Standard Fuchsia WLAN policy-compatible interface (if applicable).
- `sdk/fidl/fuchsia.wlan.wlanix`: Android-compatible interface (if applicable).
- `sdk/fidl/fuchsia.wlan.device.service`: Commands to bridge policy requests to interfaces, if the command doesn't directly go through SME.
- `sdk/fidl/fuchsia.wlan.sme`: SME-level commands.
- `sdk/fidl/fuchsia.wlan.mlme`: MLME-level commands (e.g., `InstallApfPacketFilter`).
- `sdk/fidl/fuchsia.wlan.fullmac`: Vendor driver interface for FullMAC.
- `sdk/fidl/fuchsia.wlan.phyimpl`: Vendor driver interface for PHY control.
- `sdk/fidl/fuchsia.wlan.softmac`: Vendor driver interface for SoftMAC (if applicable).

### 2. Policy Layer (`wlanix` or `wlancfg`)

- **`wlanix`**: Handle requests in `src/connectivity/wlan/wlanix/src/main.rs`. Use an `IfaceManager` trait to forward calls to the SME.
- **`wlancfg`**: Add logic to state machines (e.g., `client/state_machine.rs`) to trigger the feature.

### 3. SME Layer (`src/connectivity/wlan/lib/sme`)

- Update `MlmeRequest` enum in `lib.rs` to include the new command.
- Add methods to `ClientSme` (e.g., `src/client/mod.rs`) to send the `MlmeRequest`.
- Update FIDL request handlers in `src/serve/client.rs` to call the new SME methods.

### 4. MLME Layer (`src/connectivity/wlan/lib/mlme`)

- **FullMAC Translation (`mlme/fullmac`)**:
  - Add conversion functions in `src/convert/mlme_to_fullmac.rs` (MLME -> FullMAC) and `src/convert/fullmac_to_mlme.rs` (FullMAC -> MLME).
  - Update `DeviceOps` trait and `FullmacDevice` in `src/device.rs` to include the new driver calls.
  - Handle the `MlmeRequest` in the main loop (`src/mlme_main_loop.rs`), perform the conversion, and call the driver via `DeviceOps`.

### 5. Driver Layer

- **FullMAC**: Implement the new methods in the vendor driver (e.g., `brcmfmac`).
- **SoftMAC**: Implement logic in the host-side MLME and `wlansoftmac`.

## Key FIDL Protocols

These FIDL definitions are all located in `//sdk/fidl` with the exception of `fuchsia.wlan.wlanix`, which is in the `wlanix` directory.

- `fuchsia.wlan.policy`: Policy-level API for Fuchsia components.
- `fuchsia.wlan.wlanix`: Android-compatible WiFi API used by Starnix.
- `fuchsia.wlan.device`: PHY and interface lifecycle management.
- `fuchsia.wlan.device.service`: Internal service for device monitoring and SME access. Implements `DeviceMonitor`.
- `fuchsia.wlan.sme`: High-level stack control (Station Management Entity).
- `fuchsia.wlan.mlme`: Common interface used by the SME for both SoftMAC and FullMAC.
- `fuchsia.wlan.fullmac`: The driver interface for FullMAC devices, which receives commands translated from MLME.

## Testing

- **`src/connectivity/wlan/testing`**: Infrastructure like `wlantap` (virtual driver) and `hw-sim`.
- **`src/connectivity/wlan/tests`**: Integration and E2E tests.
