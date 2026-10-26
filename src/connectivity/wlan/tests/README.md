# Fuchsia WLAN Testing

| Test Suite | Scope | Environment | Guide |
| :--- | :--- | :--- | :--- |
| **Crate Unit Tests** | Algorithms, frame parsers, crypto handshakes, and state machines in `lib/*`, `wlancfg`, `wlanix`, `wlandevicemonitor` | Host or hermetic component sandbox | [`../lib/README.md`](../lib/README.md) |
| **WLAN Core** | Core stack components without policy layer | Hardware testbed | [`core/README.md`](core/README.md) |
| **WLAN Policy** | Policy behaviors, auto-connect, network selection, roaming | Hardware testbed | [`wlancfg/`](wlancfg/) |
| **WLANIX** | Android WiFi HAL implementation for Starnix | Hardware testbed | [`wlanix/README.md`](wlanix/README.md) |
| **E2E Automation** | Executing and debugging test suites | Local / remote testbeds | [E2E Skill](../.agents/skills/wlan-run-e2e-test/SKILL.md) |
