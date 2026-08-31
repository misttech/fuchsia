# root_ssl_certificates

This directory defines the `root_ssl_certificates` package, which provides
Fuchsia's general-purpose public TLS trust store.

## Overview

* **Source**: The certificate bundle (`cert.pem`) represents the **Chrome Root Store**.
* **Synchronization**: Automatically updated on a regular schedule via an automated
  synchronization pipeline. Manual edits to `cert.pem` should not be made directly.
* **Usage**:
  * Offered to components via the `root-ssl-certificates` directory capability (mounted at `/config/ssl`).
  * Used by web engines, general HTTP clients, and non-Google network services.
* **Restricted Alternative**: First-party Google services (such as SWD, Feedback,
  Cobalt, and Timekeeper) use
  [google_root_ssl_certificates](//src/security/bin/google_root_ssl_certificates)
  instead to restrict trust exclusively to Google CAs.

