#!/usr/bin/env python3
#
# Copyright 2025 The Fuchsia Authors
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# Blanket ignores to enable mypy in Antlion
# mypy: disable-error-code="no-untyped-def"
import logging
import os
import shutil
import time
from enum import IntEnum
from queue import Empty

from antlion import context, utils
from antlion.controllers.ap_lib.hostapd_constants import BAND_2G, BAND_5G
from antlion.test_utils.wifi import wifi_constants
from mobly import asserts, signals

# Default timeout used for reboot, toggle WiFi and Airplane mode,
# for the system to settle down after the operation.
DEFAULT_TIMEOUT = 10
# Number of seconds to wait for events that are supposed to happen quickly.
# Like onSuccess for start background scan and confirmation on wifi state
# change.
SHORT_TIMEOUT = 30
ROAMING_TIMEOUT = 30
WIFI_CONNECTION_TIMEOUT_DEFAULT = 30
DEFAULT_SCAN_TRIES = 3
DEFAULT_CONNECT_TRIES = 3
# Speed of light in m/s.
SPEED_OF_LIGHT = 299792458

DEFAULT_PING_ADDR = "https://www.google.com/robots.txt"

CNSS_DIAG_CONFIG_PATH = "/data/vendor/wifi/cnss_diag/"
CNSS_DIAG_CONFIG_FILE = "cnss_diag.conf"

ROAMING_ATTN = {
    "AP1_on_AP2_off": [0, 0, 95, 95],
    "AP1_off_AP2_on": [95, 95, 0, 0],
    "default": [0, 0, 0, 0],
}


