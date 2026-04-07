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

// set the TestMetadata on TestResult
func setTestMetadata(r *sinkpb.TestResult, testDetail runtests.TestDetails) {
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
}

// testCaseToResultSink converts TestCaseResult defined in //tools/testing/runtests/runtests.go
// to ResultSink's TestResult. A testcase will not be converted if test result cannot be
// mapped to result_sink.Status.
func testCaseToResultSink(testCases []runtests.TestCaseResult, tags []*resultpb.StringPair, testDetail *runtests.TestDetails, outputRoot string) ([]*sinkpb.TestResult, []string) {
	var testResult []*sinkpb.TestResult
	var testsSkipped []string

	// Ignore the failure reason kind and error. We only check the top-level test status
	// to see if it passed, which would mean that a failed result for a test case is
	// expected and thus should be reported as a passed result.
	testStatus, _, _ := resultDBStatus(testDetail.Status)

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
		testCaseStatus, testCaseFailureReasonKind, err := resultDBStatus(testCase.Status)
		if err != nil {
			log.Printf("[Warn] Skip uploading testcase: %s to ResultDB due to error: %v", testID, err)
			continue
		}
		if testStatus != resultpb.TestResult_PASSED && testCaseStatus == resultpb.TestResult_FAILED {
			r.FailureReason = &resultpb.FailureReason{Kind: testCaseFailureReasonKind}
			if testCase.FailReason != "" {
				r.FailureReason.Errors = []*resultpb.FailureReason_Error{{Message: truncateString(testCase.FailReason, MaxFailureReasonLength)}}
			}
		} else if testCaseStatus == resultpb.TestResult_SKIPPED {
			r.SkippedReason = &resultpb.SkippedReason{Kind: resultpb.SkippedReason_DISABLED_AT_DECLARATION}
		} else if testStatus == resultpb.TestResult_PASSED {
			testCaseStatus = testStatus
		}
		r.StatusV2 = testCaseStatus
		r.StartTime = timestamppb.New(testDetail.StartTime)
		if testCase.Duration > 0 {
			r.Duration = durationpb.New(testCase.Duration)
		}
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
		setTestMetadata(&r, *testDetail)
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
		// Most consumers should use `source_label` rather than `gn_label` since
		// it better corresponds to the location of the test's source code for
		// Bazel tests.
		{Key: "source_label", Value: testDetail.SourceLabel},
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
	testStatus, failureReasonKind, err := resultDBStatus(testDetail.Status)
	if err != nil {
		log.Printf("[Warn] Skip uploading test target: %s to ResultDB due to error: %v", testDetail.Name, err)
		return nil, testsSkipped, err
	}
	r.StatusV2 = testStatus
	if testStatus == resultpb.TestResult_FAILED {
		r.FailureReason = &resultpb.FailureReason{Kind: failureReasonKind, Errors: []*resultpb.FailureReason_Error{{Message: createTopLevelFailureReason(testDetail)}}}
	} else if testStatus == resultpb.TestResult_SKIPPED {
		r.SkippedReason = &resultpb.SkippedReason{Kind: resultpb.SkippedReason_OTHER, ReasonMessage: "skipped because unaffected"}
	}

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

	setTestMetadata(&r, *testDetail)
	return &r, testsSkipped, nil
}

func createTopLevelFailureReason(topLevelTest *runtests.TestDetails) string {
	if topLevelTest.FailureReason != "" {
		return truncateString(topLevelTest.FailureReason, MaxFailureReasonLength)
	}
	var builder strings.Builder
	for _, testCase := range topLevelTest.Cases {
		if testCase.Status != runtests.TestSuccess {
			var text string
			if testCase.FailReason != "" {
				text = fmt.Sprintf("%s: %s", testCase.CaseName, testCase.FailReason)
			} else if testCase.Status == runtests.TestFailure {
				text = fmt.Sprintf("%s: test case failed", testCase.CaseName)
			}

			if text != "" {
				if builder.Len() > 0 {
					builder.WriteString("\n")
				}
				builder.WriteString(text)
			}
		}
	}
	if builder.Len() == 0 {
		return ""
	}

	// Truncate to max length.
	failureReason := builder.String()
	if len(failureReason) > MaxFailureReasonLength {
		failureReason = failureReason[:MaxFailureReasonLength]
	}
	return failureReason
}

func resultDBStatus(result runtests.TestStatus) (resultpb.TestResult_Status, resultpb.FailureReason_Kind, error) {
	switch result {
	case runtests.TestSuccess:
		return resultpb.TestResult_PASSED, resultpb.FailureReason_KIND_UNSPECIFIED, nil
	case runtests.TestFailure:
		return resultpb.TestResult_FAILED, resultpb.FailureReason_ORDINARY, nil
	case runtests.TestSkipped:
		return resultpb.TestResult_SKIPPED, resultpb.FailureReason_KIND_UNSPECIFIED, nil
	case runtests.TestAborted:
		return resultpb.TestResult_FAILED, resultpb.FailureReason_TIMEOUT, nil
	case runtests.TestInfraFailure:
		return resultpb.TestResult_FAILED, resultpb.FailureReason_CRASH, nil
	}
	return resultpb.TestResult_STATUS_UNSPECIFIED, resultpb.FailureReason_KIND_UNSPECIFIED, fmt.Errorf("cannot map Result: %s to result_sink test_result status", result)
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
