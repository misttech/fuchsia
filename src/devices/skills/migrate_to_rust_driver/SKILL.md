---
name: migrate_cpp_to_rust_driver
description: Guide for migrating a C++ driver in Fuchsia to a Rust driver using the modern Driver Framework (DFv2).
---
# Migration Guide: C++ Driver to Rust Driver (DFv2)

This guide outlines the steps to migrate a C++ driver to a Rust driver in Fuchsia, targeting the Driver Framework v2 (DFv2).

## 1. Understand the Source Driver
Analyze the existing C++ driver to understand:
- Its bind rules (what parents it binds to).
- The resources it accesses (MMIO, IRQs, etc.).
- The services it offers and consumes via FIDL.
- Its internal state and logic.

## 2. Create the Rust Target
In the `BUILD.gn` file, define a new Rust target for the driver using the `fuchsia_rust_driver` template.

Example `BUILD.gn` snippet:
```gn
import("//build/rust/rustc_library.gni")
import("//build/drivers.gni")

fuchsia_rust_driver("my-driver-lib") {
  output_name = "my-driver"
  edition = "2024"
  source_root = "src/lib.rs"
  sources = [ "src/lib.rs" ]
  deps = [
    "//sdk/fidl/fuchsia.driver.framework:fuchsia.driver.framework_rust",
    "//sdk/lib/driver/component/rust",
    "//sdk/rust/zx",
    "//src/lib/fidl/rust_next/fidl_next",
    "//src/lib/fuchsia-async",
    "//src/lib/fuchsia-component",
    "//third_party/rust_crates:futures",
    "//third_party/rust_crates:log",
  ]
}

fuchsia_driver_component("my-driver-component") {
  component_name = "my-driver"
  manifest = "meta/my-driver.cml"
  deps = [
    ":my-driver-lib",
    ":my_driver_bind", # Reference your bind rules target
  ]
}
```

## 3. Implement the Driver Trait
In your Rust source (e.g., `src/lib.rs`), use the `fdf_component` library to define the driver.

```rust
use fdf_component::{Driver, DriverContext, Node, driver_register};
use zx::Status;

struct MyDriver {
    node: Node,
    // Add driver state here
}

driver_register!(MyDriver);

impl Driver for MyDriver {
    const NAME: &str = "my_driver";

    async fn start(mut context: DriverContext) -> Result<Self, Status> {
        let node = context.take_node()?;
        
        // Initialize driver, connect to services, etc.
        
        Ok(MyDriver { node })
    }

    async fn stop(&self) {
        // Cleanup resources
    }
}
```

## 4. Handle Binding
Ensure the bind rules are correctly referenced in the `fuchsia_driver_component` target. The bind rules themselves (`.bind` files) might not need to change much, but the `BUILD.gn` target must link them.

### Type Mismatches in Bind Rules
If a bind rule compares a property with a value from a bind library (e.g., `fuchsia.test.BIND_PROTOCOL.DEVICE`), ensure the value type matches in the driver. If the bind library defines it as `extend uint`, it is an integer, and you must use `NodePropertyValue::IntValue(0x50)` (or the appropriate value) in Rust, even if it looks like a string or enum in C++ helpers or bind rules. Mismatches will cause `Comparing different value types` errors in `driver_index`.

### Enum Values in Properties
When setting properties or bind rules with enums (e.g., from a bind library), use `NodePropertyValue::EnumValue` and ensure the string matches the fully qualified enum name in the bind library (e.g., `fuchsia.nodegroupbind.test.TEST_BIND_PROPERTY.DRIVER_LEFT`). Be careful with casing, as bind libraries typically use uppercase for enum values.

## 5. Port Functionality
Translate C++ logic to Rust.
- Use `fuchsia_async` for asynchronous operations.
- Use `fidl` crate for interacting with FIDL protocols.
- Use `log` crate.
- For serving services, use `fuchsia_component::server::ServiceFs`.
- **Interrupts via GPIO**: If the original C++ driver obtained and configured interrupts via the GPIO protocol (e.g., `gpio_->ConfigureInterrupt`), ensure the Rust driver matches this behavior rather than falling back to `pdev.get_interrupt_by_id`.

## 6. Update Component Manifest (.cml)
Ensure the manifest reflects the Rust driver's needs, including services it uses and exposes.

