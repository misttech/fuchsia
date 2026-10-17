# Userboot

## Userboot Protocol (`fuchsia.boot.Userboot`)

The booted program (`userboot.next`) receives the server end of a
`fuchsia.boot.Userboot` channel in its bootstrap message as `PA_USER0` with
argument `0`.

Userboot sends pipelined one-way messages on this channel before closing its
client end:
- `fuchsia.boot.Userboot/PostStashSvc`: Transfers the server end of the
  `fuchsia.boot.SvcStash` protocol to the booted program.
- `fuchsia.boot.Userboot/PostBootfsVmo`: Transfers the uncompressed BOOTFS VMO
  to the booted program.
- `fuchsia.boot.Userboot/PostBootfsFiles`: Transfers the executable BOOTFS file
  VMOs already extracted for the first process and its loader service so that
  the booted program (e.g., `component_manager`) can reuse them without
  duplicating pages.
- `fuchsia.boot.Userboot/PostBootHandles`: Transfers non-bootstrap handles
  provided by the kernel (such as the ZBI VMO, kernel file VMOs, and system,
  MMIO, IRQ, I/O port, and SMC resource handles) to the booted program.

## Svc Stub and Svc Stash (`fuchsia.boot.SvcStash`)

Processes launched by userboot may be provided with a handle of `PA_NS_DIR`
type and path `/svc`. This allows early boot processes to pipeline requests to
services that will eventually become available (such as
`fuchsia.debugdata.Publisher`).

For each launched process given a `/svc` handle, userboot sends the
corresponding server end to the `SvcStash` channel via
`fuchsia.boot.SvcStash/Store`.

