// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package mobly_test_parser

import (
	"bytes"
	"errors"
	"fmt"
	"io"
	"os"
	"regexp"
	"time"

	"gopkg.in/yaml.v2"

	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

const (
	// LINT.IfChange(mobly_test_end)
	moblyTestResultYAMLHeaderPatternStr = `^\[=====MOBLY RESULTS=====\]$`
	// LINT.ThenChange(//src/testing/end_to_end/mobly_driver/mobly_driver/api/api_infra.py:mobly_test_end)
	// LINT.IfChange(mobly_test_timeout)
	moblyTimeoutType = "MoblyDriverTimeout"
	// LINT.ThenChange(//src/testing/end_to_end/mobly_driver/mobly_driver/api/api_infra.py:mobly_test_timeout)
	moblyTestCaseType       = "Record"
	moblyTestNameListType   = "TestNameList"
	moblySummaryType        = "Summary"
	moblyTestAbortAllSignal = "TestAbortAll"
)

var (
	moblyTestResultYAMLHeaderPattern = regexp.MustCompile(moblyTestResultYAMLHeaderPatternStr)
)

type moblyTestCase struct {
	BeginTimeMillis int `yaml:"Begin Time"`
	// Description of the cause for test case termination.
	// This is set to empty string for passed test cases.
	Details        string   `yaml:"Details"`
	EndTimeMillis  int      `yaml:"End Time"`
	RequestedTests []string `yaml:"Requested Tests"`
	Result         string   `yaml:"Result"`
	TestClass      string   `yaml:"Test Class"`
	TestName       string   `yaml:"Test Name"`
	// TerminationSignal is name of the Python exception class raised on crash/failure.
	TerminationSignal string `yaml:"Termination Signal Type"`
	// Type describes the Mobly YAML document entry type which is in the set of
	// (Record, TestNameList, Summary, ControllerInfo, UserData, MoblyDriverTimeout)
	Type string `yaml:"Type"`
}

func matchYAMLHeader(lines [][]byte) [][]byte {
	for num, line := range lines {
		if moblyTestResultYAMLHeaderPattern.Match(line) {
			// Skip the matched YAML header line.
			return lines[num+1:]
		}
	}
	fmt.Fprintf(os.Stderr, "Unepxected Mobly output, Mobly result YAML header line missing: %s\n", moblyTestResultYAMLHeaderPatternStr)
	return nil
}

// Parse takes stdout from a mobly test program and returns structured results.
// If no structured results were identified, an empty slice is returned.
func Parse(stdout []byte) []runtests.TestCaseResult {
	lines := bytes.Split(stdout, []byte{'\n'})
	cases := parseMoblyTest(lines)

	// Ensure that an empty set of cases is serialized to JSON as an empty
	// array, not as null.
	if cases == nil {
		cases = []runtests.TestCaseResult{}
	}
	return cases
}

func parseMoblyTest(lines [][]byte) []runtests.TestCaseResult {
	var res []runtests.TestCaseResult

	// Find YAML header and decode YAML document.
	yamlLines := matchYAMLHeader(lines)
	data := bytes.Join(yamlLines, []byte{'\n'})
	reader := bytes.NewReader(data)
	d := yaml.NewDecoder(reader)

	// Mobly reports a "summary" record at the end of the YAML document. Its
	// presence means that the Mobly test ran to completion.
	summarySeen := false
	timeoutSeen := false
	var timeoutDetails string
	var abortedCaseIndices []int
	var firstTestClass string
	var requestedTests []string
	executedTests := make(map[string]bool)

	// Since yaml.Unmarshal() is only capable of parsing a single document,
	// we use a for-loop and yaml.Decoder() to handle the remaining documents.
	for {
		var tc moblyTestCase
		if err := d.Decode(&tc); err != nil {
			// Break the loop at EOF.
			if errors.Is(err, io.EOF) {
				break
			}

			fmt.Fprintf(os.Stderr, "Error unmarshaling YAML: %s\n", err)
			break
		}

		if tc.TestClass != "" && firstTestClass == "" {
			firstTestClass = tc.TestClass
		}

		// Skip records that are not test cases.
		if tc.Type != moblyTestCaseType {
			switch tc.Type {
			case moblySummaryType:
				summarySeen = true
			case moblyTimeoutType:
				timeoutSeen = true
				timeoutDetails = tc.Details
			case moblyTestNameListType:
				requestedTests = append(requestedTests, tc.RequestedTests...)
			}
			continue
		}

		executedTests[tc.TestName] = true

		var status runtests.TestStatus
		switch tc.Result {
		case "PASS":
			status = runtests.TestSuccess
		case "FAIL":
			status = runtests.TestFailure
		case "SKIP":
			// If an earlier test fails and prevents later tests from running (or if
			// an error occurs in setup_class), Mobly marks the remaining tests as
			// SKIP and sets TerminationSignal to the exception that caused the abort.
			// Omit these results since the tests did not actually run.
			if len(tc.TerminationSignal) != 0 && tc.TerminationSignal != "TestSkip" {
				continue
			}
			status = runtests.TestSkipped
		case "ERROR":
			status = runtests.TestFailure
		}

		displayName := fmt.Sprintf("%s.%s", tc.TestClass, tc.TestName)

		if tc.TerminationSignal == moblyTestAbortAllSignal {
			abortedCaseIndices = append(abortedCaseIndices, len(res))
		}

		failureReason := tc.Details
		if len(tc.TerminationSignal) != 0 {
			failureReason = fmt.Sprintf("[%s] %s", tc.TerminationSignal, failureReason)
		}

		var failureReasonStruct *runtests.FailureReason
		if runtests.IsFailure(status) {
			failureReasonStruct = runtests.FailureReasonFromMessage(failureReason)
		}

		res = append(res, runtests.TestCaseResult{
			DisplayName:   displayName,
			FailureReason: failureReasonStruct,
			SuiteName:     tc.TestClass,
			CaseName:      tc.TestName,
			Status:        status,
			Duration:      time.Duration(tc.EndTimeMillis-tc.BeginTimeMillis) * time.Millisecond,
			Format:        "Mobly",
		})
	}

	if timeoutSeen {
		reason := timeoutDetails
		if reason == "" {
			reason = "Test timed out."
		}
		if len(abortedCaseIndices) > 0 {
			for _, idx := range abortedCaseIndices {
				res[idx].Status = runtests.TestAborted
				res[idx].FailureReason = runtests.FailureReasonFromMessage(
					fmt.Sprintf("[%s] %s: %s", moblyTimeoutType, res[idx].DisplayName, reason),
				)
			}
		} else {
			var nextTest string
			for _, name := range requestedTests {
				if !executedTests[name] {
					nextTest = name
					break
				}
			}
			var target string
			if firstTestClass != "" && nextTest != "" {
				target = fmt.Sprintf("%s.%s", firstTestClass, nextTest)
			} else if firstTestClass != "" {
				target = firstTestClass
			} else if nextTest != "" {
				target = nextTest
			}
			msg := fmt.Sprintf("[%s] %s", moblyTimeoutType, reason)
			if target != "" {
				msg = fmt.Sprintf("[%s] %s: %s", moblyTimeoutType, target, reason)
			}
			res = append(res, runtests.TestCaseResult{
				DisplayName:   moblyTimeoutType,
				FailureReason: runtests.FailureReasonFromMessage(msg),
				SuiteName:     "Synthetic",
				CaseName:      "Synthetic",
				Status:        runtests.TestAborted,
				Format:        "Mobly",
			})
		}
	} else if !summarySeen {
		// Generate a synthetic result if the summary record is not seen, which
		// indicates that the test crashed or exited before Mobly finished.
		res = append(res, runtests.TestCaseResult{
			DisplayName: "TestparserError",
			FailureReason: runtests.FailureReasonFromMessage(
				"[TestparserError] Missing Mobly summary record - potental infra timeout.",
			),
			SuiteName: "Synthetic",
			CaseName:  "Synthetic",
			Status:    runtests.TestFailure,
			Format:    "Mobly",
		})
	}
	return res
}
