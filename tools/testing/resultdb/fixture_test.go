// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package resultdb

import (
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"
	"unicode/utf8"

	"github.com/google/go-cmp/cmp"
	resultpb "go.chromium.org/luci/resultdb/proto/v1"
	sinkpb "go.chromium.org/luci/resultdb/sink/proto/v1"

	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

func boolPtr(b bool) *bool { return &b }
func intPtr(i int) *int    { return &i }

// testCaseResults returns the test case results of detail, from which
// synthesizeFixtureResult derives the *fixture's fine_name.
func testCaseResults(t *testing.T, detail *runtests.TestDetails) []*sinkpb.TestResult {
	t.Helper()
	results, _, _ := testCaseToResultSink(detail.Cases, nil, detail, t.TempDir())
	return results
}

func TestClassifyFixture(t *testing.T) {
	tests := []struct {
		name       string
		detail     *runtests.TestDetails
		wantOK     bool
		wantStatus resultpb.TestResult_Status
	}{
		{
			name: "lifecycle fields unset",
			detail: &runtests.TestDetails{
				Name:   "fuchsia-pkg://fuchsia.com/foo#meta/foo.cm",
				Status: runtests.TestFailure,
			},
			wantOK: false,
		},
		{
			name: "Scenario 1: clean pass",
			detail: &runtests.TestDetails{
				Name:   "fuchsia-pkg://fuchsia.com/bt-host-smoke-test#meta/bt-host-smoke-test.cm",
				Status: runtests.TestSuccess,
				TestResult: runtests.TestResult{
					SetupSucceeded:    boolPtr(true),
					TeardownSucceeded: boolPtr(true),
				},
			},
			wantOK: false,
		},
		{
			name: "Scenario 2: case assertion failure with clean fixture",
			detail: &runtests.TestDetails{
				Name:   "fuchsia-pkg://fuchsia.com/bt-host-smoke-test#meta/bt-host-smoke-test.cm",
				Status: runtests.TestFailure,
				TestResult: runtests.TestResult{
					SetupSucceeded:    boolPtr(true),
					TeardownSucceeded: boolPtr(true),
				},
			},
			wantOK: false,
		},
		{
			name: "Scenario 3: intentional skip",
			detail: &runtests.TestDetails{
				Name:   "fuchsia-pkg://fuchsia.com/bt-host-smoke-test#meta/bt-host-smoke-test.cm",
				Status: runtests.TestSuccess,
				TestResult: runtests.TestResult{
					SetupSucceeded:    boolPtr(true),
					TeardownSucceeded: boolPtr(true),
				},
			},
			wantOK: false,
		},
		{
			name: "Scenario 4: setup failed (intrinsic fixture bug)",
			detail: &runtests.TestDetails{
				Name:   "fuchsia-pkg://fuchsia.com/bt-host-smoke-test#meta/bt-host-smoke-test.cm",
				Status: runtests.TestFailure,
				TestResult: runtests.TestResult{
					SetupSucceeded: boolPtr(false),
				},
			},
			wantOK:     true,
			wantStatus: resultpb.TestResult_FAILED,
		},
		{
			name: "Scenario 5: mid-suite abort",
			detail: &runtests.TestDetails{
				Name:   "fuchsia-pkg://fuchsia.com/bt-host-smoke-test#meta/bt-host-smoke-test.cm",
				Status: runtests.TestAborted,
				TestResult: runtests.TestResult{
					SetupSucceeded: boolPtr(true),
				},
			},
			wantOK: false,
		},
		{
			name: "setup failed with non-failure suite status is ignored",
			detail: &runtests.TestDetails{
				Name:   "fuchsia-pkg://fuchsia.com/bt-host-smoke-test#meta/bt-host-smoke-test.cm",
				Status: runtests.TestSuccess,
				TestResult: runtests.TestResult{
					SetupSucceeded: boolPtr(false),
				},
			},
			wantOK: false,
		},
		{
			name: "Scenario 6: teardown failed after cases passed",
			detail: &runtests.TestDetails{
				Name:   "fuchsia-pkg://fuchsia.com/bt-host-smoke-test#meta/bt-host-smoke-test.cm",
				Status: runtests.TestFailure,
				TestResult: runtests.TestResult{
					SetupSucceeded:    boolPtr(true),
					TeardownSucceeded: boolPtr(false),
				},
			},
			wantOK:     true,
			wantStatus: resultpb.TestResult_FAILED,
		},
		{
			name: "Scenario 7: unaffected target skipped",
			detail: &runtests.TestDetails{
				Name:     "fuchsia-pkg://fuchsia.com/bt-host-smoke-test#meta/bt-host-smoke-test.cm",
				Status:   runtests.TestSkipped,
				Affected: false,
			},
			wantOK: false,
		},
		{
			name: "Scenario 8: contaminated environment (noisy neighbor) setup infra fail",
			detail: &runtests.TestDetails{
				Name:   "fuchsia-pkg://fuchsia.com/bt-host-smoke-test#meta/bt-host-smoke-test.cm",
				Status: runtests.TestInfraFailure,
				TestResult: runtests.TestResult{
					SetupSucceeded: boolPtr(false),
				},
			},
			wantOK:     true,
			wantStatus: resultpb.TestResult_EXECUTION_ERRORED,
		},
		{
			name: "teardown infra fail",
			detail: &runtests.TestDetails{
				Name:   "fuchsia-pkg://fuchsia.com/bt-host-smoke-test#meta/bt-host-smoke-test.cm",
				Status: runtests.TestInfraFailure,
				TestResult: runtests.TestResult{
					SetupSucceeded:    boolPtr(true),
					TeardownSucceeded: boolPtr(false),
				},
			},
			wantOK:     true,
			wantStatus: resultpb.TestResult_EXECUTION_ERRORED,
		},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			status, reason, ok := classifyFixture(tc.detail)
			if ok != tc.wantOK {
				t.Fatalf("classifyFixture() ok = %v, want %v", ok, tc.wantOK)
			}
			if !ok {
				return
			}
			if status != tc.wantStatus {
				t.Errorf("classifyFixture() status = %v, want %v (reason %q)", status, tc.wantStatus, reason)
			}
			if reason == "" {
				t.Errorf("classifyFixture() reason is empty")
			}
		})
	}
}

