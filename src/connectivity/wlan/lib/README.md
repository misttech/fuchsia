# WLAN Core & Shared Libraries

This directory contains the shared Rust and C++ libraries for the Fuchsia WLAN stack. These libraries implement IEEE 802.11 protocols, state machines, cryptographic handshakes, and platform utilities.

## Library Catalog

| Directory | Target / Crate Name | Language | Purpose & Key Types |
| :--- | :--- | :--- | :--- |
| [`bitfield`](bitfield/) | `wlan-bitfield` | Rust (Macro) | Declarative procedural macro for bitfield packing and unpacking across byte boundaries. |
| [`common`](common/) | `wlan-common` / `common` | Rust / C++ | General stack utilities: IE parsing, MAC frame parsers, fake frame/STA test helpers, BSS descriptors. See [`common/README.md`](common/README.md). |
| [`eapol`](eapol/) | `eapol` | Rust | EAPOL (Extensible Authentication Protocol over LAN) frame parsing, serialization, and key descriptors (RC4, AES key wrap). |
| [`fcg-crypto`](fcg-crypto/) | `wlan-fcg-crypto` | Rust | Finite Cyclic Group (FCG) cryptography, Simultaneous Authentication of Equals (SAE / WPA3-Personal), and Opportunistic Wireless Encryption (OWE / Enhanced Open) using BoringSSL. |
| [`ffi-transport`](ffi-transport/) | `wlan-ffi-transport` | Rust | Memory transport and buffer management bridging the Rust MLME to the C++ `wlansoftmac` driver without unnecessary copies. |
| [`fidl-ext`](fidl-ext/) | `wlan-fidl-ext` | Rust | Extensions and helper traits (`ResponderExt`, `TryUnpack`, `WithName`) for FIDL request and response handling. |
| [`frame_writer`](frame_writer/) | `wlan-frame-writer` | Rust | Procedural macro and buffer utilities for declarative zero-copy IEEE 802.11 frame serialization. |
| [`ieee80211`](ieee80211/) | `ieee80211` | Rust | Pure primitive definitions for IEEE 802.11: MAC addresses (`MacAddr`, `Bssid`), frequency bands, channels, and management frame formats. |
| [`ieee80211-testutils`](ieee80211-testutils/) | `ieee80211-testutils` | Rust | Test fixture generators, mock beacon and probe response builders, and frame synthesis helpers. |
| [`mlme`](mlme/) | `wlan-mlme` / `mlme-fullmac` | Rust | MAC Sublayer Management Entity: SoftMAC host state machines (`mlme/rust`) and FullMAC driver translation (`mlme/fullmac`). See [`mlme/README.md`](mlme/README.md). |
| [`power_manager`](power_manager/) | `power_manager` | Rust | System power management client for monitoring system power states and applying WiFi power scenarios. |
| [`rsn`](rsn/) | `wlan-rsn` | Rust | IEEE 802.11i RSN: 4-Way Handshake supplicant and authenticator, GTK/PTK key derivation, and PMKSA caching. |
| [`sme`](sme/) | `wlan-sme` | Rust | Station Management Entity: high-level interface management, scan orchestration, roaming logic, and SME FIDL service. See [`sme/README.md`](sme/README.md). |
| [`statemachine`](statemachine/) | `wlan-statemachine` | Rust | Compile-time verified state machine abstraction used by SME and MLME connection states. |
| [`storage`](storage/) | `wlan-storage` | Rust | Stash-backed key-value configuration storage and serialization for saved network profiles and credentials. |
| [`telemetry`](telemetry/) | `wlan-telemetry` | Rust | WLAN metrics aggregation, structured Inspect reporting, event throttling, and uptime tracking. |
| [`trace`](trace/) | `wlan-trace` | Rust | Low-overhead Fuchsia trace duration and async event helpers for performance instrumentation. |
| [`wep_deprecated`](wep_deprecated/) | `wep_deprecated` | Rust | Legacy WEP key parsing and authentication verification for backward compatibility. |

## Key Architectural Boundaries

See the following guides for the architecture of the primary core libraries:
- [`sme/README.md`](sme/README.md): Station Management Entity client and AP state machines.
- [`mlme/README.md`](mlme/README.md): MAC Sublayer Management Entity (SoftMAC state machines and FullMAC translation).
- [`common/README.md`](common/README.md): Common frame parsers, element parsing, and test utilities.
