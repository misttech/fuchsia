# antlion-runner

A program to run antlion locally and in infrastructure. Includes a config
generator with mDNS discovery and sensible defaults.

## Using locally with an emulator

Running antlion locally with a Fuchsia emulator allows developers to perform a
sanity checks on their changes. Running this way is very quick (~5 seconds) and
can spot simple mistakes before code review!

1. Build Fuchsia with antlion support

   ```sh
   jiri update -gc # if you haven't updated in a while
   fx set workstation_eng_paused.qemu-x64 \
      --with-host //src/testing/end_to_end/antlion:e2e_tests \
      --with-host //src/testing/end_to_end/antlion:tests
   fx build # if you haven't built in a while
   ```

2. Start the package server. Keep this running in the background.

   ```sh
   fx serve
   ```

3. In a separate terminal, start the emulator with access to external networks.

   ```sh
   fx ffx emu stop && fx ffx emu start -H --net tap && fx ffx log
   ```

4. In a separate terminal, run a test

   ```sh
   fx test --e2e --output //src/testing/end_to_end/antlion:sl4f_sanity_test
   ```

## Using a specified config file

```sh
fx test --e2e --output //src/testing/end_to_end/antlion:sl4f_sanity_test -- --config-override $(pwd)/config.yaml
```

## Testing

```sh
fx test --output //src/testing/end_to_end/antlion/runner:runner_test
```