func TestSynthesizeFixtureResult_NoFixture(t *testing.T) {
	for name, detail := range map[string]*runtests.TestDetails{
		"tefmocheck": {
			Name:                 "testing_failure_mode/serial_log/ panic",
			Status:               runtests.TestFailure,
			TestResult:           runtests.TestResult{SetupSucceeded: boolPtr(false)},
			IsTestingFailureMode: true,
		},
		"clean pass": {
			Name:   "fuchsia-pkg://fuchsia.com/foo#meta/foo.cm",
			Status: runtests.TestSuccess,
			TestResult: runtests.TestResult{
				SetupSucceeded:    boolPtr(true),
				TeardownSucceeded: boolPtr(true),
				Cases:             []runtests.TestCaseResult{{SuiteName: "Suite", CaseName: "Case", Format: "GoogleTest"}},
			},
		},
	} {
		if got, skipped := synthesizeFixtureResult(testCaseResults(t, detail), detail); got != nil || skipped != "" {
			t.Errorf("%s: synthesizeFixtureResult() = %v, %q, want nil, \"\"", name, got, skipped)
		}
	}
}

func TestSynthesizeFixtureResult_FineNameAndTestID(t *testing.T) {
	const name = "fuchsia-pkg://fuchsia.com/bt-host-smoke-test#meta/bt-host-smoke-test.cm"
	tests := []struct {
		desc         string
		detail       runtests.TestDetails
		wantFineName string
	}{
		{
			desc: "gtest with a single suite uses the suite",
			detail: runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{
						{SuiteName: "BtHostTestSuite", CaseName: "TestInitializeSuccess", Format: "GoogleTest"},
						{SuiteName: "BtHostTestSuite", CaseName: "TestConnectPeer", Format: "GoogleTest"},
					},
				},
			},
			wantFineName: "BtHostTestSuite",
		},
		{
			desc: "gtest with multiple suites reports a single fixture",
			detail: runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{
						{SuiteName: "BtHostTestSuite", CaseName: "TestInitializeSuccess", Format: "GoogleTest"},
						{SuiteName: "BtHostTestSuite", CaseName: "TestConnectPeer", Format: "GoogleTest"},
						{SuiteName: "BtGapTestSuite", CaseName: "TestDiscovery", Format: "GoogleTest"},
					},
				},
			},
			wantFineName: "test",
		},
		{
			desc: "gtest without suite names falls back to the scheme default",
			detail: runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{CaseName: "TestFoo", Format: "GoogleTest"}},
				},
			},
			wantFineName: "test",
		},
		{
			desc: "mobly with a single class uses the class",
			detail: runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{
						{SuiteName: "TiltToWakeTest", CaseName: "test_tilt_to_wake", Format: "Mobly"},
					},
				},
			},
			wantFineName: "TiltToWakeTest",
		},
		{
			desc: "mobly with a synthetic test parser error falls back to the scheme default",
			detail: runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{
						{SuiteName: "TiltToWakeTest", CaseName: "test_tilt_to_wake", Format: "Mobly"},
						{SuiteName: "Synthetic", CaseName: "Synthetic", Format: "Mobly"},
					},
				},
			},
			wantFineName: "test",
		},
		{
			desc: "rust with a single module uses the module",
			detail: runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{SuiteName: "tests", CaseName: "test_foo", Format: "Rust"}},
				},
			},
			wantFineName: "tests",
		},
		{
			desc: "rust at the crate root uses the crate root module",
			detail: runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{CaseName: "test_foo", Format: "Rust"}},
				},
			},
			wantFineName: "crate",
		},
		{
			desc: "rust with multiple modules uses the crate root module",
			detail: runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{
						{SuiteName: "tests", CaseName: "test_foo", Format: "Rust"},
						{SuiteName: "other_tests", CaseName: "test_bar", Format: "Rust"},
					},
				},
			},
			wantFineName: "crate",
		},
		{
			desc: "go uses the Go package",
			detail: runtests.TestDetails{
				SourceLabel: "//tools/testing/resultdb:resultdb_lib_tests(//build/toolchain:host_x64)",
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{CaseName: "TestFoo", Format: "Go"}},
				},
			},
			wantFineName: "tools/testing/resultdb",
		},
		{
			desc: "flat uses an empty fine name",
			detail: runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{SuiteName: "Suite", CaseName: "test_foo", Format: "FTF"}},
				},
			},
			wantFineName: "",
		},
		{
			desc: "single with test cases uses an empty fine name",
			detail: runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{CaseName: "test_foo", Format: "single"}},
				},
			},
			wantFineName: "",
		},
	}
	for _, tc := range tests {
		t.Run(tc.desc, func(t *testing.T) {
			detail := tc.detail
			detail.Name = name
			detail.Status = runtests.TestFailure
			detail.StartTime = time.Unix(1700000000, 0)
			detail.SetupSucceeded = boolPtr(false)
			// Test cases need a status to be reported, and the *fixture
			// shares the fine_name of the reported test cases.
			detail.Cases = slices.Clone(detail.Cases)
			for i := range detail.Cases {
				detail.Cases[i].Status = runtests.TestSkipped
			}

			r, skipped := synthesizeFixtureResult(testCaseResults(t, &detail), &detail)
			if r == nil || skipped != "" {
				t.Fatalf("synthesizeFixtureResult() = %v, %q, want a fixture", r, skipped)
			}
			wantID := name + "/" + tc.wantFineName + ":" + FixtureCaseName
			if r.TestId != wantID {
				t.Errorf("TestId = %q, want %q", r.TestId, wantID)
			}
			id := r.TestIdStructured
			if id.GetCoarseName() != "" || id.GetFineName() != tc.wantFineName {
				t.Errorf("CoarseName, FineName = %q, %q, want \"\", %q", id.GetCoarseName(), id.GetFineName(), tc.wantFineName)
			}
			if got := id.GetCaseNameComponents(); len(got) != 1 || got[0] != FixtureCaseName {
				t.Errorf("CaseNameComponents = %q, want [%q]", got, FixtureCaseName)
			}
		})
	}
}

