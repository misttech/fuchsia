---
name: wlan-luci-triage
description: >
  Workflow to query LUCI APIs to triage Fuchsia WLAN E2E test failures, find patterns in recurring flakes, analyze network failure modes, check hardware correlations, and parse syslogs, test logs, and AP logs for triaging issues.
---

# Fuchsia WLAN LUCI Triage

This skill provides a standard workflow tailored for the Fuchsia WLAN team to analyze CI/CQ E2E test failures, identify recurring network flakes, and find hardware/bot correlations using raw PRPC APIs for Buildbucket and ResultDB.

**Note on Usage**: This workflow is meant to be executed against a specific target. The developer invoking the agent MUST provide the specific test case, test suite, or builder they want to investigate in their prompt.

## Prerequisites

This skill requires `depot_tools` to be installed and available in your `PATH`. Specifically, the workflow relies on:
*   `prpc`: To query Buildbucket.
*   `rdb`: To query ResultDB.

*Note for Agents: If these tools are not present in your default `PATH`, you may need to clone `depot_tools` or locate them in the environment before proceeding.*

If these tools are not present, the automation script will fail and direct you to install them. You can install `depot_tools` by following the instructions at [go/depottools#_setting_up](http://go/depottools#_setting_up).

## Workflow

### Step 1: Query Buildbucket for Failing WLAN Builds
To get a list of builds for a specific builder and date range, use the `prpc` CLI to query `buildbucket.v2.Builds.SearchBuilds`. Note that builders can be in different buckets like `global.ci` or `smart.ci`, so verify which bucket your target builder belongs to.

*   **Command**: `prpc call cr-buildbucket.appspot.com buildbucket.v2.Builds.SearchBuilds`
*   **Input JSON Payload Example**:
    ```json
    {
        "predicate": {
            "builder": {
                "project": "turquoise",
                "bucket": "global.ci",
                "builder": "fuchsia_internal.arm64-release-fyi"
            }
        },
        "pageSize": 50
    }
    ```

### Step 2: Query ResultDB for WLAN Test Results & Bot IDs
Find specific WLAN tests that failed within those builds using the `rdb query` command.

**ResultDB Filtering Logic**:
* **Builder**: If the user provides a builder, restrict your search to that builder.
* **Test Suite**: If the user provides a test suite name, you may be looking across multiple builders (unless a builder is also specified).
* **Regex**: If the user provides a regex, apply that alongside whatever builder and test suite name was supplied.
* **Fallback**: Otherwise, assume we want all test variants in the supplied builder.

Using `rdb query` handles pagination automatically and provides a simpler interface than raw PRPC.
To get tags (which contain the `swarming_bot_id`) and all test runs (which is required to calculate accurate pass/fail rates per bot), you should omit the `-u` flag so it returns all executions, both expected and unexpected:

*   **Command**: `rdb query -json -tr-fields testId,status,name,failureReason,summaryHtml,tags "build-<BUILD_ID>"`

*(If you only need to investigate specific failures without caring about total test volumes, you can append `-u` to filter only unexpected results. You can run `rdb query --help` to learn about other filtering options.)*

### Step 3: Analyze Hardware Correlations
WLAN E2E flakes can sometimes be caused by hardware issues.
1. Iterate through the `tags` array on each `FAIL` test result.
2. Find the object where `key: "swarming_bot_id"`.
3. Tally the failures by `swarming_bot_id`.
4. **Validation (CRITICAL)**: You MUST check both test failures AND total test runs to ensure that an increased number of failures on a specific bot isn't just proportional to an increased number of runs scheduled on that bot. Query ResultDB again using `expectancy: "ALL"` to get the total runs. Compare the pass/fail rate for the suspicious bot against the rest of the fleet. If a single bot has a dramatically lower pass rate (e.g. 70% vs 99% fleet average) or is accounting for 90% of all `Network not found` failures despite normal run volume, it's highly likely to be a hardware or AP broadcast issue.

### Step 4: Fetch & Parse Artifacts
To root cause WLAN test failures:
1. Extract the `name` field from the ResultDB test result.
2. Query `luci.resultdb.v1.ResultDB/QueryArtifacts` passing the `name` as `searchString`.
3. Locate the `fetchUrl` for the relevant artifacts.

**Which Artifacts to Look At:**
Most issues can be triaged using a combination of the test logs and Fuchsia device logs (syslog/snapshots), but AP logs and metadata are sometimes helpful to confirm root causes.

*   **Fuchsia Device Logs**:
    *   `InfraTestbed/.../Snapshot_*.zip`: The primary Fuchsia snapshot. It contains the `syslog`, `inspect` data, traces, and other Fuchsia-specific debugging information. **Always prefer the syslog inside this snapshot over the parent invocation syslogs. When reading the syslog, search for log lines tagged with `lacewing` that contain your test case name to find the exact start and end of the test.**
*   **Test Framework Logs**:
    *   `InfraTestbed/.../test_log.DEBUG` and `test_log.INFO`: Shows the test execution from the test framework's perspective.
    *   `stdout-and-stderr.txt` or `infra_and_test_std_and_klog.txt`: Combined test framework and infra output. Useful for crashes in the test harness itself.
*   **AP Logs (for confirming network rejections/issues)**:
    *   `InfraTestbed/.../ap_dhcp_*.log`: DHCP logs from the Access Point.
    *   `InfraTestbed/.../ap_hostapd_*.log`: hostapd logs from the Access Point. Useful for verifying if the AP actually sent frames.
    *   `InfraTestbed/.../ap_systemd_*.log`: Systemd logs from the AP.
*   **Ancillary Logs**:
    *   `InfraTestbed/.../wifi_log.txt`: Wi-Fi logs retrieved via ADB (only present if the test uses ADB).
    *   `InfraTestbed/.../ffx/ffx.log`: Logs from the `ffx` tool, which are helpful if the test failed due to a host-device communication issue.
    *   `InfraTestbed/.../test_summary.yaml` and `triage_output`: Test metadata.
*   **Parent Invocation Logs**:
    *   You may see `syslog.txt` or `serial_log.txt` at the root of the parent invocation. These logs contain output from your test along with other tests, so they can be noisy. Rely on the snapshot syslogs unless debugging a hardware lockup.


### Step 5: Triage Strategy
When debugging a failure, follow this specific order of operations:

1. **Check Test Logs First:**
   Before diving into device syslogs, ALWAYS start with `test_log.INFO` or `stdout-and-stderr.txt`. Look for the exact assertion that failed and what test case was running.
2. **Consult Test Source Code:**
   Use the local Fuchsia source code or `code_search` to find the E2E test code itself. Understand what the test was trying to do when the assertion failed (e.g. was it waiting for a connection? expecting a disconnect?).
3. **Find the Test Boundaries in Syslog:**
   Open `log.system.txt` from the device snapshot. Search for the exact test case name (e.g., `test_wlan_connection_with_suspend_resume`). The test framework logs the start and end of each test case. **Specifically, look for log lines tagged with `lacewing` that contain the test case name to ensure you are looking at the exact boundaries of the test run.** You MUST restrict your analysis to the logs between these markers to avoid cross-pollution.
4. **Filter by WLAN Tags:**
   Within those boundaries, look for evidence of what occurred on the device. WLAN logs are typically tagged with:
   *   `wlan` or `wlanif` (Generic core WLAN stack)
   *   `wlancfg` or `wlanix` (WLAN policy layer)
   *   `wlansoftmac` (SoftMAC layer)
   *   `brcmfmac` or `iwlwifi` or `synadhd` (Specific driver logs)
   *   `wlan-hw-sim` (Simulated environment)
5. **Consult Fuchsia Source Code:**
   When you find suspicious logs (e.g. `AP Rejection status: 2` or `wlanif: interface destroyed`), use local Fuchsia code or `code_search` to find the relevant code in the Fuchsia platform to understand exactly what triggers that log.

## Helper Scripts
A Python script (`scripts/luci_triage.py`) automates extracting failure distributions by `swarming_bot_id`. It fetches both total runs and failures to accurately compute the failure rate.
Run it as follows (use `--test-pattern ".*"` if you want all variants, and ensure `--bucket` matches your builder):
```bash
python3 src/connectivity/wlan/.agents/skills/wlan-luci-triage/scripts/luci_triage.py --builder fuchsia_internal.arm64-release-fyi --bucket smart.ci --cutoff-date 2026-06-01 --test-pattern ".*"
```

## Example Case Study: Triaging `e2e_connection_test_using_adb`
When the user asks you to triage a test suite like `e2e_connection_test_using_adb`, follow these examples:

**Scenario 1: Hardware Flake Analysis ("Network not found")**
*   **Observation**: The test failed with "Network not found".
*   **Action**: Query ResultDB for all recent failures of this test and parse the `swarming_bot_id` from the `tags`.
*   **Result**: We discovered that out of 40 recent failures, 39 occurred on a single testbed (`fuchsia-meadowpoint-8-4-02`).
*   **Conclusion**: We proved this was a hardware/AP broadcast issue on that specific bot, allowing the lab team to fix the testbed without us chasing ghosts in the driver code.

**Scenario 2: Root Causing AP Rejections on Suspend/Resume**
*   **Observation**: The `with_suspend_resume` variants of the test were occasionally failing because the connection dropped.
*   **Action**: We checked `test_log.INFO` and saw the test failing at an assertion waiting for the connection. We downloaded the `Snapshot_*.zip`, extracted `log.system.txt`, and isolated the logs to the exact test boundaries.
*   **Result**: By filtering for `wlan` tags, we found an `AP Rejection status: 2` followed immediately by `wlanif: interface destroyed`, occurring the exact moment the device logged a `resume` from sleep.
*   **Conclusion**: We proved the test wasn't flaky; the AP was intentionally de-authenticating the device while it slept, requiring the test code to be refactored to expect this rejection and wait longer for a reconnection.
