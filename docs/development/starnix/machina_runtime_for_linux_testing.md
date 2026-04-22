# Machina runtime for Linux testing

This guide provides instructions for running and debugging Starnix tests against
a Linux environment using Machina.

Machina runs tests inside a Linux virtual machine, providing an isolated and
standardized environment using our target kernel version. Note that running
Machina-based Starnix tests requires Debian guest images available locally.
Googlers will have such images available by default, while external developers
will need to bring their own. External developers should read the
[Virtualization Get Started][virtualization-get-started] guide for more
information on how to build and provide their own Debian images.

## Running existing Machina tests locally {#running-existing-machina-tests-locally}

### Host prerequisites

Machina is a virtualization environment that is limited to modern Intel-based
host machines. For Googlers, a modern VM-capable Cloudtop provides the ideal
environment. Older, deprecated Cloudtops may need to resized to a more modern
image. Ensure that your host is compatible with the following checks:

-   **Intel-based**:

    ```posix-terminal
    lscpu | grep "Vendor ID"
    ```

    You should see: `Vendor ID: GenuineIntel`. If not, you'll need to procure an
    Intel-based machine. For Googlers, a Cloudtop with nested virtualization
    should be sufficient.

-   **Virtualization capable**:

    Follow the instructions in [Enable VM acceleration][enable-vm-acceleration].

-   **Modern, performant CPU**:

    ```posix-terminal
    if lscpu | grep -qE "Clear CPU buffers"; then echo 'CPU has performance-hindering mitigations.'; else echo 'No performance issues found.'; fi
    ```

    You should see: `No performance issues found`. Some older Intel CPUs may
    have security vulnerability mitigations which greatly impact Machina
    performance.

If your environment meets the Intel requirement, but not the other requirements,
then you may consider increasing the `fx test` timeout as a workaround.

### Environment setup {#environmental-setup}

1.  Set a product configuration which supports virtualization. Most products
    will work, so long as the board is `x64`. Here's an example configuration:

    ```posix-terminal
    fx set fuchsia.x64 --main-pb workbench_eng.x64
    ```

1.  Add the `linux_vm_tests` target:

    ```posix-terminal
    fx add-test //src/starnix/tests:linux_vm_tests
    ```

1.  Bootstrap a headless emulator and package repository with the standard
    workflows. If you are unfamiliar, you can reference the
    [Set up FEMU][set-up-femu] guide.

### Running tests {#running-tests}

The test suites are the same as the vanilla syscalls, but prefixed with
`linux_`. Here are some examples of common target inclusion:

-   **Run all syscall test suites:**

    ```posix-terminal
    fx test linux_syscalls_cpp_tests
    ```

-   **Run individual suites:**

    ```posix-terminal
    fx test linux_fcntl_test
    ```

-   **Run on both Starnix and Linux:** Specify the base name of the target,
    which will run all variants that your environment may be configured to run
    (Starnix, Machina, Host). For example:

    ```posix-terminal
    fx test mount_test
    ```

## Debugging a failing Machina-based syscall test {#debugging-failing-tests}

### Understanding Logs {#understanding-logs}

The syscall tests are gTest suites, and output is piped through the test
framework. This means that the output from the gTest invocation appears in
stdout on failing tests. You can view all gTest logs, regardless of outcome,
using the `--output` arg in your `fx test` invocation.

Logs related to the Machina runtime are output to the system log (`ffx log`),
and typically associated with an identifying tag. Currently, there is one
runtime being employed for all syscall tests, using the `linux_guest` tag. The
exception to this rule is system logs related to kernel-side virtualization, for
instance logs emitted by the Zircon hypervisor. These logs will not have a
guest-specific identifier associated with them.

The following documents the system logs associated with a typical flow.