func TestSynthesizeFixtureResult_FineNameMatchesTestCases(t *testing.T) {
	detail := &runtests.TestDetails{
		Name:      "fuchsia-pkg://fuchsia.com/foo#meta/foo.cm",
		Status:    runtests.TestFailure,
		StartTime: time.Unix(1700000000, 0),
		TestResult: runtests.TestResult{
			TeardownSucceeded: boolPtr(false),
			Cases: []runtests.TestCaseResult{
				// A leading space is reserved by ResultDB, so the fine_name is
				// sanitized.
				{SuiteName: " Suite", CaseName: "Case", Status: runtests.TestSuccess, Format: "GoogleTest"},
			},
		},
	}
	f, _ := synthesizeFixtureResult(testCaseResults(t, detail), detail)
	if f == nil {
		t.Fatalf("synthesizeFixtureResult() = nil, want a fixture")
	}
	caseResults, _, _ := testCaseToResultSink(detail.Cases, nil, detail, t.TempDir())
	if len(caseResults) != 1 {
		t.Fatalf("got %d case results, want 1", len(caseResults))
	}
	const wantFineName = "[ Suite]"
	if got := caseResults[0].TestIdStructured.GetFineName(); got != wantFineName {
		t.Errorf("case FineName = %q, want %q", got, wantFineName)
	}
	if got := f.TestIdStructured.GetFineName(); got != wantFineName {
		t.Errorf("fixture FineName = %q, want %q", got, wantFineName)
	}
}

