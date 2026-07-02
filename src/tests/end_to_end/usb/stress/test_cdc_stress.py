# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
"""CDC-ECM / CDC-NCM USB Networking stress test suite.

This test suite verifies the resilience, memory stability, and long-term
reliability of Fuchsia's virtual USB Ethernet drivers (CDC-ECM and CDC-NCM)
under continuous strain, addressing b/523412025 and b/519743129.

Specifically, it exercises:
1. Sustained Packet Transfers: Verifies that intensive data and RPC traffic
   over the USB link-local Ethernet link does not cause dropped packets,
   driver lockups, or memory leaks.
2. Large File Transfer Stress: Simulates multi-megabyte high-throughput
   payload streaming across SSH over the virtual Ethernet link while stressing
   Bulk IN/OUT DMA transfer paths.
3. Varying Packet Size Stress: Exercises packet boundary handling, MTU
   thresholds, and CDC-NCM Network Transfer Block aggregation across varying
   burst sizes (from 64B up to 64KB jumbo frames).
4. Continuous FIDL Interface Polling: Verifies that continuous state querying
   over native FIDL (fuchsia.net.interfaces/State) exercises driver property
   tables and MAC/IP structures without triggering Overnet transport deadlocks.
5. Repeated USB Power Cycling: Verifies that physically cutting and restoring
   VBUS power via a hardware USB power hub allows the CDC network adapter to
   cleanly de-enumerate and automatically recover network routing upon reboot.
"""

import asyncio
import hashlib
import logging
import os
import subprocess
import tempfile

import fuchsia_base_test
from honeydew.affordances.connectivity.netstack.types import PortClass
from honeydew.auxiliary_devices.usb_power_hub import usb_power_hub
from honeydew.transports.ffx.types import MachineFormat
from mobly import asserts, test_runner

_LOGGER: logging.Logger = logging.getLogger(__name__)


