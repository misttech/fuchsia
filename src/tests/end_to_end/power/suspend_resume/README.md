Verifies that the device suspends and resumes at the appropriate times.

### Running the test locally

`test_suspend_resume` disconnects the device from USB so that it can suspend:

<!-- TODO(https://fxbug.dev/573221590): Move the external USB power hub and
     manual USB suspend/resume flows into SystemPowerStateController
     implementations so this test uniformly uses system_power_state_controller. -->
1. In infra (or when `DMC_PATH` is set), an external USB power hub cuts the
   power and data lines of the USB cable.
2. When no external USB power hub is configured (such as at a developer desk),
   the test uses Honeydew's `Workbench` `system_power_state_controller`
   (`usb-cli`) affordance if supported by the target (`usb-cli cable-breaker
   --duration <N>s`), which arms a wake alarm in `usb-policy` and opens the
   Type-C CC1/CC2 terminations in the TCPC while the cable stays plugged in.
3. If `usb-cli` is not available on the target, the test logs instructions when
   you should manually unplug and replug the USB cable, and uses more forgiving
   timeouts to give you time to do so.

```sh
fx add-host-test //src/tests/end_to_end/power/suspend_resume:suspend_resume_test
fx build
fx test //src/tests/end_to_end/power/suspend_resume:suspend_resume_test --e2e --output
```
