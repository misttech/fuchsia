# AMD Framebuffer Display Driver

This is a simple display driver for AMD GPUs that uses the framebuffer configured by
the bootloader. It is intended for early-boot use before a full AMD display driver is
available.

## Supported hardware

This driver does not implement any AMD-specific hardware programming and it relies
entirely on the framebuffer that the bootloader (UEFI/Gigaboot) has already
configured. It will bind to any PCI device with AMD's vendor ID and PCI `Display`
class, and reads the framebuffer through PCI BAR 0 (a convention that holds
across AMD's discrete and integrated GPUs).

The driver has only been validated on the following hardware:

* AMD Strix Halo (Ryzen AI Max+ 395)

## Manual testing

We do not currently have automated integration tests. Behavior changes in this driver
must be validated using this manual test.

1. Build and flash `core.x64` on a machine with an AMD GPU.

2. Verify that the virtcon (Fuchsia `f` logo) shows on the display and that the
   terminal responds to keyboard input.

3. Add the following footer to your CL description, to document having performed
   the test.

   ```
   Test: Manual virtcon check with keyboard input on <supported AMD device>
   ```