class CdcStressTest(fuchsia_base_test.FuchsiaBaseTest):
    """Mobly test suite verifying CDC-ECM/NCM stack resilience under strain."""

    async def setup_class(self) -> None:
        """Called once before running test cases in the class."""
        await super().setup_class()
        self._usb_power_hub: usb_power_hub.UsbPowerHub
        self._usb_port: int | None
        (self._usb_power_hub, self._usb_port) = self._lookup_usb_power_hub(
            self.dut
        )

    async def _verify_cdc_routing(self) -> None:
        """Verifies via FIDL that active FFX SSH traffic traverses the USB CDC Ethernet adapter."""
        await self.dut.wait_for_online()
        await self.dut.on_device_boot()
        interfaces = await self.dut.netstack.list_interfaces()
        cdc_ips: list[str] = []
        wlan_ips: list[str] = []
        for iface in interfaces:
            if iface.name != "lo":
                ip_strs = [
                    str(ip).split("/")[0].split("%")[0]
                    for ip in (iface.ipv4_addresses + iface.ipv6_addresses)
                ]
                if iface.port_class == PortClass.ETHERNET:
                    cdc_ips.extend(ip_strs)
                    _LOGGER.info(
                        "Verified CDC Ethernet interface '%s' (ID: %s, MAC: %s, IPs: %s)",
                        iface.name,
                        iface.id_,
                        iface.mac,
                        ip_strs,
                    )
                elif iface.port_class == PortClass.WLAN_CLIENT:
                    wlan_ips.extend(ip_strs)

        asserts.assert_true(
            len(cdc_ips) > 0,
            "Pre-flight check failed: No active CDC USB Ethernet IP addresses detected via FIDL.",
        )

        ssh_addr = self.dut.ffx.get_target_ssh_address()
        asserts.assert_is_not_none(
            ssh_addr,
            "Pre-flight check failed: Unable to obtain FFX SSH target address.",
        )
        raw_ssh_ip = str(ssh_addr.ip).split("%")[0]
        _LOGGER.info(
            "Active FFX SSH raw IP: %s | CDC IPs: %s | WLAN IPs: %s",
            raw_ssh_ip,
            cdc_ips,
            wlan_ips,
        )

        asserts.assert_in(
            raw_ssh_ip,
            cdc_ips,
            f"Pre-flight check failed: SSH IP '{raw_ssh_ip}' does not match CDC Ethernet ({cdc_ips}). Traffic might be leaking over WLAN ({wlan_ips})!",
        )

    # TODO(b/528457918): Add `test_vsock_sustained_packet_transfer` suite once FFX and
    # Honeydew implement host-side `TargetVsock` targeting (blocked on b/406262417 and b/346425048).
    async def test_cdc_sustained_packet_transfer(self) -> None:
        """Verifies CDC network stack under continuous packet transfer strain.

        Performs repeated intensive payload queries and data transfers across the
        link-local USB Ethernet connection to verify 0 dropped RPC frames or timeouts.
        """
        num_iterations = int(self.user_params.get("packet_iterations", 15))
        _LOGGER.info(
            "Starting CDC sustained packet transfer stress test across %d iterations.",
            num_iterations,
        )

        await self._verify_cdc_routing()

        for i in range(1, num_iterations + 1):
            _LOGGER.info(
                "Packet transfer stress iteration %d/%d", i, num_iterations
            )
            # Execute heavy data stream commands over the CDC link
            try:
                # Stream large payload over SSH across link-local virtual Ethernet IP
                large_payload = "CDC_STRAIN_TEST_" + ("X" * 10000)
                ssh_output = await asyncio.to_thread(
                    self.dut.ffx.run_ssh_cmd,
                    f"echo '{large_payload}'",
                )
                asserts.assert_equal(
                    ssh_output.strip(),
                    large_payload,
                    f"Iteration {i}: SSH packet payload mismatch over CDC network link.",
                )
            except Exception as e:
                asserts.fail(
                    f"CDC network stack failed during packet transfer iteration {i}: {e}"
                )

        _LOGGER.info(
            "Successfully completed %d iterations of CDC packet stress.",
            num_iterations,
        )

    async def test_cdc_large_file_transfer_target_to_host(self) -> None:
        """Verifies CDC network stack under Target->Host multi-megabyte data transfers.

        Simulates high-throughput streaming by transferring multi-megabyte binary streams
        from Target to Host over SSH across the CDC USB Ethernet link while stressing Bulk IN DMA.
        """
        num_iterations = int(
            self.user_params.get("large_transfer_iterations", 5)
        )
        transfer_mb = int(self.user_params.get("transfer_size_mb", 20))
        expected_bytes = transfer_mb * 1024 * 1024
        _LOGGER.info(
            "Starting CDC Target->Host large file transfer stress test (%d MB per transfer) across %d iterations.",
            transfer_mb,
            num_iterations,
        )

        await self._verify_cdc_routing()

        for i in range(1, num_iterations + 1):
            _LOGGER.info(
                "Target->Host large file transfer stress iteration %d/%d (%d MB)",
                i,
                num_iterations,
                transfer_mb,
            )
            try:
                ssh_output = await asyncio.to_thread(
                    self.dut.ffx.run_ssh_cmd,
                    f'printf "X%{expected_bytes - 2}sX" ""',
                )
                actual_bytes = len(ssh_output)
                asserts.assert_equal(
                    actual_bytes,
                    expected_bytes,
                    f"Iteration {i}: Transferred Target->Host payload size mismatch over CDC link (expected {expected_bytes}, got {actual_bytes}).",
                )
                _LOGGER.info(
                    "Iteration %d successfully streamed %d bytes from Target->Host over CDC link.",
                    i,
                    actual_bytes,
                )
            except Exception as e:
                asserts.fail(
                    f"CDC network stack failed during Target->Host large file transfer iteration {i}: {e}"
                )

        _LOGGER.info(
            "Successfully completed %d iterations of CDC Target->Host large file transfer stress.",
            num_iterations,
        )

    async def test_cdc_large_file_transfer_host_to_target(self) -> None:
        """Verifies CDC network stack under Host->Target multi-megabyte data transfers.

        Simulates OTA payload delivery by streaming multi-megabyte binary payloads from
        Host to Target over SSH across the CDC USB Ethernet link while stressing Bulk OUT DMA.
        """
        num_iterations = int(
            self.user_params.get("large_transfer_iterations", 5)
        )
        transfer_mb = int(self.user_params.get("transfer_size_mb", 20))
        expected_bytes = transfer_mb * 1024 * 1024
        _LOGGER.info(
            "Starting CDC Host->Target large file transfer stress test (%d MB per transfer) across %d iterations.",
            transfer_mb,
            num_iterations,
        )

        await self._verify_cdc_routing()

        target_payload_path = f"/tmp/cdc_ota_target_{os.getpid()}.bin"

        with tempfile.TemporaryDirectory(prefix="cdc_ota_") as tmp_dir:
            host_payload_path = os.path.join(tmp_dir, "host_payload.bin")
            _LOGGER.info(
                "Generating %d MB temporary host payload at %s",
                transfer_mb,
                host_payload_path,
            )
            with open(host_payload_path, "wb") as f:
                f.write(b"X" * expected_bytes)

            with open(host_payload_path, "rb") as f:
                expected_sha1 = hashlib.sha1(f.read()).hexdigest()
            _LOGGER.info(
                "Expected SHA1 hash of %d MB payload: %s",
                transfer_mb,
                expected_sha1,
            )

            try:
                for i in range(1, num_iterations + 1):
                    _LOGGER.info(
                        "Host->Target large file transfer stress iteration %d/%d (%d MB)",
                        i,
                        num_iterations,
                        transfer_mb,
                    )
                    try:
                        ffx_cmd = self.dut.ffx.generate_ffx_cmd(
                            cmd=[
                                "target",
                                "ssh",
                                f"cat > {target_payload_path}",
                            ],
                            include_target=True,
                            machine=MachineFormat.RAW,
                        )
                        with open(host_payload_path, "rb") as f:
                            await asyncio.to_thread(
                                subprocess.run,
                                ffx_cmd,
                                stdin=f,
                                stdout=subprocess.PIPE,
                                stderr=subprocess.PIPE,
                                check=True,
                            )

                        sha1_str = await asyncio.to_thread(
                            self.dut.ffx.run_ssh_cmd,
                            f"sha1sum {target_payload_path}",
                        )
                        target_sha1 = sha1_str.strip().split()[0]
                        asserts.assert_equal(
                            target_sha1,
                            expected_sha1,
                            f"Iteration {i}: Transferred Host->Target payload SHA1 content mismatch over CDC link (expected {expected_sha1}, got {target_sha1}).",
                        )
                        _LOGGER.info(
                            "Iteration %d successfully verified SHA1 hash (%s) of %d transferred bytes from Host->Target over CDC link.",
                            i,
                            target_sha1,
                            expected_bytes,
                        )
                    except Exception as e:
                        err_msg = str(e)
                        if (
                            isinstance(e, subprocess.CalledProcessError)
                            and e.stderr
                        ):
                            err_msg += f" (stderr: {e.stderr.decode('utf-8', errors='replace')})"
                        asserts.fail(
                            f"CDC network stack failed during Host->Target large file transfer iteration {i}: {err_msg}"
                        )
            finally:
                try:
                    await asyncio.to_thread(
                        self.dut.ffx.run_ssh_cmd,
                        f"rm -f {target_payload_path}",
                    )
                except Exception:
                    pass

        _LOGGER.info(
            "Successfully completed %d iterations of CDC Host->Target large file transfer stress.",
            num_iterations,
        )

    async def test_cdc_varying_packet_size_stress(self) -> None:
        """Verifies CDC network stack stability across varying packet and block sizes.

        Exercises packet boundary handling, MTU thresholds, and CDC-NCM Network Transfer Block (NTB)
        aggregation by streaming ICMPv6 echo datagram bursts of varying payload sizes (from tiny 64-byte
        chunks up to 32KB jumbo frames) across the USB Ethernet link. Using a packet-oriented protocol
        ensures datagram boundaries are preserved without TCP stream re-segmentation.
        """
        # We test sizes up to 32,768 bytes (32 KB) because CDC Ethernet has an MTU of 1500.
        # Payloads larger than 1452 bytes require IPv6 fragmentation (32 KB = 23 fragments).
        # When testing larger jumbo frames (e.g., 65 KB / 46 fragments), the target's network
        # stack (Netstack3) drops ICMP datagrams exceeding its fragment reassembly queue/buffer
        # threshold. 32 KB provides robust multi-packet NTB aggregation and fragment testing
        # without exceeding stack reassembly limits.
        packet_sizes = [64, 256, 512, 1280, 1420, 4096, 16384, 32768]
        burst_count = int(self.user_params.get("varying_burst_count", 50))
        _LOGGER.info(
            "Starting CDC varying packet size stress across block sizes: %s (burst count: %d)",
            packet_sizes,
            burst_count,
        )

        await self._verify_cdc_routing()

        ssh_addr = self.dut.ffx.get_target_ssh_address()
        target_ip = str(ssh_addr.ip)

        for bs in packet_sizes:
            _LOGGER.info(
                "Testing ICMPv6 packet burst with payload size %d bytes (%d packets) to %s",
                bs,
                burst_count,
                target_ip,
            )
            try:
                # Construct ping command with specific ICMPv6 flags:
                #   -6: Force IPv6 protocol (required for CDC Ethernet link-local addresses).
                #   -c <burst_count>: Number of echo request packets to send in this burst.
                #   -i 0.05: Wait 50 milliseconds between sending each packet (20 packets/sec)
                #            to stress the network stack rapidly without requiring root privileges.
                #   -s <bs>: Specify the exact payload size in bytes to test MTU boundary
                #            fragmentation and CDC-NCM Network Transfer Block aggregation.
                #   -W 5: Timeout in seconds to wait for a response before failing.
                cmd = [
                    "ping",
                    "-6",
                    "-c",
                    str(burst_count),
                    "-i",
                    "0.05",
                    "-s",
                    str(bs),
                    "-W",
                    "5",
                    target_ip,
                ]
                result = await asyncio.to_thread(
                    subprocess.run,
                    cmd,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    text=True,
                )
                asserts.assert_equal(
                    result.returncode,
                    0,
                    f"Payload size {bs}B: ping failed with exit code {result.returncode}. Stderr: {result.stderr}",
                )
                asserts.assert_in(
                    " 0% packet loss",
                    result.stdout,
                    f"Payload size {bs}B: Detected packet loss during ping stress. Output:\n{result.stdout}",
                )
                _LOGGER.info(
                    "Payload size %dB: Successfully transmitted and received %d ICMPv6 echo packets with 0%% packet loss.",
                    bs,
                    burst_count,
                )
            except Exception as e:
                err_msg = str(e)
                if isinstance(e, subprocess.CalledProcessError) and e.stderr:
                    err_msg += f" (stderr: {e.stderr})"
                asserts.fail(
                    f"CDC network stack failed during ICMPv6 packet stress with payload size {bs}B: {err_msg}"
                )

        _LOGGER.info(
            "Successfully completed CDC varying packet size stress across all block sizes."
        )

    # TODO(b/530262848): Explore a self-recovering link toggle mechanism (like a restart
    # command instead of disable/enable) that automatically recovers the link without
    # permanently locking out the USB connection, and add a dedicated link toggling test.
    async def test_cdc_interface_polling_stress(self) -> None:
        """Verifies CDC network driver stability via continuous FIDL polling.

        Note on architectural design and the Overnet Transport Catch-22:
        In test frameworks communicating from the host PC over Overnet/RCS tunneled
        through the CDC USB Ethernet interface (fe80::...), invoking destructive
        administrative teardown methods (such as fuchsia.net.interfaces.admin/Control.Disable)
        sever the underlying link-local carrier. Once the carrier drops, Overnet
        disconnects immediately, locking out any subsequent RPCs (such as Enable)
        from reaching the device.

        To rigorously verify driver IPC resilience and table management without
        severing our own transport channel, this test uses Honeydew's native FIDL
        netstack affordance (fuchsia.net.interfaces/State.GetWatcher). By continuously
        querying and inspecting full interface property tables across rapid polling
        iterations, we force the CDC network driver and netstack to continuously
        serialize, transmit, and validate MAC addresses, device class attributes,
        and online state flags under continuous RPC pressure.
        """
        num_iterations = int(
            self.user_params.get(
                "polling_iterations",
                self.user_params.get("toggle_iterations", 10),
            )
        )
        _LOGGER.info(
            "Starting CDC interface FIDL polling stress test across %d iterations.",
            num_iterations,
        )

        for i in range(1, num_iterations + 1):
            _LOGGER.info(
                "FIDL interface polling iteration %d/%d", i, num_iterations
            )
            try:
                # Query network interfaces via native FIDL netstack affordance
                # (invokes fuchsia.net.interfaces/State.GetWatcher over Overnet).
                interfaces = await self.dut.netstack.list_interfaces()
                cdc_iface_found = False
                for iface in interfaces:
                    # Filter for non-loopback interfaces with active IPv6 addresses (CDC Ethernet link).
                    if iface.name != "lo" and len(iface.ipv6_addresses) > 0:
                        cdc_iface_found = True
                        _LOGGER.debug(
                            "Iteration %d: Verified active CDC interface '%s' (ID: %s, MAC: %s)",
                            i,
                            iface.name,
                            iface.id_,
                            iface.mac,
                        )
                        break
                asserts.assert_true(
                    cdc_iface_found,
                    f"Iteration {i}: Failed to discover online CDC network interface via FIDL.",
                )
                await asyncio.sleep(1)
            except Exception as e:
                asserts.fail(
                    f"CDC network stack failed during FIDL interface polling iteration {i}: {e}"
                )

        _LOGGER.info(
            "Successfully completed %d iterations of FIDL interface polling.",
            num_iterations,
        )

    async def test_cdc_power_cycle_stress(self) -> None:
        """Verifies CDC virtual Ethernet auto-recovery across repeated VBUS power cycles.

        Physically cuts USB VBUS power, verifies device drop from host bus, restores
        power, and confirms automatic re-enumeration and IP connection recovery.
        """
        num_iterations = int(self.user_params.get("power_iterations", 3))
        disconnect_duration = int(
            self.user_params.get("disconnect_duration_sec", 5)
        )

        _LOGGER.info(
            "Starting CDC USB power cycle stress test across %d iterations.",
            num_iterations,
        )

        for i in range(1, num_iterations + 1):
            _LOGGER.info("Power cycle iteration %d/%d", i, num_iterations)
            await self.dut.wait_for_online()

            try:
                # Cut physical VBUS power via hardware USB hub
                self._usb_power_hub.power_off(port=self._usb_port)
                _LOGGER.info(
                    "Powered off USB port %s. Waiting for offline...",
                    self._usb_port,
                )
                await asyncio.to_thread(self.dut.wait_for_offline)

                if disconnect_duration > 0:
                    await asyncio.sleep(disconnect_duration)
            finally:
                # Restore VBUS power and verify CDC Ethernet network re-enumeration
                self._usb_power_hub.power_on(port=self._usb_port)
                _LOGGER.info(
                    "Powered on USB port %s. Waiting for CDC network recovery...",
                    self._usb_port,
                )
                await self.dut.wait_for_online()
                await self.dut.on_device_boot()

            # Verify network connectivity is fully functional after recovery
            ssh_check = await asyncio.to_thread(
                self.dut.ffx.run_ssh_cmd, "echo 'cdc_recovered'"
            )
            asserts.assert_in(
                "cdc_recovered",
                ssh_check,
                f"Iteration {i}: CDC network link failed to transmit data after power recovery.",
            )

        _LOGGER.info(
            "Successfully completed %d iterations of CDC power cycle stress.",
            num_iterations,
        )


if __name__ == "__main__":
    test_runner.main()
