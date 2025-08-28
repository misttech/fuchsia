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
      --with-host //third_party/antlion:e2e_tests_quick
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
   fx test --e2e --output //third_party/antlion/tests/examples:sl4f_sanity_test
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
      --with-host //third_party/antlion:e2e_tests
   fx build
   ```

2. Ensure your device is flashed with an appropriate build

3. In a separate terminal, run a package server

   ```sh
   fx serve
   ```

4. Run an antlion test

   ```sh
   fx test --e2e --output //third_party/antlion/tests/functional:ping_stress_test
   ```

If you would like to include an AP in your test config:

1. Run a test with an AP

   ```sh
   fx test --e2e --output //third_party/antlion/tests/functional:wlan_scan_test_without_wpa2 \
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
   fx test --e2e --output //third_party/antlion/tests/functional:ping_stress_test -- --config-override $PWD/my-antlion-config.yaml
   ```

## Running without a Fuchsia checkout

Requires Python 3.11+

1. Clone the repo

   ```sh
   git clone https://fuchsia.googlesource.com/antlion
   ```

2. Install dependencies using [venv](https://docs.python.org/3/library/venv.html#how-venvs-work)

   ```sh
   cd antlion
   python3 -m venv .venv      # Create a virtual environment in the `.venv` directory
   source .venv/bin/activate  # Activate the virtual environment
   pip install --editable ".[mdns]"
   # Run `deactivate` later to exit the virtual environment
   ```

3. Write the sample config and update the Fuchsia controller to match your
   development environment

   ```sh
   cat <<EOF > simple-config.yaml
   TestBeds:
   - Name: antlion-runner
     Controllers:
       FuchsiaDevice:
       - ip: fuchsia-00e0-4c01-04df
   MoblyParams:
     LogPath: logs
   EOF
   ```

   Replace `fuchsia-00e0-4c01-04df` with your device's nodename, or
   `fuchsia-emulator` if using an emulator. The nodename can be found by looking
   for a log similar to the one below.

   ```text
   [0.524][klog][klog][I] netsvc: nodename='fuchsia-emulator'
   ```

4. Run the sanity test

   ```sh
   python tests/examples/Sl4fSanityTest.py -c simple-config.yaml
   ```

## Contributing

Contributions are what make open source projects a great place to learn,
inspire, and create. Any contributions you make are **greatly appreciated**.
If you have a suggestion that would make this better, please create a CL.

Before contributing, additional setup is necessary:

- Install developer Python packages for formatting and linting

  ```sh
  pip install --editable ".[dev]"
  ```

- Install an [EditorConfig](https://editorconfig.org/) plugin for consistent
  whitespace

- Complete the steps in '[Contribute source changes]' to gain authorization to
  upload CLs to Fuchsia's Gerrit.

To create a CL:

1. Create a branch (`git checkout -b feature/amazing-feature`)
2. Make changes
3. Document the changes in `CHANGELOG.md`
4. Auto-format changes (`./format.sh`)

   > Note: antlion follows the [Black code style] (rather than the
   > [Google Python Style Guide])

5. Verify no typing errors (`mypy .`)
6. Commit changes (`git add . && git commit -m 'Add some amazing feature'`)
7. Upload CL (`git push origin HEAD:refs/for/main`)

> A public bug tracker is not (yet) available.

[Black code style]: https://black.readthedocs.io/en/stable/the_black_code_style/current_style.html
[Google Python Style Guide]: https://google.github.io/styleguide/pyguide.html
[Contribute source changes]: https://fuchsia.dev/fuchsia-src/development/source_code/contribute_changes#prerequisites

### Recommended git aliases

There are a handful of git commands that will be commonly used throughout the
process of contributing. Here are a few aliases to add to your git config
(`~/.gitconfig`) for a smoother workflow:

- `git amend` to modify your CL in response to code review comments
- `git uc` to upload your CL, run pre-submit tests, enable auto-submit, and
   add a reviewer

```gitconfig
[alias]
  amend = commit --amend --no-edit
  uc = push origin HEAD:refs/for/main%l=Commit-Queue+1,l=Fuchsia-Auto-Submit+1,publish-comments,r=sbalana
```

You may also want to add a section to ignore the project's large formatting changes:

```gitconfig
[blame]
  ignoreRevsFile = .git-blame-ignore-revs
```

## License

Distributed under the Apache 2.0 License. See `LICENSE` for more information.

## Acknowledgments

This is a fork of [ACTS][ACTS], the connectivity testing framework used by
Android. The folks over there did a great job at cultivating amazing tools, much
of which are being used or have been extended with additional features.

[ACTS]: https://fuchsia.googlesource.com/third_party/android.googlesource.com/platform/tools/test/connectivity/

### Migrating CLs from ACTS

`antlion` and ACTS share the same git history, so migrating existing changes is
straightforward:

1. Checkout to latest `main`

   ```sh
   git checkout main
   git pull --rebase origin main
   ```

2. Cherry-pick the ACTS change

   ```sh
   git fetch acts refs/changes/16/12345/6 && git checkout -b change-12345 FETCH_HEAD
   git fetch https://android.googlesource.com/platform/tools/test/connectivity refs/changes/30/2320530/1 && git cherry-pick FETCH_HEAD
   ```

3. Resolve any merge conflicts, if any

   ```sh
   git add [...]
   git rebase --continue
   ```

4. Upload CL

   ```sh
   git push origin HEAD:refs/for/main # or "git uc" if using the alias
   ```