class WifiEnums:
    SSID_KEY = "SSID"  # Used for Wifi & SoftAp
    SSID_PATTERN_KEY = "ssidPattern"
    NETID_KEY = "network_id"
    BSSID_KEY = "BSSID"  # Used for Wifi & SoftAp
    BSSID_PATTERN_KEY = "bssidPattern"
    PWD_KEY = "password"  # Used for Wifi & SoftAp
    frequency_key = "frequency"
    HIDDEN_KEY = "hiddenSSID"  # Used for Wifi & SoftAp
    IS_APP_INTERACTION_REQUIRED = "isAppInteractionRequired"
    IS_USER_INTERACTION_REQUIRED = "isUserInteractionRequired"
    IS_SUGGESTION_METERED = "isMetered"
    PRIORITY = "priority"
    SECURITY = "security"  # Used for Wifi & SoftAp

    # Used for SoftAp
    AP_BAND_KEY = "apBand"
    AP_CHANNEL_KEY = "apChannel"
    AP_BANDS_KEY = "apBands"
    AP_CHANNEL_FREQUENCYS_KEY = "apChannelFrequencies"
    AP_MAC_RANDOMIZATION_SETTING_KEY = "MacRandomizationSetting"
    AP_BRIDGED_OPPORTUNISTIC_SHUTDOWN_ENABLE_KEY = (
        "BridgedModeOpportunisticShutdownEnabled"
    )
    AP_IEEE80211AX_ENABLED_KEY = "Ieee80211axEnabled"
    AP_MAXCLIENTS_KEY = "MaxNumberOfClients"
    AP_SHUTDOWNTIMEOUT_KEY = "ShutdownTimeoutMillis"
    AP_SHUTDOWNTIMEOUTENABLE_KEY = "AutoShutdownEnabled"
    AP_CLIENTCONTROL_KEY = "ClientControlByUserEnabled"
    AP_ALLOWEDLIST_KEY = "AllowedClientList"
    AP_BLOCKEDLIST_KEY = "BlockedClientList"

    WIFI_CONFIG_SOFTAP_BAND_2G = 1
    WIFI_CONFIG_SOFTAP_BAND_5G = 2
    WIFI_CONFIG_SOFTAP_BAND_2G_5G = 3
    WIFI_CONFIG_SOFTAP_BAND_6G = 4
    WIFI_CONFIG_SOFTAP_BAND_2G_6G = 5
    WIFI_CONFIG_SOFTAP_BAND_5G_6G = 6
    WIFI_CONFIG_SOFTAP_BAND_ANY = 7

    # DO NOT USE IT for new test case! Replaced by WIFI_CONFIG_SOFTAP_BAND_
    WIFI_CONFIG_APBAND_2G = WIFI_CONFIG_SOFTAP_BAND_2G
    WIFI_CONFIG_APBAND_5G = WIFI_CONFIG_SOFTAP_BAND_5G
    WIFI_CONFIG_APBAND_AUTO = WIFI_CONFIG_SOFTAP_BAND_2G_5G

    WIFI_CONFIG_APBAND_2G_OLD = 0
    WIFI_CONFIG_APBAND_5G_OLD = 1
    WIFI_CONFIG_APBAND_AUTO_OLD = -1

    WIFI_WPS_INFO_PBC = 0
    WIFI_WPS_INFO_DISPLAY = 1
    WIFI_WPS_INFO_KEYPAD = 2
    WIFI_WPS_INFO_LABEL = 3
    WIFI_WPS_INFO_INVALID = 4

    class CountryCode:
        AUSTRALIA = "AU"
        CHINA = "CN"
        GERMANY = "DE"
        JAPAN = "JP"
        UK = "GB"
        US = "US"
        UNKNOWN = "UNKNOWN"

    # Start of Macros for EAP
    # EAP types
    class Eap(IntEnum):
        NONE = -1
        PEAP = 0
        TLS = 1
        TTLS = 2
        PWD = 3
        SIM = 4
        AKA = 5
        AKA_PRIME = 6
        UNAUTH_TLS = 7

    # EAP Phase2 types
    class EapPhase2(IntEnum):
        NONE = 0
        PAP = 1
        MSCHAP = 2
        MSCHAPV2 = 3
        GTC = 4

    class Enterprise:
        # Enterprise Config Macros
        EMPTY_VALUE = "NULL"
        EAP = "eap"
        PHASE2 = "phase2"
        IDENTITY = "identity"
        ANON_IDENTITY = "anonymous_identity"
        PASSWORD = "password"
        SUBJECT_MATCH = "subject_match"
        ALTSUBJECT_MATCH = "altsubject_match"
        DOM_SUFFIX_MATCH = "domain_suffix_match"
        CLIENT_CERT = "client_cert"
        CA_CERT = "ca_cert"
        ENGINE = "engine"
        ENGINE_ID = "engine_id"
        PRIVATE_KEY_ID = "key_id"
        REALM = "realm"
        PLMN = "plmn"
        FQDN = "FQDN"
        FRIENDLY_NAME = "providerFriendlyName"
        ROAMING_IDS = "roamingConsortiumIds"
        OCSP = "ocsp"

    # End of Macros for EAP

    # Macros as specified in the WifiScanner code.
    WIFI_BAND_UNSPECIFIED = 0  # not specified
    WIFI_BAND_24_GHZ = 1  # 2.4 GHz band
    WIFI_BAND_5_GHZ = 2  # 5 GHz band without DFS channels
    WIFI_BAND_5_GHZ_DFS_ONLY = 4  # 5 GHz band with DFS channels
    WIFI_BAND_5_GHZ_WITH_DFS = 6  # 5 GHz band with DFS channels
    WIFI_BAND_BOTH = 3  # both bands without DFS channels
    WIFI_BAND_BOTH_WITH_DFS = 7  # both bands with DFS channels

    SCAN_TYPE_LOW_LATENCY = 0
    SCAN_TYPE_LOW_POWER = 1
    SCAN_TYPE_HIGH_ACCURACY = 2

    # US Wifi frequencies
    ALL_2G_FREQUENCIES = [
        2412,
        2417,
        2422,
        2427,
        2432,
        2437,
        2442,
        2447,
        2452,
        2457,
        2462,
    ]
    DFS_5G_FREQUENCIES = [
        5260,
        5280,
        5300,
        5320,
        5500,
        5520,
        5540,
        5560,
        5580,
        5600,
        5620,
        5640,
        5660,
        5680,
        5700,
        5720,
    ]
    NONE_DFS_5G_FREQUENCIES = [
        5180,
        5200,
        5220,
        5240,
        5745,
        5765,
        5785,
        5805,
        5825,
    ]
    ALL_5G_FREQUENCIES = DFS_5G_FREQUENCIES + NONE_DFS_5G_FREQUENCIES

    band_to_frequencies = {
        WIFI_BAND_24_GHZ: ALL_2G_FREQUENCIES,
        WIFI_BAND_5_GHZ: NONE_DFS_5G_FREQUENCIES,
        WIFI_BAND_5_GHZ_DFS_ONLY: DFS_5G_FREQUENCIES,
        WIFI_BAND_5_GHZ_WITH_DFS: ALL_5G_FREQUENCIES,
        WIFI_BAND_BOTH: ALL_2G_FREQUENCIES + NONE_DFS_5G_FREQUENCIES,
        WIFI_BAND_BOTH_WITH_DFS: ALL_5G_FREQUENCIES + ALL_2G_FREQUENCIES,
    }

    # TODO: add all of the band mapping.
    softap_band_frequencies = {
        WIFI_CONFIG_SOFTAP_BAND_2G: ALL_2G_FREQUENCIES,
        WIFI_CONFIG_SOFTAP_BAND_5G: ALL_5G_FREQUENCIES,
    }

    # All Wifi frequencies to channels lookup.
    freq_to_channel = {
        2412: 1,
        2417: 2,
        2422: 3,
        2427: 4,
        2432: 5,
        2437: 6,
        2442: 7,
        2447: 8,
        2452: 9,
        2457: 10,
        2462: 11,
        2467: 12,
        2472: 13,
        2484: 14,
        4915: 183,
        4920: 184,
        4925: 185,
        4935: 187,
        4940: 188,
        4945: 189,
        4960: 192,
        4980: 196,
        5035: 7,
        5040: 8,
        5045: 9,
        5055: 11,
        5060: 12,
        5080: 16,
        5170: 34,
        5180: 36,
        5190: 38,
        5200: 40,
        5210: 42,
        5220: 44,
        5230: 46,
        5240: 48,
        5260: 52,
        5280: 56,
        5300: 60,
        5320: 64,
        5500: 100,
        5520: 104,
        5540: 108,
        5560: 112,
        5580: 116,
        5600: 120,
        5620: 124,
        5640: 128,
        5660: 132,
        5680: 136,
        5700: 140,
        5745: 149,
        5765: 153,
        5785: 157,
        5795: 159,
        5805: 161,
        5825: 165,
    }

    # All Wifi channels to frequencies lookup.
    channel_2G_to_freq = {
        1: 2412,
        2: 2417,
        3: 2422,
        4: 2427,
        5: 2432,
        6: 2437,
        7: 2442,
        8: 2447,
        9: 2452,
        10: 2457,
        11: 2462,
        12: 2467,
        13: 2472,
        14: 2484,
    }

    channel_5G_to_freq = {
        183: 4915,
        184: 4920,
        185: 4925,
        187: 4935,
        188: 4940,
        189: 4945,
        192: 4960,
        196: 4980,
        7: 5035,
        8: 5040,
        9: 5045,
        11: 5055,
        12: 5060,
        16: 5080,
        34: 5170,
        36: 5180,
        38: 5190,
        40: 5200,
        42: 5210,
        44: 5220,
        46: 5230,
        48: 5240,
        50: 5250,
        52: 5260,
        56: 5280,
        60: 5300,
        64: 5320,
        100: 5500,
        104: 5520,
        108: 5540,
        112: 5560,
        116: 5580,
        120: 5600,
        124: 5620,
        128: 5640,
        132: 5660,
        136: 5680,
        140: 5700,
        149: 5745,
        151: 5755,
        153: 5765,
        155: 5775,
        157: 5785,
        159: 5795,
        161: 5805,
        165: 5825,
    }

    channel_6G_to_freq = {4 * x + 1: 5955 + 20 * x for x in range(59)}

    channel_to_freq = {
        "2G": channel_2G_to_freq,
        "5G": channel_5G_to_freq,
        "6G": channel_6G_to_freq,
    }


