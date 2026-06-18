---
name: rust-driver-best-practices
description: >
  Apply idiomatic patterns and the standard libraries for writing or reviewing
  a modern Rust DFv2 driver in Fuchsia. Use when authoring a new Rust driver
  or porting one and needing fidl_next, fdf_component
  (Driver/DriverContext/driver_register!), fuchsia_async (Scope/OnInterrupt),
  safe MMIO via the mmio register!/register_block! macros, the pdev library,
  service_marker connections, lock-free concurrency, NodeController lifecycle,
  or VMO-backed driver tests -- and to review a finished Rust driver change.
  Covers patterns only, not the migration process (use migrate-cpp-to-rust-
  driver).
---

# Rust Driver Best Practices in Fuchsia

This guide outlines the recommended patterns, libraries, and best practices for
authoring modern Rust drivers in Fuchsia. It focuses on using `fidl_next`, the
`fdf_component` framework, safe memory-mapped I/O (MMIO), and idiomatic Rust
patterns.

For guidance on the *process* of migrating a C++ driver to Rust, refer to the
`migrate_cpp_to_rust_driver` skill.

## 1. Available Libraries for Rust Drivers

When writing a Rust driver, you will typically rely on the following key
libraries:

* **`fdf_component`**: The core Driver Framework v2 (DFv2) library. Provides
  macros and structures for defining drivers (`driver_register!`, `Driver`,
  `DriverContext`), creating nodes (`Node`, `NodeBuilder`), and offering
  services (`ServiceOffer`, `ServiceInstance`).
* **`fidl_next`**: The modern, next-generation FIDL bindings for Rust. Provides
  asynchronous abstractions like `ClientEnd`, `ServerEnd`, `Request`, and
  `Responder`. It is preferred over the legacy `fidl` crate.
* **`fuchsia_async` (`fasync`)**: The Fuchsia asynchronous runtime. Provides
  utilities for running concurrent tasks (`fasync::Scope`, `fasync::Task`),
  waiting for interrupts (`fasync::OnInterrupt`), and handling timers
  (`fasync::Timer`).
* **`fuchsia_component::server::ServiceFs`**: Used to construct the outgoing
  directory and route FIDL protocols to clients.
* **`mmio`**: Provides abstractions for safely mapping and accessing device
  registers (`MmioRegion`, `VmoMapping`, `VmoMemory`).
* **`pdev`**: Helper library for interacting with the Platform Device protocol
  (`fidl_fuchsia_hardware_platform_device`). It simplifies tasks like fetching
  MMIO regions (`map_mmio_by_id`, `map_mmio_by_name`). Prefer the `by_name`
  variants.
* **`fdf_metadata`**: Used for interacting with driver metadata (e.g.,
  `MetadataServer`).
* **`log`**: The standard Rust logging facade (`info!`, `error!`, `warn!`,
  `debug!`), which integrates with Fuchsia's syslog.
* **`zx`**: The Zircon kernel bindings, used for handling Zircon primitives like
  `Status`, `Vmo`, and `Interrupt`.

## 2. Build Rules

When defining a Rust driver in your `BUILD.gn`, use the following targets:

* **`fuchsia_rust_driver`**: Use this template for the main driver library. This
  compiles the driver logic into a shared library.
* **`fuchsia_driver_component`**: This target binds the compiled driver library,
  the component manifest (`.cml`), and the bind rules (`.bind`) into a single
  driver component that the Driver Framework can load.

## 3. Style and Naming Conventions

For generic Rust style, naming conventions, and best practices (like `use`
groupings, avoiding `allow_unused`, constants vs magic values, `Mutex`
preferences, etc.), please refer to the `rust_best_practices` skill.

**Driver-Specific Guidelines:**
* **References to Specs**: If a name, value, or logic flow is derived from a
  hardware specification or a reference driver (e.g., from the Linux kernel),
  include a comment referencing that source so it can be easily looked up by
  future maintainers.
* **Missing Libraries**: If a utility library seems to be missing in Rust when
  porting from C++, try to find the equivalent library in the C++
  implementation. It might be trivial to implement the logic directly in Rust
  based on the C++ source.
* **Associated Functions vs Top-Level Functions**: When authoring helper
  functions in your driver file, if a function does not take `&self` or `&mut
  self` and does not need access to the struct's private fields or constructor,
  prefer moving it out of the `impl` block to become a top-level function in the
  file or module. This is considered more idiomatic Rust.

## 4. Using `fidl_next` for FIDL Operations

Modern Rust drivers should use `fidl_next` instead of the legacy `fidl` crate.
**Note:** For complete examples, prefer looking at the codebase (e.g.,
`examples/drivers/transport/zircon/rust_next/parent/src/lib.rs`).