1.  **Initial guest bootstrap:**

    In the following logs, you see the `starnix_test_runner` component
    requesting a Machina guest with the `linux_guest` tag. The
    `starnix_test_runner` logs two lines about its interaction request. The
    third line is from the `interactive-debian-guest` component, which can be
    thought of as the running Machina guest component. It receives the request
    and starts bootstrapping. You can see all logs across the system are tagged
    with an identifier (`linux_guest`). As logs indicate, the Machina guest is
    bootstrapped lazily. This initial bootstrap typically takes **~60 seconds**
    to become ready for interactions, but is only necessary for the very first
    run.

    ```none {:.devsite-disable-click-to-copy}
    [00043.071313][starnix_test_runner][linux_guest] INFO: Pushing data to guest (destination: /data/tests/deps/clone_exec_helper)
    [00043.071321][starnix_test_runner][linux_guest] INFO: Interaction requested, lazily starting the guest instance.
    [00046.686157][interactive-debian-guest][linux_guest] INFO: [interactive_debian_guest_impl.cc(110)] Start requested for an interactive Debian guest.
    ```

1.  **Pushing test dependencies and binaries:**

    Once the guest is bootstrapped, preliminary data begins to be pushed to the
    guest. These are the required dependencies for syscall tests, followed by
    the test binary itself:

    ```none {:.devsite-disable-click-to-copy}
    [00439.934416][starnix_test_runner.cm][linux_guest,starnix_test_runner] INFO: Pushing data to guest (destination: /data/tests/deps/simple_ext4.img)
    [00441.040568][starnix_test_runner.cm][linux_guest,starnix_test_runner] INFO: Successfully pushed data to guest (destination: /data/tests/deps/simple_ext4.img)
    [00444.834216][starnix_test_runner.cm][linux_guest,starnix_test_runner] INFO: Pushing data to guest (destination: /starnix_linux_fuse_test_fuse_test_bin)
    [00448.652779][starnix_test_runner.cm][linux_guest,starnix_test_runner] INFO: Successfully pushed data to guest (destination: /starnix_linux_fuse_test_fuse_test_bin)
    ```

1.  **Test execution and processing:**

    Once the environmental setup steps are done, you should see the execution
    command issued. This is executing the gTest binary on the guest. Once again,
    the output of this binary execution is piped into stdout on your terminal,
    as you would expect from any other `fx test` invocation for a gTest suite.
    When execution completes, the results file is copied back over to the host
    for processing:

    ```none {:.devsite-disable-click-to-copy}
    [00448.653126][starnix_test_runner.cm][linux_guest,starnix_test_runner] INFO: Executing command on guest: /starnix_linux_fuse_test_fuse_test_bin --gtest_output=json:/test_result-ccde95ca-acb3-4b84-af4d-f371b9582d20.json)
    [00448.848658][starnix_test_runner.cm][linux_guest,starnix_test_runner] INFO: Command '/starnix_linux_fuse_test_fuse_test_bin --gtest_output=json:/test_result-ccde95ca-acb3-4b84-af4d-f371b9582d20.json'
    ...terminated with status Status(OK), return code 0
    [00448.849066][starnix_test_runner.cm][linux_guest,starnix_test_runner] INFO: Fetching file from guest (remote_path: /test_result-ccde95ca-acb3-4b84-af4d-f371b9582d20.json)
    ```

### Understanding the Runtime {#understanding-runtime}

The Starnix test runner is responsible for orchestrating the tests, and can be
thought of as the glue between the Fuchsia test framework and the Machina
runtime. There are two key things to understand related to the runtime:

1.  Syscall tests are identified in their CML by the `test_type: "syscall"`
    program tag in the `[syscalls_cpp_test.cml][syscalls-cml]`.
1.  The Starnix test runner looks for this tag when handling suite requests, and
    branches the logic accordingly to handle these tests. The core handling
    logic can be found in `[syscalls.rs][syscalls-rs]`, the main entry point
    being `run_syscall_tests`.

While there is much more to the runtime than just these two points, maintaining
it all as documentation would be arduous. Hopefully these two core pieces of the
system will provide solid anchor points for your own investigation and
debugging.

## Advanced debugging {#advanced-debugging}

### Verbose Linux Kernel Logging {#verbose-logging}