def _assert_on_fail_handler(func, assert_on_fail, *args, **kwargs):
    """Wrapper function that handles the bahevior of assert_on_fail.

    When assert_on_fail is True, let all test signals through, which can
    terminate test cases directly. When assert_on_fail is False, the wrapper
    raises no test signals and reports operation status by returning True or
    False.

    Args:
        func: The function to wrap. This function reports operation status by
              raising test signals.
        assert_on_fail: A boolean that specifies if the output of the wrapper
                        is test signal based or return value based.
        args: Positional args for func.
        kwargs: Name args for func.

    Returns:
        If assert_on_fail is True, returns True/False to signal operation
        status, otherwise return nothing.
    """
    try:
        func(*args, **kwargs)
        if not assert_on_fail:
            return True
    except signals.TestSignal:
        if assert_on_fail:
            raise
        return False


def match_networks(target_params, networks):
    """Finds the WiFi networks that match a given set of parameters in a list
    of WiFi networks.

    To be considered a match, the network should contain every key-value pair
    of target_params

    Args:
        target_params: A dict with 1 or more key-value pairs representing a Wi-Fi network.
                       E.g { 'SSID': 'wh_ap1_5g', 'BSSID': '30:b5:c2:33:e4:47' }
        networks: A list of dict objects representing WiFi networks.

    Returns:
        The networks that match the target parameters.
    """
    results = []
    asserts.assert_true(
        target_params, "Expected networks object 'target_params' is empty"
    )
    for n in networks:
        add_network = 1
        for k, v in target_params.items():
            if k not in n:
                add_network = 0
                break
            if n[k] != v:
                add_network = 0
                break
        if add_network:
            results.append(n)
    return results


