---
name: driver_interrupt_implementation_rust
description: Implement interrupt handling in Fuchsia Rust drivers
---

# Driver Interrupt Implementation (Rust)

## Dependencies

To use
[fuchsia_async::OnInterrupt](/src/lib/fuchsia-async/src/handle/zircon/on_interrupt.rs),
add the following to your build file:

**GN:**
```gn
deps = [
  "//src/lib/fuchsia-async",
  "//third_party/rust_crates:futures",
]
```

**Bazel:**
```bazel
deps = [
  "@fuchsia_sdk//pkg/fuchsia-async",
  "//third_party/rust_crates/vendor:futures",
]
```

## Acquire an Interrupt

Drivers typically acquire an interrupt object from a FIDL service.

#### **If** the interrupt is acquired from a standard FIDL service (e.g., GPIO):

A driver might request a GPIO interrupt via
[`fuchsia.hardware.gpio/Gpio.GetInterrupt`](/sdk/fidl/fuchsia.hardware.gpio/gpio.fidl).

#### **Otherwise** (If the interrupt is defined via devicetree for a platform device):

The driver typically uses the Platform Device service to acquire it via methods
like `GetInterruptById` or `GetInterruptByName` on the
[`fuchsia.hardware.platform.device/Device`](/sdk/fidl/fuchsia.hardware.platform.device/platform-device.fidl)
protocol.

**Lifecycle and Cleanup**: No manual cleanup of the interrupt handle is
necessary. The `zx::Interrupt` type implements the `Drop` trait, which
automatically closes the underlying Zircon handle when the object goes
out of scope.

## Listen to an Interrupt

* Use `fuchsia_async::OnInterrupt` to create a stream from the interrupt handle.
* Process the stream in a spawned task.
* Acknowledge the interrupt using `interrupt_stream.ack()` after handling.

## Implement an Interrupt Handler

```rust
use fdf_component::{Driver, DriverContext};
use fuchsia_async::{OnInterrupt, Task};
use futures::StreamExt;
use zx::Interrupt;

pub struct MyDriver {
  interrupt_handler: Task<()>,
}

impl Driver for MyDriver {
    async fn start(context: DriverContext) -> Result<Self, Status> {
        let interrupt: Interrupt = todo!();
        let interrupt_stream = OnInterrupt::new(interrupt);
        let interrupt_handler = Task::spawn(async move {
          while let Some(Ok(_time)) = interrupt_stream.next().await {
            if let Err(e) = Self::handle_interrupt() {
                log::error!("Failed to handle interrupt: {e:?}");
            }
            if let Err(e) = interrupt_stream.ack() {
                log::error!("Failed to ack interrupt: {e:?}");
            }
          }
        });

        Ok(Self { interrupt_handler })
    }

    async fn handle_interrupt() -> Result<(), Status> {
        todo!();
    }
}
```

## Common Pitfalls

* **Forgetting to acknowledge the interrupt**: Failing to call
  `interrupt_stream.ack()` will prevent the interrupt from triggering again.
  Ensure you call this after processing each event.
* **Task cancellation**: The interrupt stream handler runs in a spawned task. If
  the `Task` handle is dropped, the task is canceled and the driver will stop
  receiving interrupts. Ensure the `Task` is stored in your driver struct to
  keep it alive.

## Further Reading

* [Handle Interrupts in a Driver](/docs/development/drivers/developer_guide/handle-interrupts-in-a-driver.md) -
  Comprehensive Fuchsia developer guide covering implementation and testing in
  both C++ and Rust.
* [Interrupts Reference](/docs/reference/kernel_objects/interrupts.md) - Kernel
  object reference detailing Zircon interrupts and the
  `ZX_VIRTUAL_INTERRUPT_UNTRIGGERED` signal.
* For guidance on testing interrupts, see the
  [Driver Interrupt Testing (Rust)](/src/devices/skills/driver_interrupt/testing/rust/SKILL.md)
  skill.
