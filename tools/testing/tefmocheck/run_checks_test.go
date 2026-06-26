// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package tefmocheck

import (
	"bytes"
	"log"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/tools/testing/runtests"

	"github.com/google/go-cmp/cmp"
	"github.com/google/go-cmp/cmp/cmpopts"
)

type alwaysTrueCheck struct {
	baseCheck
	outputsDir string
}

func (c alwaysTrueCheck) Check(*TestingOutputs) bool {
	return true
}

func (c alwaysTrueCheck) Name() string {
	return "always_true"
}

func (c alwaysTrueCheck) DebugText() string {
	return "True dat"
}

func (c alwaysTrueCheck) OutputFiles() []string {
	return []string{filepath.Join(c.outputsDir, "true.txt")}
}

type alwaysFalseCheck struct{ baseCheck }

func (c alwaysFalseCheck) Check(*TestingOutputs) bool {
	return false
}

func (c alwaysFalseCheck) Name() string {
	return "always_false"
}

func (c alwaysFalseCheck) DebugText() string {
	return "Lies!"
}

func (c alwaysFalseCheck) OutputFiles() []string {
	return []string{}
}

type alwaysPanicCheck struct{ baseCheck }

func (c alwaysPanicCheck) Check(*TestingOutputs) bool {
	panic("oh dear")
}

func (c alwaysPanicCheck) Name() string {
	return "always_panic"
}

func (c alwaysPanicCheck) DebugText() string {
	return ""
}

func (c alwaysPanicCheck) OutputFiles() []string {
	return []string{}
}

type alwaysFlakeCheck struct{ baseCheck }

func (c alwaysFlakeCheck) Check(*TestingOutputs) bool {
	return true
}

func (c alwaysFlakeCheck) Name() string {
	return "always_flake"
}

func (c alwaysFlakeCheck) IsFlake() bool {
	return true
}

type infraFailureCheck struct {
	baseCheck
}

func (c infraFailureCheck) Check(*TestingOutputs) bool {
	return true
}

func (c infraFailureCheck) Name() string {
	return "always_infra"
}

func (c infraFailureCheck) DebugText() string {
	return "Infra failure"
}

func (c infraFailureCheck) IsInfraFailure() bool {
	return true
}

func TestRunChecks(t *testing.T) {
	falseCheck := alwaysFalseCheck{}
	outputsDir := t.TempDir()
	trueCheck := alwaysTrueCheck{outputsDir: outputsDir}
	panicCheck := alwaysPanicCheck{}
	flakeCheck := alwaysFlakeCheck{}
	infraFailCheck := infraFailureCheck{}

	tests := []struct {
		name   string
		checks []FailureModeCheck
		want   []runtests.TestDetails
	}{
		{
			name: "mixed_checks",
			checks: []FailureModeCheck{
				flakeCheck, falseCheck, trueCheck, panicCheck,
			},
			want: []runtests.TestDetails{
				{
					Name:                 checkTestNamePrefix + flakeCheck.Name(),
					Status:               runtests.TestFailure,
					IsTestingFailureMode: true,
					TestResult:           runtests.TestResult{OutputFiles: []string{debugPathForCheck(flakeCheck)}},
				},
				{
					Name:                 checkTestNamePrefix + flakeCheck.Name(),
					Status:               runtests.TestSuccess,
					IsTestingFailureMode: true,
				},
				{
					Name:                 checkTestNamePrefix + trueCheck.Name(),
					Status:               runtests.TestFailure,
					IsTestingFailureMode: true,
					TestResult:           runtests.TestResult{OutputFiles: []string{debugPathForCheck(trueCheck)}},
				},
			},
		}, {
			name:   "infra_failure_check",
			checks: []FailureModeCheck{infraFailCheck},
			want: []runtests.TestDetails{
				{
					Name:                 checkTestNamePrefix + infraFailCheck.Name(),
					Status:               runtests.TestInfraFailure,
					IsTestingFailureMode: true,
					TestResult:           runtests.TestResult{OutputFiles: []string{debugPathForCheck(infraFailCheck)}},
				},
			},
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if test.name == "mixed_checks" {
				for _, of := range trueCheck.OutputFiles() {
					test.want[2].OutputFiles = append(test.want[2].OutputFiles, filepath.Base(of))
				}
			}
			startTime := time.Now()

			got, err := RunChecks(test.checks, nil, outputsDir)
			if err != nil {
				t.Error("RunChecks() failed with:", err)
			}
			for i, td := range got {
				if td.StartTime.Sub(startTime) < 0 {
					t.Errorf("start time should be later than %v, got %v", startTime, td.StartTime)
				}
				// Since the start time and duration are based on the current time, we should
				// set those values to the default values so that we don't check them when
				// comparing the actual and expected test details.
				var defaultTime time.Time
				got[i].StartTime = defaultTime
				got[i].DurationMillis = 0
				for _, outputFile := range td.OutputFiles {
					// RunChecks() is only responsible for writing the debug text to a file.
					if outputFile != test.want[i].OutputFiles[0] {
						continue
					}
					if _, err := os.Stat(filepath.Join(outputsDir, outputFile)); err != nil {
						t.Errorf("failed to stat OutputFile %s: %v", outputFile, err)
					}
				}
			}
			if diff := cmp.Diff(test.want, got, cmpopts.EquateEmpty()); diff != "" {
				t.Errorf("RunChecks() returned unexpected tests (-want +got):\n%s", diff)
			}
		})
	}
}

