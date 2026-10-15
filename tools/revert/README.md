# `fx revert`

`//tools/revert` (`fx revert`) is a host tool for orchestrating pure reverts
of submitted Gerrit changes across Fuchsia repositories (e.g. `fuchsia.git`
and `vendor/google`).

Reverts are created server-side through Gerrit's REST API rather than by
pushing locally generated commits. This produces authentic Gerrit reverts that
**Rubber Stamper** (`rubber-stamper@appspot.gserviceaccount.com`) recognizes
and approves automatically, so the tree can be unblocked without waiting for a
human code review.

The tool is under active development; usage documentation will be added as
functionality lands.
