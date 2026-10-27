# PCI Driver and Bus Test Suite

This directory contains unit and integration tests for the Fuchsia PCI bus driver (`bus-pci`) and the `fuchsia.hardware.pci` protocol.

## Overview

The test suite consists of two primary test packages:

1. **`pci-unit-test`**: C++ unit tests verifying internal bus driver subsystems against mock DDK and fakes.
2. **`pci-driver-test`**: Modern DFv2 Rust integration test suite verifying all `fuchsia.hardware.pci` protocol operations against `fake-bus-pci` (build target `pci_fake`) inside `DriverTestRealm` (DTR).

---

## Test Suites

### `pci-unit-test`

* **Source**: [`unit/`](unit/)
* **Build Target**: `//src/devices/pci/drivers/pci/test:pci-unit-test`
* **Framework**: C++ (GoogleTest / `mock-ddk`)

Runs isolated unit tests across driver internal components:

* `allocation_tests.cc`: Address space allocation book-keeping and resource window management.
* `bus_tests.cc`: Bus scanning, device topology lifecycle, and upstream node configuration.
* `config_tests.cc`: Configuration space accessors for Type 0 and Type 1 headers.
* `device_tests.cc`: PCI device lifecycle, capability traversal, BAR allocation, and IRQ modes.
* `msix_tests.cc`: MSI-X vector allocation, table programming, and capability configuration.

Unit tests are supported by test fakes located in [`fakes/`](fakes/):
* `fake_allocator.h`: VMO-backed `PciAllocation` fakes.
* `fake_bus.h`: Hardware-decoupled `BusDeviceInterface` implementation.
* `fake_config.h`, `fake_ecam.h`: MMIO-backed fake configuration space.
* `fake_upstream_node.h`: Fake upstream bridges and root complexes.
* `test_device.h`: NVIDIA Quadro K2200 reference configuration dump.

---

### `pci-driver-test`

* **Source**: [`driver/driver_tests.rs`](driver/driver_tests.rs)
* **Build Target**: `//src/devices/pci/drivers/pci/test:pci_driver_test`
* **Manifest**: [`meta/pci-driver-test.cml`](meta/pci-driver-test.cml)
* **Framework**: Rust (`fuchsia-driver-test`, `DriverTestRealmBuilder2`, `fuchsia-component-test`)

Validates the `fuchsia.hardware.pci.Device` protocol and `fuchsia.hardware.pci.Service` across 24 test cases against `fake-bus-pci` inside an isolated `DriverTestRealm`. The package runs as a `system` test component (`meta/pci-driver-test.cml`) so it can route `fuchsia.kernel.IoportResource` into `DriverTestRealm` for `fake-bus-pci` to back I/O port BAR allocations (`GetBar(5)`).

#### Test Architecture & `TestFixture` Pattern

The integration test uses the `TestFixture` struct to manage DriverTestRealm lifecycle:

* **Realm Construction**: Builds a hermetic test realm via `RealmBuilder::new()` and `driver_test_realm_setup()`.
* **Root Driver & Software Device**: Sets `platform-bus` as the root driver and publishes a software platform device named `pci` matching `fake-bus-pci`'s bind rules (`fuchsia.BIND_PLATFORM_DEV_DID == 0`).
* **Capability Routing**: Routes `fuchsia.kernel.IoportResource` into `DriverTestRealm` via `driver_offers` and exposes `fuchsia.hardware.pci.Service` directly to the test via `driver_exposes`.
* **Client Connection**: Waits for realm bootup (`instance.wait_for_bootup()`) and connects to `fuchsia.hardware.pci.Service` to acquire a `DeviceProxy`.

---

### `fake-bus-pci` (Target: `pci_fake`)

* **Source**: [`driver/fake_bus_driver.cc`](driver/fake_bus_driver.cc), [`driver/fake_bus_driver.h`](driver/fake_bus_driver.h)
* **Build Target**: `//src/devices/pci/drivers/pci/test:pci_fake`
* **Driver Binary**: `driver/fake-bus-pci.so`
* **Component Name**: `fake-bus-pci`
* **Manifest**: [`meta/pci_fake.cml`](meta/pci_fake.cml)
* **Bind Rules**: [`driver/meta/fake_pci_bus_driver.bind`](driver/meta/fake_pci_bus_driver.bind)

`fake-bus-pci` is a fake bus driver loaded into `DriverTestRealm`:

* Binds to the `pci` software device published by `platform-bus`.
* Initializes an MMIO ECAM view populated with the NVIDIA Quadro K2200 configuration space from `test_device.h`.
* Creates a `pci::Device` instance with fake upstream node and address space backing.
* Serves `fuchsia.hardware.pci.Service`, exposing the device FIDL interface to DriverTestRealm clients.

---

## Running Tests

Run both test suites:

```bash
fx test pci-driver-test pci-unit-test
```

Run only the Rust integration test suite:

```bash
fx test pci-driver-test
```

Run only the C++ unit test suite:

```bash
fx test pci-unit-test
```

Run a specific test case in the integration test suite:

```bash
fx test pci-driver-test -- --exact get_device_info
```
