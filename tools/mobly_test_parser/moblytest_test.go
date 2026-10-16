// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package mobly_test_parser

import (
	"testing"
	"time"

	"github.com/google/go-cmp/cmp"
	"github.com/google/go-cmp/cmp/cmpopts"

	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

func testCaseCmp(t *testing.T, stdout string, want []runtests.TestCaseResult) {
	r := Parse([]byte(stdout))
	if diff := cmp.Diff(want, r, cmpopts.SortSlices(func(a, b runtests.TestCaseResult) bool { return a.DisplayName < b.DisplayName })); diff != "" {
		t.Errorf("Found mismatch in %s (-want +got):\n%s", stdout, diff)
	}
}

func TestParseEmpty(t *testing.T) {
	testCaseCmp(t, "", []runtests.TestCaseResult{
		{
			DisplayName:   "TestparserError",
			FailureReason: runtests.FailureReasonFromMessage("[TestparserError] Missing Mobly summary record - potental infra timeout."),
			SuiteName:     "Synthetic",
			CaseName:      "Synthetic",
			Status:        runtests.TestFailure,
			Format:        "Mobly",
		},
	})
}

func TestParseMoblyTest(t *testing.T) {
	stdout := `
Running [InfraDriver]
======== Mobly config content ========
MoblyParams:
  LogPath: /tmp
TestBeds:
- Controllers:
    FuchsiaDevice:
    - name: fuchsia-emulator
      transport: fuchsia-controller
  Name: InfraTestbed
  TestParams: {}

======================================

[=====MOBLY RESULTS=====]
---
Requested Tests:
- test_goodbye
- test_hello
- test_skipped
- test_error
Type: TestNameList
---
Begin Time: 1668122321142
Details: null
End Time: 1668122321143
Extra Errors: {}
Extras: null
Result: PASS
Retry Parent: null
Signature: test_goodbye-1668122321142
Stacktrace: null
Termination Signal Type: null
Test Class: GreetingsTest
Test Name: test_goodbye
Type: Record
UID: null
---
Begin Time: 1668122321143
Details: Real test failure
End Time: 1668122321149
Extra Errors: {}
Extras: null
Result: FAIL
Retry Parent: null
Signature: test_hello-1668122321143
Stacktrace: null
Termination Signal Type: TestFailure
Test Class: GreetingsTest
Test Name: test_hello
Type: Record
UID: null
---
Details: null
Extra Errors: {}
Extras: null
Result: SKIP
Retry Parent: null
Signature: test_skipped-1668122321143
Stacktrace: null
Termination Signal Type: null
Test Class: GreetingsTest
Test Name: test_skipped
Type: Record
UID: null
---
Begin Time: 1668122321143
Details: 'Some multi-line error:
    line-1,
    line-2'
End Time: 1668122321149
Extra Errors: {}
Extras: null
Result: ERROR
Retry Parent: null
Signature: test_error-1668122321143
Stacktrace: null
Termination Signal Type: FuchsiaDeviceError
Test Class: GreetingsTest
Test Name: test_error
Type: Record
UID: null
---
Error: 0
Executed: 2
Failed: 0
Passed: 2
Requested: 2
Skipped: 0
Type: Summary

`

	want := []runtests.TestCaseResult{
		{
			DisplayName: "GreetingsTest.test_goodbye",
			SuiteName:   "GreetingsTest",
			CaseName:    "test_goodbye",
			Status:      runtests.TestSuccess,
			Duration:    1 * time.Millisecond,
			Format:      "Mobly",
		},
		{
			DisplayName:   "GreetingsTest.test_hello",
			FailureReason: runtests.FailureReasonFromMessage("[TestFailure] Real test failure"),
			SuiteName:     "GreetingsTest",
			CaseName:      "test_hello",
			Status:        runtests.TestFailure,
			Duration:      6 * time.Millisecond,
			Format:        "Mobly",
		},
		{
			DisplayName: "GreetingsTest.test_skipped",
			SuiteName:   "GreetingsTest",
			CaseName:    "test_skipped",
			Status:      runtests.TestSkipped,
			Duration:    0,
			Format:      "Mobly",
		},
		{
			DisplayName:   "GreetingsTest.test_error",
			FailureReason: runtests.FailureReasonFromMessage("[FuchsiaDeviceError] Some multi-line error: line-1, line-2"),
			SuiteName:     "GreetingsTest",
			CaseName:      "test_error",
			Status:        runtests.TestFailure,
			Duration:      6 * time.Millisecond,
			Format:        "Mobly",
		},
	}

	testCaseCmp(t, stdout, want)
}

func TestParseMoblyTestSkipWithTerminationSignal(t *testing.T) {
	stdout := `
Running [InfraDriver]
======== Mobly config content ========
MoblyParams:
  LogPath: /tmp
TestBeds:
- Controllers:
    FuchsiaDevice:
    - name: fuchsia-emulator
      transport: fuchsia-controller
  Name: InfraTestbed
  TestParams: {}

======================================

[=====MOBLY RESULTS=====]
---
Requested Tests:
- test_explicit_skip
- test_aborted
- test_skip_with_sig
Type: TestNameList
...
---
Begin Time: 1668122321140
Details: Explicitly skipped
End Time: 1668122321141
Extra Errors: {}
Extras: null
Parent: null
Result: SKIP
Retry Parent: null
Signature: test_explicit_skip-1668122321140
Stacktrace: null
Termination Signal Type: TestSkip
Test Class: GreetingsTest
Test Name: test_explicit_skip
Type: Record
UID: null
...
---
Begin Time: 1668122321142
Details: Test received a SIGTERM.
End Time: 1668122321145
Extra Errors: {}
Extras: null
Parent: null
Result: FAIL
Retry Parent: null
Signature: test_aborted-1668122321142
Stacktrace: null
Termination Signal Type: TestAbortAll
Test Class: GreetingsTest
Test Name: test_aborted
Type: Record
UID: null
...
---
Begin Time: null
Details: 'All remaining tests aborted due to: Test received a SIGTERM.'
End Time: null
Extra Errors: {}
Extras: null
Parent: null
Result: SKIP
Retry Parent: null
Signature: null
Stacktrace: null
Termination Signal Type: TestAbortAll
Test Class: GreetingsTest
Test Name: test_skip_with_sig
Type: Record
UID: null
...
---
Error: 0
Executed: 1
Failed: 1
Passed: 0
Requested: 3
Skipped: 2
Type: Summary
...
`

	want := []runtests.TestCaseResult{
		{
			DisplayName: "GreetingsTest.test_explicit_skip",
			SuiteName:   "GreetingsTest",
			CaseName:    "test_explicit_skip",
			Status:      runtests.TestSkipped,
			Duration:    1 * time.Millisecond,
			Format:      "Mobly",
		},
		{
			DisplayName:   "GreetingsTest.test_aborted",
			FailureReason: runtests.FailureReasonFromMessage("[TestAbortAll] Test received a SIGTERM."),
			SuiteName:     "GreetingsTest",
			CaseName:      "test_aborted",
			Status:        runtests.TestFailure,
			Duration:      3 * time.Millisecond,
			Format:        "Mobly",
		},
	}

	testCaseCmp(t, stdout, want)
}

func TestParseMoblyTestTimeout(t *testing.T) {
	stdout := `
Running [InfraDriver]
======== Mobly config content ========
MoblyParams:
  LogPath: /tmp
TestBeds:
- Controllers:
    FuchsiaDevice:
    - name: fuchsia-emulator
      transport: fuchsia-controller
  Name: InfraTestbed
  TestParams: {}

======================================

[=====MOBLY RESULTS=====]
---
Requested Tests:
- test_aborted
- test_skip_with_sig
Type: TestNameList
...
---
Begin Time: 1668122321142
Details: Test received a SIGTERM.
End Time: 1668122321145
Extra Errors: {}
Extras: null
Parent: null
Result: FAIL
Retry Parent: null
Signature: test_aborted-1668122321142
Stacktrace: null
Termination Signal Type: TestAbortAll
Test Class: GreetingsTest
Test Name: test_aborted
Type: Record
UID: null
...
---
Begin Time: null
Details: 'All remaining tests aborted due to: Test received a SIGTERM.'
End Time: null
Extra Errors: {}
Extras: null
Parent: null
Result: SKIP
Retry Parent: null
Signature: null
Stacktrace: null
Termination Signal Type: TestAbortAll
Test Class: GreetingsTest
Test Name: test_skip_with_sig
Type: Record
UID: null
...
---
Error: 0
Executed: 1
Failed: 1
Passed: 0
Requested: 2
Skipped: 1
Type: Summary
...
---
Details: Test timed out after 0:04:20.
Type: MoblyDriverTimeout
...
`

	// Note: test_skip_with_sig is omitted because parseMoblyTest filters out
	// SKIP records that have a non-TestSkip Termination Signal Type.
	want := []runtests.TestCaseResult{
		{
			DisplayName:   "GreetingsTest.test_aborted",
			FailureReason: runtests.FailureReasonFromMessage("[MoblyDriverTimeout] GreetingsTest.test_aborted: Test timed out after 0:04:20."),
			SuiteName:     "GreetingsTest",
			CaseName:      "test_aborted",
			Status:        runtests.TestAborted,
			Duration:      3 * time.Millisecond,
			Format:        "Mobly",
		},
	}

	testCaseCmp(t, stdout, want)
}

func TestParseMoblyTestTimeoutWithoutAbortedRecord(t *testing.T) {
	stdout := `
Running [InfraDriver]
======== Mobly config content ========
MoblyParams:
  LogPath: /tmp
TestBeds:
- Controllers:
    FuchsiaDevice:
    - name: fuchsia-emulator
      transport: fuchsia-controller
  Name: InfraTestbed
  TestParams: {}

======================================

[=====MOBLY RESULTS=====]
---
Requested Tests:
- test_hung
Type: TestNameList
...
---
Details: Test timed out after 0:04:20.
Type: MoblyDriverTimeout
...
`

	want := []runtests.TestCaseResult{
		{
			DisplayName:   "MoblyDriverTimeout",
			FailureReason: runtests.FailureReasonFromMessage("[MoblyDriverTimeout] test_hung: Test timed out after 0:04:20."),
			SuiteName:     "Synthetic",
			CaseName:      "Synthetic",
			Status:        runtests.TestAborted,
			Format:        "Mobly",
		},
	}

	testCaseCmp(t, stdout, want)
}

func TestParseMoblyTestNonSigtermAbortAll(t *testing.T) {
	stdout := `
Running [InfraDriver]
======== Mobly config content ========
MoblyParams:
  LogPath: /tmp
TestBeds:
- Controllers:
    FuchsiaDevice:
    - name: fuchsia-emulator
      transport: fuchsia-controller
  Name: InfraTestbed
  TestParams: {}

======================================

[=====MOBLY RESULTS=====]
---
Requested Tests:
- test_fatal_device_error
- test_skipped_after_abort
Type: TestNameList
...
---
Begin Time: 1668122321142
Details: FatalDeviceError during test
End Time: 1668122321145
Extra Errors: {}
Extras: null
Parent: null
Result: FAIL
Retry Parent: null
Signature: test_fatal_device_error-1668122321142
Stacktrace: null
Termination Signal Type: TestAbortAll
Test Class: GreetingsTest
Test Name: test_fatal_device_error
Type: Record
UID: null
...
---
Begin Time: null
Details: 'All remaining tests aborted due to: FatalDeviceError during test'
End Time: null
Extra Errors: {}
Extras: null
Parent: null
Result: SKIP
Retry Parent: null
Signature: null
Stacktrace: null
Termination Signal Type: TestAbortAll
Test Class: GreetingsTest
Test Name: test_skipped_after_abort
Type: Record
UID: null
...
---
Error: 0
Executed: 1
Failed: 1
Passed: 0
Requested: 2
Skipped: 1
Type: Summary
...
`

	want := []runtests.TestCaseResult{
		{
			DisplayName:   "GreetingsTest.test_fatal_device_error",
			FailureReason: runtests.FailureReasonFromMessage("[TestAbortAll] FatalDeviceError during test"),
			SuiteName:     "GreetingsTest",
			CaseName:      "test_fatal_device_error",
			Status:        runtests.TestFailure,
			Duration:      3 * time.Millisecond,
			Format:        "Mobly",
		},
	}

	testCaseCmp(t, stdout, want)
}

func TestParseMoblyTestOnFailAbortAllWithTeardownTimeout(t *testing.T) {
	stdout := `
Running [InfraDriver]
======== Mobly config content ========
MoblyParams:
  LogPath: /tmp
TestBeds:
- Controllers:
    FuchsiaDevice:
    - name: fuchsia-emulator
      transport: fuchsia-controller
  Name: InfraTestbed
  TestParams: {}

======================================

[=====MOBLY RESULTS=====]
---
Requested Tests:
- test_fatal_device_error
- test_skipped_after_abort
Type: TestNameList
...
---
Begin Time: 1668122321142
Details: FatalDeviceError during test
End Time: 1668122321145
Extra Errors: {}
Extras: null
Parent: null
Result: FAIL
Retry Parent: null
Signature: test_fatal_device_error-1668122321142
Stacktrace: null
Termination Signal Type: FatalDeviceError
Test Class: GreetingsTest
Test Name: test_fatal_device_error
Type: Record
UID: null
...
---
Begin Time: null
Details: 'All remaining tests aborted due to: Aborting all remaining tests due to fatal device error: FatalDeviceError during test'
End Time: null
Extra Errors: {}
Extras: null
Parent: null
Result: SKIP
Retry Parent: null
Signature: null
Stacktrace: null
Termination Signal Type: TestAbortAll
Test Class: GreetingsTest
Test Name: test_skipped_after_abort
Type: Record
UID: null
...
---
Details: Test timed out after 0:04:20.
Type: MoblyDriverTimeout
...
`

	want := []runtests.TestCaseResult{
		{
			DisplayName:   "GreetingsTest.test_fatal_device_error",
			FailureReason: runtests.FailureReasonFromMessage("[FatalDeviceError] FatalDeviceError during test"),
			SuiteName:     "GreetingsTest",
			CaseName:      "test_fatal_device_error",
			Status:        runtests.TestFailure,
			Duration:      3 * time.Millisecond,
			Format:        "Mobly",
		},
		{
			DisplayName:   "MoblyDriverTimeout",
			FailureReason: runtests.FailureReasonFromMessage("[MoblyDriverTimeout] GreetingsTest: Test timed out after 0:04:20."),
			SuiteName:     "Synthetic",
			CaseName:      "Synthetic",
			Status:        runtests.TestAborted,
			Format:        "Mobly",
		},
	}

	testCaseCmp(t, stdout, want)
}

func TestParseMoblyTestSetupClassFailure(t *testing.T) {
	stdout := `
Running [InfraDriver]
======== Mobly config content ========
MoblyParams:
  LogPath: /tmp
TestBeds:
- Controllers:
    FuchsiaDevice:
    - name: fuchsia-emulator
      transport: fuchsia-controller
  Name: InfraTestbed
  TestParams: {}

======================================

[=====MOBLY RESULTS=====]
---
Requested Tests:
- test_adb_pull
- test_adb_push
Type: TestNameList
...
---
Begin Time: 1790657357608
Details: 'Failed to create device for ''fuchsia-3e95-6dff-148f'': health check failed
    on ''fuchsia-3e95-6dff-148f'''
End Time: 1790657654783
Extra Errors: {}
Extras: null
Parent: null
Result: ERROR
Retry Parent: null
Signature: setup_class-1790657357608
Stacktrace: null
Termination Signal Type: FuchsiaDeviceError
Test Class: AdbThroughputTest
Test Name: setup_class
Type: Record
UID: null
...
---
Begin Time: null
Details: 'Failed to create device for ''fuchsia-3e95-6dff-148f'': health check failed
    on ''fuchsia-3e95-6dff-148f'''
End Time: null
Extra Errors: {}
Extras: null
Parent: null
Result: SKIP
Retry Parent: null
Signature: null
Stacktrace: null
Termination Signal Type: FuchsiaDeviceError
Test Class: AdbThroughputTest
Test Name: test_adb_pull
Type: Record
UID: null
...
---
Begin Time: null
Details: 'Failed to create device for ''fuchsia-3e95-6dff-148f'': health check failed
    on ''fuchsia-3e95-6dff-148f'''
End Time: null
Extra Errors: {}
Extras: null
Parent: null
Result: SKIP
Retry Parent: null
Signature: null
Stacktrace: null
Termination Signal Type: FuchsiaDeviceError
Test Class: AdbThroughputTest
Test Name: test_adb_push
Type: Record
UID: null
...
---
Error: 1
Executed: 0
Failed: 0
Passed: 0
Requested: 2
Skipped: 2
Type: Summary
...
`

	want := []runtests.TestCaseResult{
		{
			DisplayName:   "AdbThroughputTest.setup_class",
			FailureReason: runtests.FailureReasonFromMessage("[FuchsiaDeviceError] Failed to create device for 'fuchsia-3e95-6dff-148f': health check failed on 'fuchsia-3e95-6dff-148f'"),
			SuiteName:     "AdbThroughputTest",
			CaseName:      "setup_class",
			Status:        runtests.TestFailure,
			Duration:      297175 * time.Millisecond,
			Format:        "Mobly",
		},
	}

	testCaseCmp(t, stdout, want)
}

func TestParseMoblyTestErrorNoSummary(t *testing.T) {
	stdout := `
Running [InfraDriver]
======== Mobly config content ========
MoblyParams:
  LogPath: /tmp
TestBeds:
- Controllers:
    FuchsiaDevice:
    - name: fuchsia-emulator
      transport: fuchsia-controller
  Name: InfraTestbed
  TestParams: {}

======================================

[=====MOBLY RESULTS=====]
---
Requested Tests:
- test_goodbye
- test_hello
- test_skipped
- test_error
Type: TestNameList
---
Begin Time: 1668122321142
Details: null
End Time: 1668122321143
Extra Errors: {}
Extras: null
Result: PASS
Retry Parent: null
Signature: test_goodbye-1668122321142
Stacktrace: null
Termination Signal Type: null
Test Class: GreetingsTest
Test Name: test_goodbye
Type: Record
UID: null

`

	want := []runtests.TestCaseResult{
		{
			DisplayName: "GreetingsTest.test_goodbye",
			SuiteName:   "GreetingsTest",
			CaseName:    "test_goodbye",
			Status:      runtests.TestSuccess,
			Duration:    1 * time.Millisecond,
			Format:      "Mobly",
		},
		{
			DisplayName:   "TestparserError",
			FailureReason: runtests.FailureReasonFromMessage("[TestparserError] Missing Mobly summary record - potental infra timeout."),
			SuiteName:     "Synthetic",
			CaseName:      "Synthetic",
			Status:        runtests.TestFailure,
			Format:        "Mobly",
		},
	}

	testCaseCmp(t, stdout, want)
}

func TestParseMoblyTestErrorMissingHeader(t *testing.T) {
	stdout := `
Running [InfraDriver]
======== Mobly config content ========
MoblyParams:
  LogPath: /tmp
TestBeds:
- Controllers:
    FuchsiaDevice:
    - name: fuchsia-emulator
      transport: fuchsia-controller
  Name: InfraTestbed
  TestParams: {}

======================================

`

	want := []runtests.TestCaseResult{
		{
			DisplayName:   "TestparserError",
			FailureReason: runtests.FailureReasonFromMessage("[TestparserError] Missing Mobly summary record - potental infra timeout."),
			SuiteName:     "Synthetic",
			CaseName:      "Synthetic",
			Status:        runtests.TestFailure,
			Format:        "Mobly",
		},
	}

	testCaseCmp(t, stdout, want)
}

func TestParseMoblyTestErrorMalformedYAML(t *testing.T) {
	stdout := `
Running [InfraDriver]
======== Mobly config content ========
MoblyParams:
  LogPath: /tmp
TestBeds:
- Controllers:
    FuchsiaDevice:
    - name: fuchsia-emulator
      transport: fuchsia-controller
  Name: InfraTestbed
  TestParams: {}

======================================

[=====MOBLY RESULTS=====]
---
Requested Tests:
- test_goodbye
  test_hello:
    malformed

`

	want := []runtests.TestCaseResult{
		{
			DisplayName:   "TestparserError",
			FailureReason: runtests.FailureReasonFromMessage("[TestparserError] Missing Mobly summary record - potental infra timeout."),
			SuiteName:     "Synthetic",
			CaseName:      "Synthetic",
			Status:        runtests.TestFailure,
			Format:        "Mobly",
		},
	}

	testCaseCmp(t, stdout, want)
}