If you suspect something is going wrong in the Linux kernel itself, you can
enable more verbose Linux kernel logging by setting the following GN argument:

```gn
redirect_guest_serial_logs = true
```

This funnels Linux kernel logs into the standard system output, meaning you can
see guest kernel logs emitted by `ffx log`. Logs are emitted under the `vmm`
component, and will be tagged with `(guest klog)` for clarity on the source. For
instance:

```text {:.devsite-disable-click-to-copy}
[00352.454169][vmm.cm][vmm] INFO: [vmm.cc(491)] (guest klog): [    0.000000] Linux version 6.6.13-amd64 (debian-kernel@lists.debian.org) (gcc-13 (Debian 13.2.0-10) 13.2.0, GNU ld (GNU Binutils for Debian) 2.41.90.20240115) #1 SMP PREEMPT_DYNAMIC Debian 6.6.13-1 (2024-01-20)
```

In this log example, you see the `vmm` component emit a Linux kernel log line.
The `vmm` emitted this log at system uptime `00352.454169`. The `(guest klog)`
shows that the Linux kernel emitted their line at `[ 0.000000]`, or the zeroth
second of uptime from its perspective.

### Shelling into Machina {#shelling-into-machina}

You can access a fairly standard command line interface for the running Linux
guest. Note that test binaries and artifacts will **not** be present in the
images by default, as they are dynamically pushed by the test runner
scaffolding. See the following sections for more information on getting test
binaries and artifacts into the guest. Here are the steps to get access to this
shell:

1.  Follow the [Virtualization Get Started][virtualization-get-started] guide,
    which shows you how to set up your local GN args to enable virtualization
    tools.
1.  Launch an emulator and connect to the shell:

    ```posix-terminal
    fx shell
    ```

1.  Launch the Debian guest:

    ```posix-terminal
    guest launch debian
    ```

    This drops you into the Debian shell.

Note that the shell behavior can be confusing:

-   `exit` will **not** exit the guest shell.
-   `CTRL+C` will **not** terminate a running program, but instead exit the
    Debian shell back to Fuchsia.

As a tip, you can use `guest attach debian` to return into the shell if you
accidentally CTRL+C out of the Linux guest and into Fuchsia.

### Modifying Debian Images {#modifying-debian-images}

In some cases, you may wish to alter the default Debian images (e.g., to add
binaries or debug programs). Since there is no simple way to push artifacts
over, you must modify the images directly.

1.  **Mount the image:** On your Linux host, install tools and mount the image:

    ```
    sudo apt-get install libguestfs-tools
    sudo mkdir /mnt/machina_guest_img
    sudo guestmount -a prebuilt/virtualization/packages/debian_guest/images/x64/rootfs.qcow2 -m /dev/vda /mnt/machina_guest_img/
    ```

1.  **Interact with the image:** You can now copy files to the mounted
    directory. For example, to copy the mount test binary and dependencies:

    ```
    sudo cp ./out/core.x64-balanced/linux_x64/linux_mount_test_bin /mnt/machina_guest_img/home/
    sudo mkdir -p /mnt/machina_guest_img/home/data/tests/deps/
    sudo cp src/starnix/tests/syscalls/cpp/data/* /mnt/machina_guest_img/home/data/tests/deps/
    ```

1.  **Unmount the image:**

    ```posix-terminal
    sudo guestunmount /mnt/machina_guest_img
    ```

After unmounting, the image will contain your changes. You can then shell into
the environment (as detailed above) and execute binaries as needed.

<!-- Reference links -->

[enable-vm-acceleration]: /docs/get-started/set_up_femu.md#enable-vm-acceleration
[set-up-femu]: /docs/get-started/set_up_femu.md
[syscalls-cml]: /src/starnix/tests/syscalls/cpp/meta/syscalls_cpp_test.cml
[syscalls-rs]: /src/sys/test_runners/starnix/src/syscalls.rs
[virtualization-get-started]: /docs/development/virtualization/get_started.md
