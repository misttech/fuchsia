# Validating driver changes in QEMU

**Prerequisites:** This process requires a graphical desktop environment.
The steps below do not work in a headless SSH session.

Use the steps below to validate a code change in QEMU.

1. Ensure the emulator is stopped: `ffx emu stop --all`
2. Build the project (may take a few minutes): `fx build --quiet`
3. Create the logs storage directory: `mkdir -p local`
4. Delete any old logs: `rm -f local/logs.qemu.*`
5. Start the emulator, saving QEMU tool logs:
   `ffx emu start --engine qemu --net tap --log local/logs.qemu`
6. Wait for `ffx` to exit. The last output line should be: `Emulator is ready.`
7. Wait for the emulator ffx connection to stabilize: `ffx target wait`
8. Check for errors and inconsistencies in the driver's output in the UART logs.
   Assuming the `ffx emu start` command above, the UART logs are at
   `local/logs.qemu.serial`. Example:
   `grep --context=3 "virtio-gpu-display" local/logs.qemu.serial`
9. Optionally, save the Fuchsia system logs:
   `ffx log dump > local/logs.qemu.fuchsia`
10. Check for ERROR entries (software implementation errors) in the UART logs or
    in the Fuchsia system logs. Example:
    `grep --context=3 "ERROR" local/logs.qemu.serial`

Use the steps below to run shell tools in the emulator, such as
`display-tool`.

1. Run a package server (blocks; needs its own terminal):
   `fx serve --foreground`
2. Wait for the command above to output a line similar to:
   `Serving repository '/ssd/fuchsia/out/x64/amber-files' over address '[::]:8083'.`
3. Run the command: `ffx target ssh -- {COMMAND} [{ARGUMENTS...}]`.
   Example: `ffx target ssh -- display-tool info`
4. If starting the command fails and the output contains
   `Cannot create child process: -25 (ZX_ERR_NOT_FOUND)`, wait a few seconds for
   the VM to connect to the package server, and try again.
