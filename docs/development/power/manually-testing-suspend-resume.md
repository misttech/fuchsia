# Manually test suspend/resume on Workbench

This guide provides instructions on how to manually test suspend and resume on a
device running a suitable [Workbench][workbench-guide] image.

## Perform manual suspend and resume {:#perform-manual-suspend-and-resume}

*   {With serial}

    1.  Connect to the device via USB.

    1.  Take an Application Activity lease via `ffx power`'s helper component:

        ```posix-terminal
        ffx power suspend prevent
        ```

    1.  Drop the session wake lease:

        ```posix-terminal
        ffx session drop-power-lease
        ```

    1.  Disconnect USB to eliminate the persistent wake lease caused by the USB
        or charger connection.

        Either physically unplug the USB cable, or, if the device supports
        [`cable-breaker`](#check-cable-breaker-support), disconnect USB in
        software and schedule an automatic reconnect over serial:

        ```posix-terminal
        usb-cli cable-breaker --duration 15s
        ```

        Ensure `--duration` is long enough to run the next command and allow the
        device to suspend before USB automatically reconnects.

    1.  Using serial, drop the Application Activity lease and reacquire it after
        a delay:

        ```posix-terminal
        powerutil suspend prevent --restart --wait-time 5s
        ```

        This will allow the system to suspend as long as no other wake leases
        are active.

    1.  Wait until serial logs show evidence of system suspension. Logs are not
        necessarily flushed prior to suspension, so you may see them abruptly
        stop.

    1.  Reconnect USB (or wait for the `cable-breaker` `--duration` timer to
        expire and automatically reconnect USB) or take another suitable action
        (touch, button press, etc.) to resume the device. If `--wait-time` has
        elapsed, the helper component will reacquire an Application Activity
        lease and prevent further suspension on its own.

*   {Without serial}

    1.  Connect to the device via USB.

    1.  Take an Application Activity lease via `ffx power`'s helper component:

        ```posix-terminal
        ffx power suspend prevent
        ```

    1.  Drop the session wake lease:

        ```posix-terminal
        ffx session drop-power-lease
        ```

    1.  Drop the Application Activity lease (reacquiring it after a delay) and
        disconnect USB to eliminate the persistent wake lease caused by the USB
        or charger connection:

        *   **Physical unplug**: Run the following command and unplug the USB
            cable before `--wait-time` elapses:

            ```posix-terminal
            ffx power suspend prevent --restart --wait-time 5s
            ```

        *   **Using [`cable-breaker`](#check-cable-breaker-support)**: Because
            `ffx power suspend prevent --restart` blocks until `--wait-time`
            elapses, run it in the background and immediately invoke
            `cable-breaker` with a `--duration` longer than `--wait-time`:

            ```posix-terminal
            ffx power suspend prevent --restart --wait-time 5s &
            ffx component explore /core/usb-policy -c "usb-cli cable-breaker --duration 15s"
            ```

            Note: Both `ffx` commands will lose their connection once
            `cable-breaker` disconnects USB (`ffx component explore` may hang
            until interrupted with `Ctrl+C`). This is expected; both timers
            continue running on the device.

    1.  Wait for several seconds. Without a serial connection, you will not
        be able to confirm whether suspension has occurred until USB is
        reconnected.

    1.  Reconnect USB (or wait for the `cable-breaker` `--duration` timer to
        expire and automatically reconnect USB).

    1.  Use `ffx log` to look for evidence of suspension. There isn't a
        canonical message to indicate this, but at time of writing, the kernel
        message `INFO: Suspending non-boot CPUs...` is a good indicator. You can
        also check `suspend_stats` via Inspect:

        ```posix-terminal
        ffx inspect show bootstrap/system-activity-governor:root/suspend_stats
        ```

## Troubleshooting {:#troubleshooting}

### Check `cable-breaker` support {:#check-cable-breaker-support}

To check whether your device supports software USB disconnect via
`usb-cli cable-breaker`, verify that `/core/usb-policy` exposes
`fuchsia.usb.policy.Configuration`:

```posix-terminal
ffx component capability fuchsia.usb.policy.Configuration
```

### Check wake leases {:#check-wake-leases}

Any active wake leases prevent the system from suspending. To check active wake
leases:

*   {Serial}

    ```posix-terminal
    inspect show bootstrap/system-activity-governor:root/wake_leases
    ```

*   {USB}

    ```posix-terminal
    ffx inspect show bootstrap/system-activity-governor:root/wake_leases
    ```

### Increase delay before wake lease reacquisition {:#increase-delay-before-wake-lease-reacquisition}

To give the system longer to suspend, increase the delay using the `--wait-time`
option:

```posix-terminal
powerutil suspend prevent --restart --wait-time 10s
```

<!-- Reference links -->

[workbench-guide]: /docs/development/build/build_workbench.md