func TestSynthesizeFixtureResult_TestIDTooLong(t *testing.T) {
	detail := &runtests.TestDetails{
		Name:   strings.Repeat("a", MaxTestIDLength),
		Status: runtests.TestFailure,
		TestResult: runtests.TestResult{
			SetupSucceeded: boolPtr(false),
			Cases:          []runtests.TestCaseResult{{SuiteName: "Suite", CaseName: "Case", Format: "GoogleTest"}},
		},
	}
	got, skipped := synthesizeFixtureResult(testCaseResults(t, detail), detail)
	if got != nil {
		t.Errorf("synthesizeFixtureResult() = %v, want nil", got)
	}
	// The test case's test ID is also too long, so there is no test case result
	// to share a fine_name with, and the *fixture falls back to the scheme
	// default.
	if want := detail.Name + "/test:" + FixtureCaseName; skipped != want {
		t.Errorf("synthesizeFixtureResult() skipped = %q, want %q", skipped, want)
	}
}

func TestSynthesizeFixtureResult_Scenarios4And6And8(t *testing.T) {
	t.Run("Scenario 4: setup failure reports FAILED with CRASH kind and leaves case statuses unchanged", func(t *testing.T) {
		outputRoot := t.TempDir()
		artifactRel := "case_log.txt"
		if err := os.WriteFile(filepath.Join(outputRoot, artifactRel), []byte("log"), 0o600); err != nil {
			t.Fatal(err)
		}
		detail := &runtests.TestDetails{
			Name:        "fuchsia-pkg://fuchsia.com/bt-host-smoke-test#meta/bt-host-smoke-test.cm",
			SourceLabel: "//src/connectivity/bluetooth/tests/bt_host:bt_host_smoke_test",
			Status:      runtests.TestFailure,
			StartTime:   time.Unix(1700000000, 0),
			TestResult: runtests.TestResult{
				SetupSucceeded: boolPtr(false),
				ExitCode:       intPtr(1),
				FailureReason:  runtests.FailureReasonFromMessage("SetUpTestSuite failed: Unable to bind Bluetooth host realm channel"),
				Cases: []runtests.TestCaseResult{
					{
						DisplayName: "BtHostTestSuite.TestInitializeSuccess",
						SuiteName:   "BtHostTestSuite",
						CaseName:    "TestInitializeSuccess",
						Status:      runtests.TestSkipped,
						Format:      "GoogleTest",
						OutputFiles: []string{artifactRel},
					},
					{
						DisplayName: "BtHostTestSuite.TestConnectPeer",
						SuiteName:   "BtHostTestSuite",
						CaseName:    "TestConnectPeer",
						Status:      runtests.TestSkipped,
						Format:      "GoogleTest",
						OutputFiles: []string{artifactRel},
					},
				},
			},
		}

		f, _ := synthesizeFixtureResult(testCaseResults(t, detail), detail)
		if f == nil {
			t.Fatalf("synthesizeFixtureResult() = nil, want a fixture")
		}
		if f.StatusV2 != resultpb.TestResult_FAILED {
			t.Errorf("fixture StatusV2 = %v, want FAILED", f.StatusV2)
		}
		if f.FailureReason.GetKind() != resultpb.FailureReason_CRASH {
			t.Errorf("fixture FailureReason.Kind = %v, want CRASH", f.FailureReason.GetKind())
		}
		errs := f.FailureReason.GetErrors()
		if len(errs) != 1 || errs[0].GetMessage() != "SetUpTestSuite failed: Unable to bind Bluetooth host realm channel" {
			t.Errorf("fixture FailureReason.Errors = %v, want exact setup failure message", errs)
		}

		// Test cases keep the status reported by the test runner. An
		// intentionally skipped test case stays SKIPPED.
		caseResults, _, _ := testCaseToResultSink(detail.Cases, nil, detail, outputRoot)
		if len(caseResults) != 2 {
			t.Fatalf("got %d case results, want 2", len(caseResults))
		}
		for _, c := range caseResults {
			if c.StatusV2 != resultpb.TestResult_SKIPPED {
				t.Errorf("case %q StatusV2 = %v, want SKIPPED", c.TestId, c.StatusV2)
			}
			if c.SkippedReason.GetKind() != resultpb.SkippedReason_DISABLED_AT_DECLARATION {
				t.Errorf("case %q SkippedReason.Kind = %v, want DISABLED_AT_DECLARATION", c.TestId, c.SkippedReason.GetKind())
			}
			if len(c.Artifacts) != 1 {
				t.Errorf("case %q has %d artifacts, want 1", c.TestId, len(c.Artifacts))
			}
		}
	})

	t.Run("Scenario 6: teardown failure reports FAILED with ORDINARY kind while cases stay PASSED", func(t *testing.T) {
		detail := &runtests.TestDetails{
			Name:        "fuchsia-pkg://fuchsia.com/bt-host-smoke-test#meta/bt-host-smoke-test.cm",
			SourceLabel: "//src/connectivity/bluetooth/tests/bt_host:bt_host_smoke_test",
			Status:      runtests.TestFailure,
			StartTime:   time.Unix(1700000000, 0),
			TestResult: runtests.TestResult{
				SetupSucceeded:    boolPtr(true),
				TeardownSucceeded: boolPtr(false),
				FailureReason:     runtests.FailureReasonFromMessage("TearDownTestSuite failed: Lingering child component instance detected"),
				Cases: []runtests.TestCaseResult{
					{
						DisplayName: "BtHostTestSuite.TestInitializeSuccess",
						SuiteName:   "BtHostTestSuite",
						CaseName:    "TestInitializeSuccess",
						Status:      runtests.TestSuccess,
						Format:      "GoogleTest",
					},
					{
						DisplayName: "BtHostTestSuite.TestConnectPeer",
						SuiteName:   "BtHostTestSuite",
						CaseName:    "TestConnectPeer",
						Status:      runtests.TestSuccess,
						Format:      "GoogleTest",
					},
				},
			},
		}

		f, _ := synthesizeFixtureResult(testCaseResults(t, detail), detail)
		if f == nil {
			t.Fatalf("synthesizeFixtureResult() = nil, want a fixture")
		}
		if f.StatusV2 != resultpb.TestResult_FAILED {
			t.Errorf("fixture StatusV2 = %v, want FAILED", f.StatusV2)
		}
		if f.FailureReason.GetKind() != resultpb.FailureReason_ORDINARY {
			t.Errorf("fixture FailureReason.Kind = %v, want ORDINARY", f.FailureReason.GetKind())
		}
		errs := f.FailureReason.GetErrors()
		if len(errs) != 1 || errs[0].GetMessage() != "TearDownTestSuite failed: Lingering child component instance detected" {
			t.Errorf("fixture FailureReason.Errors = %v, want exact teardown failure message", errs)
		}

		caseResults, _, _ := testCaseToResultSink(detail.Cases, nil, detail, t.TempDir())
		if len(caseResults) != 2 {
			t.Fatalf("got %d case results, want 2", len(caseResults))
		}
		for _, c := range caseResults {
			if c.StatusV2 != resultpb.TestResult_PASSED {
				t.Errorf("case %q StatusV2 = %v, want PASSED", c.TestId, c.StatusV2)
			}
		}
	})

	t.Run("Scenario 8: contaminated environment reports *fixture as EXECUTION_ERRORED", func(t *testing.T) {
		detail := &runtests.TestDetails{
			Name:        "fuchsia-pkg://fuchsia.com/bt-host-smoke-test#meta/bt-host-smoke-test.cm",
			SourceLabel: "//src/connectivity/bluetooth/tests/bt_host:bt_host_smoke_test",
			Status:      runtests.TestInfraFailure,
			StartTime:   time.Unix(1700000000, 0),
			TestResult: runtests.TestResult{
				SetupSucceeded: boolPtr(false),
				ExitCode:       intPtr(1),
				FailureReason:  runtests.FailureReasonFromMessage("SetUpTestSuite failed: Unable to connect to component realm: PEER_CLOSED"),
				Cases: []runtests.TestCaseResult{
					{
						DisplayName: "BtHostTestSuite.TestInitializeSuccess",
						SuiteName:   "BtHostTestSuite",
						CaseName:    "TestInitializeSuccess",
						Status:      runtests.TestSkipped,
						Format:      "GoogleTest",
					},
				},
			},
		}

		f, _ := synthesizeFixtureResult(testCaseResults(t, detail), detail)
		if f == nil {
			t.Fatalf("synthesizeFixtureResult() = nil, want a fixture")
		}
		if f.StatusV2 != resultpb.TestResult_EXECUTION_ERRORED {
			t.Errorf("fixture StatusV2 = %v, want EXECUTION_ERRORED", f.StatusV2)
		}
		if f.FailureReason != nil {
			t.Errorf("fixture FailureReason = %v, want nil for EXECUTION_ERRORED", f.FailureReason)
		}
		if !strings.Contains(f.SummaryHtml, "PEER_CLOSED") {
			t.Errorf("fixture SummaryHtml = %q, want it to contain the setup failure message", f.SummaryHtml)
		}
		if !strings.HasPrefix(f.SummaryHtml, "<p>Exit code: 1</p>") {
			t.Errorf("fixture SummaryHtml = %q, want it to start with the exit code", f.SummaryHtml)
		}
	})
}

