# root_ssl_certificates

This directory contains the `root_ssl_certificates` package.

The certificates file, `third_party/cert/cert.pem`, is automatically updated via
a Copybara pipeline from google3 (`//security/cacerts/mozilla:roots.pem`).
The pipeline runs on a weekly schedule and exports the compiled certificates
directly to this repository.

Manual updates are no longer required.

The contents of `third_party/cert/cert.pem` are covered by the license file
`third_party/cert/LICENSE.MPLv2`.