## 7. Advanced: Concurrency and Avoiding Locks

Modern Fuchsia drivers should avoid heavy synchronization like `Mutex` when possible, preferring actor pattern when it makes sense.

### Patterns to avoid Mutex/Arc:
- **Task-Local State**: Own the state in a local task spawned on the driver's scope.
- **Channels for Connection Handoff**: Since `ServiceFs` requires `Send` handlers, use an `mpsc::unbounded` channel to send connections from `ServiceFs` to your local task.
- **Concurrent Processing**: Use `rx.for_each_concurrent` on the channel receiver to process multiple connections simultaneously on the same local task.
- **State Ownership in Tasks**: For interrupt-driven drivers, consider moving ownership of the mutable state directly into the long-running async task that processes interrupts. This eliminates the need for locks if only that task needs to mutate the state after initialization.

### Two approaches based on FIDL bindings:
1. **With `fidl_next`**: Pass `ServerEnd` over the channel. The handler task constructs the server and runs it.
2. **With Standard `fidl`**: Pass the `RequestStream` over the channel. The handler task processes the stream directly.

Both approaches allow you to share a plain reference `&RefCell<State>` among all connection handlers, avoiding both `Mutex` and `Rc`!

### Recommendation: Default to `fidl_next` for New Drivers
For new drivers or new basic support, prefer using `fidl_next` for serving FIDL protocols to match modern best practices.
- Ensure `enable_rust_next = true` is set in the corresponding `fidl(...)` target in `BUILD.gn`.
- Depend on `//src/lib/fidl/rust_next/fidl_next` and the `_rust_next` version of the FIDL target.
- Implement `VregLocalServerHandler` (or equivalent) instead of matching on request streams.

## 8. Hardware Register Access with `mmio::registers`

When interacting with memory-mapped hardware registers:
- **Use Macros**: Prefer `mmio::register!` and `mmio::register_block!` over manual `read_volatile`/`write_volatile` pointer operations.
- **Type Safety**: These macros generate safe accessors for registers and fields, reducing errors.
- **Integration**: They work well with `mmio::vmo::VmoMapping` for memory mapping.
- **Trait Bounds**: If using generated register blocks, you may need to wrap `VmoMapping` in `MmioRegion<VmoMemory>` to satisfy trait bounds required by the `mmio` crate.

## 9. Safe Interrupt Handling
 
 When waiting on interrupts in Rust:
 - **Avoid `zx::Handle` for Interrupts**: Do not store interrupts as raw `zx::Handle` just to support different kinds (Real vs Virtual). This forces the use of `unsafe` when waiting on them.
 - **Generic `InterruptKind`**: Make your device struct generic over `K: zx::InterruptKind` (e.g., `Device<K>`). This allows storing `zx::Interrupt<K>` directly and using `fuchsia_async::OnInterrupt` safely in both production and tests.
 - **Async Wait Loop**: Instead of spawning a dedicated thread with `std::thread::spawn` and blocking on `irq.wait()`, use `fasync::OnInterrupt` to create a stream of interrupts and process them in an async task.

Example:
```rust
        let irq = self.tsensor_irq.duplicate(zx::Rights::SAME_RIGHTS)?;
        scope.spawn(async move {
            let interrupt = fasync::OnInterrupt::new(irq);
            futures::pin_mut!(interrupt);

            while running.load(Ordering::SeqCst) {
                if let Some(Ok(_)) = interrupt.next().await {
                    // Handle interrupt
                }
            }
        });
```
 
 ## 10. Connecting to Services

When migrating a driver that connects to services (like `pdev` or custom services offered via dictionaries), use the service capability instead of the protocol.

### Idiomatic Connection Pattern
Use the `service_marker` API on `context.incoming`. This returns a `ServiceConnector` which allows overriding the instance name before connecting.

```rust
        let service = context
            .incoming
            .service_marker(ft::MyServiceMarker)
            .instance("custom_instance") // Optional: defaults to "default"
            .connect()?;
        
        let proxy = service.connect_to_my_protocol()?;
```

### Platform Device (pdev) Instance Naming Rules
- **Composite Drivers**: Only specify the instance as `"pdev"` if the driver is a composite bind and the platform device parent is listed as `"pdev"` in the driver bind rules.
  ```rust
  .instance("pdev")
  ```
