# Kernel User ABI (`userabi`)

This directory contains the kernel-side support for the Zircon user-space ABI:

* **vDSO (`libzircon.so`)**: Located in [`vdso/`](vdso/), providing the system
  call entry points and fast-path time/ticks routines mapped into user-space
  processes.
* **Kernel-to-`userboot` bootstrap**: [`userabi.rs`](userabi.rs) (along with
  [`elf.rs`](elf.rs), [`resource.rs`](resource.rs), and [`vdso.rs`](vdso.rs))
  loads the first user-space process (`userboot`) from the kernel package and
  sends it the two bootstrap channel messages (the process capability message
  followed by the system capability message).

The canonical documentation for the kernel-to-`userboot` bootstrap protocol and
the `userboot` startup support library lives in
[`//src/bringup/lib/userboot/README.md`](/src/bringup/lib/userboot/README.md).
The `userboot` binary implementation itself lives in
[`//src/bringup/bin/userboot/`](/src/bringup/bin/userboot/).
