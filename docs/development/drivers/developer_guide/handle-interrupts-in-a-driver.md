# Handle interrupts in a driver

## Overview

This document covers how to write and test a Fuchsia driver that can listen to
[interrupts](/reference/kernel_objects/interrupts.md) in an efficient manner.
Interrupts are a common tool for letting a driver know when a certain hardware
(or virtual) event has occurred. In C++, interrupts are represented by the
[`zx::interrupt` class](/zircon/system/ulib/zx/include/lib/zx/interrupt.h). You
may see the words "interrupt" and "irq" used interchangeably. In this context,
they both represent an interrupt.

## Acquiring an interrupt

How the driver acquires an interrupt object is context-dependent. A common
approach is to request an interrupt from a FIDL service instance. For example,
if a driver wanted an interrupt object that represented GPIO events related to a
specific GPIO pin then the driver can request one by sending a
[`fuchsia.hardware.gpio.Gpio:GetInterrupt()` FIDL request](/sdk/fidl/fuchsia.hardware.gpio/gpio.fidl)
to the [`fuchsia.hardware.gpio.Service` FIDL service](/sdk/fidl/fuchsia.hardware.gpio/gpio.fidl)
instance within the driver's incoming namespace.

## Listening to an interrupt

Listening to an interrupt means executing code when the interrupt is triggered.
In drivers, it is common for interrupts to be triggered more than once over the
course of the interrupt's lifetime. The driver should be able to handle
interrupts as quickly as possible and not cause data races with other driver
code. Based on these requirements, it is recommended to use the
[`async::IrqMethod` class](/sdk/lib/async/include/lib/async/cpp/irq.h) to listen
to an interrupt.

`IrqMethod` accepts a class instance method (i.e. a callback) that will be
executed every time the corresponding interrupt is triggered. It also accepts a
dispatcher used to execute the callback. It is recommended to use the driver's
dispatcher `DriverBase::dispatcher()`. If the driver's dispatcher is
synchronized (driver dispatchers are synchronized by default) then the execution
of the callback will wait until the dispatcher is not currently executing other
code. Keep in mind that this means the interrupt handler's callback execution
will block the dispatcher from executing other code until it completes. This is
opposed to executing code in a separate thread when an interrupt trigger occurs.
In that scenario, the driver might be executing other code in the first thread
for other reasons and data races may occur between the two threads. This
approach is not recommended as it requires synchronization methods that increase
the driver's complexity, reduce its readability/maintainability, and introduce
synchronization bugs which are difficult to debug.

Here's an example of how a driver can listen to an interrupt using
`async::IrqMethod`:

```cpp {:.devsite-disable-click-to-copy}
#include <lib/async/cpp/irq.h>

class MyDriver : public fdf::DriverBase {
 public:
  zx::result<> Start() override {
    // Get the interrupt for a GPIO FIDL service.
    zx::result<fidl::ClientEnd<fuchsia_hardware_gpio::Gpio>> gpio =
      incoming()->Connect<fuchsia_hardware_gpio::Service::Device>(kIrqGpioParentName);
    if (gpio.is_error()) {
      fdf::error("Failed to connect to irq gpio: {}", gpio);
      return gpio.take_error();
    }
    fidl::WireResult interrupt = fidl::WireCall(gpio.value())->GetInterrupt({});
    if (!interrupt.ok()) {
      fdf::error("Failed to send GetInterrupt request: {}", interrupt.status_string());
      return zx::error(interrupt.status());
    }
    if (interrupt->is_error()) {
      fdf::error("Failed to get interrupt: {}", zx_status_get_string(interrupt->error_value()));
      return interrupt->take_error();
    }
    interrupt_ = std::move(interrupt->value()->interrupt);

    // Start listening to `interrupt_`. `interrupt_handler_` will execute its
    // associated callback on dispatcher `dispatcher()` when `interrupt_` is
    // triggered.
    interrupt_handler_.set_object(interrupt_.get());
    zx_status_t status = interrupt_handler_.Begin(dispatcher());
    if (status != ZX_OK) {
      fdf::error("Failed to listen to interrupt: {}",
        zx_status_get_string(status));
      return zx::error(status);
    }

    return zx::ok();
  }

 private:
  // Called by `interrupt_handler_` when `interrupt_` is triggered.
  void HandleInterrupt(
    // Dispatcher that `HandleInterrupt()` was executed on.
    async_dispatcher_t* dispatcher,

    // Object that executed `HandleInterrupt()`.
    async::IrqBase* irq,

    // Status of handling the interrupt.
    zx_status_t status,

    // Information related to the interrupt.
    const zx_packet_interrupt_t* interrupt_packet) {

    if (status != ZX_OK) {
      if (status == ZX_ERR_CANCELED) {
        // Expected behavior as this occurs when `interrupt_handler_` is
        // destructed.
        fdf::debug("Interrupt handler cancelled");
      } else {
        fdf::error("Failed to handle interrupt: {}",
          zx_status_get_string(status));
      }

      // An error status means that the interrupt was not triggered so don't
      // handle it.
      return;
    }

    // Wrap the interrupt ack in a defer to ensure that the interrupt is
    // acknowledged even in the case that an error occurs while trying to
    // handle the interrupt.
    auto ack_interrupt = fit::defer([this] {
      // Acknowledge the interrupt. This "re-arms" the interrupt. If the
      // interrupt is not acknowledged then `interrupt_` cannot be triggered
      // again and `HandleInterrupt()` will not get called again.
      interrupt_.ack();
    });

    // Perform work in response to triggered interrupt.
  }

  // Interrupt to listen to.
  zx::interrupt interrupt_;

  // Calls `this->HandleInterrupt()` every time `interrupt_` is triggered.
  // Destructing `interrupt_handler_` means to no longer listen to `interrupt_`.
  async::IrqMethod<MyDriver, &MyDriver::HandleInterrupt> interrupt_handler_{this};
};
```