func TestFixtureSummaryHTML_EscapesAndTruncates(t *testing.T) {
	detail := &runtests.TestDetails{
		Name:   "fuchsia-pkg://fuchsia.com/foo#meta/foo.cm",
		Status: runtests.TestInfraFailure,
		TestResult: runtests.TestResult{
			SetupSucceeded: boolPtr(false),
			ExitCode:       intPtr(-1),
		},
	}
	detail.FailureReason = &runtests.FailureReason{}
	for i := 0; i < 20; i++ {
		detail.FailureReason.Errors = append(detail.FailureReason.Errors, &runtests.FailureReasonError{
			Message: strings.Repeat("<&>", 200),
		})
	}
	got := fixtureSummaryHTML(detail)
	if len(got) > MaxSummaryHTMLLength {
		t.Errorf("len(summary) = %d, want <= %d", len(got), MaxSummaryHTMLLength)
	}
	if !strings.HasPrefix(got, "<p>Exit code: -1</p><pre>") {
		t.Errorf("summary does not start with the exit code followed by an error message: %q", got[:40])
	}
	if strings.Contains(got, "<&>") {
		t.Errorf("summary contains unescaped text")
	}
	if strings.Count(got, "<pre>") != strings.Count(got, "</pre>") {
		t.Errorf("summary has an element cut in half")
	}
	if strings.Contains(got, "<pre>...</pre>") {
		t.Errorf("summary contains an error message of which nothing fits")
	}

	// Verify messages longer than MaxFailureReasonLength (1024 bytes) are not
	// prematurely truncated when they fit within MaxSummaryHTMLLength (4096 bytes).
	longTrace := strings.Repeat("x", 2500)
	longDetail := &runtests.TestDetails{
		Name:   "fuchsia-pkg://fuchsia.com/foo#meta/foo.cm",
		Status: runtests.TestInfraFailure,
		TestResult: runtests.TestResult{
			SetupSucceeded: boolPtr(false),
			FailureReason:  runtests.FailureReasonFromMessage(longTrace),
		},
	}
	gotLong := fixtureSummaryHTML(longDetail)
	if want := "<pre>" + longTrace + "</pre>"; gotLong != want {
		t.Errorf("expected 2500-byte message to be preserved in full as the whole SummaryHtml, got len=%d", len(gotLong))
	}

	// Without an exit code or error messages, the summary is empty.
	emptyDetail := &runtests.TestDetails{
		Name:       "fuchsia-pkg://fuchsia.com/foo#meta/foo.cm",
		Status:     runtests.TestInfraFailure,
		TestResult: runtests.TestResult{SetupSucceeded: boolPtr(false)},
	}
	if got := fixtureSummaryHTML(emptyDetail); got != "" {
		t.Errorf("fixtureSummaryHTML() = %q, want \"\"", got)
	}
}

