# Device power management

In Fuchsia, drivers participate in power management through integration with the
Driver Framework and the Fuchsia Power Framework (including the
[Power Broker][power-broker] and the
[System Activity Governor][system-activity-governor]).

## System suspend and resume

If a platform has enabled system suspend, the Driver Manager will create a
binary power element for each node bound to a driver where a power level of `0`
is OFF/suspended and a power level of `1` is ON/active. The power element's
dependencies will consist of the power elements of the node's ancestors that are
bound to a driver and a few other power elements like the CPU power element, the
storage power element, etc. This ensures the correct suspend/resume order of
nodes so that the bound driver isn't trying to use services provided by its
ancestor nodes while the ancestors are suspended.

## Handling suspend and resume

A driver can handle suspend and resume in a few different ways, outlined below.

### Runtime-managed dispatchers

The recommended way for drivers to support suspend and resume is by enabling
**runtime-managed dispatchers**. In this state, the driver host (instead of the
driver instance) runs the `fuchsia.power.broker/ElementRunner` associated with
the driver's power element and coordinates with the driver runtime around power
transitions:

1. **Suspending**: When the driver's power element transitions to the suspended
   state (`0`), the driver runtime will:
   - Pause all power-managed dispatchers.
   - Move queued and newly arriving tasks into a sleep queue.
   - Wait for actively executing callbacks to complete.
   - Invoke the driver's suspend hook on the always-on view of the driver's
     default dispatcher.
2. **Resuming**: When the driver's power element transitions to the active state
   (`1`) or a registered wake vector triggers, the runtime will:
   - Invoke the driver's resume hook on the always-on dispatcher first so
     hardware state can be restored.
   - Unpause the power-managed dispatchers (executing any triggered wake
     vector tasks first, followed by queued tasks).
3. **Stopping while suspended**: If the Driver Framework stops a driver while it
   is suspended, the runtime will:
   - Resume the driver (`SystemResume` or `system_resume`).
   - Invoke the driver's stop hook (`Stop` or `stop`).

Note: Because regular power-managed dispatchers remain paused while
`SystemSuspend` (`system_suspend`) and `SystemResume` (`system_resume`) execute,
do not await asynchronous work or FIDL calls bound to a paused regular
dispatcher inside these hooks, as doing so will deadlock. Use synchronous calls
or bind asynchronous work to the always-on dispatcher (`GetAlwaysOnDispatcher()`
in C++, or `always_on_dispatcher()` / `fuchsia_async` Zircon channels in Rust).

#### Enabling runtime-managed dispatchers

You can enable runtime-managed dispatchers by setting
`power_managed_dispatchers_enabled: "true"` in the driver's `.cml` file.
Including `driver_component/power_managed_dispatchers.shard.cml` will also do
this for you while routing the required power protocols:

```json5
include: [
    // Enables runtime-managed dispatchers.
    "driver_component/power_managed_dispatchers.shard.cml",
],
program: {
    runner: "driver",
    binary: "driver/my_driver.so",
    bind: "meta/bind/my_driver.bindbc",
},
```

Drivers can implement suspend and resume lifecycle methods to put hardware into
a low-power state after power-managed dispatchers are paused, and restore
hardware state before power-managed dispatchers are unpaused upon waking.

When a wakeup is triggered by one of the driver's own registered runtime wake
vectors (such as an interrupt or channel wait bound to a power-managed
dispatcher), the driver host automatically acquires a lease on the driver's
power element via `fuchsia.power.broker/Topology.Lease` and passes the
`LeaseToken` (`pe_lease` in C++ or `lease` in Rust) to the resume hook. If the
driver is resumed as a dependency of another driver or during a system-wide
resume, the lease parameter is `std::nullopt` (or `None`).

* {C++}

  In C++, override `SystemSuspend` and `SystemResume` on `fdf::DriverBase2`:

  ```cpp
  #include <lib/driver/component/cpp/driver_base2.h>

  class MyDriver : public fdf::DriverBase2 {
   public:
    MyDriver() : fdf::DriverBase2("my_driver") {}

    void SystemSuspend(fdf::SuspendCompleter completer) override {
      // Put hardware into low-power mode or save register state.
      completer(zx::ok());
    }

    void SystemResume(std::optional<fuchsia_power_broker::LeaseToken> pe_lease,
                      fdf::ResumeCompleter completer) override {
      // Restore hardware configuration. If woken by a wake vector, retain
      // `pe_lease` until the wake task finishes handling the event.
      completer(zx::ok());
    }
  };
  ```

