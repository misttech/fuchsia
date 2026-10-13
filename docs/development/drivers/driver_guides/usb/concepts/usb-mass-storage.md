# USB mass storage driver

The USB mass storage driver is used to communicate with mass storage devices
such as flash drives, external hard drives, and other types of removable media
connected through USB. The USB mass storage driver is split into two parts:

* [SCSI block device][scsi-block-device] serves
  [`fuchsia.hardware.block.volume.Service`][block].
* [Core][core] device interfaces with the USB stack.

## SCSI block device

The block device serves [`fuchsia.hardware.block.volume.Service`][block] using
the [`block_server`][block-server] library. It supports read, write, and flush
operations. If power is lost between a write operation and a flush operation,
changes written to a USB mass storage device may not be persisted to the device.
The driver has no mechanism to inform drivers higher up in the stack of when
a write has actually been written to physical media. For the purposes of USB
mass storage, a write is considered complete when the device acknowledges the
write.

## Core device

The core device serves as the interface between the SCSI block device and the
USB stack. The core accepts SCSI requests from the block device, and converts
them into USB requests, which are eventually sent to hardware through the USB
stack. For each request, the following steps are performed:

*   Request is received from the block server by the SCSI layer and queued for
    the worker thread.
*   Worker thread picks up the request and sends the SCSI command to the device
    over USB.
*   Data is transferred between the request VMO and the device over USB (if
    applicable).
*   Request status is read back from the device.
*   Request is completed, sending a reply back through the block server.

Some USB mass storage devices may have multiple block devices such as an array
of disks. In this case, the core driver creates one block device per disk.

<!-- Reference links -->

[scsi-block-device]: /src/devices/block/lib/scsi/block-device.cc
[block]: /sdk/fidl/fuchsia.hardware.block.volume/volume.fidl
[block-server]: /src/storage/lib/block_server/src/lib.rs
[core]: /src/devices/block/drivers/usb-mass-storage/usb-mass-storage.cc