func TestHasFixture(t *testing.T) {
	results := []*sinkpb.TestResult{
		{TestIdStructured: &sinkpb.TestIdentifier{FineName: "Suite", CaseNameComponents: []string{"Case"}}},
		{TestIdStructured: &sinkpb.TestIdentifier{FineName: "", CaseNameComponents: []string{FixtureCaseName}}},
		// Top-level results of tests with cases have no structured ID.
		{TestId: "fuchsia-pkg://fuchsia.com/foo#meta/foo.cm"},
	}
	for _, tc := range []struct {
		fineName string
		want     bool
	}{
		{fineName: "", want: true},
		{fineName: "test", want: false},
		{fineName: "Suite", want: false},
	} {
		if got := hasFixture(results, tc.fineName); got != tc.want {
			t.Errorf("hasFixture(%q) = %v, want %v", tc.fineName, got, tc.want)
		}
	}
}

func TestTestToResultSink_FixtureNotDuplicated(t *testing.T) {
	for _, tc := range []struct {
		desc       string
		detail     *runtests.TestDetails
		wantTestID string
	}{
		{
			desc: "top-level result of a test without test cases is the fixture",
			detail: &runtests.TestDetails{
				Name:       "fuchsia-pkg://fuchsia.com/setup-fail#meta/setup-fail.cm",
				Status:     runtests.TestFailure,
				StartTime:  time.Unix(1700000000, 0),
				TestResult: runtests.TestResult{SetupSucceeded: boolPtr(false)},
			},
			wantTestID: "fuchsia-pkg://fuchsia.com/setup-fail#meta/setup-fail.cm",
		},
		{
			desc: "fixture is synthesized for a test with test cases",
			detail: &runtests.TestDetails{
				Name:      "fuchsia-pkg://fuchsia.com/setup-fail#meta/setup-fail.cm",
				Status:    runtests.TestFailure,
				StartTime: time.Unix(1700000000, 0),
				TestResult: runtests.TestResult{
					SetupSucceeded: boolPtr(false),
					Cases: []runtests.TestCaseResult{
						{SuiteName: "Suite", CaseName: "Case", Status: runtests.TestSkipped, Format: "GoogleTest"},
					},
				},
			},
			wantTestID: "fuchsia-pkg://fuchsia.com/setup-fail#meta/setup-fail.cm/Suite:" + FixtureCaseName,
		},
	} {
		t.Run(tc.desc, func(t *testing.T) {
			results, _, _ := testToResultSink(tc.detail, nil, t.TempDir())
			var fixtureIDs []string
			for _, r := range results {
				if slices.Equal(r.GetTestIdStructured().GetCaseNameComponents(), []string{FixtureCaseName}) {
					fixtureIDs = append(fixtureIDs, r.TestId)
				}
			}
			if diff := cmp.Diff([]string{tc.wantTestID}, fixtureIDs); diff != "" {
				t.Errorf("*fixture test IDs mismatch (-want +got):\n%s", diff)
			}
		})
	}
}