* {Rust}

  In Rust, implement `system_suspend` and `system_resume` on the
  `fdf_component::Driver` trait:

  ```rust
  impl fdf_component::Driver for MyDriver {
      async fn system_suspend(&self) -> Result<(), fdf_component::DriverError> {
          // Put hardware into low-power mode or save register state.
          Ok(())
      }

      async fn system_resume(
          &self,
          lease: Option<zx::EventPair>,
      ) -> Result<(), fdf_component::DriverError> {
          // Restore hardware configuration. If woken by a wake vector, retain
          // `lease` until the wake task finishes handling the event.
          Ok(())
      }
  }
  ```

### Driver-owned suspend

This approach is for drivers that want to listen to suspend and resume **but do
not want their dispatchers paused during a suspend**.

#### Enabling driver-owned suspend

Set `suspend_enabled: "true"` in the driver's `.cml` file.

**Make sure `power_managed_dispatchers_enabled` is set to `"false"` (or
omitted) if you set `suspend_enabled` to `"true"`**, otherwise
`power_managed_dispatchers_enabled` takes priority and suspend events will
never be forwarded to `fdf_power::Suspendable` (`SuspendableDriver`).

```json5
program: {
    runner: "driver",
    binary: "driver/my_driver.so",
    bind: "meta/bind/my_driver.bindbc",
    suspend_enabled: "true",
},
```

* {C++}

  In C++, inherit from `fdf_power::Suspendable`, provide
  `take_power_element_runner()`, and call `InitializeSuspend()` during `Start`:

  ```cpp
  #include <lib/driver/component/cpp/driver_base2.h>
  #include <lib/driver/power/cpp/suspend.h>

  class MyDriver : public fdf::DriverBase2,
                   public fdf_power::Suspendable<MyDriver> {
   public:
    MyDriver() : fdf::DriverBase2("my_driver") {}

    zx::result<> Start(fdf::DriverContext context) override {
      power_element_runner_ = context.take_power_element_runner();
      if (zx::result result =
              InitializeSuspend(dispatcher(), context.incoming(), name());
          result.is_error()) {
        return result.take_error();
      }
      return zx::ok();
    }

    void Suspend(fdf_power::SuspendCompleter completer) override {
      // Drain internal queues and put hardware into a low-power state.
      completer();
    }

    void Resume(fdf_power::ResumeCompleter completer) override {
      // Restore hardware state and resume internal queues.
      completer();
    }

    bool SuspendEnabled() override { return true; }

    std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>>
    take_power_element_runner() {
      return std::move(power_element_runner_);
    }

   private:
    std::optional<fidl::ServerEnd<fuchsia_power_broker::ElementRunner>>
        power_element_runner_;
  };
  ```

* {Rust}

  In Rust, implement `fdf_power::SuspendableDriver` and register the driver
  wrapped in `fdf_power::Suspendable`:

  ```rust
  use fdf_component::{Driver, DriverContext, DriverError, driver_register};
  use fdf_power::{Suspendable, SuspendableDriver};

  impl SuspendableDriver for MyDriver {
      async fn suspend(&self) {
          // Drain internal queues and put hardware into a low-power state.
      }

      async fn resume(&self) {
          // Restore hardware state and resume internal queues.
      }

      fn suspend_enabled(&self) -> bool {
          true
      }
  }

  driver_register!(Suspendable<MyDriver>);
  ```

## Custom power elements

In addition to the power element created by the Driver Manager, drivers can
manually create and manage their own custom power elements with various power
levels and dependencies via `fuchsia.power.broker/Topology` (or the helper
libraries in `//sdk/lib/driver/power/cpp`). Custom power elements can be used
alongside either `power_managed_dispatchers_enabled` or `suspend_enabled`,
though level transitions on custom power elements do not pause the driver's
dispatchers.

## Power leases and wake leases

- **Power Broker leases**: When a registered wake vector wakes a suspended
  driver with runtime-managed dispatchers, the driver host acquires a lease on
  the driver's power element via `fuchsia.power.broker/Topology.Lease` and
  passes it to `SystemResume` (`system_resume`). Because the driver's power
  element depends on its ancestor drivers and the CPU power element, retaining
  this lease keeps the device and CPU active until the wake event is handled or
  handed off upstream.
- **System wake leases**: Drivers that need to directly prevent system
  suspension outside of runtime-managed wake vectors can acquire wake leases
  from the [System Activity Governor][system-activity-governor]
  (`fuchsia.power.system/ActivityGovernor.AcquireWakeLease`, wrapped by helpers
  such as `fdf_power::WakeLeaseProvider` and `fdf_power::TimeoutWakeLease`).

For more information on the power framework and testing suspend/resume, see
[Testing suspend and resume][testing-suspend-resume].

<!-- Reference links -->

[power-broker]: /sdk/fidl/fuchsia.power.broker/broker.fidl
[system-activity-governor]: /sdk/fidl/fuchsia.power.system/system.fidl
[testing-suspend-resume]: /docs/development/power/manually-testing-suspend-resume.md