def wifi_toggle_state(ad, new_state=None, assert_on_fail=True):
    """Toggles the state of wifi.

    Args:
        ad: An AndroidDevice object.
        new_state: Wifi state to set to. If None, opposite of the current state.
        assert_on_fail: If True, error checks in this function will raise test
                        failure signals.

    Returns:
        If assert_on_fail is False, function returns True if the toggle was
        successful, False otherwise. If assert_on_fail is True, no return value.
    """
    return _assert_on_fail_handler(
        _wifi_toggle_state, assert_on_fail, ad, new_state=new_state
    )


def _wifi_toggle_state(ad, new_state=None):
    """Toggles the state of wifi.

    TestFailure signals are raised when something goes wrong.

    Args:
        ad: An AndroidDevice object.
        new_state: The state to set Wi-Fi to. If None, opposite of the current
                   state will be set.
    """
    if new_state is None:
        new_state = not ad.droid.wifiCheckState()
    elif new_state == ad.droid.wifiCheckState():
        # Check if the new_state is already achieved, so we don't wait for the
        # state change event by mistake.
        return
    ad.droid.wifiStartTrackingStateChange()
    ad.log.info("Setting Wi-Fi state to %s.", new_state)
    ad.ed.clear_all_events()
    # Setting wifi state.
    ad.droid.wifiToggleState(new_state)
    time.sleep(2)
    fail_msg = f"Failed to set Wi-Fi state to {new_state} on {ad.serial}."
    try:
        ad.ed.wait_for_event(
            wifi_constants.WIFI_STATE_CHANGED,
            lambda x: x["data"]["enabled"] == new_state,
            SHORT_TIMEOUT,
        )
    except Empty:
        asserts.assert_equal(new_state, ad.droid.wifiCheckState(), fail_msg)
    finally:
        ad.droid.wifiStopTrackingStateChange()


def reset_wifi(ad):
    """Clears all saved Wi-Fi networks on a device.

    This will turn Wi-Fi on.

    Args:
        ad: An AndroidDevice object.

    """
    networks = ad.droid.wifiGetConfiguredNetworks()
    if not networks:
        return
    removed = []
    for n in networks:
        if n["networkId"] not in removed:
            ad.droid.wifiForgetNetwork(n["networkId"])
            removed.append(n["networkId"])
        else:
            continue
        try:
            event = ad.ed.pop_event(
                wifi_constants.WIFI_FORGET_NW_SUCCESS, SHORT_TIMEOUT
            )
        except Empty:
            logging.warning("Could not confirm the removal of network %s.", n)
    # Check again to see if there's any network left.
    asserts.assert_true(
        not ad.droid.wifiGetConfiguredNetworks(),
        f"Failed to remove these configured Wi-Fi networks: {networks}",
    )


def wifi_test_device_init(ad, country_code=WifiEnums.CountryCode.US):
    """Initializes an android device for wifi testing.

    0. Make sure SL4A connection is established on the android device.
    1. Disable location service's WiFi scan.
    2. Turn WiFi on.
    3. Clear all saved networks.
    4. Set country code to US.
    5. Enable WiFi verbose logging.
    6. Sync device time with computer time.
    7. Turn off cellular data.
    8. Turn off ambient display.
    """
    utils.require_sl4a([ad])
    ad.droid.wifiScannerToggleAlwaysAvailable(False)
    msg = "Failed to turn off location service's scan."
    asserts.assert_true(not ad.droid.wifiScannerIsAlwaysAvailable(), msg)
    wifi_toggle_state(ad, True)
    reset_wifi(ad)
    ad.droid.wifiEnableVerboseLogging(1)
    msg = "Failed to enable WiFi verbose logging."
    asserts.assert_equal(ad.droid.wifiGetVerboseLoggingLevel(), 1, msg)
    # We don't verify the following settings since they are not critical.
    # Set wpa_supplicant log level to EXCESSIVE.
    output = ad.adb.shell(
        "wpa_cli -i wlan0 -p -g@android:wpa_wlan0 IFNAME="
        "wlan0 log_level EXCESSIVE",
        ignore_status=True,
    )
    ad.log.info("wpa_supplicant log change status: %s", output)
    utils.sync_device_time(ad)
    ad.droid.telephonyToggleDataConnection(False)
    set_wifi_country_code(ad, country_code)
    utils.set_ambient_display(ad, False)


