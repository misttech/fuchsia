# Steps

## SuspendBlockers {#suspendblockers}

Moving away from SuspendBlockers is straightforward, drivers simply need to opt-in to using what is already available. It is these steps:

* Update the component manifest
* Implement the fdf\_power::Suspendable mix-in (C++) or the fdf\_power::SuspendableDriver trait (Rust)
* Implement `SuspendEnabled` (C++) or `suspend_enabled` (Rust)
* Move any logic from `AfterResume` and `BeforeSuspend` to the driver's `Resume` and `Suspend` calls

For drivers that already use the Suspendable mix-in or SuspendDriver trait, only the first step is required. For drivers that don't implement these you'll need to do the other steps. implement_suspendable.md has information about how perform the actions called out in these steps.

### Update the component manifest

Add `suspend_enabled: "true"` program section of the component manifest, [src/devices/block/drivers/sdmmc/meta/sdmmc.cml](https://cs.opensource.google/fuchsia/fuchsia/+/30ec5f992257d1fb2a646e100c18b16eb7108aee:src/devices/block/drivers/sdmmc/meta/sdmmc.cml;l=16) has an example of this.

If a driver already uses the Suspendable mix-in for C++ or the SuspendableDriver trait for Rust, that's all, you're done. The driver's `Suspend` and `Resume` calls will now be triggered by its power element instead of registering a `SuspendBlocker`.

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
