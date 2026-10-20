# devscreen

A readable boot dashboard for display-only `eng` builds (e.g. `minimal.*`).

On products without a UI stack, the only thing on the panel is virtcon's logo
tab, which is unreadable on a small, high-DPI display. `devscreen` is a small
Carnelian app that runs directly on the display coordinator above virtcon's
priority (`COMPOSITOR_CLIENT_PRIORITY_VALUE`) and shows, in large type:

* product / board / build version, device name, serial, uptime
* last boot: reason, whether it was graceful, previous uptime
  (`fuchsia.feedback.LastRebootInfoProvider`; red when ungraceful)
* IP addresses of online interfaces
* WLAN: station MAC of the client iface, and a count of networks seen by a
  scan every ~30 s (`fuchsia.wlan.policy.ClientProvider`). SSIDs are never
  shown. On builds where nothing else starts client connections, devscreen
  calls `StartClientConnections` once so a scan is possible.
* battery level, charge state, voltage, smoothed power (mW, `+` charging),
  pack temperature coloured by the driver's health band
  (straight from `fuchsia.hardware.power.battery`, no `battery-manager` needed)
* charger: source type, charge phase / operating mode, input voltage and
  current, float voltage (`fuchsia.hardware.power.charger`). On `minimal`
  nothing programs the charger, so the pack floats at the default voltage
  and never reports `Full`; this row shows why.
* free memory, CPU load, per-domain CPU frequency (`fuchsia.hardware.cpu.ctrl`)
* the hottest temperature sensors, coloured by band
  (green < 45 °C, amber < 60 °C, orange < 75 °C, red above)

and four buttons:

* **SAVE SNAPSHOT**: `fuchsia.feedback.DataProvider.GetSnapshot`, saved to
  the component's `data` storage; the newest five are kept. Fetch with
  `ffx component storage copy /core/devscreen::/snapshot-<uptime>.zip .`
  (`ffx component storage list /core/devscreen::/` to see them).
* **FASTBOOT**: `Admin.RebootToBootloader` (two taps to confirm)
* **REBOOT**: `Admin.PerformReboot` (two taps to confirm)
* **VIRTCON**: exits; the display coordinator hands the panel back to virtcon (there is no way back yet, see below).
  Relaunch with `ffx component start /core/devscreen`.

Every data source is optional: every `use` is `availability: "optional"` and
the core shard offers from `#feedback`, `#wlancfg`, `#wlandevicemonitor` etc.
with `source_availability: "unknown"`, so a product that lacks any of them
still starts devscreen and shows `N/A` for that row.

Driver-backed readings (battery, charger, temperature, CPU clocks) are
collected on their own task: a driver that is slow, hung or restarted (for
example by a driver test) cannot stall the dashboard, and new driver
instances are picked up again within a few seconds.

## Enabling it

devscreen ships as the `devscreen` assembly input bundle and is **off by
default** on every product. Turn it on with the platform configuration knob
`platform.development_support.include_devscreen` (`standard` `eng` builds
only; it is never included on `user` or `userdebug` builds).

To try it on `minimal` without touching product configuration, use a
developer override (`//local/BUILD.gn`):

```gn
import("//build/assembly/developer_overrides.gni")

assembly_developer_overrides("devscreen") {
  platform = {
    development_support = {
      include_devscreen = true
    }
  }
}
```

and `fx set ... --assembly-override //local:devscreen`.

Products opt in by setting the same key in their product configuration.

## Debugging

```
ffx log --filter devscreen dump
ffx component show /core/devscreen
ffx component start /core/devscreen      # after VIRTCON
ffx component stop /core/devscreen       # hand the panel back to virtcon
```

## Known gaps

* **No way back after VIRTCON.** Once devscreen exits, nothing on the device
  relaunches it; use `ffx component start /core/devscreen` from the host. A
  hardware-button chord needs a long-lived process that does not own the
  display (follow-up).
* Hit-testing uses cached button rects, so taps register even while the
  scene is being rebuilt; if buttons ever feel dead, `ffx log` shows
  `touch down at (x, y) hit no button` lines with the coordinates.
