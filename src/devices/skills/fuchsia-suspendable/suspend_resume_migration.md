# Steps

## SuspendBlockers {#suspendblockers}

Moving away from SuspendBlockers is straightforward, drivers simply need to opt-in to using what is already available. It is these steps:

> [!NOTE]
> Drivers that use `SuspendBlocker` (or libraries like `FeedForwardWakeLease` that implement it) **only** to acquire wake leases to prevent suspend during interrupt handling are still allowed and do not need to be migrated. Focus migration efforts on drivers that use `SuspendBlocker` to coordinate hardware state changes or other suspend/resume logic.

* Update the component manifest
* Implement the fdf\_power::Suspendable mix-in (C++) or the fdf\_power::SuspendableDriver trait (Rust)
* Implement `SuspendEnabled` (C++) or `suspend_enabled` (Rust)
* Move any logic from `AfterResume` and `BeforeSuspend` to the driver's `Resume` and `Suspend` calls

For drivers that already use the Suspendable mix-in, only the first two steps are required. For drivers that don't implement these you'll need to do the other steps. implement_suspendable.md has information about how to perform the actions called out in these steps.

For Rust drivers that implement `SuspendableDriver`, only the first step is required.

### Update the component manifest

Add `suspend_enabled: "true"` program section of the component manifest, [src/devices/block/drivers/sdmmc/meta/sdmmc.cml](https://cs.opensource.google/fuchsia/fuchsia/+/30ec5f992257d1fb2a646e100c18b16eb7108aee:src/devices/block/drivers/sdmmc/meta/sdmmc.cml;l=16) has an example of this.

If a driver already uses the Suspendable mix-in for C++ or the SuspendableDriver trait for Rust, that's all, you're done. The driver's `Suspend` and `Resume` calls will now be triggered by its power element instead of registering a `SuspendBlocker`.

### Auditing Existing Migrations

Look for drivers that have already implemented the `fdf_power::Suspendable` mix-in (C++) or `SuspendableDriver` trait (Rust) but have **not** updated their component manifest to include `suspend_enabled: "true"` in the `program` section. These drivers are not actually using the framework power elements until that flag is set. Additionally, verify that the driver correctly overrides `take_power_element_runner()` to return a valid runner if it is expected to receive one from the framework or context, rather than returning `std::nullopt`.


### Implement the fdf\_power::Suspendable mix-in (C++) or the fdf\_power::SuspendableDriver trait (Rust)

See implement_suspendable.md for information on how to do this.

### Implement `SuspendEnabled` (C++) or `suspend_enabled` (Rust)

To do this

* Use a config capability in the manifest
* Tie the capability to a structured config type for the driver
* Import the structured config type into the driver implementation
* Add a field to the driver to store the structured config type
* Returning the suspend value from `SuspendEnabled` (C++) or `suspend_enabled` (Rust) call

implement_suspendable.md has detailed information on each of these steps.

### Move logic from `AfterResume` and `BeforeSuspend` to the driver's `Resume` and `Suspend` calls

You should be able to move code from an existing `AfterResume` into the new `Resume` function and likewise for `BeforeSuspend` into `Suspend`. One consideration is that in C++ is if there is asynchronous work to do, you need to move the relevant completer into the asynchronous lambda or `fit::promise` or whatever the code may use. This is extremely rare, but if the driver is in Rust and isn't straight-thru code with or without sync, you'll need to make sure the suspend/resume awaits whatever task may be spawned.