func TestTruncateEscapedHTML(t *testing.T) {
	for _, tc := range []struct {
		str  string
		max  int
		want string
	}{
		{str: "abc", max: 10, want: "abc"},
		// Room for "..." is always reserved.
		{str: "ab", max: 3, want: "..."},
		{str: "abcdef", max: 3, want: "..."},
		{str: "abcdefgh", max: 6, want: "abc..."},
		// Truncates str rather than the escaped string, so entities are kept
		// whole.
		{str: "<ab", max: 8, want: "&lt;a..."},
		{str: "<b>", max: 9, want: "&lt;b..."},
		{str: "<b>", max: 8, want: "&lt;b..."},
		{str: "<<<<", max: 9, want: "&lt;..."},
		{str: "<<<<", max: 6, want: "..."},
		// str is truncated at rune boundaries, never mid-character.
		{str: "日本語", max: 8, want: "日..."},
		{str: "日本語", max: 9, want: "日本..."},
		{str: "日本語", max: 5, want: "..."},
		// Mixed multibyte and escaped characters.
		{str: "日<本", max: 10, want: "日&lt;..."},
		// Long input is clipped up front.
		{str: strings.Repeat("日", 100000), max: 12, want: "日日日..."},
	} {
		got := truncateEscapedHTML(tc.str, tc.max)
		if got != tc.want {
			t.Errorf("truncateEscapedHTML(%q, %d) = %q, want %q", tc.str, tc.max, got, tc.want)
		}
		if len(got) > tc.max {
			t.Errorf("truncateEscapedHTML(%q, %d) returned %d bytes, want <= %d", tc.str, tc.max, len(got), tc.max)
		}
		if !utf8.ValidString(got) {
			t.Errorf("truncateEscapedHTML(%q, %d) = %q, which is not valid UTF-8", tc.str, tc.max, got)
		}
	}
}

