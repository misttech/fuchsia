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

2. Launch the `squares` demo in the `display-tool` test utility.

   ```posix-terminal
   ffx target ssh display-tool squares
   ```

3. Add the following footer to your CL description, to document having
   performed the test.

   ```
   Test: ffx target ssh display-tool squares
   ```

These instructions will work with a `workbench_eng.x64` build that includes the
`//src/graphics/display:tools` GN target. The `//src/graphics/display:tests`
target is also recommended, as it builds the automated unit tests. Debug
assertions, which are extensively used in display drivers, are only enabled in
debug builds.

```posix-terminal
fx set workbench_eng.x64 --debug --with //src/graphics/display:tools \
    --with //src/graphics/display:tests
```

[fuchsia-hardware-support]: https://fuchsia.dev/fuchsia-src/reference/hardware/support-system-config
