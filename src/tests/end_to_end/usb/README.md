# Running `usb_virtual_disconnect_test` and `usb_reset_test` at desk

The runner script verifies that a Fuchsia USB device is attached, configures
one-time host `udev` rules, adds
`//src/tests/end_to_end/usb:usb_virtual_disconnect_test` to the build graph if
missing, and executes `fx test`:

```sh
./src/tests/end_to_end/usb/lib/disconnect/run_usb_virtual_disconnect_test_at_desk.sh
```

To target a specific device or pass extra flags through to `fx test` (for
example, `--test-filter` to run a single iteration instead of all 10):

```sh
./src/tests/end_to_end/usb/lib/disconnect/run_usb_virtual_disconnect_test_at_desk.sh \
  -t <device-name> -- --test-filter test_usb_disconnect_1
```

To reset the device's USB port with the `USBDEVFS_RESET` ioctl instead of
toggling its USB `authorized` attribute, run `usb_reset_test` with
`--mode reset`:

```sh
./src/tests/end_to_end/usb/lib/disconnect/run_usb_virtual_disconnect_test_at_desk.sh \
  --mode reset
```
