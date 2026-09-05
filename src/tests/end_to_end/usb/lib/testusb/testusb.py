#!/usr/bin/env fuchsia-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""USB Zero Function Test Host Utility.

A Python 3 CLI tool and library for testing USB devices using either Linux
`usbtest` ioctls or `libusb-1.0` (via pyusb or ctypes).

Supports test case selection, loop counts, random ordering, buffer size and
transfer count customization, and JSON/text reporting.
"""

from __future__ import annotations

import collections.abc
import ctypes
import ctypes.util
import dataclasses
import enum
import functools
import logging
from typing import Any

_LOGGER = logging.getLogger(__name__)

# ==============================================================================
# Constants & Ioctl Definitions
# ==============================================================================


class UsbDescriptorType(enum.IntEnum):
    """USB Descriptor Types per USB 2.0 / 3.x Specs."""

    DEVICE = 0x01
    CONFIG = 0x02
    STRING = 0x03
    INTERFACE = 0x04
    ENDPOINT = 0x05
    DEVICE_QUALIFIER = 0x06
    OTHER_SPEED_CONFIG = 0x07
    INTERFACE_POWER = 0x08


USB_DT_DEVICE = UsbDescriptorType.DEVICE
USB_DT_CONFIG = UsbDescriptorType.CONFIG
USB_DT_STRING = UsbDescriptorType.STRING
USB_DT_INTERFACE = UsbDescriptorType.INTERFACE
USB_DT_ENDPOINT = UsbDescriptorType.ENDPOINT
USB_DT_DEVICE_QUALIFIER = UsbDescriptorType.DEVICE_QUALIFIER
USB_DT_OTHER_SPEED_CONFIG = UsbDescriptorType.OTHER_SPEED_CONFIG
USB_DT_INTERFACE_POWER = UsbDescriptorType.INTERFACE_POWER

USB_ENDPOINT_XFER_CONTROL = 0x00
USB_ENDPOINT_XFER_ISOC = 0x01
USB_ENDPOINT_XFER_BULK = 0x02
USB_ENDPOINT_XFER_INT = 0x03
USB_ENDPOINT_XFERTYPE_MASK = 0x03

USB_DIR_OUT = 0x00
USB_DIR_IN = 0x80


@functools.cache
def _get_libc() -> ctypes.CDLL:
    """Load and return the C standard library with ioctl arg/restypes configured."""
    libc_name = ctypes.util.find_library("c") or "libc.so.6"
    libc = ctypes.CDLL(libc_name, use_errno=True)
    # int ioctl(int fd, unsigned long request, ...);
    libc.ioctl.argtypes = [ctypes.c_int, ctypes.c_ulong, ctypes.c_void_p]
    libc.ioctl.restype = ctypes.c_int
    return libc


def _ioc(direction: int, io_type: int, seq_number: int, size: int) -> int:
    """Calculate a Linux ioctl command number from directional and type fields.

    Matches the Linux kernel _IOC macro calculation:
    _IOC(dir, type, nr, size) = (dir << 30) | (type << 8) | (nr << 0) | (size << 16)
    """
    return (direction << 30) | (io_type << 8) | (seq_number << 0) | (size << 16)


def _ior(type_char: str, seq_number: int, size: int) -> int:
    """Calculate a Linux _IOR ioctl command number."""
    return _ioc(2, ord(type_char), seq_number, size)


def _iowr(type_char: str, seq_number: int, size: int) -> int:
    """Calculate a Linux _IOWR ioctl command number."""
    return _ioc(3, ord(type_char), seq_number, size)


def _io(type_char: str, seq_number: int) -> int:
    """Calculate a Linux _IO ioctl command number."""
    return _ioc(0, ord(type_char), seq_number, 0)


class UsbdevfsIoctl(ctypes.Structure):
    _fields_ = [
        ("ifno", ctypes.c_int),
        ("ioctl_code", ctypes.c_int),
        ("data", ctypes.c_void_p),
    ]


class UsbdevfsSetinterface(ctypes.Structure):
    _fields_ = [
        ("interface", ctypes.c_uint),
        ("altsetting", ctypes.c_uint),
    ]


class UsbdevfsBulktransfer(ctypes.Structure):
    _fields_ = [
        ("ep", ctypes.c_uint),
        ("len", ctypes.c_uint),
        ("timeout", ctypes.c_uint),
        ("data", ctypes.c_void_p),
    ]


class UsbdevfsCtrltransfer(ctypes.Structure):
    _fields_ = [
        ("bRequestType", ctypes.c_uint8),
        ("bRequest", ctypes.c_uint8),
        ("wValue", ctypes.c_uint16),
        ("wIndex", ctypes.c_uint16),
        ("wLength", ctypes.c_uint16),
        ("timeout", ctypes.c_uint32),
        ("data", ctypes.c_void_p),
    ]


class UsbdevfsDisconnectClaim(ctypes.Structure):
    _fields_ = [
        ("interface", ctypes.c_uint),
        ("flags", ctypes.c_uint),
        ("driver", ctypes.c_char * 256),
    ]


class Timeval(ctypes.Structure):
    _fields_ = [
        ("tv_sec", ctypes.c_long),
        ("tv_usec", ctypes.c_long),
    ]


class UsbtestParam(ctypes.Structure):
    _fields_ = [
        ("test_num", ctypes.c_uint32),
        ("iterations", ctypes.c_uint32),
        ("length", ctypes.c_uint32),
        ("vary", ctypes.c_uint32),
        ("sglen", ctypes.c_uint32),
        ("duration", Timeval),
    ]


USBDEVFS_CONTROL = _iowr("U", 0, ctypes.sizeof(UsbdevfsCtrltransfer))
USBDEVFS_BULK = _iowr("U", 2, ctypes.sizeof(UsbdevfsBulktransfer))
USBDEVFS_SETINTERFACE = _ior("U", 4, ctypes.sizeof(UsbdevfsSetinterface))
USBDEVFS_SETCONFIGURATION = _ior("U", 5, ctypes.sizeof(ctypes.c_uint))
USBDEVFS_CLAIMINTERFACE = _ior("U", 15, ctypes.sizeof(ctypes.c_uint))
USBDEVFS_RELEASEINTERFACE = _ior("U", 16, ctypes.sizeof(ctypes.c_uint))
USBDEVFS_IOCTL = _iowr("U", 18, ctypes.sizeof(UsbdevfsIoctl))
USBTEST_REQUEST = _iowr("U", 100, ctypes.sizeof(UsbtestParam))
USBDEVFS_DISCONNECT_CLAIM = _ior(
    "U", 27, ctypes.sizeof(UsbdevfsDisconnectClaim)
)
USBDEVFS_DISCONNECT = _io("U", 22)
USBDEVFS_CONNECT = _io("U", 23)
USBDEVFS_RESET = _io("U", 20)
USBDEVFS_CLEAR_HALT = _ior("U", 21, ctypes.sizeof(ctypes.c_uint))
USBDEVFS_GET_SPEED = _io("U", 31)

USBDEVFS_DISCONNECT_CLAIM_IF_DRIVER = 0x01

# Recognized USB Test Device VENDOR:PRODUCT IDs
KNOWN_TEST_DEVICES: tuple[tuple[int, int], ...] = (
    (0x18D1, 0xA022),  # Fuchsia USB Test Gadget (Source/Sink)
    (0x18D1, 0xA023),  # Fuchsia USB Test Gadget (Loopback)
)


# ==============================================================================
# Data Models & Interfaces
# ==============================================================================


class TestStatus(str, enum.Enum):
    PASS = "PASS"
    FAIL = "FAIL"
    SKIP = "SKIP"
    ERROR = "ERROR"


@dataclasses.dataclass
class TestParams:
    """Parameters controlling USB test execution.

    Attributes:
        iterations: Number of transfer repetitions to execute per test.
        length: Transfer buffer length in bytes.
        vary: Transfer size variation increment in bytes for varying tests.
        sglen: Number of scatter-gather entries per transfer request.
        timeout_ms: I/O completion timeout in milliseconds.
    """

    iterations: int = 1000
    length: int = 1024
    vary: int = 1024
    sglen: int = 32
    timeout_ms: int = 5000

    @property
    def buffer_size(self) -> int:
        """Alias for length representing transfer buffer size in bytes."""
        return self.length

    @buffer_size.setter
    def buffer_size(self, val: int) -> None:
        if val <= 0:
            raise ValueError("buffer_size must be > 0")
        self.length = val

    def __post_init__(self) -> None:
        if self.iterations <= 0:
            raise ValueError("iterations must be > 0")
        if self.length <= 0:
            raise ValueError("length must be > 0")
        if self.vary < 0:
            raise ValueError("vary must be >= 0")
        if self.sglen <= 0:
            raise ValueError("sglen must be > 0")
        if self.timeout_ms <= 0:
            raise ValueError("timeout_ms must be > 0")


@dataclasses.dataclass
class TestResult:
    """Execution result and metrics for a single USB test case.

    Attributes:
        test_id: Numerical identifier of the executed test.
        test_name: Name of the executed test.
        status: Pass/fail outcome status.
        duration_secs: Execution duration in seconds.
        error_message: Optional error message if the test failed or errored.
        iterations_completed: Count of completed test iterations.
        bytes_transferred: Total bytes transferred during test execution.
        throughput_mbps: Calculated transfer throughput in Megabits/sec.
        throughput_mbs: Calculated transfer throughput in Megabytes/sec.
    """

    test_id: int
    test_name: str
    status: TestStatus
    duration_secs: float
    error_message: str | None = None
    iterations_completed: int = 0
    bytes_transferred: int = 0
    throughput_mbps: float = 0.0
    throughput_mbs: float = 0.0

    def to_dict(self) -> dict[str, Any]:
        return dict(
            test_id=self.test_id,
            test_name=self.name_without_id(),
            status=self.status.value,
            duration_secs=round(self.duration_secs, 6),
            error_message=self.error_message,
            iterations_completed=self.iterations_completed,
            bytes_transferred=self.bytes_transferred,
            throughput_mbps=round(self.throughput_mbps, 3),
            throughput_mbs=round(self.throughput_mbs, 3),
        )

    def name_without_id(self) -> str:
        """Return the test name without the numerical test ID prefix."""
        prefix, sep, suffix = self.test_name.partition(":")
        return suffix.strip() if sep else prefix


@dataclasses.dataclass
class TestCase:
    """Definition of a USB test case.

    Attributes:
        id: Numerical test identifier matching Linux kernel usbtest.
        name: Human-readable test name.
        description: Summary of test operations and endpoint expectations.
    """

    id: int
    name: str
    description: str


ALL_TEST_CASES: dict[int, TestCase] = {
    0: TestCase(
        0,
        "Test 0: NOP / Speed check",
        "Checks device speed and responsiveness",
    ),
    1: TestCase(1, "Test 1: OUT4K", "4KB OUT transfers to bulk endpoint"),
    2: TestCase(2, "Test 2: IN4K", "4KB IN transfers from bulk endpoint"),
    3: TestCase(3, "Test 3: OUT_VARY", "OUT transfers with varying sizes"),
    4: TestCase(4, "Test 4: IN_VARY", "IN transfers with varying sizes"),
    5: TestCase(5, "Test 5: OUT_SG", "OUT transfers with scatter/gather"),
    6: TestCase(6, "Test 6: IN_SG", "IN transfers with scatter/gather"),
    7: TestCase(
        7,
        "Test 7: OUT_VARY_SG",
        "OUT transfers with varying sizes & scatter/gather",
    ),
    8: TestCase(
        8,
        "Test 8: IN_VARY_SG",
        "IN transfers with varying sizes & scatter/gather",
    ),
    9: TestCase(9, "Test 9: CTRL_CH9", "Chapter 9 standard control tests"),
    10: TestCase(10, "Test 10: CTRL_QUEUE_32", "Queue 32 control calls"),
    11: TestCase(11, "Test 11: UNLINK_IN", "Unlink bulk IN read transfers"),
    12: TestCase(12, "Test 12: UNLINK_OUT", "Unlink bulk OUT write transfers"),
    13: TestCase(13, "Test 13: SET_HALT", "Set and clear 1000 endpoint halts"),
    14: TestCase(
        14, "Test 14: CTRL_QUEUE", "Control queue transfer permutations"
    ),
    15: TestCase(
        15,
        "Test 15: ISO_OUT",
        "Isochronous OUT transfers",
    ),
    16: TestCase(
        16,
        "Test 16: ISO_IN",
        "Isochronous IN transfers",
    ),
    17: TestCase(
        17,
        "Test 17: OUT_UNALIGNED",
        "OUT unaligned/odd address transfers",
    ),
    18: TestCase(
        18, "Test 18: IN_UNALIGNED", "IN unaligned/odd address transfers"
    ),
    19: TestCase(
        19,
        "Test 19: OUT_PREMAPPED",
        "OUT pre-mapped coherent DMA transfers",
    ),
    20: TestCase(
        20,
        "Test 20: IN_PREMAPPED",
        "IN pre-mapped coherent DMA transfers",
    ),
    21: TestCase(21, "Test 21: CTRL_WRITE", "Vendor control write transfer"),
    22: TestCase(
        22, "Test 22: ISO_OUT_VARY", "Isochronous OUT with varying sizes"
    ),
    23: TestCase(
        23, "Test 23: ISO_IN_VARY", "Isochronous IN with varying sizes"
    ),
    24: TestCase(
        24,
        "Test 24: ISO_UNLINK",
        "Isochronous transfer unlink and cancel",
    ),
    25: TestCase(
        25,
        "Test 25: INT_OUT",
        "Interrupt OUT transfer",
    ),
    26: TestCase(
        26,
        "Test 26: INT_IN",
        "Interrupt IN transfer",
    ),
    27: TestCase(
        27,
        "Test 27: STREAM_OUT_31M",
        "Bulk OUT 31Mbytes continuous stream",
    ),
    28: TestCase(
        28,
        "Test 28: STREAM_IN_31M",
        "Bulk IN 31Mbytes continuous stream",
    ),
    29: TestCase(
        29,
        "Test 29: TOGGLE_CLEAR",
        "Clear toggle between bulk writes 1000 times",
    ),
    30: TestCase(30, "Test 30: SG_QUEUE_OUT", "Scatter-gather write queue"),
    31: TestCase(31, "Test 31: SG_QUEUE_IN", "Scatter-gather read queue"),
    32: TestCase(
        32,
        "Test 32: LOOPBACK_BULK_FIXED",
        "Bidirectional bulk loopback fixed buffer size",
    ),
    33: TestCase(
        33,
        "Test 33: LOOPBACK_BULK_VARY",
        "Bidirectional bulk loopback varying buffer size",
    ),
    34: TestCase(
        34,
        "Test 34: LOOPBACK_BULK_SHORT",
        "Bidirectional bulk loopback short packets",
    ),
    35: TestCase(
        35,
        "Test 35: LOOPBACK_BULK_ZLP",
        "Bidirectional bulk loopback zero-length packets",
    ),
}


# ==============================================================================
# USB Backends & Descriptors
# ==============================================================================


class MalformedDescriptorError(Exception):
    """Raised when a USB configuration descriptor payload is malformed."""


def iter_descriptors(
    config_descriptor: bytes,
) -> collections.abc.Iterator[bytes]:
    """Yield individual descriptor chunks from a raw USB configuration descriptor.

    Args:
        config_descriptor: Raw bytes of the USB configuration descriptor.

    Yields:
        Individual descriptor byte chunks.

    Raises:
        MalformedDescriptorError: If a descriptor's declared length is less than 2
            or exceeds the remaining buffer length.
    """
    idx = 0
    total_len = len(config_descriptor)
    while idx < total_len:
        b_length = config_descriptor[idx]
        if b_length < 2 or idx + b_length > total_len:
            raise MalformedDescriptorError(
                f"Malformed descriptor at offset {idx}: length {b_length}, "
                f"total remaining {total_len - idx}"
            )
        yield config_descriptor[idx : idx + b_length]
        idx += b_length


def parse_endpoints_from_config_descriptor(
    config_descriptor: bytes,
) -> tuple[int | None, int | None, int | None]:
    """Walk raw USB configuration descriptor bytes to locate paired endpoints.

    Scans the descriptor hierarchy and associates endpoints with their
    enclosing interface, returning endpoints from the same interface to
    prevent cross-interface mismatch.

    Args:
        config_descriptor: Raw bytes of the USB configuration descriptor.

    Returns:
        A 3-tuple of (ep_out, ep_in, target_ifnum). If either ep_out or ep_in
        is found, target_ifnum is guaranteed to be non-None (identifying the
        interface containing the endpoint(s)). If neither endpoint is found,
        returns (None, None, None).
    """
    curr_intf = None
    intf_eps = {}

    try:
        for descriptor in iter_descriptors(config_descriptor):
            if len(descriptor) < 2:
                continue
            b_descriptor_type = descriptor[1]
            match b_descriptor_type:
                case UsbDescriptorType.INTERFACE:
                    if len(descriptor) >= 9:
                        curr_intf = descriptor[2]
                        if curr_intf not in intf_eps:
                            intf_eps[curr_intf] = (None, None)
                case UsbDescriptorType.ENDPOINT:
                    if len(descriptor) >= 7 and curr_intf is not None:
                        ep_addr = descriptor[2]
                        ep_attr = descriptor[3]
                        if (
                            ep_attr & USB_ENDPOINT_XFERTYPE_MASK
                        ) == USB_ENDPOINT_XFER_BULK:
                            out_ep, in_ep = intf_eps[curr_intf]
                            if (ep_addr & USB_DIR_IN) != 0 and in_ep is None:
                                intf_eps[curr_intf] = out_ep, ep_addr
                            elif (ep_addr & USB_DIR_IN) == 0 and out_ep is None:
                                intf_eps[curr_intf] = ep_addr, in_ep
                case _:
                    pass
    except MalformedDescriptorError as err:
        _LOGGER.warning(
            "Malformed descriptor encountered while parsing USB endpoints: %s",
            err,
        )
        return None, None, None

    # 1. Return the first interface that has both Bulk OUT and Bulk IN endpoints
    for ifnum, (out_ep, in_ep) in intf_eps.items():
        if out_ep is not None and in_ep is not None:
            assert ifnum is not None
            return out_ep, in_ep, ifnum

    # 2. If no single interface has both, return the first interface with either endpoint
    # without pairing endpoints across different interface boundaries.
    for ifnum, (out_ep, in_ep) in intf_eps.items():
        if out_ep is not None or in_ep is not None:
            assert ifnum is not None
            return out_ep, in_ep, ifnum

    return None, None, None
