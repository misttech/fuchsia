### Implementing the `fdf_power::Suspendable` mix-in or `fdf_power::SuspendableDriver` trait

This is provided by the [power support library](https://cs.opensource.google/fuchsia/fuchsia/+/main:sdk/lib/driver/power/) and has both Rust and C++ versions.

#### C++

Include the header:
```c
#include <lib/driver/power/cpp/suspend.h>
```

Typically the C++ implementation will look something like

```c
class MyDriver : public fdf::DriverBase2,
                 public fdf_power::Suspendable<MyDriver> {
  public:
    void Resume(fdf_power::ResumeCompleter completer) override {
      // Your resume logic here
      completer();
    }

    void Suspend(fdf_power::SuspendCompleter completer) override {
      // Your suspend logic here
      completer();
    }

    // This function is called before your driver's Start() is called
    // and therefore must be valid at that time.
    bool SuspendEnabled() override {
      return config_.suspend_enabled();
    }

  private:
    // This will often be a structured config value populated by using the
    // `fuchsia.power.SuspendEnabled` config capability, but your driver may
    // have a more specific way of doing its configuration.
    my_driver_config::Config config_;
}
```

For simplicity the above example puts the implementation in the header, but that is obviously not required.

#### Rust

In Rust the driver implements the `SuspendableDriver` trait, which looks very similar to the `Suspendable` mix-in.

```rust
impl SuspendableDriver for MyDriver {
    async fn suspend(&self) {
        // Your suspend logic here
    }

    async fn resume(&self) {
        // Your resume logic here
    }

    fn suspend_enabled(&self) {
        return self.config.suspend_enabled;
    }
}
```

### Using a capability in CML

This shows how to use a config capability, the most interesting things to note are the capability name is `fuchsia.power.SuspendEnabled` and the value of the capability is then bound at runtime to a field defined by `key`, in this case `enable_suspend`.

```
    use: [
       {
            config: "fuchsia.power.SuspendEnabled",
            key: "enable_suspend",
            type: "bool",
            availability: "optional",
            default: false,
        },
        ...
    ],
```

### Build a structured config library

This generates code so that the structured config values defined in the manifest can be used at runtime.

For both GN and Bazel examples below we assume that in the same file there is a `fuchsia_component_manifest` target called "manifest". For both GN and Bazel in the below snippets replace `${lang}` with `cpp_elf` or `rust` if your component is written in C++ or Rust, respectively.

For GN add

```
fuchsia_structured_config_${lang}_lib("my_driver") {
    cm_label = ":manifest"
}
```

For Bazel add

```
fuchsia_structured_config${lang}_lib(
    name = "my-driver",
    cm_label = ":manifest",
)
```

### Importing the structure config type

This is how to link Rust and C++ against the structured config type. The values here assume the names specified for building the structured config library match those in the "Build a structured config library section".

For C++ this is

```c
#include "path/to/driver/my_driver_config.h"
```

Note that the name of the structured config label in the build file influences the name of the header. Any hyphens in the label name are replaced with underscores and "\_config.h" is appended to form the header file name.

For Rust the driver should be able to access the config as the type `my_driver_config::Config` without adding a `use` declaration.

### Initialize the structured config

These examples show how to initialize a field of the structured config type. The values here assume the names specified in previous sections are used for building the structured config type.
In C++

```c
class MyDriver {
  public:
    explicit MyDevice(fdf::DriverStartArgs start_args, fdf::UnownedSynchronizedDispatcher driver_dispatcher)
        : config_(take_config<my_driver::Config>()) {}
    ...
  private:
    my_driver::Config config_;
  ...
}
```

In Rust

```rust
struct MyDriver {
    config: my_driver_config::Config,
    ...
}

impl MyDriver {
    async fn start(context: fdf_component::DriverContext) -> Result<Self, zx::Status> {
        Self {
             config: context.take_config::<my_driver_config::Config>()?,
             ...
        }
}
```

### Returning the suspend value from `SuspendEnabled` (C++) or `suspend_enabled` (Rust) call

In the appropriate call return the value from the structured config. The values here assume the names specified in previous sections are used for building the structured config type.

In C++

```c
class MyDriver {
  bool MyDriver::SuspendEnabled() {
    return config_.enable_suspend();
  }
}
```

In Rust

```rust
impl MyDriver {
    fn suspend_enabled(&self) -> bool {
        self.config.enable_suspend
    }
}
```

