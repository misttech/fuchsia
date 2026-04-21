# Intel Framebuffer Display Driver

This is a simple display driver for Intel GPUs that uses the framebuffer
configured by the bootloader. It is intended for early-boot use before
`intel-display` is available.

## Manual testing

We do not currently have automated integration tests. Behavior changes in this
driver must be validated using this manual test.

Start with a [supported Intel device][fuchsia-hardware-support].

1. Remove the device list from
   `src/graphics/display/drivers/intel-display/meta/intel-display.bind` so the
   display binds to this driver instead of `intel-display`.

2. Build and flash `core.x64`.

3. Verify that the virtcon (Fuchsia `f` logo) shows on the display and that the
   terminal responds to keyboard input.

4. Add the following footer to your CL description, to document having
   performed the test.

   ```
   Test: Manual virtcon check with keyboard input on <supported Intel device>
   ```

[fuchsia-hardware-support]: https://fuchsia.dev/fuchsia-src/reference/hardware/support-system-config
