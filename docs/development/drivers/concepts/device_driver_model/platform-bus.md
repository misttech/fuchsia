# Platform Bus

## Introduction

The platform bus is a framework that allows drivers access to platform-specific hardware resources
(MMIO, IRQs, BTIs, SMCs).

In the wild, the term **platform bus** can describe two things:

- The driver [//src/devices/bus/drivers/platform/](/src/devices/bus/drivers/platform/).
- The framework itself.

In this document, **platform bus driver** refers to the specific driver and **platform bus**
refers to the general framework.

## Platform Bus Initialization

When the system boots, the driver manager explicitly starts the platform bus driver. The platform
bus driver is generic and does not know about the hardware it is running on. On start, the driver
creates a child node for a **board driver** to bind to. Board drivers have knowledge of the hardware
present on the device/board.

It is important that the correct board driver is bound to the node. This is done by setting the bind
properties of the platform bus driver's child node to allow only a specific board driver to bind to
it. The platform bus driver sets these properties by inspecting the `ZBI_TYPE_PLATFORM_ID` record
within the [ZBI data][zbi-data] provided by the bootloader. These bind properties include vendor ID
(`fuchsia.BIND_PLATFORM_DEV_VID`) and product ID (`fuchsia.BIND_PLATFORM_DEV_PID`).

The platform bus driver exposes the [`fuchsia.hardware.platform.bus/PlatformBus`][platform-bus-fidl]
and [`fuchsia.hardware.platform.bus/Firmware`][platform-bus-fidl] FIDL protocols to the board
driver. The board driver uses `PlatformBus` and its knowledge of the hardware to create **platform
devices**, which is how other drivers gain access to hardware resources.

Modern board drivers use [devicetree][devicetree-overview]
([//sdk/lib/driver/devicetree/](/sdk/lib/driver/devicetree/)) to discover hardware instead
of hardcoding that knowledge in the driver. A devicetree board driver calls `Firmware.GetFirmware`
with `FirmwareType::DEVICE_TREE`, which returns a VMO containing the devicetree blob
(`ZBI_TYPE_DEVICETREE`) from the ZBI. The board driver reads and parses the devicetree nodes from
this VMO and publishes the resulting platform devices to `PlatformBus`.

## Platform Devices

Platform devices are nodes that provide access to platform-specific hardware resources
for the drivers that bind to them. Examples of such resources include regions of MMIO, interrupts,
BTIs, and SMCs.

The board driver will send `NodeAdd` or `AddCompositeNodeSpec` requests to the platform
bus for it to create child nodes that act as the platform devices. The request includes
the node's name (e.g. `"gpio"`), identifiers used to populate bind properties on the node
(`fuchsia.BIND_PLATFORM_DEV_VID`, `fuchsia.BIND_PLATFORM_DEV_PID`, `fuchsia.BIND_PLATFORM_DEV_DID`,
and `fuchsia.BIND_PLATFORM_DEV_INSTANCE_ID`, along with any custom properties such as
`fuchsia.COMPATIBLE`), and the resources that the node requires (e.g. MMIO ranges, IRQs, BTIs, and
SMC ranges):

- **Standalone platform devices** - If a driver only needs platform bus resources, the board driver
  calls `NodeAdd` and the driver binds directly to the resulting platform device node.
- **Composite platform devices** - Most hardware devices require platform bus resources alongside
  capabilities from other drivers (such as GPIO, I2C, or clocks). For these devices, the driver
  binds to a [composite node][composite-node-spec] where the platform device node is one parent
  (conventionally named `"pdev"`) and the other capability providers are the remaining parents.
  Board drivers can create the platform device node and register its composite node specification in
  one step via `AddCompositeNodeSpec` (or call `NodeAdd` and register the composite specification
  with `fuchsia.driver.framework/CompositeNodeManager` directly, as devicetree board drivers do).

### Platform Device Protocol

Each platform device node will offer the
[`fuchsia.hardware.platform.device/Device`][platform-device-fidl] protocol to the driver that binds
to it. It provides access to hardware resources allocated to the platform device (as specified by
the board driver):

- **MMIO ranges** - Accessed by index or name via `GetMmioById` / `GetMmioByName`.
- **Interrupts** - Accessed by index or name via `GetInterruptById` / `GetInterruptByName`.
- **BTIs (Bus Transaction Initiators)** - Accessed by index or name via `GetBtiById` / `GetBtiByName`.
- **SMCs (Secure Monitor Calls)** - Accessed by index or name via `GetSmcById` / `GetSmcByName`.

By requesting resources by index or name rather than hardcoding physical addresses or IRQ
numbers, platform device drivers can be reused across different SoC variants.

<!-- Reference links -->

[zbi-data]: /sdk/lib/zbi-format/include/lib/zbi-format/zbi.h
[composite-node-spec]: /docs/development/drivers/developer_guide/create-a-composite-node.md
[devicetree-overview]: /docs/development/boards/devicetree-overview.md
[platform-device-fidl]: /sdk/fidl/fuchsia.hardware.platform.device/platform-device.fidl
[platform-bus-fidl]: /sdk/fidl/fuchsia.hardware.platform.bus/platform-bus.fidl