def set_wifi_country_code(ad, country_code):
    """Sets the wifi country code on the device.

    Args:
        ad: An AndroidDevice object.
        country_code: 2 letter ISO country code

    Raises:
        An RpcException if unable to set the country code.
    """
    try:
        ad.adb.shell(f"cmd wifi force-country-code enabled {country_code}")
    except Exception as e:
        ad.log.warn(
            f"Failed to set country code to {country_code}; defaulting to US. Error: {e}"
        )
        ad.droid.wifiSetCountryCode(WifiEnums.CountryCode.US)


def start_wifi_connection_scan_and_return_status(ad):
    """
    Starts a wifi connection scan and wait for results to become available
    or a scan failure to be reported.

    Args:
        ad: An AndroidDevice object.
    Returns:
        True: if scan succeeded & results are available
        False: if scan failed
    """
    ad.ed.clear_all_events()
    ad.droid.wifiStartScan()
    try:
        events = ad.ed.pop_events(
            "WifiManagerScan(ResultsAvailable|Failure)", 60
        )
    except Empty:
        asserts.fail(
            "Wi-Fi scan results/failure did not become available within 60s."
        )
    # If there are multiple matches, we check for atleast one success.
    for event in events:
        if event["name"] == "WifiManagerScanResultsAvailable":
            return True
        elif event["name"] == "WifiManagerScanFailure":
            ad.log.debug("Scan failure received")
    return False


def start_wifi_connection_scan_and_check_for_network(
    ad, network_ssid, max_tries=3
):
    """
    Start connectivity scans & checks if the |network_ssid| is seen in
    scan results. The method performs a max of |max_tries| connectivity scans
    to find the network.

    Args:
        ad: An AndroidDevice object.
        network_ssid: SSID of the network we are looking for.
        max_tries: Number of scans to try.
    Returns:
        True: if network_ssid is found in scan results.
        False: if network_ssid is not found in scan results.
    """
    start_time = time.time()
    for num_tries in range(max_tries):
        if start_wifi_connection_scan_and_return_status(ad):
            scan_results = ad.droid.wifiGetScanResults()
            match_results = match_networks(
                {WifiEnums.SSID_KEY: network_ssid}, scan_results
            )
            if len(match_results) > 0:
                ad.log.debug(
                    f"Found network in {time.time() - start_time} seconds."
                )
                return True
    ad.log.debug(f"Did not find network in {time.time() - start_time} seconds.")
    return False


def start_wifi_connection_scan_and_ensure_network_found(
    ad, network_ssid, max_tries=3
):
    """
    Start connectivity scans & ensure the |network_ssid| is seen in
    scan results. The method performs a max of |max_tries| connectivity scans
    to find the network.
    This method asserts on failure!

    Args:
        ad: An AndroidDevice object.
        network_ssid: SSID of the network we are looking for.
        max_tries: Number of scans to try.
    """
    ad.log.info("Starting scans to ensure %s is present", network_ssid)
    assert_msg = f"Failed to find {network_ssid} in scan results after {str(max_tries)} tries"
    asserts.assert_true(
        start_wifi_connection_scan_and_check_for_network(
            ad, network_ssid, max_tries
        ),
        assert_msg,
    )


def start_wifi_connection_scan_and_ensure_network_not_found(
    ad, network_ssid, max_tries=3
):
    """
    Start connectivity scans & ensure the |network_ssid| is not seen in
    scan results. The method performs a max of |max_tries| connectivity scans
    to find the network.
    This method asserts on failure!

    Args:
        ad: An AndroidDevice object.
        network_ssid: SSID of the network we are looking for.
        max_tries: Number of scans to try.
    """
    ad.log.info("Starting scans to ensure %s is not present", network_ssid)
    assert_msg = (
        f"Found {network_ssid} in scan results after {str(max_tries)} tries"
    )
    asserts.assert_false(
        start_wifi_connection_scan_and_check_for_network(
            ad, network_ssid, max_tries
        ),
        assert_msg,
    )


