# Driver examples

This page contains a list of the examples intended to demonstrate Fuchsia
driver concepts.

## Basic examples

These examples demonstrate common [driver concepts][driver-concepts]:

* **Skeleton driver** - The [skeleton driver example][skeleton-driver] is
  a minimal driver.

* **Template driver** - The [template driver example][driver-template] is a
  minimal driver built with SDK rules (which can be used in both the Fuchsia
  source checkout and Fuchsia SDK environments).

* **Simple driver** - The [simple driver example][simple-driver] demonstrates
  how to write a driver with the following common patterns:

  * Adding logs
  * Adding a child node
  * Implementing stop functions

* **Metadata** - The [metadata example][driver-metadata] demonstrates how
  to define metadata types and pass metadata from a driver to its children.

## Transport examples

These examples demonstrate communication between drivers:

* **Zircon transport** - The [Zircon transport example][zircon-transport]
  demonstrates a parent driver serving a FIDL protocol over Zircon transport and
  a child driver that connects and queries data from it.

* **Driver transport** - The [driver transport example][driver-transport]
  demonstrates a parent driver serving a FIDL protocol over driver transport and
  a child driver that connects and queries data from it.

## Bind examples

These examples demonstrate how to write and use bind libraries for drivers:

* **Bind library** - The [bind library example][bindlib] demonstrates how to
  write a bind library.

* **Bind library codegen** - The [bind library codegen example][bindlib-codegen]
  demonstrates how to use the C++ and Rust constants generated from a bind
  library.

## Test examples

These examples demonstrate writing unit and integration tests for drivers.

### Unit tests

Unit test examples:

* [**Simple driver**][test-simple] - Barebones unit test for the simple driver
  example
* [**Zircon transport**][test-zircon] - Unit test that verifies that the
  child driver can query from a fake FIDL zircon server
* [**Driver transport**][test-driver] - Unit test that verifies that the child
  driver can query from a fake FIDL driver server

### Integration tests

The [Driver Test Realm examples][driver-test-realm] demonstrate how to write
hermetic and non-hermetic integration tests for a driver.

<!-- Reference links -->

[driver-concepts]: /docs/concepts/drivers/README.md
[skeleton-driver]: /examples/drivers/skeleton/
[driver-template]: /examples/drivers/template/
[simple-driver]: /examples/drivers/simple/dfv2/
[driver-metadata]: /examples/drivers/metadata/
[zircon-transport]: /examples/drivers/transport/zircon/v2/
[driver-transport]: /examples/drivers/transport/driver/v2/
[bindlib]: /examples/drivers/bind/bindlib/
[bindlib-codegen]: /examples/drivers/bind/bindlib_codegen/
[test-simple]: /examples/drivers/simple/dfv2/tests/
[test-zircon]: /examples/drivers/transport/zircon/v2/tests/
[test-driver]: /examples/drivers/transport/driver/v2/tests/
[driver-test-realm]: /examples/drivers/driver_test_realm/

