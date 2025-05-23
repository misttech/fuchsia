// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package resultdb

import (
	"encoding/json"
	"fmt"
	"log"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"time"

	resultpb "go.chromium.org/luci/resultdb/proto/v1"
	sinkpb "go.chromium.org/luci/resultdb/sink/proto/v1"
	"google.golang.org/protobuf/types/known/durationpb"
	"google.golang.org/protobuf/types/known/timestamppb"

	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

const (
	// Test ID is limited to 512 bytes.
	// https://source.chromium.org/chromium/infra/infra_superproject/+/main:infra/go/src/go.chromium.org/luci/resultdb/pbutil/test_id.go;l=508;drc=14e57c2183912ea2a9c93cd19dbe6eb7347283e8
	MaxTestIDLength = 512
	// Failure reason is limited to 1024 bytes.
	// https://source.chromium.org/chromium/infra/infra_superproject/+/main:infra/go/src/go.chromium.org/luci/resultdb/pbutil/test_result.go;l=44;drc=bfb50731e1b97d7ca771fb2d31dc7338c3db40f5
	MaxFailureReasonLength = 1024
)

// ParseSummary unmarshals the summary.json file content into runtests.TestSummary struct.
func ParseSummary(filePath string) (*runtests.TestSummary, error) {
	content, err := os.ReadFile(filePath)
	if err != nil {
		return nil, err
	}
	var summary runtests.TestSummary
	if err := json.Unmarshal(content, &summary); err != nil {
		return nil, err
	}
	return &summary, nil
}

// SummaryToResultSink converts runtests.TestSummary data into an array of result_sink TestResult.
func SummaryToResultSink(s *runtests.TestSummary, tags []*resultpb.StringPair, outputRoot string) ([]*sinkpb.TestResult, []string) {
	if len(outputRoot) == 0 {
		outputRoot, _ = os.Getwd()
	}
	rootPath, _ := filepath.Abs(outputRoot)
	var r []*sinkpb.TestResult
	var ts []string
	for _, test := range s.Tests {
		if len(test.Cases) > 0 {
			testCases, testsSkipped := testCaseToResultSink(test.Cases, tags, &test, rootPath)
			r = append(r, testCases...)
			ts = append(ts, testsSkipped...)
		}
		if testResult, testsSkipped, err := testDetailsToResultSink(tags, &test, rootPath); err == nil {
			r = append(r, testResult)
			ts = append(ts, testsSkipped...)
		}
	}
	return r, ts
}

// InvocationLevelArtifacts creates resultdb artifacts for invocation-level files to be sent to ResultDB.
func InvocationLevelArtifacts(outputRoot string, invocationArtifacts []string) map[string]*sinkpb.Artifact {
	if len(outputRoot) == 0 {
		outputRoot, _ = os.Getwd()
	}
	rootPath, _ := filepath.Abs(outputRoot)
	artifacts := map[string]*sinkpb.Artifact{}

	// TODO(ihuh): Remove once these are passed in through recipes.
	if len(invocationArtifacts) == 0 {
		invocationArtifacts = []string{
			"infra_and_test_std_and_klog.txt",
			"serial_log.txt",
			"syslog.txt",
			"triage_output",
		}
	}
	for _, invocationArtifact := range invocationArtifacts {
		artifactFile := filepath.Join(rootPath, invocationArtifact)
		if isReadable(artifactFile) {
			artifacts[invocationArtifact] = &sinkpb.Artifact{
				Body:        &sinkpb.Artifact_FilePath{FilePath: artifactFile},
				ContentType: "text/plain",
			}
		}
	}
	return artifacts
}

func ProcessSummaries(summaries []string, tags []*resultpb.StringPair, outputRoot string) ([]*sinkpb.ReportTestResultsRequest, []string, error) {
	var requests []*sinkpb.ReportTestResultsRequest
	var allTestsSkipped []string

	for _, summaryFile := range summaries {
		summary, err := ParseSummary(summaryFile)
		if err != nil {
			return nil, nil, err
		}
		testResults, testsSkipped := SummaryToResultSink(summary, tags, outputRoot)
		requests = append(requests, createTestResultsRequests(testResults, 250)...)
		allTestsSkipped = append(allTestsSkipped, testsSkipped...)
	}

	return requests, allTestsSkipped, nil
}

// artifactName returns a unique name to correspond to the file which
// will be uploaded as a resultDB artifact.
func artifactName(file string) string {
	re := regexp.MustCompile(`[^a-zA-Z0-9-_.\/]`)
	invalidChars := re.FindAllString(file, -1)
	for _, ch := range invalidChars {
		file = strings.ReplaceAll(file, ch, "_")
	}
	return file
}

// testCaseToResultSink converts TestCaseResult defined in //tools/testing/testparser/result.go
// to ResultSink's TestResult. A testcase will not be converted if test result cannot be
// mapped to result_sink.Status.
func testCaseToResultSink(testCases []runtests.TestCaseResult, tags []*resultpb.StringPair, testDetail *runtests.TestDetails, outputRoot string) ([]*sinkpb.TestResult, []string) {
	var testResult []*sinkpb.TestResult
	var testsSkipped []string

	// Ignore error, testStatus will be set to resultpb.TestStatus_STATUS_UNSPECIFIED if error != nil.
	// And when passed to determineExpected, resultpb.TestStatus_STATUS_UNSPECIFIED will be handled correctly.
	testStatus, _ := resultDBStatus(testDetail.Result)

	for _, testCase := range testCases {
		testID := fmt.Sprintf("%s/%s:%s", testDetail.Name, testCase.SuiteName, testCase.CaseName)
		if len(testID) > MaxTestIDLength {
			log.Printf("[ERROR] Skip uploading to ResultDB due to test_id exceeding %d bytes max limit: %q", MaxTestIDLength, testID)
			testsSkipped = append(testsSkipped, testID)
			continue
		}
		testCaseTags := append([]*resultpb.StringPair{
			{Key: "format", Value: testCase.Format},
			{Key: "is_test_case", Value: "true"},
		}, tags...)
		for _, tag := range testCase.Tags {
			testCaseTags = append(testCaseTags, &resultpb.StringPair{
				Key: tag.Key, Value: tag.Value,
			})
		}
		r := sinkpb.TestResult{
			TestId: testID,
			Tags:   testCaseTags,
		}
		testCaseStatus, err := resultDBStatus(testCase.Status)
		if err != nil {
			log.Printf("[Warn] Skip uploading testcase: %s to ResultDB due to error: %v", testID, err)
			continue
		}
		if testCase.FailReason != "" {
			r.FailureReason = &resultpb.FailureReason{PrimaryErrorMessage: truncateString(testCase.FailReason, MaxFailureReasonLength)}
		}
		r.Status = testCaseStatus
		r.StartTime = timestamppb.New(testDetail.StartTime)
		if testCase.Duration > 0 {
			r.Duration = durationpb.New(testCase.Duration)
		}
		r.Expected = determineExpected(testStatus, testCaseStatus)
		r.Artifacts = make(map[string]*sinkpb.Artifact)
		for _, of := range testCase.OutputFiles {
			outputFile := filepath.Join(outputRoot, testDetail.OutputDir, of)
			if isReadable(outputFile) {
				r.Artifacts[artifactName(of)] = &sinkpb.Artifact{
					Body: &sinkpb.Artifact_FilePath{FilePath: outputFile},
				}
			} else {
				log.Printf("[Warn] outputFile: %s is not readable, skip.", outputFile)
			}
		}

		testResult = append(testResult, &r)
	}
	return testResult, testsSkipped
}

// testDetailsToResultSink converts TestDetail defined in /tools/testing/runtests/runtests.go
// to ResultSink's TestResult. Returns (nil, error) if a test result cannot be mapped to
// result_sink.Status
func testDetailsToResultSink(tags []*resultpb.StringPair, testDetail *runtests.TestDetails, outputRoot string) (*sinkpb.TestResult, []string, error) {
	var testsSkipped []string
	if len(testDetail.Name) > MaxTestIDLength {
		testsSkipped = append(testsSkipped, testDetail.Name)
		log.Printf("[ERROR] Skip uploading to ResultDB due to test_id exceeding %d bytes max limit: %q", MaxTestIDLength, testDetail.Name)
		return nil, testsSkipped, fmt.Errorf("The test name exceeds %d bytes max limit: %q ", MaxTestIDLength, testDetail.Name)
	}
	testTags := append([]*resultpb.StringPair{
		{Key: "gn_label", Value: testDetail.GNLabel},
		{Key: "test_case_count", Value: strconv.Itoa(len(testDetail.Cases))},
		{Key: "affected", Value: strconv.FormatBool(testDetail.Affected)},
		{Key: "is_top_level_test", Value: "true"},
	}, tags...)
	for _, tag := range testDetail.Tags {
		testTags = append(testTags, &resultpb.StringPair{
			Key: tag.Key, Value: tag.Value,
		})
	}
	r := sinkpb.TestResult{
		TestId: testDetail.Name,
		Tags:   testTags,
	}
	testStatus, err := resultDBStatus(testDetail.Result)
	if err != nil {
		log.Printf("[Warn] Skip uploading test target: %s to ResultDB due to error: %v", testDetail.Name, err)
		return nil, testsSkipped, err
	}
	r.Status = testStatus

	r.StartTime = timestamppb.New(testDetail.StartTime)
	if testDetail.DurationMillis > 0 {
		r.Duration = durationpb.New(time.Duration(testDetail.DurationMillis) * time.Millisecond)
	}
	r.Artifacts = make(map[string]*sinkpb.Artifact)
	for _, of := range testDetail.OutputFiles {
		outputFile := filepath.Join(outputRoot, testDetail.OutputDir, of)
		if isReadable(outputFile) {
			r.Artifacts[artifactName(of)] = &sinkpb.Artifact{
				Body: &sinkpb.Artifact_FilePath{FilePath: outputFile},
			}
		} else {
			log.Printf("[Warn] outputFile: %s is not readable, skip.", outputFile)
		}
	}

	r.Expected = determineExpected(testStatus, resultpb.TestStatus_STATUS_UNSPECIFIED)
	if testDetail.FailureReason != "" {
		r.FailureReason = &resultpb.FailureReason{
			PrimaryErrorMessage: truncateString(testDetail.FailureReason, MaxFailureReasonLength),
		}
	} else if hasFailedTest(testDetail) {
		r.FailureReason = &resultpb.FailureReason{
			PrimaryErrorMessage: createDefaultTopLevelFailureReason(testDetail),
		}
	}
	if testDetail.Metadata.ComponentID > 0 {
		r.TestMetadata = &resultpb.TestMetadata{
			Name: testDetail.Name,
			BugComponent: &resultpb.BugComponent{
				System: &resultpb.BugComponent_IssueTracker{
					IssueTracker: &resultpb.IssueTrackerComponent{
						ComponentId: int64(testDetail.Metadata.ComponentID),
					},
				},
			},
		}
	}
	if len(testDetail.Metadata.Owners) > 0 {
		listOfOwners := testDetail.Metadata.Owners
		truncatedListOfOwners := listOfOwners
		if len(listOfOwners) > 5 {
			truncatedListOfOwners = listOfOwners[:5]
		}
		owners := strings.Join(truncatedListOfOwners, ",")
		r.Tags = append(r.Tags, &resultpb.StringPair{Key: "owners", Value: owners})
	}
	return &r, testsSkipped, nil
}

func hasFailedTest(topLevelTest *runtests.TestDetails) bool {
	for _, testCase := range topLevelTest.Cases {
		if testCase.Status == runtests.TestFailure {
			return true
		}
	}
	return false
}

func createDefaultTopLevelFailureReason(topLevelTest *runtests.TestDetails) string {
	var builder strings.Builder
	failedTestCaseCount := 0
	for _, testCase := range topLevelTest.Cases {
		if testCase.Status == runtests.TestFailure {
			if builder.Len() > 0 {
				builder.WriteString("\n")
			}
			builder.WriteString(fmt.Sprintf("%s: test case failed", testCase.CaseName))
			failedTestCaseCount++
		}
	}
	failureReason := builder.String()
	if len(failureReason) > MaxFailureReasonLength {
		failureReason = fmt.Sprintf("%d test cases failed", failedTestCaseCount)
	}
	return failureReason
}

// determineExpected checks if a test result is expected.
//
// For example, if a test case failed but fail is the correct behavior, we will mark
// expected to true. On the other hand, if a test case failed and failure is the incorrect
// behavior then we will mark expected to false. This is completely determined by
// the status recorded by the test suite vs. status recorded for the test case.
//
// If a test is reported "PASS", then we will report all test cases within the same
// test to pass as well. If a test is reported other than "PASS" or "SKIP", we will
// process the test cases based on the test case result.
func determineExpected(testStatus resultpb.TestStatus, testCaseStatus resultpb.TestStatus) bool {
	switch testStatus {
	case resultpb.TestStatus_PASS, resultpb.TestStatus_SKIP:
		return true
	case resultpb.TestStatus_FAIL, resultpb.TestStatus_CRASH, resultpb.TestStatus_ABORT, resultpb.TestStatus_STATUS_UNSPECIFIED:
		switch testCaseStatus {
		case resultpb.TestStatus_PASS, resultpb.TestStatus_SKIP:
			return true
		case resultpb.TestStatus_FAIL, resultpb.TestStatus_CRASH, resultpb.TestStatus_ABORT, resultpb.TestStatus_STATUS_UNSPECIFIED:
			return false
		}
	}
	return false
}

func resultDBStatus(result runtests.TestResult) (resultpb.TestStatus, error) {
	switch result {
	case runtests.TestSuccess:
		return resultpb.TestStatus_PASS, nil
	case runtests.TestFailure:
		return resultpb.TestStatus_FAIL, nil
	case runtests.TestSkipped:
		return resultpb.TestStatus_SKIP, nil
	case runtests.TestAborted:
		return resultpb.TestStatus_ABORT, nil
	case runtests.TestInfraFailure:
		return resultpb.TestStatus_CRASH, nil
	}
	return resultpb.TestStatus_STATUS_UNSPECIFIED, fmt.Errorf("cannot map Result: %s to result_sink test_result status", result)
}

func isReadable(p string) bool {
	if len(p) == 0 {
		return false
	}
	info, err := os.Stat(p)
	if err != nil {
		return false
	}
	if info.IsDir() {
		return false
	}
	f, err := os.Open(p)
	if err != nil {
		return false
	}
	_ = f.Close()
	return true
}

func truncateString(str string, maxLength int) string {
	if len(str) <= maxLength {
		return str
	}
	// We want to append "..." to maxLength, which takes up 3 spaces. If maxLength is less than that, just return empty.
	if maxLength <= 3 {
		return ""
	}
	runes := []rune(str)
	byteCount := 0
	for _, char := range runes {
		if byteCount+len(string(char)) > (maxLength - 3) {
			if byteCount == 0 {
				return ""
			}
			return str[:byteCount] + "..."
		}
		byteCount = byteCount + len(string(char))
	}
	return str
}

// createTestResultsRequests breaks an array of resultpb.TestResult into an array of resultpb.ReportTestResultsRequest
// chunkSize defined the number of TestResult contained in each ReportTrestResultsRequest.
func createTestResultsRequests(results []*sinkpb.TestResult, chunkSize int) []*sinkpb.ReportTestResultsRequest {
	totalChunks := (len(results)-1)/chunkSize + 1
	requests := make([]*sinkpb.ReportTestResultsRequest, totalChunks)
	for i := 0; i < totalChunks; i++ {
		requests[i] = &sinkpb.ReportTestResultsRequest{
			TestResults: make([]*sinkpb.TestResult, 0, chunkSize),
		}
	}
	for i, result := range results {
		requestIndex := i / chunkSize
		requests[requestIndex].TestResults = append(requests[requestIndex].TestResults, result)
	}
	return requests
}
