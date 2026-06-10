// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Package runtests contains specifics related to the runtests command.
package runtests

import (
	"time"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/integration/testsharder/metadata"
)

// TODO(olivernewman): Move the contents of this file into a separate library as
// it's no longer specific to runtests - in fact, runtests only implements a
// subset of the types and fields defined here.

const (
	// TestSummaryFilename is the summary file name expected by the fuchsia
	// recipe module.
	TestSummaryFilename = "summary.json"

	// TestOutputFilename is the default output file name for a test.
	TestOutputFilename = "stdout-and-stderr.txt"
)

// TestStatus is the exit status of a test.
type TestStatus string

// Possible test statuses, names chosen for consistency with ResultDB:
// https://chromium.googlesource.com/infra/luci/luci-go/+/15458901113063d2a0d95be2e27a245c4cfd5690/resultdb/proto/v1/test_result.proto#142
const (
	// TestSuccess represents a passed test.
	TestSuccess TestStatus = "PASS"

	// TestFailure represents a failed test.
	TestFailure TestStatus = "FAIL"

	// TestAborted represents an aborted test (likely a timeout).
	TestAborted TestStatus = "ABORT"

	// TestSkipped represents a skipped test.
	TestSkipped TestStatus = "SKIP"

	// TestInfraFailure means that the test failed because of infra related issue.
	TestInfraFailure TestStatus = "INFRA_FAIL"

	// TestExonerated represents an exonerated test.
	TestExonerated TestStatus = "EXONERATED"
)

// IsFailure returns whether a test result corresponds to any failure condition
// (failure, timeout, etc.).
func IsFailure(tr TestStatus) bool {
	return tr != TestSuccess && tr != TestSkipped
}

// TestSummary is a summary of a suite of test runs. It represents the output
// file format of a runtests invocation.
type TestSummary struct {
	// Tests is a list of the details of the test runs.
	Tests []TestDetails `json:"tests"`

	// Outputs gives the suite-wide outputs, mapping canonical name of the
	// output to its path.
	Outputs map[string]string `json:"outputs,omitempty"`
}

// DataSink is a data sink exported by the test.
type DataSink struct {
	// Name is the name of the sink.
	Name string `json:"name"`

	// File is the file containing the sink data.
	File string `json:"file"`
}

// DataSinkMap is mapping from a data sink name to a list of the corresponding
// data sink files.
type DataSinkMap map[string][]DataSink

// TestDetails contains the details of a test run.
type TestDetails struct {
	TestResult

	// Name is the name of the test.
	Name string `json:"name"`

	// GNLabel is label of the test target (with toolchain).
	GNLabel string `json:"gn_label"`

	// SourceLabel is the build-system agnostic (either GN or Bazel) label corresponding to the test.
	SourceLabel string `json:"source_label"`

	// Status is the status of the test.
	Status TestStatus `json:"result"`

	// DataSinks gives the data sinks attached to a test.
	DataSinks DataSinkMap `json:"data_sinks,omitempty"`

	// StartTime is the UTC time when the test was started.
	StartTime time.Time `json:"start_time"`

	// EndTime is the UTC time when the test completed. Used to calculate the DurationMillis.
	EndTime time.Time `json:"-"`

	// Duration is how long the test execution took.
	DurationMillis int64 `json:"duration_milliseconds"`

	// IsTestingFailureMode is true iff this test was produced by tefmocheck.
	IsTestingFailureMode bool `json:"is_testing_failure_mode"`

	// Affected indicates whether the test is affected by the change under test.
	// It will only be set for tests running within tryjobs.
	Affected bool `json:"affected"`

	// Tags contain test metadata.
	Tags []build.TestTag `json:"tags"`

	// Test metadata
	Metadata metadata.TestMetadata `json:"metadata,omitempty"`

	// RunIndex is the index of this test run among all the runs of the same test.
	RunIndex int `json:"-"`

	// The combined stdout and stderr from this test.
	Stdio []byte `json:"-"`
}

// Passed indicates whether the test completed successfully. This will be false
// if the test timed out or failed.
func (r TestDetails) Passed() bool {
	return r.Status == TestSuccess
}

func (r TestDetails) Duration() time.Duration {
	return r.EndTime.Sub(r.StartTime)
}

// TestResult contains the details of a test run. A test can structure its results
// in this format and write it to TEST_OUTPUT_SUMMARY_PATH.
type TestResult struct {
	// OutputFiles are paths to the test's output files relative to the OutputDir.
	OutputFiles []string `json:"output_files"`

	// OutputDir is the common dir that all the OutputFiles should be located in.
	OutputDir string `json:"output_dir"`

	// Cases is individual test case results.
	Cases []TestCaseResult `json:"cases"`

	// FailureReason is an optional human-readable error message or explanation of the failure.
	// This will be ignored if the test status is a success. Host tests must exit with
	// a non-zero exit code to be considered a failure.
	FailureReason string `json:"error_line,omitempty"`
}

// TestCaseResult contains the details of a single test case, nested within a
// top-level TestDetails.
type TestCaseResult struct {
	DisplayName string        `json:"display_name"`
	SuiteName   string        `json:"suite_name"`
	CaseName    string        `json:"case_name"`
	Status      TestStatus    `json:"status"`
	Duration    time.Duration `json:"duration_nanos"`
	// Format is the test runner used to execute the test.
	Format string `json:"format"`
	// FailReason is a concise and distinctive error message captured from stdout when the test case fails.
	// The message is used to group similar failures and shouldn't contain stacktrace or line numbers.
	FailReason  string   `json:"fail_reason"`
	OutputFiles []string `json:"output_files,omitempty"`
	// The directory where the OutputFiles live if given as relative paths.
	OutputDir string `json:"output_dir,omitempty"`
	// Tags contain test case metadata.
	Tags []build.TestTag `json:"tags"`
}
