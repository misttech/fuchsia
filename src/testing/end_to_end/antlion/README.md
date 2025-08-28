# antlion

Collection of host-driven, hardware-agnostic Fuchsia connectivity tests. Mainly
targeting WLAN and Netstack testing.

[Docs] | [Report Bug] | [Request Feature]

[TOC]

[Docs]: http://go/antlion
[Report Bug]: http://go/conn-test-bug
[Request Feature]: http://b/issues/new?component=1182297&template=1680893

## Getting started with QEMU

The quickest way to run antlion is by using the Fuchsia QEMU emulator. This
enables antlion tests that do not require hardware-specific capabilities like
WLAN. This is especially useful to verify if antlion builds and runs without
syntax errors. If you require WLAN capabilities, see
[below](#running-with-a-local-physical-device).

1. [Checkout Fuchsia](https://fuchsia.dev/fuchsia-src/get-started/get_fuchsia_source)

2. Configure and build Fuchsia to run antlion tests virtually on QEMU

   ```sh
   fx set core.qemu-x64 \
      --with //src/testing/sl4f \
      --with //src/sys/bin/start_sl4f \
      --args 'core_realm_shards += [ "//src/testing/sl4f:sl4f_core_shard" ]' \
      --with-host //src/testing/end_to_end/antlion:e2e_tests_quick
   fx build
   ```

3. In a separate terminal, run the emulator with networking enabled

   ```sh
   ffx emu stop && ffx emu start -H --net tap && ffx log
   ```

4. In a separate terminal, run a package server

   ```sh
   fx serve
   ```

5. Run an antlion test

   ```sh
   fx test --e2e --output //src/testing/end_to_end/antlion/tests/examples:sl4f_sanity_test
   ```

## Running with a local physical device

A physical device is required for most antlion tests, which rely on physical I/O
such as WLAN and Bluetooth. Antlion is designed to make testing physical devices
as easy, reliable, and reproducible as possible. The device will be discovered
using FFX or mDNS, so make sure your host machine has a network connection to
the device.

1. Configure and build Fuchsia for your target with the following extra
   arguments:

   ```sh
   fx set core.my-super-cool-product \
      --with-host //src/testing/end_to_end/antlion:e2e_tests
   fx build
   ```

2. Ensure your device is flashed with an appropriate build

3. In a separate terminal, run a package server

   ```sh
   fx serve
   ```

4. Run an antlion test

   ```sh
   fx test --e2e --output //src/testing/end_to_end/antlion/tests/functional:ping_stress_test
   ```

If you would like to include an AP in your test config:

1. Run a test with an AP

   ```sh
   fx test --e2e --output //src/testing/end_to_end/antlion/tests/functional:wlan_scan_test_without_wpa2 \
      -- --ap-ip 192.168.1.50 --ap-ssh-port 22
   ```

If you would like to skip device discovery, or use further auxiliary devices,
you can generate your own Mobly config.

1. Write the config

   ```sh
   cat <<EOF > my-antlion-config.yaml
   TestBeds:

   - Name: antlion-runner
   Controllers:
      FuchsiaDevice:
      - mdns_name: fuchsia-00e0-4c01-04df
        ip: ::1
        ssh_port: 8022
   MoblyParams:
      LogPath: logs
   EOF
   ```

1. Run an antlion test

   ```sh
   fx test --e2e --output //src/testing/end_to_end/antlion/tests/functional:ping_stress_test -- --config-override $PWD/my-antlion-config.yaml
   ```
