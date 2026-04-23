---
name: migrate_driver_base_to_driver_base2
description: Migrate Fuchsia drivers from fdf::DriverBase to fdf::DriverBase2.
---

# Migrate to DriverBase2

## Update Headers

Modify the include directive to use the new header:

```cpp
// Before
#include <lib/driver/component/cpp/driver_base.h>

// After
#include <lib/driver/component/cpp/driver_base2.h>
```

## Update Class Inheritance

Inherit from `fdf::DriverBase2` instead of `fdf::DriverBase`:

```cpp
// Before
class MyDriver : public fdf::DriverBase { ... };

// After
class MyDriver : public fdf::DriverBase2 { ... };
```

## Update Constructor

The constructor must take no arguments (be default constructible), but it still needs to forward the driver name to `DriverBase2`. You no longer receive `DriverContext` or `UnownedSynchronizedDispatcher` in the constructor; they are provided in `Start()` or handled internally.

```cpp
// Before
MyDriver(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
    : fdf::DriverBase("my_driver", std::move(start_args), std::move(driver_dispatcher)) {}

// After
MyDriver() : fdf::DriverBase2("my_driver") {}
```

## Update Export Macro

You must use `FUCHSIA_DRIVER_EXPORT2` instead of `FUCHSIA_DRIVER_EXPORT` for
`DriverBase2` drivers. This also requires including
`<lib/driver/component/cpp/driver_export2.h>`.

```cpp
// Before
#include <lib/driver/component/cpp/driver_export.h>
FUCHSIA_DRIVER_EXPORT(MyDriver);

// After
#include <lib/driver/component/cpp/driver_export2.h>
FUCHSIA_DRIVER_EXPORT2(MyDriver);
```

## Update Start Method

The `Start` method now receives a `DriverContext` by value. Many methods
previously available on `DriverBase` are now only accessible via this context
during `Start`.

```cpp
// Before
zx::result<> Start() override {
  incoming()->Connect(...);
  auto& offers = node_offers();
  ...
}

// After
zx::result<> Start(fdf::DriverContext context) override {
  context.incoming().Connect(...);
  auto& offers = context.node_offers();
  ...
}
```

### Handle Incoming Namespace

If you need to access the incoming namespace after the `Start` method
completes, you must take ownership of it from the context.

> [!NOTE]
> Some SDK APIs (like `compat::DeviceServer::Initialize` or `compat::ConnectBanjo`) still require a `std::shared_ptr<fdf::Namespace>`. You can create one by converting the unique pointer returned by `context.take_incoming()`:
> `auto incoming_ptr = std::shared_ptr<fdf::Namespace>(context.take_incoming());`

```cpp
// Example of retaining incoming namespace
zx::result<> Start(fdf::DriverContext context) override {
  incoming_ = context.take_incoming();
  ...
}
```

### Create Inspector

The `inspector()` method is removed. Use `context.CreateInspector` to create a
component inspector.

```cpp
// Before
inspector().Health().Ok();

// After
auto inspector = context.CreateInspector(this);
inspector.Health().Ok();
```

## Update Stop and Teardown

`fdf::DriverBase2` simplifies the stopping lifecycle.

* `PrepareStop(PrepareStopCompleter)` is renamed to `Stop(StopCompleter)`.
* The synchronous `Stop()` method is removed. Use the destructor for
  synchronous cleanup.

### Async Stop

```cpp
// Before
void PrepareStop(fdf::PrepareStopCompleter completer) override {
  // Do async cleanup
  completer(zx::ok());
}

// After
void Stop(fdf::StopCompleter completer) override {
  // Do async cleanup
  completer(zx::ok());
}
```

### Sync Stop

```cpp
// Before
void Stop() override {
  // Do sync cleanup
}

// After
~MyDriver() override {
  // Do sync cleanup in destructor
}
```

## Common Pitfalls

* **Constructor Logging**: Calling `fdf::info` or similar logging macros in the constructor will crash the driver. The logger is not initialized until `DriverBaseInternalInit` is called by the framework (which happens after construction but before `Start`). Perform any logging in `Start` instead.

* **Missing Incoming Namespace**: Accessing `incoming()` after `Start` without
  having called `context.take_incoming()` will fail.
* **Constructor Typo**: Forgetting to remove `start_args` and
  `driver_dispatcher` from the base class initialization list.
* **Cleanup Order**: Relying on `Stop()` being called before destruction. In
  `DriverBase2`, synchronous cleanup belongs in the destructor.

## Further Reading

* [driver_base.h](/sdk/lib/driver/component/cpp/driver_base.h)
* [driver_base2.h](/sdk/lib/driver/component/cpp/driver_base2.h)