`async::IrqMethod` belongs to the `async-cpp` library so don't forget to add it
as a dependency to the driver:

   * {GN} {:.devsite-disable-click-to-copy}

     ```gn
     source_set("my-driver") {
       deps = [
         "//sdk/lib/async:async-cpp",
       ]
     }
     ```

   * {Bazel} {:.devsite-disable-click-to-copy}

     ```bazel
     cc_library(
         name = "my-driver",
         deps = [
             "@fuchsia_sdk//pkg/async-cpp",
         ],
     )
     ```

## Testing an interrupt

The driver's unit tests should test the driver's ability to respond to
interrupts. This requires that the test can trigger interrupts without waiting
for a real hardware event and can verify that the driver is acknowledging
interrupts.

### Providing an interrupt

The test should create a virtual interrupt to provide to the driver. Virtual
interrupts are interrupts that can be triggered "virtually" (i.e. the test's
code can explicitly trigger the interrupt without waiting for an actual hardware
event). A virtual interrupt can be created like so:

```cpp
zx::interrupt interrupt;
ASSERT_EQ(
  zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt),
  ZX_OK);
```

The test should duplicate this interrupt and send the duplicate to the driver.
How the test sends the duplicate to the driver is context-dependent. It
is recommended to simulate how the driver really acquires an interrupt. For
example, if a driver acquires a GPIO interrupt from a
`fuchsia.hardware.gpio.Service` FIDL service instance then the test should fake
that FIDL service instance. Here is how to duplicate an interrupt:

```cpp
zx::interrupt duplicate;
ASSERT_EQ(interrupt.duplicate(ZX_RIGHT_SAME_RIGHTS, &duplicate), ZX_OK);
```

### Triggering an interrupt

The test can trigger a virtual interrupt like so:
```cpp
ASSERT_EQ(
  interrupt.trigger(
    // Options.
    0,

    // Timestamp of when the interrupt was triggered.
    zx::clock::get_boot()),
  ZX_OK);
```

This will cause the driver's interrupt handler to execute its callback for
interrupt triggers.

### Verifying an interrupt was acknowledged

The next step is verifying that the driver acknowledged the interrupt. When a
driver acknowledges an interrupt trigger, the interrupt returns to an
"untriggered" state. The interrupt will also send a signal about this state
change. The test will listen for this signal to know when the interrupt has been
acknowledged. This signal is also sent when a virtual interrupt is first
created.

It is recommended to use
[`async::WaitMethod` class](/sdk/lib/async/include/lib/async/cpp/wait.h) in
order to wait for the interrupt's signals. Similar to `async::IrqMethod`, it will
call its callback when the corresponding interrupt sends a specific signal. One
important difference is that `async::WaitMethod` will need to be "re-armed"
after its callback is called, otherwise, it will not call its callback when it
receives multiple signals.

Here's an example of how to listen to an interrupt acknowledgement:

