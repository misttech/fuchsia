#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Blanket ignores to enable mypy in Antlion
# mypy: disable-error-code="no-untyped-def"
import os

from antlion.controllers import adb
from antlion.test_utils.net import connectivity_const as cconst
from antlion.utils import start_standing_subprocess, stop_standing_subprocess

VPN_CONST = cconst.VpnProfile
VPN_TYPE = cconst.VpnProfileType
VPN_PARAMS = cconst.VpnReqParams
TCPDUMP_PATH = "/data/local/tmp/"
USB_CHARGE_MODE = "svc usb setFunctions"
USB_TETHERING_MODE = "svc usb setFunctions rndis"
ENABLE_HARDWARE_OFFLOAD = "settings put global tether_offload_disabled 0"
DISABLE_HARDWARE_OFFLOAD = "settings put global tether_offload_disabled 1"
DEVICE_IP_ADDRESS = "ip address"
LOCALHOST = "192.168.1.1"

# Time to wait for radio to up and running after reboot
WAIT_TIME_AFTER_REBOOT = 10

GCE_SSH = "gcloud compute ssh "
GCE_SCP = "gcloud compute scp "


def start_tcpdump(ad, test_name, interface="any"):
    """Start tcpdump on all interfaces.

    Args:
        ad: android device object.
        test_name: tcpdump file name will have this
    """
    ad.log.info("Starting tcpdump on all interfaces")
    ad.adb.shell("killall -9 tcpdump", ignore_status=True)
    ad.adb.shell(f"mkdir {TCPDUMP_PATH}", ignore_status=True)
    ad.adb.shell(f"rm -rf {TCPDUMP_PATH}/*", ignore_status=True)

    file_name = f"{TCPDUMP_PATH}/tcpdump_{ad.serial}_{test_name}.pcap"
    ad.log.info("tcpdump file is %s", file_name)
    cmd = f"adb -s {ad.serial} shell tcpdump -i {interface} -s0 -w {file_name}"
    try:
        return start_standing_subprocess(cmd, 5)
    except Exception:
        ad.log.exception(f"Could not start standing process {repr(cmd)}")

    return None


def stop_tcpdump(
    ad,
    proc,
    test_name,
    pull_dump=True,
    adb_pull_timeout=adb.DEFAULT_ADB_PULL_TIMEOUT,
):
    """Stops tcpdump on any iface.

       Pulls the tcpdump file in the tcpdump dir if necessary.

    Args:
        ad: android device object.
        proc: need to know which pid to stop
        test_name: test name to save the tcpdump file
        pull_dump: pull tcpdump file or not
        adb_pull_timeout: timeout for adb_pull

    Returns:
      log_path of the tcpdump file
    """
    ad.log.info("Stopping and pulling tcpdump if any")
    if proc is None:
        return None
    try:
        stop_standing_subprocess(proc)
    except Exception as e:
        ad.log.warning(e)
    if pull_dump:
        log_path = os.path.join(ad.device_log_path, f"TCPDUMP_{ad.serial}")
        os.makedirs(log_path, exist_ok=True)
        ad.adb.pull(f"{TCPDUMP_PATH}/. {log_path}", timeout=adb_pull_timeout)
        ad.adb.shell(f"rm -rf {TCPDUMP_PATH}/*", ignore_status=True)
        file_name = f"tcpdump_{ad.serial}_{test_name}.pcap"
        return f"{log_path}/{file_name}"
    return None
