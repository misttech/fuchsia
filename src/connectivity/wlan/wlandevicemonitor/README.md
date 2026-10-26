# WLAN Device Monitor

`wlandevicemonitor` is the central device management component for the Fuchsia WLAN stack. It discovers physical WLAN hardware (PHYs) from vendor drivers and manages interface lifecycles. It also connects policy services to the Station Management Entity (SME).

## Responsibilities

1. **Hardware Discovery (`device_watch.rs`)**:
   - Watches device filesystem and driver framework nodes for newly bound WLAN PHY devices.
   - Maintains an active inventory of PHY devices and their hardware capabilities (bands, channels, supported roles).

2. **Interface Lifecycle Management**:
   - Serves `fuchsia.wlan.device.service.DeviceMonitor`.
   - Handles `CreateIface` requests from policy by issuing `fuchsia.wlan.phy/WlanPhy.CreateIface` calls to the underlying vendor driver.
   - Manages interface teardown (`DestroyIface`) and assigns stable system-wide interface IDs.

3. **SME Channel Handoff**:
   - During interface creation, `wlandevicemonitor` passes a `GenericSme` server endpoint to the driver.
   - `wlandevicemonitor` returns the matching `GenericSme` client endpoint to the requesting policy component (`wlancfg` or `wlanix`).
   - All subsequent scanning, association, and station management operations occur directly between the policy layer and the driver-hosted SME.

4. **Event Streaming & Diagnostics**:
   - Exposes `WatchDevices` and `WatchPhyEvents` to stream real-time PHY and interface addition/removal events to subscribers.
   - Publishes device counts, interface states, and hardware capabilities into Fuchsia Inspect.