type syntheticCheck struct {
	baseCheck
}

func (c syntheticCheck) Check(*TestingOutputs) bool {
	return true
}

func (c syntheticCheck) Name() string {
	return "synthetic_check"
}

func (c syntheticCheck) EmitSyntheticTestCase() bool {
	return true
}

func (c syntheticCheck) FailureReason() string {
	return "synthetic failure"
}

type targetedCheck struct {
	syntheticCheck
	testName string
}

func (c targetedCheck) TestName() string {
	return c.testName
}

func TestRunChecks_TargetedSyntheticTestCase(t *testing.T) {
	summary := runtests.TestSummary{
		Tests: []runtests.TestDetails{
			{
				Name:   "passing_test",
				Status: runtests.TestSuccess,
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{},
				},
			},
			{
				Name:   "failing_test_1",
				Status: runtests.TestFailure,
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{},
				},
			},
			{
				Name:   "failing_test_2",
				Status: runtests.TestFailure,
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{},
				},
			},
		},
	}
	to := TestingOutputs{
		TestSummary: &summary,
	}
	outputsDir := t.TempDir()

	// This check should only be attributed to "failing_test_1".
	check := targetedCheck{testName: "failing_test_1"}

	_, err := RunChecks([]FailureModeCheck{check}, &to, outputsDir)
	if err != nil {
		t.Fatalf("RunChecks() failed: %v", err)
	}

	// Passing test should have NO synthetic case.
	if got := len(summary.Tests[0].Cases); got != 0 {
		t.Errorf("summary.Tests[0].Cases length = %d, want 0", got)
	}

	// failing_test_1 should have ONE synthetic case (Targeted attribution).
	if got := len(summary.Tests[1].Cases); got != 1 {
		t.Errorf("summary.Tests[1].Cases length = %d, want 1", got)
	}

	// failing_test_2 should have ZERO synthetic cases.
	if got := len(summary.Tests[2].Cases); got != 0 {
		t.Errorf("summary.Tests[2].Cases length = %d, want 0", got)
	}
}

func TestRunChecks_GlobalSyntheticTestCase(t *testing.T) {
	summary := runtests.TestSummary{
		Tests: []runtests.TestDetails{
			{
				Name:   "passing_test",
				Status: runtests.TestSuccess,
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{},
				},
			},
			{
				Name:   "failing_test_1",
				Status: runtests.TestFailure,
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{},
				},
			},
			{
				Name:   "failing_test_2",
				Status: runtests.TestFailure,
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{
						{
							CaseName:   "failing_test_2_test-case-name",
							Status:     runtests.TestFailure,
							FailReason: "failing_test_2_test-case-failure-reason",
						},
					},
				},
			},
		},
	}
	to := TestingOutputs{
		TestSummary: &summary,
	}
	outputsDir := t.TempDir()
	check := syntheticCheck{}

	_, err := RunChecks([]FailureModeCheck{check}, &to, outputsDir)
	if err != nil {
		t.Fatalf("RunChecks() failed: %v", err)
	}

	// Passing test should have NO synthetic case.
	if got := len(summary.Tests[0].Cases); got != 0 {
		t.Errorf("summary.Tests[0].Cases length = %d, want 0", got)
	}

	// Failing tests with no test cases should have exactly ONE synthetic case.
	if got := len(summary.Tests[1].Cases); got != 1 {
		t.Errorf("summary.Tests[1].Cases length = %d, want 1", got)
	} else {
		tc := summary.Tests[1].Cases[0]
		if tc.SuiteName != "tefmocheck" {
			t.Errorf("TestCase.SuiteName = %q, want %q", tc.SuiteName, "tefmocheck")
		}
		if tc.CaseName != check.Name() {
			t.Errorf("TestCase.CaseName = %q, want %q", tc.CaseName, check.Name())
		}
		if tc.FailReason != check.FailureReason() {
			t.Errorf("TestCase.FailReason = %q, want %q", tc.FailReason, check.FailureReason())
		}
	}

	// Failing tests with test cases should have exactly ONE synthetic case (one more test case than before).
	if got := len(summary.Tests[2].Cases); got != 2 {
		t.Errorf("summary.Tests[2].Cases length = %d, want 2", got)
	} else {
		tc := summary.Tests[2].Cases[1] // The last test case is the synthetic one.
		if tc.SuiteName != "tefmocheck" {
			t.Errorf("TestCase.SuiteName = %q, want %q", tc.SuiteName, "tefmocheck")
		}
		if tc.CaseName != check.Name() {
			t.Errorf("TestCase.CaseName = %q, want %q", tc.CaseName, check.Name())
		}
		if tc.FailReason != check.FailureReason() {
			t.Errorf("TestCase.FailReason = %q, want %q", tc.FailReason, check.FailureReason())
		}
	}
}

