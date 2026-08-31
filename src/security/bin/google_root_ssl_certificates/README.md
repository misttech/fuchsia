# google_root_ssl_certificates

This directory defines the `google_root_ssl_certificates` package, which provides
the restricted Google-only TLS trust store for Fuchsia.

## Overview

* **Source**: The certificate bundle (`cert.pem`) contains exclusively **Google Root CAs**.
  It excludes all public and commercial third-party Certificate Authorities.
* **Synchronization**: Automatically updated on a regular schedule via an automated
  synchronization pipeline. Manual edits to `cert.pem` should not be made directly.
* **Usage**:
  * Used by first-party Google services connecting to Google backends (such as
    `omaha-client`, `system-updater`, `feedback`, `cobalt`, and `timekeeper`).
  * In assembly, products configure `software_delivery.trust_store = "restricted"` to
    enforce that SWD uses this store.
* **Defense-in-Depth**:
  * In-tree Scrutiny static analysis and server-side signing validation guarantee that
    production Google devices route SWD traffic strictly through this restricted store.
