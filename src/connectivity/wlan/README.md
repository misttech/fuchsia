# Fuchsia WLAN Stack

Fuchsia's wireless networking stack provides an IEEE Std 802.11 compliant implementation supporting both FullMAC and SoftMAC hardware architectures. The stack is structured into modular layers that communicate through asynchronous FIDL protocols.

## Architecture & Layers

The WLAN stack consists of four primary layers:

1. **Policy Layer**: Manages client connections, access points, network selection, auto-connect logic, and persistent configurations.
   - [`wlancfg`](wlancfg/README.md): Standard Fuchsia WLAN policy service implementing `fuchsia.wlan.policy`.
   - [`wlanix`](wlanix/README.md): Android-compatible WiFi HAL service implementing `fuchsia.wlan.wlanix` for Starnix.
   - Note: Products select either `wlancfg` or `wlanix` based on system architecture. Both interact with `wlandevicemonitor`.

2. **Device Management Layer**:
   - [`wlandevicemonitor`](wlandevicemonitor/README.md): Discovers and monitors WLAN physical devices (PHYs), coordinates interface lifecycles (`CreateIface`, `DestroyIface`), and vends `fuchsia.wlan.sme/GenericSme` channels to the active policy component.

3. **Core / Stack Libraries**:
   - [`lib/sme`](lib/sme/README.md): Station Management Entity (SME) managing client connection states, scanning schedules, security handshakes, and roaming.
   - [`lib/mlme`](lib/mlme/README.md): MAC Sublayer Management Entity (MLME) providing SoftMAC 802.11 state machines (`lib/mlme/rust`) and FullMAC translation (`lib/mlme/fullmac`).
   - [`lib/`](lib/README.md): Modular shared libraries spanning IEEE 802.11 protocols, cryptographic primitives (SAE/OWE in `fcg-crypto`), frame parsing, and FFI transport.

4. **Driver Layer**:
   - [`drivers/`](drivers/README.md): Platform and vendor driver implementations.
     - **Platform Drivers**: `wlanif` (FullMAC driver platform bridge) and `wlansoftmac` (SoftMAC driver hosting MLME state machines with a C++/Rust FFI bridge).
     - **Vendor Drivers**: Hardware drivers for Broadcom, Intel, and other chipsets implementing `fuchsia.wlan.phy` and either `fuchsia.wlan.fullmac` or `fuchsia.wlan.softmac`.

## Protocol & API Boundaries

[FIDL.md](./FIDL.md) documents inter-layer communication contracts and stability levels. Refer to that document for layer boundary classifications and the domain error handling strategy.

## Testing

For test suites, simulated hardware testing, and on-device tests, see [`tests/README.md`](tests/README.md).

## Developer & Agent Guidance

For instructions on adding features through the stack and cross-cutting build and FFI rules, see [AGENTS.md](./AGENTS.md).