### Serving a Protocol
When offering a service, use `fidl_next::ServerDispatcher` or
`server_end.spawn_on` to route incoming requests to a local handler struct.
Ensure that the handler logic executes on the driver's local async scope.

```rust
impl i2c::ServiceHandler for Service {
    fn device(&self, server_end: ServerEnd<i2c::Device>) {
        // Spawn the server on the driver's scope
        server_end.spawn_on(DeviceServer, &self.scope);
    }
}
```

### Recommendation: Default to `fidl_next` for New Drivers
For new drivers or new basic support, prefer using `fidl_next` for serving FIDL
protocols to match modern best practices.
- Ensure `enable_rust_next = true` is set in the corresponding `fidl(...)`
  target in `BUILD.gn`.
- Depend on `//src/lib/fidl/rust_next/fidl_next` and the `_rust_next` version of
  the FIDL target.
- Implement `VregLocalServerHandler` (or equivalent) instead of matching on
  request streams.

## 5. Connecting to Services

When connecting to services (like `pdev` or custom services offered via
dictionaries), use the service capability instead of the protocol.

### Idiomatic Connection Pattern
Use the `service_marker` API on `context.incoming`. This returns a
`ServiceConnector` which allows overriding the instance name before connecting.

```rust
        let service = context
            .incoming
            .service_marker(ft::MyServiceMarker)
            .instance("custom_instance") // Optional: defaults to "default"
            .connect()?;

        let proxy = service.connect_to_my_protocol()?;
```

### Platform Device (pdev) Instance Naming Rules
- **Composite Drivers**: Only specify the instance as `"pdev"` if the driver is
  a composite bind and the platform device parent is listed as `"pdev"` in the
  driver bind rules.
- **Non-Composite Drivers**: Avoid specifying the instance name. It will default
  to `"default"`.

### Dictionary-offered Services
If a service is offered via a dictionary, it may be exposed as a specific
instance in the incoming namespace (e.g., `"default"`, `"left"`, `"right"`,
`"opt"`).
- Use `.instance("instance_name")` to connect to a specific instance.
- If you need to fall back to other instances, you can use `if let Ok(...)` or
  match on the result of `.connect()`.

### Service Instance Connection Fallbacks
When connecting to service instances, note that `connect()` or
`connect_to_protocol()` might not fail immediately if the instance doesn't exist
(it creates a channel that will be closed by the peer). If you need to try
multiple instances (e.g., "default" then "leaf"), be aware that the first
attempt might return `Ok(proxy)` even if it fails later. It is better to
directly connect to the expected instance if known, or check for `PEER_CLOSED`
errors if you must fall back.

### Composite Nodes and Services
When a driver binds to a composite node, it can access services offered by its
parents.
- **Instance Naming**: The service instance names in the composite driver's
  incoming directory will match the **PARENT NAMES** defined in the bind rules
  or node spec, NOT the instance names used when offering the service to the
  child nodes.
- **Primary Parent Alias**: In C++, connecting to a service without an instance
  name typically defaults to `"default"`. If this automatic aliasing does not
  work in Rust, a valid workaround is to explicitly map `"default"` to the
  primary parent's name in the driver's connection logic.

## 6. Safe Hardware Register Access and `pdev`

### Using Macros for MMIO
Never perform manual bitwise operations (e.g., `val |= 1 << 5;`) on raw integers
when accessing MMIO registers. Instead, use the `mmio::register!` and
`mmio::register_block!` macros.

```rust
register! {
    RtcCtrl, u32, 0x00, RW, {
        pub bool, osc_sel, set_osc_sel: 8;
        pub bool, enable, set_enable: 12;
    }
}
```

These macros generate safe accessors for registers and fields. Update fields via
`regs.ctrl_mut().update(|r| { r.set_enable(true); });`. If using generated
register blocks, you may need to wrap `VmoMapping` in `MmioRegion<VmoMemory>` to
satisfy trait bounds.

### Platform Device and MMIO Access
When using the platform device protocol to access MMIO:
- **Use `pdev` library**: Prefer using the `pdev` library
  (`//sdk/lib/driver/platform-device/rust:pdev`) instead of manual FIDL calls
  and manual VMO mapping.
- **Convenience**: `pdev::PlatformDevice` handles fetching MMIO resources and
  mapping them into `MmioRegion<VmoMemory>` automatically via `map_mmio_by_id`
  or `map_mmio_by_name`.

```rust
use pdev::PlatformDevice;

// In start method:
let pdev = context.incoming.service_marker(fidl_fuchsia_hardware_platform_device::ServiceMarker).connect()?.connect_to_device()?;
let mmio = pdev.map_mmio_by_id(0).await?;
```

### Non-MMIO Bitfields
For data structures that are not memory-mapped registers, use the `bitfield`
crate's `bitfield!` macro.

