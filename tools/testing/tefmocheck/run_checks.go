// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package tefmocheck

import (
	"fmt"
	"log"
	"os"
	"path/filepath"
	"time"

	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

const checkTestNamePrefix = "testing_failure_mode/"

func debugPathForCheck(check FailureModeCheck) string {
	return filepath.Join(checkTestNamePrefix, check.Name(), "debug.txt")
}

// RunChecks runs the given checks on the given TestingOutputs.
// A failed test means the Check() returned true. After the first failed test, all
// later Checks() will be skipped. Tests will not be returned for skipped or passed checks.
// Rationale: In order for these bugs to be useful, we want a failure to be associated only with the most
// specific, helpful failure modes, which then get routed to specific bugs. Hence only a single failure
// is returned.
// Rationale for not returning passed tests:
// We want to be able to add many checks without cluttering the output test summary
// with noise. Our flake detection system will identify a test that appears a a failure on
// one run of a swarming task, and then disappears on other runs of that same task as a flake.
func RunChecks(checks []FailureModeCheck, to *TestingOutputs, outputsDir string) ([]runtests.TestDetails, error) {
	var checkTests []runtests.TestDetails
	for _, check := range checks {
		if failed := check.Check(to); !failed {
			continue
		}

		if check.IsExoneration() {
			if to == nil || to.TestSummary == nil {
				log.Printf("Warning: exoneration check %s matched but TestingOutputs or TestSummary is nil; ignoring", check.Name())
				continue
			}
			attributedTestName := check.TestName()
			if attributedTestName == "" {
				log.Printf("Warning: exoneration check %s matched but is not attributed to a specific test; ignoring", check.Name())
				continue
			}
			foundMatch := false
			for i := range to.TestSummary.Tests {
				test := &to.TestSummary.Tests[i]
				if !runtests.IsFailure(test.Status) {
					continue
				}
				if test.Name != attributedTestName {
					continue
				}
				foundMatch = true
				test.Status = runtests.TestExonerated
				for j := range test.Cases {
					if runtests.IsFailure(test.Cases[j].Status) {
						test.Cases[j].Status = runtests.TestExonerated
					}
				}
			}
			if !foundMatch {
				log.Printf("Warning: exoneration check %s attributed to test %q but test not found in summary", check.Name(), attributedTestName)
			}
			// For exoneration, we don't emit synthetic test or test case.
			// Just update the status and continue.
			continue
		}

		// Some checks are difficult to attribute to a single test (e.g. syslogs and serial logs).
		// However, we would still like the check's FailureReason to be associated with a top-level
		// test's FailureReason.
		// By emitting synthetic test case(s) for failed test(s), we can attempt to attribute
		// potential failures to specific failure modes in our tracking systems (e.g. ResultDB),
		// making it easier to group and route failures.
		// There are two modes supported:
		// 1. **Targeted:** If the check is attributed to a specific test (via TestName()), we add
		//    the synthetic test case ONLY to that specific test. (See https://fxbug.dev/496991183)
		// 2. **Global:** If the check is not attributed to a specific test (e.g., broad syslog or
		//    serial log parse failures), we add it to ALL failed tests in the task. (See https://fxbug.dev/488476740)
		if check.EmitSyntheticTestCase() && to != nil && to.TestSummary != nil {
			attributedTestName := check.TestName()
			foundMatch := false
			for i := range to.TestSummary.Tests {
				test := &to.TestSummary.Tests[i]
				if runtests.IsFailure(test.Status) {
					if attributedTestName == "" || test.Name == attributedTestName {
						if attributedTestName != "" {
							foundMatch = true
						}
						test.Cases = append(test.Cases, runtests.TestCaseResult{
							DisplayName: "tefmocheck: " + check.Name(),
							SuiteName:   "tefmocheck",
							CaseName:    check.Name(),
							Status:      runtests.TestFailure,
							FailReason:  check.FailureReason(),
						})
					}
				}
			}
			if attributedTestName != "" && !foundMatch {
				log.Printf("Warning: targeted check %s attributed to test %q but test not found in summary", check.Name(), attributedTestName)
			}
		}

		testDetails := runtests.TestDetails{
			Name:                 checkTestNamePrefix + check.Name(),
			IsTestingFailureMode: true,
			Status:               runtests.TestFailure,
			TestResult: runtests.TestResult{
				// Specify an empty slice so it gets serialized to an empty JSON
				// array instead of null.
				Cases:         []runtests.TestCaseResult{},
				FailureReason: check.FailureReason(),
			},
			StartTime: time.Now(), // needed by ResultDB
			Tags:      check.Tags(),
		}
		// Check if failure is an infrastructure failure.
		if check.IsInfraFailure() {
			// Infra failures will have status value of "INFRA_FAIL"
			// in summary.json. This will help distinguish regular failures
			// which have status value "FAIL" from infra failures.
			testDetails.Status = runtests.TestInfraFailure
		}

		if len(outputsDir) > 0 {
			outputFile := debugPathForCheck(check)
			testDetails.OutputFiles = []string{outputFile}
			outputFileAbsPath := filepath.Join(outputsDir, outputFile)
			if err := os.MkdirAll(filepath.Dir(outputFileAbsPath), 0o777); err != nil {
				return nil, err
			}
			debugText := fmt.Sprintf(
				"This is a synthetic test that was produced by the tefmocheck tool during post-processing of test results. See https://fuchsia.googlesource.com/fuchsia/+/HEAD/tools/testing/tefmocheck/README.md\n%s",
				check.DebugText())
			if err := os.WriteFile(outputFileAbsPath, []byte(debugText), 0o666); err != nil {
				return nil, err
			}
		}
		for _, cof := range check.OutputFiles() {
			relPath, err := filepath.Rel(outputsDir, cof)
			if err != nil {
				return nil, err
			}
			testDetails.OutputFiles = append(testDetails.OutputFiles, relPath)
		}
		checkTests = append(checkTests, testDetails)
		if check.IsFlake() {
			checkTests = append(checkTests, runtests.TestDetails{
				Name:                 checkTestNamePrefix + check.Name(),
				IsTestingFailureMode: true,
				TestResult: runtests.TestResult{
					// Specify an empty slice so it gets serialized to an empty JSON
					// array instead of null.
					Cases: []runtests.TestCaseResult{},
				},
				Status:    runtests.TestSuccess,
				StartTime: time.Now(), // needed by ResultDB
			})
			// If this check was a flake, continue to see if we get another failure.
			continue
		}
		// We run more specific checks first, so it's not useful to run any checks
		// once we have our first failure.
		break
	}
	return checkTests, nil
}