```cpp {:.devsite-disable-click-to-copy}
#include <lib/async/cpp/wait.h>

class MyDriverEnvironment : public fdf_testing::Environment {
 public:
  zx::result<> Serve(fdf::OutgoingDirectory& to_driver_vfs) override {
    // Create a virtual interrupt to be listened to by the driver.
    EXPECT_EQ(
      zx::interrupt::create(zx::resource(), 0, ZX_INTERRUPT_VIRTUAL, &interrupt_),
      ZX_OK);


    zx::interrupt duplicate;
    EXPECT_EQ(interrupt_.duplicate(ZX_RIGHT_SAME_RIGHTS, &duplicate), ZX_OK);
    // Send duplicate interrupt to driver.

    // Dispatcher used to execute `HandleInterruptAck()`. In a driver unit test,
    // it is recommended to use the environment dispatcher so that
    // `HandleInterruptAck()` doesn't block the driver's code execution.
    async_dispatcher_t* dispatcher = fdf::Dispatcher::GetCurrent()->async_dispatcher();

    // Listen for when `interrupt_` is acknowledged.
    interrupt_ack_handler_.set_object(interrupt_.get());
    EXPECT_EQ(interrupt_ack_handler_.Begin(dispatcher), ZX_OK);

    return zx::ok();
  }

 private:
  // Called when `interrupt_` receives an acknowledgement.
  void HandleInterruptAck(
    // Dispatcher that `HandleInterruptAck()` was called on.
    async_dispatcher_t* dispatcher,

    // Object responsible for calling `HandleInterruptAck()`.
    async::WaitBase* wait,

    // Status of waiting for the acknowledgement.
    zx_status_t status,

    // Information related to the acknowledgement.
    const zx_packet_signal_t* signal) {

    if (status != ZX_OK) {
        FAIL() << "Failed to wait for interrupt ack" << zx_status_get_string(status);
    }

    // Do something in response to the acknowledgement.

    // "Re-arm" the listener. Wait for the next time `interrupt_` is
    // acknowledged.
    status = wait->Begin(dispatcher);
    if (status != ZX_OK) {
        fdf::error("Failed to re-arm interrupt ack handler: {}", zx_status_get_string(status));
    }
  }

  // Virtual interrupt that the driver is listening to.
  zx::interrupt interrupt_;

  // Calls `HandleInterruptAck()` whenever `interrupt_` receives an
  // acknowledgement.
  async::WaitMethod<InterruptController, &InterruptController::HandleInterruptAck>
    interrupt_ack_handler_{
      // Class instance to call `HandleInterruptAck()` on.
      this,

      // The object that the signals belong to. The test will provide the
      // interrupt object to `interrupt_ack_handler_` after the interrupt is
      // constructed.
      ZX_HANDLE_INVALID,

      // Call the callback when `interrupt_` is in the "untriggered" state.
      ZX_VIRTUAL_INTERRUPT_UNTRIGGERED,

      // Only call `HandleInterruptAck()` if the
      // ZX_VIRTUAL_INTERRUPT_UNTRIGGERED signal was received after
      // `interrupt_ack_handler_.Begin()` was called. If `interrupt_` is already
      // in the "untriggered" state before `interrupt_ack_handler_.Begin()` is
      // called then don't call `HandleInterruptAck()`.
      ZX_WAIT_ASYNC_EDGE
  };
};

class MyDriverTestConfiguration final {
 public:
  using DriverType = MyDriver;
  using EnvironmentType = MyDriverEnvironment;
};

class MyDriverTest : public testing::Test {
 public:
  void SetUp() override {
    ASSERT_EQ(driver_test_.StartDriver().status_value(), ZX_OK);
  }

 private:
  fdf_testing::BackgroundDriverTest<MyDriverTestConfiguration> driver_test_;
};
```

`async::WaitMethod` belongs to the `async-cpp` library so don't forget to add it
as a dependency to the driver's tests:

   * {GN}

     ```gn {:.devsite-disable-click-to-copy}
     test("my-driver-test-bin") {
       deps = [
         "//sdk/lib/async:async-cpp",
       ]
     }
     ```

   * {Bazel}

     ```bazel {:.devsite-disable-click-to-copy}
     fuchsia_cc_test(
         name = "my-driver-test",
         deps = [
             "@fuchsia_sdk//pkg/async-cpp",
         ],
     )
     ```