def _wait_for_connect_event(ad, ssid=None, id=None, tries=1):
    """Wait for a connect event on queue and pop when available.

    Args:
        ad: An Android device object.
        ssid: SSID of the network to connect to.
        id: Network Id of the network to connect to.
        tries: An integer that is the number of times to try before failing.

    Returns:
        A dict with details of the connection data, which looks like this:
        {
         'time': 1485460337798,
         'name': 'WifiNetworkConnected',
         'data': {
                  'rssi': -27,
                  'is_24ghz': True,
                  'mac_address': '02:00:00:00:00:00',
                  'network_id': 1,
                  'BSSID': '30:b5:c2:33:d3:fc',
                  'ip_address': 117483712,
                  'link_speed': 54,
                  'supplicant_state': 'completed',
                  'hidden_ssid': False,
                  'SSID': 'wh_ap1_2g',
                  'is_5ghz': False}
        }

    """
    conn_result = None

    # If ssid and network id is None, just wait for any connect event.
    if id is None and ssid is None:
        for i in range(tries):
            try:
                conn_result = ad.ed.pop_event(wifi_constants.WIFI_CONNECTED, 30)
                break
            except Empty:
                pass
    else:
        # If ssid or network id is specified, wait for specific connect event.
        for i in range(tries):
            try:
                conn_result = ad.ed.pop_event(wifi_constants.WIFI_CONNECTED, 30)
                if id and conn_result["data"][WifiEnums.NETID_KEY] == id:
                    break
                elif ssid and conn_result["data"][WifiEnums.SSID_KEY] == ssid:
                    break
            except Empty:
                pass

    return conn_result


def connect_to_wifi_network(
    ad,
    network,
    assert_on_fail=True,
    check_connectivity=True,
    hidden=False,
    num_of_scan_tries=DEFAULT_SCAN_TRIES,
    num_of_connect_tries=DEFAULT_CONNECT_TRIES,
):
    """Connection logic for open and psk wifi networks.

    Args:
        ad: AndroidDevice to use for connection
        network: network info of the network to connect to
        assert_on_fail: If true, errors from wifi_connect will raise
                        test failure signals.
        hidden: Is the Wifi network hidden.
        num_of_scan_tries: The number of times to try scan
                           interface before declaring failure.
        num_of_connect_tries: The number of times to try
                              connect wifi before declaring failure.
    """
    if hidden:
        start_wifi_connection_scan_and_ensure_network_not_found(
            ad, network[WifiEnums.SSID_KEY], max_tries=num_of_scan_tries
        )
    else:
        start_wifi_connection_scan_and_ensure_network_found(
            ad, network[WifiEnums.SSID_KEY], max_tries=num_of_scan_tries
        )
    wifi_connect(
        ad,
        network,
        num_of_tries=num_of_connect_tries,
        assert_on_fail=assert_on_fail,
        check_connectivity=check_connectivity,
    )


def wifi_connect(
    ad, network, num_of_tries=1, assert_on_fail=True, check_connectivity=True
):
    """Connect an Android device to a wifi network.

    Initiate connection to a wifi network, wait for the "connected" event, then
    confirm the connected ssid is the one requested.

    This will directly fail a test if anything goes wrong.

    Args:
        ad: android_device object to initiate connection on.
        network: A dictionary representing the network to connect to. The
                 dictionary must have the key "SSID".
        num_of_tries: An integer that is the number of times to try before
                      delaring failure. Default is 1.
        assert_on_fail: If True, error checks in this function will raise test
                        failure signals.

    Returns:
        Returns a value only if assert_on_fail is false.
        Returns True if the connection was successful, False otherwise.
    """
    return _assert_on_fail_handler(
        _wifi_connect,
        assert_on_fail,
        ad,
        network,
        num_of_tries=num_of_tries,
        check_connectivity=check_connectivity,
    )


def _wifi_connect(ad, network, num_of_tries=1, check_connectivity=True):
    """Connect an Android device to a wifi network.

    Initiate connection to a wifi network, wait for the "connected" event, then
    confirm the connected ssid is the one requested.

    This will directly fail a test if anything goes wrong.

    Args:
        ad: android_device object to initiate connection on.
        network: A dictionary representing the network to connect to. The
                 dictionary must have the key "SSID".
        num_of_tries: An integer that is the number of times to try before
                      delaring failure. Default is 1.
    """
    asserts.assert_true(
        WifiEnums.SSID_KEY in network,
        f"Key '{WifiEnums.SSID_KEY}' must be present in network definition.",
    )
    ad.droid.wifiStartTrackingStateChange()
    expected_ssid = network[WifiEnums.SSID_KEY]
    ad.droid.wifiConnectByConfig(network)
    ad.log.info("Starting connection process to %s", expected_ssid)
    try:
        ad.ed.pop_event(wifi_constants.CONNECT_BY_CONFIG_SUCCESS, 30)
        connect_result = _wait_for_connect_event(
            ad, ssid=expected_ssid, tries=num_of_tries
        )
        asserts.assert_true(
            connect_result,
            f"Failed to connect to Wi-Fi network {network} on {ad.serial}",
        )
        ad.log.debug("Wi-Fi connection result: %s.", connect_result)
        actual_ssid = connect_result["data"][WifiEnums.SSID_KEY]
        asserts.assert_equal(
            actual_ssid,
            expected_ssid,
            f"Connected to the wrong network on {ad.serial}.",
        )
        ad.log.info("Connected to Wi-Fi network %s.", actual_ssid)

        if check_connectivity:
            internet = validate_connection(ad, DEFAULT_PING_ADDR)
            if not internet:
                raise signals.TestFailure(
                    f"Failed to connect to internet on {expected_ssid}"
                )
    except Empty:
        asserts.fail(
            f"Failed to start connection process to {network} on {ad.serial}"
        )
    except Exception as error:
        ad.log.error(
            "Failed to connect to %s with error %s", expected_ssid, error
        )
        raise signals.TestFailure(f"Failed to connect to {network} network")

    finally:
        ad.droid.wifiStopTrackingStateChange()


