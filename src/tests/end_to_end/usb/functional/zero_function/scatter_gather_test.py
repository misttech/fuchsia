# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""Mobly E2E scatter-gather test suite for USB Zero Function (usb-zero-function).

Verifies DWC3 multi-region scatter-gather DMA, page-crossing regions,
short-packet FIFO fast-forwarding, and unchained IN ZLP TRBs in Loopback mode.
"""

import zero_function
from mobly import asserts, test_runner


class ScatterGatherTest(zero_function.ZeroFunctionBaseTest):
    """Scatter-gather functional test suite for DWC3 via USB Zero Function."""

    def test_dwc3_scatter_gather_multi_region(self) -> None:
        """Verifies DWC3 scatter-gather across 4 non-MPS regions."""
        with self.open_loopback_device() as dev:
            ep_out = dev.ep_out
            ep_in = dev.ep_in
            mps = ep_out.wMaxPacketSize

            out_regions = [
                (0, 200),
                (4096, mps - 200),
                (8192, mps // 2),
                (12288, mps - (mps // 2)),
            ]
            in_regions = [
                (16384, mps // 2),
                (20480, mps // 4),
                (24576, mps - (mps // 2) - (mps // 4)),
                (28672, mps),
            ]

            dev.configure_scatter_gather(out_regions, in_regions)
            try:
                for i in range(16):
                    tx_data = bytes(
                        ((i * 37 + j) & 0xFF) for j in range(2 * mps)
                    )
                    dev.bulk(ep_out.bEndpointAddress, tx_data)
                    rx_data = dev.bulk(ep_in.bEndpointAddress, 2 * mps)
                    asserts.assert_equal(rx_data, tx_data)
            finally:
                dev.reset_scatter_gather()

    def test_dwc3_scatter_gather_multi_page(self) -> None:
        """Verifies DWC3 scatter-gather with regions crossing page boundaries."""
        with self.open_loopback_device() as dev:
            ep_out = dev.ep_out
            ep_in = dev.ep_in

            out_regions = [(2048, 8192), (18432, 8192)]
            in_regions = [(1024, 12288), (33792, 4096)]

            dev.configure_scatter_gather(out_regions, in_regions)
            try:
                for i in range(8):
                    tx_data = bytes(((i * 53 + j) & 0xFF) for j in range(16384))
                    dev.bulk(ep_out.bEndpointAddress, tx_data)
                    rx_data = dev.bulk(ep_in.bEndpointAddress, 16384)
                    asserts.assert_equal(rx_data, tx_data)
            finally:
                dev.reset_scatter_gather()

    def test_dwc3_scatter_gather_short_packet_fast_forward(self) -> None:
        """Verifies DWC3 FIFO fast-forwarding on short packets mid-chain."""
        with self.open_loopback_device() as dev:
            ep_out = dev.ep_out
            ep_in = dev.ep_in

            out_regions = [(i * 4096, 2048) for i in range(8)]
            in_regions = [(32768 + i * 4096, 4096) for i in range(4)]

            dev.configure_scatter_gather(out_regions, in_regions)
            try:
                transfer_lengths = [
                    300,
                    16384,
                    2500,
                    16384,
                    5000,
                    16384,
                    9000,
                    16384,
                ] * 2
                for i, length in enumerate(transfer_lengths):
                    tx_data = bytes(
                        ((i * 71 + j) & 0xFF) for j in range(length)
                    )
                    dev.bulk(ep_out.bEndpointAddress, tx_data)
                    rx_data = dev.bulk(ep_in.bEndpointAddress, length)
                    asserts.assert_equal(rx_data, tx_data)
            finally:
                dev.reset_scatter_gather()

    def test_dwc3_scatter_gather_short_packet_exact_trb_boundary(self) -> None:
        """Verifies DWC3 short packet handling when packet size matches TRB size."""
        with self.open_loopback_device() as dev:
            ep_out = dev.ep_out
            ep_in = dev.ep_in
            mps = ep_out.wMaxPacketSize

            out_regions = [
                (0, 200),
                (4096, mps - 200),
                (8192, mps),
            ]
            in_regions = [
                (16384, mps),
                (20480, mps),
            ]

            dev.configure_scatter_gather(out_regions, in_regions)
            try:
                for i in range(4):
                    tx_data = bytes(((i * 83 + j) & 0xFF) for j in range(200))
                    dev.bulk(ep_out.bEndpointAddress, tx_data)
                    rx_data = dev.bulk(ep_in.bEndpointAddress, 200)
                    asserts.assert_equal(rx_data, tx_data)
            finally:
                dev.reset_scatter_gather()

    def test_dwc3_scatter_gather_in_zlp(self) -> None:
        """Verifies DWC3 appends an unchained ZLP TRB after chained IN TRBs."""
        with self.open_loopback_device() as dev:
            ep_out = dev.ep_out
            ep_in = dev.ep_in
            mps = ep_out.wMaxPacketSize

            out_regions = [(0, mps), (4096, mps)]
            in_regions = [
                (0, mps // 2),
                (4096, mps - (mps // 2)),
                (8192, mps),
                (12288, mps),
            ]

            dev.configure_scatter_gather(
                out_regions, in_regions, in_short_zlp=True
            )
            try:
                for i in range(8):
                    tx_data = bytes(
                        ((i * 97 + j) & 0xFF) for j in range(2 * mps)
                    )
                    dev.bulk(ep_out.bEndpointAddress, tx_data)
                    rx_data = dev.bulk(
                        ep_in.bEndpointAddress, 3 * mps, timeout=3000
                    )
                    asserts.assert_equal(len(rx_data), 2 * mps)
                    asserts.assert_equal(rx_data, tx_data)
            finally:
                dev.reset_scatter_gather()


if __name__ == "__main__":
    test_runner.main()