func TestRunChecks_TargetedSyntheticTestCase_NotFound(t *testing.T) {
	summary := runtests.TestSummary{
		Tests: []runtests.TestDetails{
			{
				Name:   "failing_test_1",
				Status: runtests.TestFailure,
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{},
				},
			},
			{
				Name:   "passing_test",
				Status: runtests.TestSuccess,
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{},
				},
			},
		},
	}
	to := TestingOutputs{
		TestSummary: &summary,
	}
	outputsDir := t.TempDir()

	t.Run("not_in_summary", func(t *testing.T) {
		check := targetedCheck{testName: "non_existent_test"}
		var buf bytes.Buffer
		log.SetOutput(&buf)
		defer log.SetOutput(os.Stderr)

		_, err := RunChecks([]FailureModeCheck{check}, &to, outputsDir)
		if err != nil {
			t.Fatalf("RunChecks() failed: %v", err)
		}
		wantLog := "Warning: targeted check synthetic_check attributed to test \"non_existent_test\" but test not found in summary"
		if !strings.Contains(buf.String(), wantLog) {
			t.Errorf("Log output %q does not contain expected warning %q", buf.String(), wantLog)
		}
	})

	t.Run("passing_test_in_summary", func(t *testing.T) {
		check := targetedCheck{testName: "passing_test"}
		var buf bytes.Buffer
		log.SetOutput(&buf)
		defer log.SetOutput(os.Stderr)

		_, err := RunChecks([]FailureModeCheck{check}, &to, outputsDir)
		if err != nil {
			t.Fatalf("RunChecks() failed: %v", err)
		}
		wantLog := "Warning: targeted check synthetic_check attributed to test \"passing_test\" but test not found in summary"
		if !strings.Contains(buf.String(), wantLog) {
			t.Errorf("Log output %q does not contain expected warning %q", buf.String(), wantLog)
		}
	})
}

type exonerationCheck struct {
	syntheticCheck
	testName string
}

func (c exonerationCheck) Name() string {
	return "exoneration_check"
}

func (c exonerationCheck) IsExoneration() bool {
	return true
}

func (c exonerationCheck) TestName() string {
	return c.testName
}

func TestRunChecks_Exoneration(t *testing.T) {
	summary := runtests.TestSummary{
		Tests: []runtests.TestDetails{
			{
				Name:   "failing_test",
				Status: runtests.TestFailure,
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{
						{
							CaseName: "failed_case",
							Status:   runtests.TestFailure,
						},
						{
							CaseName: "passed_case",
							Status:   runtests.TestSuccess,
						},
					},
				},
			},
		},
	}
	to := TestingOutputs{
		TestSummary: &summary,
	}
	outputsDir := t.TempDir()
	check := exonerationCheck{testName: "failing_test"}

	got, err := RunChecks([]FailureModeCheck{check}, &to, outputsDir)
	if err != nil {
		t.Fatalf("RunChecks() failed: %v", err)
	}

	// No synthetic tests should be returned for exoneration.
	if len(got) != 0 {
		t.Fatalf("RunChecks() returned %d tests, want 0", len(got))
	}

	// The actual test status should be updated to TestExonerated.
	if summary.Tests[0].Status != runtests.TestExonerated {
		t.Errorf("summary.Tests[0].Status = %v, want %v", summary.Tests[0].Status, runtests.TestExonerated)
	}

	// The failed test case status should be updated to TestExonerated.
	if summary.Tests[0].Cases[0].Status != runtests.TestExonerated {
		t.Errorf("summary.Tests[0].Cases[0].Status = %v, want %v", summary.Tests[0].Cases[0].Status, runtests.TestExonerated)
	}

	// The passed test case status should remain TestSuccess.
	if summary.Tests[0].Cases[1].Status != runtests.TestSuccess {
		t.Errorf("summary.Tests[0].Cases[1].Status = %v, want %v", summary.Tests[0].Cases[1].Status, runtests.TestSuccess)
	}

	// No synthetic test case should be added.
	if len(summary.Tests[0].Cases) != 2 {
		t.Fatalf("len(summary.Tests[0].Cases) = %d, want 2", len(summary.Tests[0].Cases))
	}
}
