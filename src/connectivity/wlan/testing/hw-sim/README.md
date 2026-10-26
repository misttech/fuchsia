# WLAN Hardware Simulator (`hw-sim`)

WLAN Hardware Simulator (`hw-sim`) is an integration testing framework. It simulates Fuchsia WLAN devices, vendor drivers, and radio environments in software without physical radio hardware.

## Architecture

`hw-sim` runs tests in hermetic test realms that `testing/realm-factory` builds. The testing architecture coordinates these components:

```
+-------------------------------------------------------+
|                Test Component (Rust)                  |
|          TestHelper & EventHandlerBuilder             |
+-------------------------------------------------------+
           |                                  ^
           | fuchsia.wlan.tap                 | Simulated Frames
           v                                  | & PHY Events
+---------------------+                       |
|   wlantap-driver    | <---------------------+
| (Virtual PHY/Iface) |
+---------------------+
           |
           v
+-------------------------------------------------------+
|                 Isolated Test Realm                   |
|  - wlancfg (Policy)                                   |
|  - wlandevicemonitor (Device Management)              |
|  - wlansoftmac (Platform SoftMAC Driver)              |
+-------------------------------------------------------+
```

## Key Components

1. **`wlantap-driver` (`fuchsia.wlan.tap`)**:
   - Creates synthetic WLAN PHY and interface devices on demand in the test realm.
   - Intercepts outbound TX frames from the stack and delivers simulated RX frames, beacons, and PHY state changes into the stack.

2. **`TestHelper` & `EventHandlerBuilder`**:
   - The test harness uses `TestHelper` to initialize the simulated environment and configure mock APs.
   - `EventHandlerBuilder` configures declarative expectations and automated responses for frame exchanges (e.g., auto-replying to probe requests, auth frames, and association handshakes).

3. **`testing/realm-factory`**:
   - Assembles an isolated component topology containing `wlancfg`, `wlandevicemonitor`, and `wlansoftmac`.
   - Ensures test executions do not depend on or interfere with the host system's real wireless state.

## Frame & Event Traversal

During a test (e.g., `simulate_scan` or `connect_to_open_network`):
1. The test harness requests `wlantap` to create a virtual PHY.
2. `wlandevicemonitor` detects the new PHY and exposes it to `wlancfg`.
3. When `wlancfg` or SME initiates a scan or connection, MLME transmits frames down to `wlansoftmac` and `wlantap`.
4. `TestHelper` receives the TX frame via `fuchsia.wlan.tap` and passes it to `EventHandlerBuilder`. It then injects synthetic RX frames back into `wlantap` to advance the stack state machine.