```rust
use bitfield::bitfield;

bitfield! {
    pub struct Descriptor(u32);
    impl Debug;
    pub bool, valid, set_valid: 0;
    pub u8, priority, set_priority: 7, 4;
}
```

## 7. Concurrency, Async Patterns, and Locks

Modern Fuchsia drivers should avoid heavy synchronization like `Mutex` when
possible, preferring actor pattern when it makes sense.

### Patterns to avoid Mutex/Arc:
- **Task-Local State**: Own the state in a local task spawned on the driver's
  scope.
- **Channels for Connection Handoff**: Use an `mpsc::unbounded` channel to send
  connections from `ServiceFs` to your local task.
- **Concurrent Processing**: Use `rx.for_each_concurrent` on the channel
  receiver to process multiple connections simultaneously on the same local
  task.
- **State Ownership in Tasks**: For interrupt-driven drivers, consider moving
  ownership of the mutable state directly into the long-running async task that
  processes interrupts. This eliminates the need for locks.

### General Async Guidelines:
* **Concurrency (`fasync::Scope`)**: Do not detach tasks using `.detach()`.
  Instead, initialize a `fuchsia_async::Scope` within the driver's start method
  and use it to spawn concurrent work. The driver must retain ownership of the
  `Scope`.

### Two approaches based on FIDL bindings:
1.  **With `fidl_next`**: Pass `ServerEnd` over the channel. The handler task
    constructs the server and runs it.
2.  **With Standard `fidl`**: Pass the `RequestStream` over the channel. The
    handler task processes the stream directly.

Both approaches allow you to share a plain reference `&RefCell<State>` among all
connection handlers, avoiding both `Mutex` and `Rc`!

## 8. Safe Interrupt Handling

When waiting on interrupts in Rust:
- **Avoid `zx::Handle` for Interrupts**: Do not store interrupts as raw
  `zx::Handle`. This forces the use of `unsafe` when waiting on them.
- **Generic `InterruptKind`**: Make your device struct generic over `K:
  zx::InterruptKind` (e.g., `Device<K>`). This allows storing `zx::Interrupt<K>`
  directly and using `fuchsia_async::OnInterrupt` safely in both production and
  tests.
- **Async Wait Loop**: Instead of spawning a dedicated thread with
  `std::thread::spawn` and blocking on `irq.wait()`, use `fasync::OnInterrupt`
  to create a stream of interrupts and process them in an async task.

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

## 9. Node Lifecycle and NodeController

When adding a child node using the Rust wrapper `Node::add_child` or
`Node::add_owned_child`:
- **NodeController is safe to drop**: In the Rust wrappers, dropping the
  `NodeController` is safe if you do not need to control the node.
- **Node Ownership**: For `add_owned_child`, the returned `Node` handle controls
  the lifetime. If you drop the `Node` handle, the node will be removed. Always
  store the `Node` handle in your driver struct if you want the node to persist.

When adding a child node directly via the `add_child` FIDL method (without using
the Rust wrappers):
- **Keep Controller Alive**: The `NodeController` client returned by `AddChild`
  controls the lifetime of the node. If you drop this client, the node will be
  removed by the Driver Framework.
- **Store in Struct**: Always store the `NodeController` client in your driver
  struct if you want the node to persist beyond the `start` method.

## 10. Testing Drivers

When testing drivers that use `MmioRegion`:
- **Avoid Manual Mocks**: Instead of mocking read/write methods, use real VMOs
  to back the memory region.
- **VMO Injection**: In tests, create a `zx::Vmo`, map it using
  `VmoMapping::map`, and pass the resulting `MmioRegion` to the driver.
- **Verification**: You can write expected values to the VMO before calling
  driver methods, or read from the VMO after driver methods to verify they wrote
  correctly.

Example:
```rust
    let sensor_vmo = zx::Vmo::create(0x1000).unwrap();
    let sensor_region = Arc::new(Mutex::new(VmoMapping::map(0, 0x1000, sensor_vmo.duplicate(zx::Rights::SAME_RIGHTS).unwrap()).unwrap()));
    // Pass sensor_region to driver
```

## 11. Reviewing Changes

Upon finishing authoring a change to a driver written in Rust, you MUST use this
skill and the `rust_best_practices` skill to review the change.

**Consider performing the review in multiple rounds, each focusing on one
aspect:**
- **Round 1: API & FIDL**: Verify that `fidl_next` is used for FIDL operations
  instead of the legacy `fidl` crate.
- **Round 2: Hardware Access**: Verify that the `pdev` library and register
  macros are used for MMIO access. Check for magic numbers.
- **Round 3: Concurrency & Lifecycle**: Verify proper async patterns, lock
  usage, and node lifecycle management (e.g., keeping `Node` handles alive if
  needed).
- **Round 4: Style & Comments**: Check for AI-targeted comments, missing
  documentation, or non-idiomatic Rust.