- **Non-Composite Drivers**: Avoid specifying the instance name. It will default to `"default"`.
  ```rust
  let pdev = context
      .incoming
      .service_marker(fidl_fuchsia_hardware_platform_device::ServiceMarker)
      .connect()?
      // ...
  ```

### Dictionary-offered Services
If a service is offered via a dictionary, it may be exposed as a specific instance in the incoming namespace (e.g., `"default"`, `"left"`, `"right"`, `"opt"`).
- Use `.instance("instance_name")` to connect to a specific instance.
- If you need to fall back to other instances, you can use `if let Ok(...)` or match on the result of `.connect()`.

### Service Instance Connection Fallbacks
When connecting to service instances, note that `connect()` or `connect_to_protocol()` might not fail immediately if the instance doesn't exist (it creates a channel that will be closed by the peer). If you need to try multiple instances (e.g., "default" then "leaf"), be aware that the first attempt might return `Ok(proxy)` even if it fails later. It is better to directly connect to the expected instance if known, or check for `PEER_CLOSED` errors if you must fall back.

### Composite Nodes and Services
When a driver binds to a composite node, it can access services offered by its parents.
- **Instance Naming**: The service instance names in the composite driver's incoming directory will match the **PARENT NAMES** defined in the bind rules or node spec, NOT the instance names used when offering the service to the child nodes.
- **Primary Parent Alias**: In C++, connecting to a service without an instance name typically defaults to `"default"`. In composite nodes, the primary parent's services are often exposed as `"default"` as well as by the parent's name. If this automatic aliasing does not work in Rust, a valid workaround is to explicitly map `"default"` to the primary parent's name in the driver's connection logic.

## 11. Verification
- Build the driver: `fx build`.
- Run tests if available.

## 12. Testing with `MmioRegion` and VMOs

When testing drivers that use `MmioRegion`:
- **Avoid Manual Mocks**: Instead of mocking read/write methods, use real VMOs to back the memory region.
- **VMO Injection**: In tests, create a `zx::Vmo`, map it using `VmoMapping::map`, and pass the resulting `MmioRegion` to the driver.
- **Verification**: You can write expected values to the VMO before calling driver methods, or read from the VMO after driver methods to verify they wrote correctly.

Example:
```rust
    let sensor_vmo = zx::Vmo::create(0x1000).unwrap();
    let sensor_region = Arc::new(Mutex::new(VmoMapping::map(0, 0x1000, sensor_vmo.duplicate(zx::Rights::SAME_RIGHTS).unwrap()).unwrap()));
    // Pass sensor_region to driver
```

## 13. Platform Device and MMIO Access

When using the platform device protocol to access MMIO:
- **Use `pdev` library**: Prefer using the `pdev` library (`//sdk/lib/driver/platform-device/rust:pdev`) instead of manual FIDL calls and manual VMO mapping.
- **PlatformDevice Trait**: Import `pdev::PlatformDevice` and use methods like `map_mmio_by_id` or `map_mmio_by_name` on the pdev proxy.
- **Convenience**: This handles fetching the MMIO resources and mapping them into `MmioRegion<VmoMemory>` automatically, reducing boilerplate code.

Example:
```rust
use pdev::PlatformDevice;

// In start method:
let pdev = context
    .incoming
    .service_marker(fidl_fuchsia_hardware_platform_device::ServiceMarker)
    .connect()?
    .connect_to_device()?;

let mmio = pdev.map_mmio_by_id(0).await?;
```

## 14. Node Lifecycle and NodeController

When adding a child node using `Node::add_child` or directly via `add_child` FIDL method:
- **Keep Controller Alive**: The `NodeController` client returned by `AddChild` controls the lifetime of the node. If you drop this client, the node will be removed by the Driver Framework.
- **Store in Struct**: Always store the `NodeController` client in your driver struct if you want the node to persist beyond the `start` method.

## 15. Rust Style: Associated Functions vs Top-Level Functions

When authoring helper functions in your driver file:
- If a function does not take `&self` or `&mut self` and does not need access to the struct's private fields or constructor, prefer moving it out of the `impl` block to become a top-level function in the file or module. This is considered more idiomatic Rust.