def validate_connection(
    ad, ping_addr=DEFAULT_PING_ADDR, wait_time=15, ping_gateway=True
):
    """Validate internet connection by pinging the address provided.

    Args:
        ad: android_device object.
        ping_addr: address on internet for pinging.
        wait_time: wait for some time before validating connection

    Returns:
        ping output if successful, NULL otherwise.
    """
    android_version = int(
        ad.adb.shell("getprop ro.vendor.build.version.release")
    )
    # wait_time to allow for DHCP to complete.
    for i in range(wait_time):
        if ad.droid.connectivityNetworkIsConnected():
            if (
                android_version > 10
                and ad.droid.connectivityGetIPv4DefaultGateway()
            ) or android_version < 11:
                break
        time.sleep(1)
    ping = False
    try:
        ping = ad.droid.httpPing(ping_addr)
        ad.log.info("Http ping result: %s.", ping)
    except:
        pass
    if android_version > 10 and not ping and ping_gateway:
        ad.log.info("Http ping failed. Pinging default gateway")
        gw = ad.droid.connectivityGetIPv4DefaultGateway()
        result = ad.adb.shell(f"ping -c 6 {gw}")
        ad.log.info(f"Default gateway ping result: {result}")
        ping = False if "100% packet loss" in result else True
    return ping


# TODO(angli): This can only verify if an actual value is exactly the same.
# Would be nice to be able to verify an actual value is one of serveral.
def verify_wifi_connection_info(ad, expected_con):
    """Verifies that the information of the currently connected wifi network is
    as expected.

    Args:
        expected_con: A dict representing expected key-value pairs for wifi
            connection. e.g. {"SSID": "test_wifi"}
    """
    current_con = ad.droid.wifiGetConnectionInfo()
    case_insensitive = ["BSSID", "supplicant_state"]
    ad.log.debug("Current connection: %s", current_con)
    for k, expected_v in expected_con.items():
        # Do not verify authentication related fields.
        if k == "password":
            continue
        msg = f"Field {k} does not exist in wifi connection info {current_con}."
        if k not in current_con:
            raise signals.TestFailure(msg)
        actual_v = current_con[k]
        if k in case_insensitive:
            actual_v = actual_v.lower()
            expected_v = expected_v.lower()
        msg = f"Expected {k} to be {expected_v}, actual {k} is {actual_v}."
        if actual_v != expected_v:
            raise signals.TestFailure(msg)


def get_current_softap_capability(ad, callbackId, need_to_wait):
    """pop up all of softap info list changed event from queue.
    Args:
        callbackId: Id of the callback associated with registering.
        need_to_wait: Wait for the info callback event before pop all.
    Returns:
        Returns last updated capability of softap.
    """
    eventStr = (
        wifi_constants.SOFTAP_CALLBACK_EVENT
        + str(callbackId)
        + wifi_constants.SOFTAP_CAPABILITY_CHANGED
    )
    ad.log.debug("softap capability dump from eventStr %s", eventStr)
    if need_to_wait:
        event = ad.ed.pop_event(eventStr, SHORT_TIMEOUT)
        capability = event["data"]

    events = ad.ed.pop_all(eventStr)
    for event in events:
        capability = event["data"]

    return capability


def get_ssrdumps(ad):
    """Pulls dumps in the ssrdump dir
    Args:
        ad: android device object.
    """
    logs = ad.get_file_names("/data/vendor/ssrdump/")
    if logs:
        ad.log.info("Pulling ssrdumps %s", logs)
        log_path = os.path.join(ad.device_log_path, f"SSRDUMPS_{ad.serial}")
        os.makedirs(log_path, exist_ok=True)
        ad.pull_files(logs, log_path)
    ad.adb.shell(
        "find /data/vendor/ssrdump/ -type f -delete", ignore_status=True
    )


