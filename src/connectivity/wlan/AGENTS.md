# Fuchsia WLAN Architecture & Agent Guide

This directory contains the Fuchsia WLAN stack. For an overview of the stack architecture and layers, see [README.md](./README.md).

## Command Flow: Adding a Feature (e.g., APF)

To plumb a new feature through the stack, follow this flow (using Android Packet Filter (APF) as an example):

### 1. FIDL Definitions
Define new types and methods in the appropriate FIDL libraries. Refer to [FIDL.md](./FIDL.md) to determine layer boundaries and stability classifications before introducing new types.

### 2. Policy Layer (`wlanix` or `wlancfg`)
- **`wlanix`**: Handle requests in `src/connectivity/wlan/wlanix/src/main.rs`. Use the `IfaceManager` trait to forward calls to the SME.
- **`wlancfg`**: Add logic to policy state machines (e.g., `client/state_machine.rs`) to trigger the feature.

### 3. SME Layer (`src/connectivity/wlan/lib/sme`)
- Update `MlmeRequest` enum in `lib.rs` to include the new command.
- Add methods to `ClientSme` (e.g., `src/client/mod.rs`) to send the `MlmeRequest`.
- Update FIDL request handlers in `src/serve/client.rs` to call the new SME methods.

### 4. MLME Layer (`src/connectivity/wlan/lib/mlme`)
- **FullMAC Translation (`mlme/fullmac`)**:
  - Add conversion functions in `src/convert/mlme_to_fullmac.rs` (MLME -> FullMAC) and `src/convert/fullmac_to_mlme.rs` (FullMAC -> MLME).
  - Update `DeviceOps` trait and `FullmacDevice` in `src/device.rs` to include the new driver calls.
  - Handle the `MlmeRequest` in the main loop (`src/mlme_main_loop.rs`). Convert the request and call the driver through `DeviceOps`.
- **SoftMAC Host (`mlme/rust`)**:
  - Implement frame processing or state machine logic in `src/client/` or `src/ap/`.

### 5. Driver Layer
- **FullMAC**: Implement new methods in vendor drivers (e.g., `brcmfmac`, `nxpfmac`).
- **SoftMAC**: Implement vendor hardware hooks in vendor SoftMAC drivers (e.g., `iwlwifi`) and `wlansoftmac`.

## Testing

For testing strategies across tiers (unit, platform driver integration, simulated hardware, and on-device E2E), see:
- [`tests/README.md`](tests/README.md): Tiered testing guide and decision matrix.
- [`testing/hw-sim/README.md`](testing/hw-sim/README.md): Simulated hardware stack testing with `wlantap`.