func TestSummaryToResultSink_FixtureSynthesis(t *testing.T) {
	longName := strings.Repeat("a", MaxTestIDLength)
	summary := &runtests.TestSummary{Tests: []runtests.TestDetails{
		// Scenario 1: Clean pass -> no fixture
		{
			Name:      "fuchsia-pkg://fuchsia.com/pass#meta/pass.cm",
			Status:    runtests.TestSuccess,
			StartTime: time.Unix(1700000000, 0),
			TestResult: runtests.TestResult{
				SetupSucceeded:    boolPtr(true),
				TeardownSucceeded: boolPtr(true),
				Cases: []runtests.TestCaseResult{
					{SuiteName: "PassSuite", CaseName: "Case1", Status: runtests.TestSuccess, Format: "GoogleTest"},
				},
			},
		},
		// Scenario 4: Setup failed -> 1 fixture (FAILED)
		{
			Name:      "fuchsia-pkg://fuchsia.com/setup-fail#meta/setup-fail.cm",
			Status:    runtests.TestFailure,
			StartTime: time.Unix(1700000000, 0),
			TestResult: runtests.TestResult{
				SetupSucceeded: boolPtr(false),
				Cases: []runtests.TestCaseResult{
					{SuiteName: "SetupFailSuite", CaseName: "Case1", Status: runtests.TestSkipped, Format: "GoogleTest"},
				},
			},
		},
		// Setup failed before test case enumeration -> no separate fixture,
		// since the top-level result already uses the *fixture case name.
		{
			Name:       "fuchsia-pkg://fuchsia.com/setup-fail-no-cases#meta/setup-fail-no-cases.cm",
			Status:     runtests.TestFailure,
			StartTime:  time.Unix(1700000000, 0),
			TestResult: runtests.TestResult{SetupSucceeded: boolPtr(false)},
		},
		// Scenario 6: Teardown failed -> 1 fixture (FAILED) for all suites
		{
			Name:      "fuchsia-pkg://fuchsia.com/teardown-fail#meta/teardown-fail.cm",
			Status:    runtests.TestFailure,
			StartTime: time.Unix(1700000000, 0),
			TestResult: runtests.TestResult{
				SetupSucceeded:    boolPtr(true),
				TeardownSucceeded: boolPtr(false),
				Cases: []runtests.TestCaseResult{
					{SuiteName: "TeardownFailSuiteA", CaseName: "Case1", Status: runtests.TestSuccess, Format: "GoogleTest"},
					{SuiteName: "TeardownFailSuiteB", CaseName: "Case1", Status: runtests.TestSuccess, Format: "GoogleTest"},
				},
			},
		},
		// Scenario 8: Noisy neighbor -> 1 fixture (EXECUTION_ERRORED)
		{
			Name:      "fuchsia-pkg://fuchsia.com/noisy#meta/noisy.cm",
			Status:    runtests.TestInfraFailure,
			StartTime: time.Unix(1700000000, 0),
			TestResult: runtests.TestResult{
				SetupSucceeded: boolPtr(false),
				Cases: []runtests.TestCaseResult{
					{SuiteName: "NoisySuite", CaseName: "Case1", Status: runtests.TestSkipped, Format: "GoogleTest"},
				},
			},
		},
		// Setup failed, but the fixture's test ID is too long -> skipped
		{
			Name:      longName,
			Status:    runtests.TestFailure,
			StartTime: time.Unix(1700000000, 0),
			TestResult: runtests.TestResult{
				SetupSucceeded: boolPtr(false),
				Cases: []runtests.TestCaseResult{
					{SuiteName: "LongSuite", CaseName: "Case1", Status: runtests.TestSkipped, Format: "GoogleTest"},
				},
			},
		},
	}}
	results, _, testsSkipped := SummaryToResultSink(summary, nil, t.TempDir())
	var fixtureIDs []string
	for _, r := range results {
		if strings.HasSuffix(r.TestId, ":"+FixtureCaseName) {
			fixtureIDs = append(fixtureIDs, r.TestId)
		}
	}
	wantFixtureIDs := []string{
		"fuchsia-pkg://fuchsia.com/setup-fail#meta/setup-fail.cm/SetupFailSuite:" + FixtureCaseName,
		"fuchsia-pkg://fuchsia.com/teardown-fail#meta/teardown-fail.cm/test:" + FixtureCaseName,
		"fuchsia-pkg://fuchsia.com/noisy#meta/noisy.cm/NoisySuite:" + FixtureCaseName,
	}
	if diff := cmp.Diff(wantFixtureIDs, fixtureIDs); diff != "" {
		t.Errorf("fixture test IDs mismatch (-want +got):\n%s", diff)
	}
	// The long-named test's case is also skipped, so only compare *fixtures.
	// With no test case result to share a fine_name with, the *fixture falls
	// back to the scheme default.
	var skippedFixtureIDs []string
	for _, id := range testsSkipped {
		if strings.HasSuffix(id, ":"+FixtureCaseName) {
			skippedFixtureIDs = append(skippedFixtureIDs, id)
		}
	}
	if diff := cmp.Diff([]string{longName + "/test:" + FixtureCaseName}, skippedFixtureIDs); diff != "" {
		t.Errorf("skipped fixture test IDs mismatch (-want +got):\n%s", diff)
	}
}