def start_pcap(pcap, wifi_band, test_name):
    """Start packet capture in monitor mode.

    Args:
        pcap: packet capture object
        wifi_band: '2g' or '5g' or 'dual'
        test_name: test name to be used for pcap file name

    Returns:
        Dictionary with wifi band as key and the tuple
        (pcap Process object, log directory) as the value
    """
    log_dir = os.path.join(
        context.get_current_context().get_full_output_path(), "PacketCapture"
    )
    os.makedirs(log_dir, exist_ok=True)
    if wifi_band == "dual":
        bands = [BAND_2G, BAND_5G]
    else:
        bands = [wifi_band]
    procs = {}
    for band in bands:
        proc = pcap.start_packet_capture(band, log_dir, test_name)
        procs[band] = (proc, os.path.join(log_dir, test_name))
    return procs


def stop_pcap(pcap, procs, test_status=None):
    """Stop packet capture in monitor mode.

    Since, the pcap logs in monitor mode can be very large, we will
    delete them if they are not required. 'test_status' if True, will delete
    the pcap files. If False, we will keep them.

    Args:
        pcap: packet capture object
        procs: dictionary returned by start_pcap
        test_status: status of the test case
    """
    for proc, fname in procs.values():
        pcap.stop_packet_capture(proc)

    if test_status:
        shutil.rmtree(os.path.dirname(fname))


def start_cnss_diags(ads, cnss_diag_file, pixel_models):
    for ad in ads:
        start_cnss_diag(ad, cnss_diag_file, pixel_models)


def start_cnss_diag(ad, cnss_diag_file, pixel_models):
    """Start cnss_diag to record extra wifi logs

    Args:
        ad: android device object.
        cnss_diag_file: cnss diag config file to push to device.
        pixel_models: pixel devices.
    """
    if ad.model not in pixel_models:
        ad.log.info("Device not supported to collect pixel logger")
        return
    if ad.model in wifi_constants.DEVICES_USING_LEGACY_PROP:
        prop = wifi_constants.LEGACY_CNSS_DIAG_PROP
    else:
        prop = wifi_constants.CNSS_DIAG_PROP
    if ad.adb.getprop(prop) != "true":
        if not int(
            ad.adb.shell(
                f"ls -l {CNSS_DIAG_CONFIG_PATH}{CNSS_DIAG_CONFIG_FILE} | wc -l"
            )
        ):
            ad.adb.push(f"{cnss_diag_file} {CNSS_DIAG_CONFIG_PATH}")
        ad.adb.shell(
            "find /data/vendor/wifi/cnss_diag/wlan_logs/ -type f -delete",
            ignore_status=True,
        )
        ad.adb.shell(f"setprop {prop} true", ignore_status=True)


def stop_cnss_diags(ads, pixel_models):
    for ad in ads:
        stop_cnss_diag(ad, pixel_models)


def stop_cnss_diag(ad, pixel_models):
    """Stops cnss_diag

    Args:
        ad: android device object.
        pixel_models: pixel devices.
    """
    if ad.model not in pixel_models:
        ad.log.info("Device not supported to collect pixel logger")
        return
    if ad.model in wifi_constants.DEVICES_USING_LEGACY_PROP:
        prop = wifi_constants.LEGACY_CNSS_DIAG_PROP
    else:
        prop = wifi_constants.CNSS_DIAG_PROP
    ad.adb.shell(f"setprop {prop} false", ignore_status=True)


def get_cnss_diag_log(ad):
    """Pulls the cnss_diag logs in the wlan_logs dir
    Args:
        ad: android device object.
    """
    logs = ad.get_file_names("/data/vendor/wifi/cnss_diag/wlan_logs/")
    if logs:
        ad.log.info("Pulling cnss_diag logs %s", logs)
        log_path = os.path.join(ad.device_log_path, f"CNSS_DIAG_{ad.serial}")
        os.makedirs(log_path, exist_ok=True)
        ad.pull_files(logs, log_path)


def turn_location_off_and_scan_toggle_off(ad):
    """Turns off wifi location scans."""
    utils.set_location_service(ad, False)
    ad.droid.wifiScannerToggleAlwaysAvailable(False)
    msg = "Failed to turn off location service's scan."
    asserts.assert_true(not ad.droid.wifiScannerIsAlwaysAvailable(), msg)
