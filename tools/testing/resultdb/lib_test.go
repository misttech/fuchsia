// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package resultdb

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"testing"
	"time"

	resultpb "go.chromium.org/luci/resultdb/proto/v1"
	sinkpb "go.chromium.org/luci/resultdb/sink/proto/v1"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/proto"
	"google.golang.org/protobuf/testing/protocmp"
	"google.golang.org/protobuf/types/known/structpb"

	"github.com/google/go-cmp/cmp"
	"github.com/google/go-cmp/cmp/cmpopts"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/integration/testsharder/metadata"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

func TestParseSummary(t *testing.T) {
	const testCount = 10
	summary := createTestSummary(testCount)
	testResults, _, _ := SummaryToResultSink(summary, []*resultpb.StringPair{}, "")
	if len(testResults) != testCount {
		t.Errorf(
			"Parsed incorrect number of resultdb tests in TestSummary, got %d, want %d",
			len(testResults), testCount)
	}
	requests := createTestResultsRequests(testResults, testCount)
	if len(requests) != 1 {
		t.Errorf(
			"Grouped incorrect chunks of ResultDB sink requests, got %d, want 1",
			len(requests))
	}
	if len(requests[0].TestResults) != testCount {
		t.Errorf(
			"Incorrect number of TestResult in the first chunk, got %d, want %d",
			len(requests[0].TestResults), testCount)
	}
	if requests[0].TestResults[0].TestId != "test_0" {
		t.Errorf("Incorrect TestId parsed for first suite. got %s, want test_0", requests[0].TestResults[0].TestId)
	}
}

func TestSetTestDetailsToResultSink(t *testing.T) {
	outputRoot := t.TempDir()
	detail := createTestDetailWithPassedAndFailedTestCase(5, 2, outputRoot)
	expectedFailureReason := "some failure reason reported by test runner"
	detail.FailureReason = runtests.FailureReasonFromMessage(expectedFailureReason)
	// include 7 owners to test truncation of owner list
	detail.Metadata = metadata.TestMetadata{
		Owners: []string{
			"testgoogler1@google.com",
			"testgoogler2@google.com",
			"testgoogler3@google.com",
			"testgoogler4@google.com",
			"testgoogler5@google.com",
			"testgoogler6@google.com",
			"testgoogler7@google.com",
		},
		ComponentID: 1478143,
	}
	extraTags := []*resultpb.StringPair{
		{Key: "key1", Value: "value1"},
	}
	result, _, _, err := testDetailsToResultSink(extraTags, detail, outputRoot)
	if err != nil {
		t.Fatalf("Cannot parse test detail. got %s", err)
	}

	var gotErr string
	if result.FailureReason != nil && len(result.FailureReason.Errors) > 0 {
		gotErr = result.FailureReason.Errors[0].Message
	}
	if !(result.StatusV2 == resultpb.TestResult_FAILED && gotErr == expectedFailureReason) {
		t.Errorf("If a test failed, the top level test should have the reported failure reason.\n The error message is %q.\n The expected failure reason is %q.", gotErr, expectedFailureReason)
	}

	tags := make(map[string]string)
	for _, tag := range result.Tags {
		tags[tag.Key] = tag.Value
	}

	expectedTags := map[string]string{
		"key1":              "value1",
		"gn_label":          detail.GNLabel,
		"source_label":      detail.SourceLabel,
		"test_case_count":   "7",
		"affected":          "false",
		"is_top_level_test": "true",
		"owners":            "testgoogler1@google.com,testgoogler2@google.com,testgoogler3@google.com,testgoogler4@google.com,testgoogler5@google.com",
	}
	if diff := cmp.Diff(tags, expectedTags); diff != "" {
		t.Errorf("tags differ (-got +want):\n%s", diff)
	}

	if len(result.Artifacts) != 2 {
		t.Errorf("Got %d artifacts, want 2", len(result.Artifacts))
	}
	artifactNames := []string{}
	for name := range result.Artifacts {
		artifactNames = append(artifactNames, name)
	}
	sort.Strings(artifactNames)
	if diff := cmp.Diff(artifactNames, []string{"dir-1/outputfile", "dir_2/outputfile"}); diff != "" {
		t.Errorf("Diff in output files (-got +want):\n%s", diff)
	}
	expectedMetadata := resultpb.TestMetadata{
		BugComponent: &resultpb.BugComponent{
			System: &resultpb.BugComponent_IssueTracker{
				IssueTracker: &resultpb.IssueTrackerComponent{
					ComponentId: 1478143,
				},
			},
		},
	}
	if diff := cmp.Diff(result.TestMetadata.Name, expectedMetadata.Name); diff != "" {
		t.Errorf("Diff in metadata name (-got +want):\n%s", diff)
	}
	if diff := cmp.Diff(result.TestMetadata.BugComponent.GetIssueTracker().ComponentId, expectedMetadata.BugComponent.GetIssueTracker().ComponentId); diff != "" {
		t.Errorf("Diff in the bug component's component id (-got +want):\n%s", diff)
	}
}

func TestSetTestDetailsToResultSink_FailureReason_ExceedsMaxSize(t *testing.T) {
	outputRoot := t.TempDir()
	detail := createTestDetailWithPassedAndFailedTestCase(5, 200, outputRoot)
	detail.FailureReason = runtests.FailureReasonFromMessage(strings.Repeat("a", 2000))
	expectedFailureReason := strings.Repeat("a", MaxFailureReasonLength-3) + "..."
	extraTags := []*resultpb.StringPair{
		{Key: "key1", Value: "value1"},
	}
	result, _, _, err := testDetailsToResultSink(extraTags, detail, outputRoot)
	if err != nil {
		t.Fatalf("Cannot parse test detail. got %s", err)
	}

	var gotErr string
	if result.FailureReason != nil && len(result.FailureReason.Errors) > 0 {
		gotErr = result.FailureReason.Errors[0].Message
	}
	if !(result.StatusV2 == resultpb.TestResult_FAILED && gotErr == expectedFailureReason) {
		t.Errorf("If a test failed, the top level test should have the reported failure reason truncated.\n The error message is %q.\n The expected failure reason is %q.", gotErr, expectedFailureReason)
	}

	tags := make(map[string]string)
	for _, tag := range result.Tags {
		tags[tag.Key] = tag.Value
	}

	expectedTags := map[string]string{
		"key1":              "value1",
		"gn_label":          detail.GNLabel,
		"source_label":      detail.SourceLabel,
		"test_case_count":   "205",
		"affected":          "false",
		"is_top_level_test": "true",
	}
	if diff := cmp.Diff(tags, expectedTags); diff != "" {
		t.Errorf("tags differ (-got +want):\n%s", diff)
	}

	if len(result.Artifacts) != 2 {
		t.Errorf("Got %d artifacts, want 2", len(result.Artifacts))
	}
	artifactNames := []string{}
	for name := range result.Artifacts {
		artifactNames = append(artifactNames, name)
	}
	sort.Strings(artifactNames)
	if diff := cmp.Diff(artifactNames, []string{"dir-1/outputfile", "dir_2/outputfile"}); diff != "" {
		t.Errorf("Diff in output files (-got +want):\n%s", diff)
	}
}

func TestSetTestDetailsToResultSink_NonSuccessCases(t *testing.T) {
	outputRoot := t.TempDir()

	tests := []struct {
		name                  string
		detail                runtests.TestDetails
		expectedFailureReason string
	}{
		{
			name: "mixed_failure_and_skipped_with_reason",
			detail: runtests.TestDetails{
				Name:        "foo",
				GNLabel:     "some label",
				SourceLabel: "some source label",
				Status:      runtests.TestFailure,
				TestResult: runtests.TestResult{
					OutputDir:     "foo",
					FailureReason: runtests.FailureReasonFromMessage("some failure reason reported by test runner"),
					Cases: []runtests.TestCaseResult{
						{CaseName: "failed_case", Status: runtests.TestFailure},
						{CaseName: "skipped_with_reason", Status: runtests.TestSkipped},
						{CaseName: "skipped_without_reason", Status: runtests.TestSkipped},
					},
				},
			},
			expectedFailureReason: "some failure reason reported by test runner",
		},
		{
			name: "all_skipped_some_with_reason",
			detail: runtests.TestDetails{
				Name:        "foo",
				GNLabel:     "some label",
				SourceLabel: "some source label",
				Status:      runtests.TestSkipped,
				TestResult: runtests.TestResult{
					OutputDir: "foo",
					Cases: []runtests.TestCaseResult{
						{CaseName: "skipped_1", Status: runtests.TestSkipped},
						{CaseName: "skipped_2", Status: runtests.TestSkipped},
					},
				},
			},
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			extraTags := []*resultpb.StringPair{}
			result, _, _, err := testDetailsToResultSink(extraTags, &tc.detail, outputRoot)
			if err != nil {
				t.Fatalf("Cannot parse test detail: %s", err)
			}

			if tc.expectedFailureReason == "" {
				if result.FailureReason != nil {
					t.Errorf("got FailureReason %+v, want nil", result.FailureReason)
				}
			} else {
				if result.FailureReason == nil {
					t.Fatalf("Expected FailureReason to be non-nil")
				}
				var gotErr string
				if len(result.FailureReason.Errors) > 0 {
					gotErr = result.FailureReason.Errors[0].Message
				}
				if gotErr != tc.expectedFailureReason {
					t.Errorf("got failure reason %q, want %q", gotErr, tc.expectedFailureReason)
				}
			}
		})
	}
}

func TestSetTestCaseToResultSink(t *testing.T) {
	outputRoot := t.TempDir()
	detail := createTestDetailWithTestCase(5, outputRoot)
	detail.Metadata = metadata.TestMetadata{
		Owners: []string{
			"testgoogler1@google.com",
			"testgoogler2@google.com",
			"testgoogler3@google.com",
			"testgoogler4@google.com",
			"testgoogler5@google.com",
			"testgoogler6@google.com",
			"testgoogler7@google.com",
		},
		ComponentID: 1478143,
	}
	results, _, _ := testCaseToResultSink(detail.Cases, []*resultpb.StringPair{}, detail, outputRoot)
	if len(results) != 5 {
		t.Errorf("Got %d test case results, want 5", len(results))
	}

	for i, result := range results {
		tags := make(map[string]string)
		for _, tag := range result.Tags {
			tags[tag.Key] = tag.Value
		}
		expectedTags := map[string]string{
			"format":       detail.Cases[i].Format,
			"is_test_case": "true",
			"key1":         "value1",
			"owners":       "testgoogler1@google.com,testgoogler2@google.com,testgoogler3@google.com,testgoogler4@google.com,testgoogler5@google.com",
		}
		if diff := cmp.Diff(tags, expectedTags); diff != "" {
			t.Errorf("tags differ (-got +want):\n%s", diff)
		}
		if len(result.Artifacts) != 2 {
			t.Errorf("Got %d artifacts for test case %d, want 2", len(result.Artifacts), i+1)
		}
		artifactNames := []string{}
		for name := range result.Artifacts {
			artifactNames = append(artifactNames, name)
		}
		sort.Strings(artifactNames)
		if diff := cmp.Diff(artifactNames, []string{"case/outputfile1", "case/outputfile2"}); diff != "" {
			t.Errorf("Diff in output files (-got +want):\n%s", diff)
		}
		expectedMetadata := resultpb.TestMetadata{
			Name: detail.Cases[i].DisplayName,
			BugComponent: &resultpb.BugComponent{
				System: &resultpb.BugComponent_IssueTracker{
					IssueTracker: &resultpb.IssueTrackerComponent{
						ComponentId: 1478143,
					},
				},
			},
		}
		if diff := cmp.Diff(result.TestMetadata.Name, expectedMetadata.Name); diff != "" {
			t.Errorf("Diff in metadata name (-got +want):\n%s", diff)
		}
		if diff := cmp.Diff(result.TestMetadata.BugComponent.GetIssueTracker().ComponentId, expectedMetadata.BugComponent.GetIssueTracker().ComponentId); diff != "" {
			t.Errorf("Diff in the bug component's component id (-got +want):\n%s", diff)
		}
	}
}

func TestSetTestCaseToResultSink_WithFailureReason(t *testing.T) {
	outputRoot := t.TempDir()
	detail := &runtests.TestDetails{
		Name:      "foo",
		Status:    runtests.TestFailure,
		StartTime: time.Now(),
		TestResult: runtests.TestResult{
			Cases: []runtests.TestCaseResult{
				{
					DisplayName: "foo/bar_failed",
					SuiteName:   "foo",
					CaseName:    "bar_failed",
					Status:      runtests.TestFailure,
					Format:      "Rust",
					FailureReason: &runtests.FailureReason{
						Errors: []*runtests.FailureReasonError{
							{Message: "error message 1"},
							{Message: "error message 2"},
						},
					},
				},
			},
		},
	}

	results, _, _ := testCaseToResultSink(detail.Cases, []*resultpb.StringPair{}, detail, outputRoot)
	if len(results) != 1 {
		t.Fatalf("Got %d test case results, want 1", len(results))
	}

	res := results[0]
	if res.FailureReason == nil {
		t.Fatalf("Expected FailureReason to be non-nil")
	}
	if len(res.FailureReason.Errors) != 2 {
		t.Fatalf("got %d Errors, want 2", len(res.FailureReason.Errors))
	}
	if res.FailureReason.Errors[0].Message != "error message 1" || res.FailureReason.Errors[1].Message != "error message 2" {
		t.Errorf("unexpected error messages: %v", res.FailureReason.Errors)
	}
}

func TestSetTestCaseToResultSink_WithFailureReason_DefaultKind(t *testing.T) {
	outputRoot := t.TempDir()
	detail := &runtests.TestDetails{
		Name:      "foo",
		Status:    runtests.TestFailure,
		StartTime: time.Now(),
		TestResult: runtests.TestResult{
			Cases: []runtests.TestCaseResult{
				{
					DisplayName: "foo/bar_failed",
					SuiteName:   "foo",
					CaseName:    "bar_failed",
					Status:      runtests.TestAborted, // Should default kind to TIMEOUT
					Format:      "Rust",
					FailureReason: &runtests.FailureReason{
						Errors: []*runtests.FailureReasonError{
							{Message: "timeout error message"},
						},
					},
				},
				{
					DisplayName:   "foo/bar_crash",
					SuiteName:     "foo",
					CaseName:      "bar_crash",
					Status:        runtests.TestInfraFailure, // Should default kind to CRASH
					Format:        "Rust",
					FailureReason: runtests.FailureReasonFromMessage("infra failure message"),
				},
			},
		},
	}

	results, _, _ := testCaseToResultSink(detail.Cases, []*resultpb.StringPair{}, detail, outputRoot)
	if len(results) != 2 {
		t.Fatalf("Got %d test case results, want 2", len(results))
	}

	res0 := results[0]
	if res0.FailureReason == nil {
		t.Fatalf("Expected res0 FailureReason to be non-nil")
	}
	if res0.FailureReason.Kind != resultpb.FailureReason_TIMEOUT {
		t.Errorf("got Kind = %v, want %v", res0.FailureReason.Kind, resultpb.FailureReason_TIMEOUT)
	}
	if len(res0.FailureReason.Errors) != 1 || res0.FailureReason.Errors[0].Message != "timeout error message" {
		t.Errorf("unexpected res0 errors: %v", res0.FailureReason.Errors)
	}

	res1 := results[1]
	if res1.FailureReason == nil {
		t.Fatalf("Expected res1 FailureReason to be non-nil")
	}
	if res1.FailureReason.Kind != resultpb.FailureReason_CRASH {
		t.Errorf("got Kind = %v, want %v", res1.FailureReason.Kind, resultpb.FailureReason_CRASH)
	}
	if len(res1.FailureReason.Errors) != 1 || res1.FailureReason.Errors[0].Message != "infra failure message" {
		t.Errorf("unexpected res1 errors: %v", res1.FailureReason.Errors)
	}
}

func TestTestDetailsToResultSink_PanicsOnNilErrorsInCases(t *testing.T) {
	defer func() {
		if r := recover(); r == nil {
			t.Errorf("expected panic when error in slice is nil")
		}
	}()

	outputRoot := t.TempDir()
	detail := &runtests.TestDetails{
		Name:      "foo",
		Status:    runtests.TestFailure,
		StartTime: time.Now(),
		TestResult: runtests.TestResult{
			FailureReason: &runtests.FailureReason{
				Errors: []*runtests.FailureReasonError{
					nil,
					{Message: "first valid error"},
				},
			},
		},
	}

	_, _, _, _ = testDetailsToResultSink([]*resultpb.StringPair{}, detail, outputRoot)
}

func TestToResultDBFailureReason_PanicsOnNilError(t *testing.T) {
	defer func() {
		if r := recover(); r == nil {
			t.Errorf("expected panic when error in slice is nil")
		}
	}()

	fr := &runtests.FailureReason{
		Errors: []*runtests.FailureReasonError{
			{Message: "valid error 1"},
			nil,
		},
	}

	_ = toResultDBFailureReason(fr, resultpb.FailureReason_ORDINARY)
}

func TestToResultDBFailureReason_SkipsEmptyMessage(t *testing.T) {
	fr := &runtests.FailureReason{
		Errors: []*runtests.FailureReasonError{
			{Message: "valid error 1"},
			{Message: ""},
			{Message: "valid error 2"},
		},
	}

	res := toResultDBFailureReason(fr, resultpb.FailureReason_ORDINARY)
	if len(res.Errors) != 2 {
		t.Fatalf("got %d errors, want 2", len(res.Errors))
	}
	if res.Errors[0].Message != "valid error 1" || res.Errors[1].Message != "valid error 2" {
		t.Errorf("got errors %+v, want ['valid error 1', 'valid error 2']", res.Errors)
	}
}

func TestToResultDBFailureReason_Truncation(t *testing.T) {
	// 1. Test message (>1024B) truncation
	fr := &runtests.FailureReason{
		Errors: []*runtests.FailureReasonError{
			{
				Message: strings.Repeat("m", 1500),
			},
		},
	}
	res := toResultDBFailureReason(fr, resultpb.FailureReason_ORDINARY)
	if len(res.Errors) != 1 {
		t.Fatalf("got %d errors, want 1", len(res.Errors))
	}
	if len(res.Errors[0].Message) != MaxFailureReasonLength {
		t.Errorf("got message len %d, want %d", len(res.Errors[0].Message), MaxFailureReasonLength)
	}
	if !strings.HasSuffix(res.Errors[0].Message, "...") {
		t.Errorf("expected message to end with '...'")
	}

	// 2. Test total error list size (>16KB) truncation
	frList := &runtests.FailureReason{}
	for i := 0; i < 20; i++ {
		frList.Errors = append(frList.Errors, &runtests.FailureReasonError{
			Message: strings.Repeat("e", 1000),
		})
	}
	resList := toResultDBFailureReason(frList, resultpb.FailureReason_ORDINARY)
	if proto.Size(resList) > MaxFailureReasonTotalSize {
		t.Errorf("got proto size %d, want <= %d", proto.Size(resList), MaxFailureReasonTotalSize)
	}
	if resList.TruncatedErrorsCount == 0 {
		t.Errorf("expected TruncatedErrorsCount > 0 when exceeding 16KB limit")
	}
	if len(resList.Errors)+int(resList.TruncatedErrorsCount) != 20 {
		t.Errorf("got %d kept errors + %d truncated errors, want 20 total", len(resList.Errors), resList.TruncatedErrorsCount)
	}
}

// mustParseProperties parses a JSON object into a properties struct, so that
// expectations can be written as the JSON that ResultDB ultimately exports.
func mustParseProperties(t *testing.T, properties string) *structpb.Struct {
	t.Helper()
	parsed := &structpb.Struct{}
	if err := protojson.Unmarshal([]byte(properties), parsed); err != nil {
		t.Fatalf("Cannot parse properties %q: %s", properties, err)
	}
	return parsed
}

func TestTestDetailProperties(t *testing.T) {
	detail := &runtests.TestDetails{
		Name:        "foo",
		GNLabel:     "//src/foo:foo-tests(//build/toolchain/fuchsia:x64)",
		SourceLabel: "//src/foo:foo-tests",
		Affected:    true,
		Tags:        []build.TestTag{{Key: "scope", Value: "hermetic"}},
		// Include 7 owners to verify that the owner list is not truncated,
		// unlike the owners tag.
		Metadata: metadata.TestMetadata{
			Owners: []string{
				"testgoogler1@google.com",
				"testgoogler2@google.com",
				"testgoogler3@google.com",
				"testgoogler4@google.com",
				"testgoogler5@google.com",
				"testgoogler6@google.com",
				"testgoogler7@google.com",
			},
		},
		TestResult: runtests.TestResult{
			Cases: []runtests.TestCaseResult{
				{CaseName: "bar_0"}, {CaseName: "bar_1"}, {CaseName: "bar_2"},
			},
		},
	}
	buildTags := []*resultpb.StringPair{
		{Key: "builder", Value: "core.x64-release"},
		{Key: "board", Value: "x64"},
	}

	got, _ := testDetailProperties(detail, buildTags)

	want := mustParseProperties(t, `{
		"gn_label": "//src/foo:foo-tests(//build/toolchain/fuchsia:x64)",
		"source_label": "//src/foo:foo-tests",
		"test_case_count": 3,
		"affected": true,
		"owners": [
			"testgoogler1@google.com",
			"testgoogler2@google.com",
			"testgoogler3@google.com",
			"testgoogler4@google.com",
			"testgoogler5@google.com",
			"testgoogler6@google.com",
			"testgoogler7@google.com"
		],
		"tags": {"scope": "hermetic"},
		"build": {"builder": "core.x64-release", "board": "x64"}
	}`)
	if diff := cmp.Diff(want, got, protocmp.Transform()); diff != "" {
		t.Errorf("Properties differ (-want +got):\n%s", diff)
	}
}

func TestTestCaseProperties(t *testing.T) {
	detail := &runtests.TestDetails{
		Name:     "foo",
		Metadata: metadata.TestMetadata{Owners: []string{"testgoogler1@google.com"}},
	}
	testCase := runtests.TestCaseResult{
		DisplayName: "foo/bar_0",
		SuiteName:   "foo",
		CaseName:    "bar_0",
		Format:      "Rust",
		Tags:        []build.TestTag{{Key: "key1", Value: "value1"}},
	}
	buildTags := []*resultpb.StringPair{{Key: "builder", Value: "core.x64-release"}}

	got, _ := testCaseProperties(testCase, detail, buildTags)

	want := mustParseProperties(t, `{
		"format": "Rust",
		"owners": ["testgoogler1@google.com"],
		"tags": {"key1": "value1"},
		"build": {"builder": "core.x64-release"}
	}`)
	if diff := cmp.Diff(want, got, protocmp.Transform()); diff != "" {
		t.Errorf("Properties differ (-want +got):\n%s", diff)
	}
}

func TestPropertiesFreeFormTags(t *testing.T) {
	// Tags are reported as a repeated list, so the same key may appear more
	// than once, and keys may be empty.
	detail := &runtests.TestDetails{
		Name: "foo",
		Tags: []build.TestTag{
			{Key: "test_outcome", Value: "PASSED"},
			{Key: "test_outcome", Value: "FAILED"},
			{Key: "", Value: "tag without a key"},
		},
	}

	properties, tags := testDetailProperties(detail, nil)

	// The last value for a key wins, tags without a key are dropped, and the
	// build metadata is omitted entirely when there is none.
	want := mustParseProperties(t, `{
		"gn_label": "",
		"source_label": "",
		"test_case_count": 0,
		"affected": false,
		"tags": {"test_outcome": "FAILED"}
	}`)
	if diff := cmp.Diff(want, properties, protocmp.Transform()); diff != "" {
		t.Errorf("Properties differ (-want +got):\n%s", diff)
	}

	// The tags report the same metadata, but, being a repeated list, they keep
	// every value of a repeated key. They are unordered, so compare them as a
	// set.
	wantTags := []*resultpb.StringPair{
		{Key: "is_top_level_test", Value: "true"},
		{Key: "gn_label", Value: ""},
		{Key: "source_label", Value: ""},
		{Key: "test_case_count", Value: "0"},
		{Key: "affected", Value: "false"},
		{Key: "test_outcome", Value: "PASSED"},
		{Key: "test_outcome", Value: "FAILED"},
	}
	sortTags := cmpopts.SortSlices(func(a, b *resultpb.StringPair) bool {
		if a.Key != b.Key {
			return a.Key < b.Key
		}
		return a.Value < b.Value
	})
	if diff := cmp.Diff(wantTags, tags, protocmp.Transform(), sortTags); diff != "" {
		t.Errorf("Tags differ (-want +got):\n%s", diff)
	}
}

func TestPropertiesSizeLimit(t *testing.T) {
	// Both of these are far larger than the properties size limit on their own.
	testTags := []build.TestTag{}
	buildTags := []*resultpb.StringPair{}
	for i := 0; i < MaxPropertiesSize/1024; i++ {
		testTags = append(testTags, build.TestTag{
			Key: fmt.Sprintf("test_tag_%d", i), Value: strings.Repeat("t", 1024),
		})
		buildTags = append(buildTags, &resultpb.StringPair{
			Key: fmt.Sprintf("build_tag_%d", i), Value: strings.Repeat("b", 1024),
		})
	}

	t.Run("OversizedTags", func(t *testing.T) {
		detail := &runtests.TestDetails{Name: "foo", Tags: testTags}
		got, _ := testDetailProperties(detail, []*resultpb.StringPair{{Key: "builder", Value: "core.x64-release"}})
		if got != nil {
			t.Errorf("Got properties of %d bytes, want nil", proto.Size(got))
		}
	})

	t.Run("OversizedBuildMetadata", func(t *testing.T) {
		detail := &runtests.TestDetails{Name: "foo"}
		got, _ := testDetailProperties(detail, buildTags)
		if got != nil {
			t.Errorf("Got properties of %d bytes, want nil", proto.Size(got))
		}
	})

	t.Run("OversizedControlledProperties", func(t *testing.T) {
		detail := &runtests.TestDetails{
			Name:    "foo",
			GNLabel: strings.Repeat("l", MaxPropertiesSize+1),
		}
		if got, _ := testDetailProperties(detail, nil); got != nil {
			t.Errorf("Got properties of %d bytes, want nil", proto.Size(got))
		}
	})

	t.Run("WithinLimit", func(t *testing.T) {
		detail := &runtests.TestDetails{
			Name: "foo",
			Tags: []build.TestTag{{Key: "scope", Value: "hermetic"}},
		}
		got, _ := testDetailProperties(detail, []*resultpb.StringPair{{Key: "builder", Value: "core.x64-release"}})
		if got == nil {
			t.Fatal("Got nil properties, want them to be reported")
		}
		for _, key := range []string{"tags", "build"} {
			if _, ok := got.Fields[key]; !ok {
				t.Errorf("Got no %q properties, want them to be kept", key)
			}
		}
	})
}

func TestPropertiesReportedInResults(t *testing.T) {
	outputRoot := t.TempDir()
	detail := createTestDetailWithTestCase(1, outputRoot)
	buildTags := []*resultpb.StringPair{{Key: "builder", Value: "core.x64-release"}}

	// The builder must stay queryable after the migration to properties, since
	// Milo regression pages rely on it. See b/527958920.
	assertBuilderReported := func(t *testing.T, result *sinkpb.TestResult) {
		t.Helper()
		gotBuild := result.Properties.GetFields()["build"].GetStructValue()
		if got := gotBuild.GetFields()["builder"].GetStringValue(); got != "core.x64-release" {
			t.Errorf("Got builder property %q, want core.x64-release", got)
		}
		// The tags are still reported alongside the properties until all the
		// downstream consumers have migrated.
		for _, tag := range result.Tags {
			if tag.Key == "builder" && tag.Value == "core.x64-release" {
				return
			}
		}
		t.Errorf("Got tags %v, want a builder tag", result.Tags)
	}

	// The suite and test case hierarchy is reported as a legacy tag only, since
	// ResultDB already models it. See b/527958757.
	assertTagReported := func(t *testing.T, result *sinkpb.TestResult, key string) {
		t.Helper()
		for _, tag := range result.Tags {
			if tag.Key == key && tag.Value == "true" {
				return
			}
		}
		t.Errorf("Got tags %v, want a %q tag", result.Tags, key)
	}

	result, _, _, err := testDetailsToResultSink(buildTags, detail, outputRoot)
	if err != nil {
		t.Fatalf("Cannot parse test detail. got %s", err)
	}
	assertTagReported(t, result, "is_top_level_test")
	if _, ok := result.Properties.GetFields()["is_top_level_test"]; ok {
		t.Error("Got an is_top_level_test property, want it to be reported as a tag only")
	}
	assertBuilderReported(t, result)

	caseResults, _, _ := testCaseToResultSink(detail.Cases, buildTags, detail, outputRoot)
	if len(caseResults) != 1 {
		t.Fatalf("Got %d test case results, want 1", len(caseResults))
	}
	assertTagReported(t, caseResults[0], "is_test_case")
	if _, ok := caseResults[0].Properties.GetFields()["is_test_case"]; ok {
		t.Error("Got an is_test_case property, want it to be reported as a tag only")
	}
	assertBuilderReported(t, caseResults[0])
}

func createTestSummary(testCount int) *runtests.TestSummary {
	t := []runtests.TestDetails{}
	for i := 0; i < testCount; i++ {
		t = append(t, runtests.TestDetails{
			Name:                 fmt.Sprintf("test_%d", i),
			GNLabel:              "some label",
			SourceLabel:          "some source label",
			TestResult:           runtests.TestResult{OutputFiles: []string{"some file path"}},
			Status:               runtests.TestSuccess,
			StartTime:            time.Now(),
			DurationMillis:       39797,
			IsTestingFailureMode: false,
		})
	}
	return &runtests.TestSummary{Tests: t}
}

func createTestDetailWithTestCase(testCase int, outputRoot string) *runtests.TestDetails {
	t := []runtests.TestCaseResult{}
	if outputRoot != "" {
		for _, f := range []string{"foo/dir-1/outputfile", "foo/dir#2/outputfile", "foo/case/outputfile1", "foo/case/outputfile2"} {
			outputfile := filepath.Join(outputRoot, f)
			os.MkdirAll(filepath.Dir(outputfile), os.ModePerm)
			os.WriteFile(outputfile, []byte("output"), os.ModePerm)
		}
	}
	for i := 0; i < testCase; i++ {
		t = append(t, runtests.TestCaseResult{
			DisplayName: fmt.Sprintf("foo/bar_%d", i),
			SuiteName:   "foo",
			CaseName:    fmt.Sprintf("bar_%d", i),
			Status:      runtests.TestSuccess,
			Format:      "Rust",
			OutputFiles: []string{"case/outputfile1", "case/outputfile2"},
			Tags:        []build.TestTag{{"key1", "value1"}},
		})
	}
	return &runtests.TestDetails{
		Name:        "foo",
		GNLabel:     "some label",
		SourceLabel: "some source label",
		TestResult: runtests.TestResult{
			OutputFiles: []string{"dir-1/outputfile", "dir#2/outputfile"},
			OutputDir:   "foo",
			Cases:       t,
		},
		Status:               runtests.TestSuccess,
		StartTime:            time.Now(),
		DurationMillis:       39797,
		IsTestingFailureMode: false,
	}
}

func createTestDetailWithPassedAndFailedTestCase(passedTestCase int, failedTestCase int, outputRoot string) *runtests.TestDetails {
	t := []runtests.TestCaseResult{}
	if outputRoot != "" {
		for _, f := range []string{"dir-1/outputfile", "dir#2/outputfile", "case/outputfile1", "case/outputfile2"} {
			outputfile := filepath.Join(outputRoot, f)
			os.MkdirAll(filepath.Dir(outputfile), os.ModePerm)
			os.WriteFile(outputfile, []byte("output"), os.ModePerm)
		}
	}
	for i := 0; i < passedTestCase; i++ {
		t = append(t, runtests.TestCaseResult{
			DisplayName: fmt.Sprintf("foo/bar_%d", i),
			SuiteName:   "foo",
			CaseName:    fmt.Sprintf("bar_%d", i),
			Status:      runtests.TestSuccess,
			Format:      "Rust",
			OutputFiles: []string{"case/outputfile1", "case/outputfile2"},
			Tags:        []build.TestTag{{"key1", "value1"}},
		})
	}
	for i := 0; i < failedTestCase; i++ {
		t = append(t, runtests.TestCaseResult{
			DisplayName: fmt.Sprintf("foo/bar_%d", i),
			SuiteName:   "foo",
			CaseName:    fmt.Sprintf("bar_%d", i),
			Status:      runtests.TestFailure,
			Format:      "Rust",
			OutputFiles: []string{"case/outputfile1", "case/outputfile2"},
			Tags:        []build.TestTag{{"key1", "value1"}},
		})
	}
	finalResult := runtests.TestSuccess
	if failedTestCase > 0 {
		finalResult = runtests.TestFailure
	}
	return &runtests.TestDetails{
		Name:        "foo",
		GNLabel:     "some label",
		SourceLabel: "some source label",
		TestResult: runtests.TestResult{
			OutputFiles: []string{"dir-1/outputfile", "dir#2/outputfile"},
			Cases:       t,
		},
		Status:               finalResult,
		StartTime:            time.Now(),
		DurationMillis:       39797,
		IsTestingFailureMode: false,
	}
}

func TestIsReadable(t *testing.T) {
	if r := isReadable(""); r {
		t.Errorf("Empty string cannot be readable. got %t, want false", r)
	}
	if r := isReadable(*testDataDir); r {
		t.Errorf("Directory should not be readable. got %t, want false", r)
	}
	luciCtx := filepath.Join(*testDataDir, "lucictx.json")
	if r := isReadable(luciCtx); !r {
		t.Errorf("File %v should be readable. got %t, want true", luciCtx, r)
	}
}

func TestInvocationLevelArtifacts(t *testing.T) {
	invocationLogs := []string{"syslog.txt", "serial_log.txt", "nonexistent_log.txt"}
	artifacts := InvocationLevelArtifacts(*testDataDir, invocationLogs)
	foundSyslog := false
	foundSerial := false
	for logName := range artifacts {
		switch logName {
		case "syslog.txt":
			foundSyslog = true
		case "serial_log.txt":
			foundSerial = true
		default:
			t.Errorf("Found unexpected log (%s), expect only syslog.txt or serial_log.txt", logName)
		}
	}
	if !foundSyslog {
		t.Errorf("Did not find syslog.txt in output")
	}
	if !foundSerial {
		t.Errorf("Did not find serial_log.txt in output")
	}
}

func TestTruncateString(t *testing.T) {
	testCases := []struct {
		testStr string
		want    string
		limit   int // bytes
	}{
		{
			testStr: "ab£cdefg",
			want:    "",
			limit:   1,
		}, {
			testStr: "ab£cdefg",
			want:    "ab...",
			limit:   5,
		}, {
			testStr: "ab£cdefg",
			want:    "ab...",
			limit:   6,
		}, {
			testStr: "ab£cdefg",
			want:    "ab£...",
			limit:   7,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "",
			limit:   3,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "",
			limit:   4,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "",
			limit:   5,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥...",
			limit:   6,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥L...",
			limit:   7,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥LoveFuc...",
			limit:   13,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥LoveFuchsia",
			limit:   14,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥LoveFuchsia",
			limit:   100,
		},
	}
	for _, tc := range testCases {
		r := truncateString(tc.testStr, tc.limit)
		if r != tc.want {
			t.Errorf("TestTruncateString failed for input: %q(%d), got %q, want %q",
				tc.testStr, tc.limit, r, tc.want)
		}
	}
}

func TestExoneratedTestCase(t *testing.T) {
	outputRoot := t.TempDir()
	detail := &runtests.TestDetails{
		Name:      "foo",
		Status:    runtests.TestFailure,
		StartTime: time.Now(),
		TestResult: runtests.TestResult{
			Cases: []runtests.TestCaseResult{
				{
					DisplayName:   "foo/bar_exonerated",
					SuiteName:     "foo",
					CaseName:      "bar_exonerated",
					Status:        runtests.TestExonerated,
					Format:        "Rust",
					FailureReason: runtests.FailureReasonFromMessage("Flaky test instance"),
				},
				{
					DisplayName: "foo/bar_passed",
					SuiteName:   "foo",
					CaseName:    "bar_passed",
					Status:      runtests.TestSuccess,
					Format:      "Rust",
				},
			},
		},
	}

	results, exonerations, skipped := testCaseToResultSink(detail.Cases, []*resultpb.StringPair{}, detail, outputRoot)

	if len(results) != 2 {
		t.Fatalf("Got %d test case results, want 2", len(results))
	}
	if len(skipped) != 0 {
		t.Errorf("Got skipped tests %v, want 0", skipped)
	}
	if len(exonerations) != 1 {
		t.Fatalf("Got %d exonerations, want 1", len(exonerations))
	}

	// Verify the first result is the exonerated one (FAILED)
	exoneratedCaseResult := results[0]
	if exoneratedCaseResult.TestId != "foo/foo:bar_exonerated" {
		t.Errorf("Unexpected TestId for exonerated case: %s", exoneratedCaseResult.TestId)
	}
	if exoneratedCaseResult.StatusV2 != resultpb.TestResult_FAILED {
		t.Errorf("Exonerated case result status should be FAILED, got: %s", exoneratedCaseResult.StatusV2)
	}
	if exoneratedCaseResult.FailureReason == nil || len(exoneratedCaseResult.FailureReason.Errors) == 0 {
		t.Fatalf("Exonerated case result should have FailureReason")
	}
	if exoneratedCaseResult.FailureReason.Errors[0].Message != "Flaky test instance" {
		t.Errorf("Unexpected FailureReason message: %s", exoneratedCaseResult.FailureReason.Errors[0].Message)
	}

	// Verify the second result is the passed one (PASSED)
	passedCaseResult := results[1]
	if passedCaseResult.TestId != "foo/foo:bar_passed" {
		t.Errorf("Unexpected TestId for passed case: %s", passedCaseResult.TestId)
	}
	if passedCaseResult.StatusV2 != resultpb.TestResult_PASSED {
		t.Errorf("Passed case result status should be PASSED, got: %s", passedCaseResult.StatusV2)
	}
	if passedCaseResult.FailureReason != nil {
		t.Errorf("Passed case result should not have FailureReason, got: %+v", passedCaseResult.FailureReason)
	}

	// Verify that the exoneration maps to the exact test case ID
	exoneration := exonerations[0]
	if exoneration.TestId != "foo/foo:bar_exonerated" {
		t.Errorf("Exoneration maps to incorrect TestId: got %s, want foo/foo:bar_exonerated", exoneration.TestId)
	}
	if exoneration.Reason != resultpb.ExonerationReason_NOT_CRITICAL {
		t.Errorf("Unexpected Exoneration reason: got %v, want NOT_CRITICAL", exoneration.Reason)
	}
	if !strings.Contains(exoneration.ExplanationHtml, "bar_exonerated") {
		t.Errorf("ExplanationHtml does not name the testcase: got %q", exoneration.ExplanationHtml)
	}
}

func TestExoneratedTestDetail(t *testing.T) {
	outputRoot := t.TempDir()
	detail := &runtests.TestDetails{
		Name:           "foo_exonerated_target",
		Status:         runtests.TestExonerated,
		StartTime:      time.Now(),
		DurationMillis: 500,
	}

	result, exoneration, skipped, err := testDetailsToResultSink([]*resultpb.StringPair{}, detail, outputRoot)
	if err != nil {
		t.Fatalf("Unexpected error mapping details: %v", err)
	}
	if skipped != "" {
		t.Errorf("Got skipped test %q, want empty", skipped)
	}
	if result == nil {
		t.Fatalf("Result is nil")
	}
	if exoneration == nil {
		t.Fatalf("Exoneration is nil")
	}

	// Verify that the exonerated target has its result mapped as FAILED
	if result.TestId != "foo_exonerated_target" {
		t.Errorf("Unexpected TestId: got %s", result.TestId)
	}
	if result.StatusV2 != resultpb.TestResult_FAILED {
		t.Errorf("Exonerated target status should be FAILED, got: %s", result.StatusV2)
	}

	// Verify the target's exoneration maps to the target's flat name
	if exoneration.TestId != "foo_exonerated_target" {
		t.Errorf("Exoneration maps to incorrect TestId: got %s", exoneration.TestId)
	}
	if exoneration.Reason != resultpb.ExonerationReason_NOT_CRITICAL {
		t.Errorf("Unexpected Exoneration reason: got %v", exoneration.Reason)
	}
	if !strings.Contains(exoneration.ExplanationHtml, "foo_exonerated_target") {
		t.Errorf("ExplanationHtml does not name the test target: got %q", exoneration.ExplanationHtml)
	}
}

func TestExoneratedSummaryToResultSink(t *testing.T) {
	summary := &runtests.TestSummary{
		Tests: []runtests.TestDetails{
			{
				Name:      "test_exonerated_target",
				Status:    runtests.TestExonerated,
				StartTime: time.Now(),
			},
			{
				Name:      "test_exonerated_case",
				Status:    runtests.TestFailure,
				StartTime: time.Now(),
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{
						{
							DisplayName: "test_exonerated_case/suite:case",
							SuiteName:   "suite",
							CaseName:    "case",
							Status:      runtests.TestExonerated,
						},
					},
				},
			},
			{
				Name:      "test_passed",
				Status:    runtests.TestSuccess,
				StartTime: time.Now(),
			},
		},
	}

	results, exonerations, skipped := SummaryToResultSink(summary, []*resultpb.StringPair{}, "")

	// test_exonerated_target reports 1 result (itself) and 1 target exoneration.
	// test_exonerated_case reports 2 results (the case and the target details) and 1 case exoneration.
	// test_passed reports 1 result (itself).
	// Total expected results: 1 (target) + 1 (case) + 1 (details of target) + 1 (passed target) = 4
	if len(results) != 4 {
		t.Errorf("Got %d results, want 4", len(results))
	}
	if len(exonerations) != 2 {
		t.Fatalf("Got %d exonerations, want 2", len(exonerations))
	}
	if len(skipped) != 0 {
		t.Errorf("Got skipped tests %v, want 0", skipped)
	}

	// Verify the two exonerations map to their respective targets
	exonerationIDs := []string{exonerations[0].TestId, exonerations[1].TestId}
	sort.Strings(exonerationIDs)

	expectedIDs := []string{"test_exonerated_case/suite:case", "test_exonerated_target"}
	if diff := cmp.Diff(exonerationIDs, expectedIDs); diff != "" {
		t.Errorf("Exoneration IDs differ (-got +want):\n%s", diff)
	}
}

func TestRequestsChunking(t *testing.T) {
	t.Run("TestResults_Chunking", func(t *testing.T) {
		const resultCount = MaxBatchSize*2 + 1
		results := make([]*sinkpb.TestResult, resultCount)
		for i := 0; i < resultCount; i++ {
			results[i] = &sinkpb.TestResult{
				TestId: fmt.Sprintf("test_result_%d", i),
			}
		}

		resultRequests := createTestResultsRequests(results, MaxBatchSize)

		// Expected partition chunks: MaxBatchSize, MaxBatchSize, 1 (total 3 chunks)
		if len(resultRequests) != 3 {
			t.Fatalf("Expected 3 TestResults request chunks, got: %d", len(resultRequests))
		}
		if len(resultRequests[0].TestResults) != MaxBatchSize {
			t.Errorf("First chunk should have %d results, got: %d", MaxBatchSize, len(resultRequests[0].TestResults))
		}
		if len(resultRequests[1].TestResults) != MaxBatchSize {
			t.Errorf("Second chunk should have %d results, got: %d", MaxBatchSize, len(resultRequests[1].TestResults))
		}
		if len(resultRequests[2].TestResults) != 1 {
			t.Errorf("Third chunk should have 1 result, got: %d", len(resultRequests[2].TestResults))
		}
		expectedLastResultId := fmt.Sprintf("test_result_%d", resultCount-1)
		if resultRequests[2].TestResults[0].TestId != expectedLastResultId {
			t.Errorf("Unexpected TestId in last chunk: got %s, want %s", resultRequests[2].TestResults[0].TestId, expectedLastResultId)
		}
	})

	t.Run("TestResults_Empty", func(t *testing.T) {
		emptyResults := createTestResultsRequests(nil, MaxBatchSize)
		if emptyResults != nil {
			t.Errorf("Expected nil for empty results chunking, got: %+v", emptyResults)
		}
	})

	t.Run("TestExonerations_Chunking", func(t *testing.T) {
		const exonerationCount = MaxBatchSize*2 + 1
		exonerations := make([]*sinkpb.TestExoneration, exonerationCount)
		for i := 0; i < exonerationCount; i++ {
			exonerations[i] = &sinkpb.TestExoneration{
				TestId: fmt.Sprintf("test_exoneration_%d", i),
			}
		}

		exonerationRequests := createTestExonerationsRequests(exonerations, MaxBatchSize)

		// Expected partition chunks: MaxBatchSize, MaxBatchSize, 1 (total 3 chunks)
		if len(exonerationRequests) != 3 {
			t.Fatalf("Expected 3 TestExonerations request chunks, got: %d", len(exonerationRequests))
		}
		if len(exonerationRequests[0].TestExonerations) != MaxBatchSize {
			t.Errorf("First chunk should have %d exonerations, got: %d", MaxBatchSize, len(exonerationRequests[0].TestExonerations))
		}
		if len(exonerationRequests[1].TestExonerations) != MaxBatchSize {
			t.Errorf("Second chunk should have %d exonerations, got: %d", MaxBatchSize, len(exonerationRequests[1].TestExonerations))
		}
		if len(exonerationRequests[2].TestExonerations) != 1 {
			t.Errorf("Third chunk should have 1 exoneration, got: %d", len(exonerationRequests[2].TestExonerations))
		}
		expectedLastExonerationId := fmt.Sprintf("test_exoneration_%d", exonerationCount-1)
		if exonerationRequests[2].TestExonerations[0].TestId != expectedLastExonerationId {
			t.Errorf("Unexpected TestId in last chunk: got %s, want %s", exonerationRequests[2].TestExonerations[0].TestId, expectedLastExonerationId)
		}
	})

	t.Run("TestExonerations_Empty", func(t *testing.T) {
		emptyExonerations := createTestExonerationsRequests(nil, MaxBatchSize)
		if emptyExonerations != nil {
			t.Errorf("Expected nil for empty exonerations chunking, got: %+v", emptyExonerations)
		}
	})
}

func TestParseTestLocation(t *testing.T) {
	testCases := []struct {
		name  string
		label string
		want  *resultpb.TestLocation
	}{
		{
			name:  "standard GN label with target",
			label: "//src/sys/test_manager:test-manager-unittests",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src/sys/test_manager",
			},
		},
		{
			name:  "GN label with toolchain",
			label: "//src/sys/test_manager:test-manager-unittests(//build/toolchain:x64)",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src/sys/test_manager",
			},
		},
		{
			name:  "GN label with trailing slash in dir",
			label: "//src/sys/test_manager/:test-manager-unittests",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src/sys/test_manager",
			},
		},
		{
			name:  "Bazel single at label",
			label: "@//src/sys/test_manager:test-manager-unittests",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src/sys/test_manager",
			},
		},
		{
			name:  "Bazel double at label",
			label: "@@//src/sys/test_manager:test-manager-unittests",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src/sys/test_manager",
			},
		},
		{
			name:  "Label with triple slashes",
			label: "///src:test",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src",
			},
		},
		{
			name:  "Label with triple slashes and file target",
			label: "///src/tests:test_runner.py",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src/tests/test_runner.py",
			},
		},
		{
			name:  "Target with python script extension",
			label: "//src/tests:test_runner.py",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src/tests/test_runner.py",
			},
		},
		{
			name:  "Target with cc extension",
			label: "//zircon/system/ulib/c/test:libc-test.cc",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//zircon/system/ulib/c/test/libc-test.cc",
			},
		},
		{
			name:  "Target with .cm component manifest",
			label: "//src/sys/test_manager:test-manager.cm",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src/sys/test_manager",
			},
		},
		{
			name:  "Root package target",
			label: "//:tests",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//",
			},
		},
		{
			name:  "Root package target with file extension",
			label: "//:test.py",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//test.py",
			},
		},
		{
			name:  "Package directory only without colon",
			label: "//src/sys/test_manager",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src/sys/test_manager",
			},
		},
		{
			name:  "Vendor google subsystem label",
			label: "//vendor/google/subsystem/tests:my_test",
			want: &resultpb.TestLocation{
				Repo:     VendorGoogleRepo,
				FileName: "//subsystem/tests",
			},
		},
		{
			name:  "Vendor google root target",
			label: "//vendor/google:my_test",
			want: &resultpb.TestLocation{
				Repo:     VendorGoogleRepo,
				FileName: "//",
			},
		},
		{
			name:  "Vendor google file target",
			label: "//vendor/google:test.py",
			want: &resultpb.TestLocation{
				Repo:     VendorGoogleRepo,
				FileName: "//test.py",
			},
		},
		{
			name:  "Vendor google nested file target",
			label: "//vendor/google/pkg:test.py",
			want: &resultpb.TestLocation{
				Repo:     VendorGoogleRepo,
				FileName: "//pkg/test.py",
			},
		},
		{
			name:  "Empty label",
			label: "",
			want:  nil,
		},
		{
			name:  "Whitespace label",
			label: "   ",
			want:  nil,
		},
		{
			name:  "Label with spaces",
			label: "some source label",
			want:  nil,
		},
		{
			name:  "Label without leading slashes",
			label: "src/sys:test",
			want:  nil,
		},
		{
			name:  "Path escaping root with ..",
			label: "//../../outside:test",
			want:  nil,
		},
		{
			name:  "Label with triple slashes escaping root with ..",
			label: "///../../outside:test",
			want:  nil,
		},
		{
			name:  "Toolchain missing closing parenthesis",
			label: "//src/sys:test(//build/toolchain:x64",
			want:  nil,
		},
		{
			name:  "Toolchain only without label",
			label: "(//build/toolchain:x64)",
			want:  nil,
		},
		{
			name:  "Unmatched closing parenthesis",
			label: "//src/sys:test)",
			want:  nil,
		},
		{
			name:  "Label with parentheses in path without toolchain",
			label: "//src/foo(bar):baz",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src/foo(bar)",
			},
		},
		{
			name:  "Label with parentheses in path and toolchain",
			label: "//src/foo(bar):baz(//build/toolchain:x64)",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src/foo(bar)",
			},
		},
		{
			name:  "Label with parentheses in directory path",
			label: "//src/foo(bar)/tests:my_test",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src/foo(bar)/tests",
			},
		},
		{
			name:  "Toolchain with @// prefix",
			label: "//src/sys:test(@//build/toolchain:x64)",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src/sys",
			},
		},
		{
			name:  "Toolchain with @@// prefix",
			label: "//src/sys:test(@@//build/toolchain:x64)",
			want: &resultpb.TestLocation{
				Repo:     DefaultRepo,
				FileName: "//src/sys",
			},
		},
		{
			name:  "Invalid toolchain ending in parenthesis",
			label: "//src/sys:test(invalid)",
			want:  nil,
		},
		{
			name:  "Filename exceeds max length",
			label: "//" + strings.Repeat("a/", MaxLocationFileNameLength/2+1) + ":my_test",
			want:  nil,
		},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			got, err := parseTestLocation(tc.label)
			if tc.want == nil {
				if err == nil {
					t.Errorf("parseTestLocation(%q) expected error, got nil", tc.label)
				}
			} else {
				if err != nil {
					t.Errorf("parseTestLocation(%q) unexpected error: %v", tc.label, err)
				}
				if !proto.Equal(got, tc.want) {
					t.Errorf("parseTestLocation(%q) = %+v, want %+v", tc.label, got, tc.want)
				}
			}
		})
	}
}

func TestSetTestMetadata_Location(t *testing.T) {
	t.Run("SourceLabel takes precedence", func(t *testing.T) {
		detail := runtests.TestDetails{
			SourceLabel: "//src/sys/foo:foo_test",
			GNLabel:     "//src/gn/bar:bar_test",
		}
		r := &sinkpb.TestResult{}
		setTestMetadata(r, detail, "foo")
		if r.TestMetadata == nil || r.TestMetadata.Location == nil {
			t.Fatalf("expected Location to be populated")
		}
		want := &resultpb.TestLocation{
			Repo:     DefaultRepo,
			FileName: "//src/sys/foo",
		}
		if !proto.Equal(r.TestMetadata.Location, want) {
			t.Errorf("Location mismatch: got %+v, want %+v", r.TestMetadata.Location, want)
		}
	})

	t.Run("GNLabel is used as fallback when SourceLabel is empty", func(t *testing.T) {
		detail := runtests.TestDetails{
			GNLabel: "//src/gn/bar:bar_test(//build/toolchain:x64)",
		}
		r := &sinkpb.TestResult{}
		setTestMetadata(r, detail, "bar")
		if r.TestMetadata == nil || r.TestMetadata.Location == nil {
			t.Fatalf("expected Location to be populated from GNLabel fallback")
		}
		want := &resultpb.TestLocation{
			Repo:     DefaultRepo,
			FileName: "//src/gn/bar",
		}
		if !proto.Equal(r.TestMetadata.Location, want) {
			t.Errorf("Location mismatch: got %+v, want %+v", r.TestMetadata.Location, want)
		}
	})

	t.Run("invalid label leaves location nil", func(t *testing.T) {
		detail := runtests.TestDetails{
			SourceLabel: "invalid label with spaces",
		}
		r := &sinkpb.TestResult{}
		setTestMetadata(r, detail, "invalid")
		if r.TestMetadata == nil {
			t.Fatalf("expected TestMetadata to not be nil")
		}
		if r.TestMetadata.Location != nil {
			t.Errorf("expected Location to be nil, got %+v", r.TestMetadata.Location)
		}
	})
}

func TestTestDetailsToResultSink_Location(t *testing.T) {
	outputRoot := t.TempDir()
	detail := &runtests.TestDetails{
		Name:        "foo_test",
		SourceLabel: "//src/sys/foo:foo_test",
		Status:      runtests.TestSuccess,
		StartTime:   time.Now(),
	}
	result, _, _, err := testDetailsToResultSink([]*resultpb.StringPair{}, detail, outputRoot)
	if err != nil {
		t.Fatalf("testDetailsToResultSink failed: %v", err)
	}
	if result.TestMetadata == nil || result.TestMetadata.Location == nil {
		t.Fatalf("expected TestMetadata.Location to be set")
	}
	want := &resultpb.TestLocation{
		Repo:     DefaultRepo,
		FileName: "//src/sys/foo",
	}
	if !proto.Equal(result.TestMetadata.Location, want) {
		t.Errorf("Location diff: got %+v, want %+v", result.TestMetadata.Location, want)
	}
}

func TestTestCaseToResultSink_Location(t *testing.T) {
	outputRoot := t.TempDir()
	detail := &runtests.TestDetails{
		Name:        "vendor_test",
		SourceLabel: "//vendor/google/tests:vendor_test",
		Status:      runtests.TestSuccess,
		StartTime:   time.Now(),
		TestResult: runtests.TestResult{
			Cases: []runtests.TestCaseResult{
				{
					DisplayName: "vendor_test/case_1",
					SuiteName:   "suite",
					CaseName:    "case_1",
					Status:      runtests.TestSuccess,
				},
			},
		},
	}
	results, _, _ := testCaseToResultSink(detail.Cases, []*resultpb.StringPair{}, detail, outputRoot)
	if len(results) != 1 {
		t.Fatalf("expected 1 result, got %d", len(results))
	}
	res := results[0]
	if res.TestMetadata == nil || res.TestMetadata.Location == nil {
		t.Fatalf("expected TestMetadata.Location to be set")
	}
	want := &resultpb.TestLocation{
		Repo:     VendorGoogleRepo,
		FileName: "//tests",
	}
	if !proto.Equal(res.TestMetadata.Location, want) {
		t.Errorf("Location diff: got %+v, want %+v", res.TestMetadata.Location, want)
	}
}

func TestTestCaseToResultSink_StructuredTestID_TargetTest(t *testing.T) {
	outputRoot := t.TempDir()
	detail := &runtests.TestDetails{
		Name:      "fuchsia-pkg://fuchsia.com/my-package#meta/my-test.cm",
		Status:    runtests.TestSuccess,
		StartTime: time.Now(),
		TestResult: runtests.TestResult{
			Cases: []runtests.TestCaseResult{
				{
					DisplayName: "FrobnicatedSuite.VerifyOutput",
					SuiteName:   "FrobnicatedSuite",
					CaseName:    "VerifyOutput",
					Status:      runtests.TestSuccess,
					Format:      "GoogleTest",
				},
			},
		},
	}
	results, _, _ := testCaseToResultSink(detail.Cases, []*resultpb.StringPair{}, detail, outputRoot)
	if len(results) != 1 {
		t.Fatalf("expected 1 result, got %d", len(results))
	}
	res := results[0]
	if res.TestIdStructured == nil {
		t.Fatalf("expected TestIdStructured to be set")
	}
	want := &sinkpb.TestIdentifier{
		FineName:           "FrobnicatedSuite",
		CaseNameComponents: []string{"VerifyOutput"},
	}
	if !proto.Equal(res.TestIdStructured, want) {
		t.Errorf("TestIdStructured diff: got %+v, want %+v", res.TestIdStructured, want)
	}
	if wantID := "fuchsia-pkg://fuchsia.com/my-package#meta/my-test.cm/FrobnicatedSuite:VerifyOutput"; res.TestId != wantID {
		t.Errorf("TestId diff: got %q, want %q", res.TestId, wantID)
	}
}

func TestTestCaseToResultSink_StructuredTestID_HostTest(t *testing.T) {
	outputRoot := t.TempDir()
	detail := &runtests.TestDetails{
		Name:      "host_x64/absl_hardening_tests",
		Status:    runtests.TestSuccess,
		StartTime: time.Now(),
		TestResult: runtests.TestResult{
			Cases: []runtests.TestCaseResult{
				{
					DisplayName: "AbseilHardeningTest.TestFeatureA",
					SuiteName:   "AbseilHardeningTest",
					CaseName:    "TestFeatureA",
					Status:      runtests.TestSuccess,
					Format:      "GoogleTest",
				},
			},
		},
	}
	results, _, _ := testCaseToResultSink(detail.Cases, []*resultpb.StringPair{}, detail, outputRoot)
	if len(results) != 1 {
		t.Fatalf("expected 1 result, got %d", len(results))
	}
	res := results[0]
	if res.TestIdStructured == nil {
		t.Fatalf("expected TestIdStructured to be set")
	}
	want := &sinkpb.TestIdentifier{
		FineName:           "AbseilHardeningTest",
		CaseNameComponents: []string{"TestFeatureA"},
	}
	if !proto.Equal(res.TestIdStructured, want) {
		t.Errorf("TestIdStructured diff: got %+v, want %+v", res.TestIdStructured, want)
	}
	if wantID := "host_x64/absl_hardening_tests/AbseilHardeningTest:TestFeatureA"; res.TestId != wantID {
		t.Errorf("TestId diff: got %q, want %q", res.TestId, wantID)
	}
}

func TestTestCaseToResultSink_StructuredTestID_RustEmptySuiteName(t *testing.T) {
	outputRoot := t.TempDir()
	detail := &runtests.TestDetails{
		Name:      "host_x64/structured_config_repackaging_test",
		Status:    runtests.TestSuccess,
		StartTime: time.Now(),
		TestResult: runtests.TestResult{
			Cases: []runtests.TestCaseResult{
				{
					DisplayName: "config_requires_values",
					SuiteName:   "",
					CaseName:    "config_requires_values",
					Status:      runtests.TestSuccess,
					Format:      "Rust",
				},
			},
		},
	}
	results, _, _ := testCaseToResultSink(detail.Cases, []*resultpb.StringPair{}, detail, outputRoot)
	if len(results) != 1 {
		t.Fatalf("expected 1 result, got %d", len(results))
	}
	res := results[0]
	if res.TestIdStructured == nil {
		t.Fatalf("expected TestIdStructured to be set")
	}
	want := &sinkpb.TestIdentifier{
		FineName:           "crate",
		CaseNameComponents: []string{"config_requires_values"},
	}
	if !proto.Equal(res.TestIdStructured, want) {
		t.Errorf("TestIdStructured diff: got %+v, want %+v", res.TestIdStructured, want)
	}
	if wantID := "host_x64/structured_config_repackaging_test/:config_requires_values"; res.TestId != wantID {
		t.Errorf("TestId diff: got %q, want %q", res.TestId, wantID)
	}
}

func TestGoPackageName(t *testing.T) {
	tests := []struct {
		name   string
		detail runtests.TestDetails
		want   string
	}{
		{
			name: "from source_label GN with toolchain",
			detail: runtests.TestDetails{
				SourceLabel: "//tools/testing/resultdb:resultdb_lib_tests(//build/toolchain:host_x64)",
			},
			want: "tools/testing/resultdb",
		},
		{
			name: "from gn_label without toolchain",
			detail: runtests.TestDetails{
				GNLabel: "//src/sys/pkg/lib/fuchsia-repo:tests",
			},
			want: "src/sys/pkg/lib/fuchsia-repo",
		},
		{
			name: "from source_label Bazel with @@// prefix",
			detail: runtests.TestDetails{
				SourceLabel: "@@//tools/go_test_parser:go_test_parser_lib_tests",
			},
			want: "tools/go_test_parser",
		},
		{
			name: "from source_label Bazel with @// prefix",
			detail: runtests.TestDetails{
				SourceLabel: "@//src/connectivity/network:tests",
			},
			want: "src/connectivity/network",
		},
		{
			name: "deeply nested directory path",
			detail: runtests.TestDetails{
				SourceLabel: "//src/developer/debug/shared:tests",
			},
			want: "src/developer/debug/shared",
		},
		{
			name: "root target falls back to Name",
			detail: runtests.TestDetails{
				Name:        "host_x64/root_target",
				SourceLabel: "//:root_target",
			},
			want: "host_x64/root_target",
		},
		{
			name: "missing labels falls back to host test Name without suffix stripping",
			detail: runtests.TestDetails{
				Name: "host_x64/resultdb_lib_tests",
			},
			want: "host_x64/resultdb_lib_tests",
		},
		{
			name: "missing labels falls back to fuchsia-pkg Name without URL parsing",
			detail: runtests.TestDetails{
				Name: "fuchsia-pkg://fuchsia.com/my-test-package#meta/my-test.cm",
			},
			want: "fuchsia-pkg://fuchsia.com/my-test-package#meta/my-test.cm",
		},
	}
	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			detail := tc.detail
			detail.Status = runtests.TestSuccess
			detail.Cases = []runtests.TestCaseResult{
				{
					CaseName: "TestFoo",
					Format:   "Go",
					Status:   runtests.TestSuccess,
				},
			}
			results, _, _ := testCaseToResultSink(detail.Cases, []*resultpb.StringPair{}, &detail, "")
			if len(results) == 0 || results[0].TestIdStructured == nil {
				t.Fatalf("expected structured test result")
			}
			if got := results[0].TestIdStructured.FineName; got != tc.want {
				t.Errorf("fine_name = %q, want %q", got, tc.want)
			}
		})
	}
}

func TestTestCaseToResultSink_StructuredTestID_Go(t *testing.T) {
	outputRoot := t.TempDir()
	detail := &runtests.TestDetails{
		Name:        "host_x64/resultdb_lib_tests",
		SourceLabel: "//tools/testing/resultdb:resultdb_lib_tests",
		Status:      runtests.TestSuccess,
		StartTime:   time.Now(),
		TestResult: runtests.TestResult{
			Cases: []runtests.TestCaseResult{
				{
					DisplayName: "TestTopLevel",
					SuiteName:   "",
					CaseName:    "TestTopLevel",
					Status:      runtests.TestSuccess,
					Format:      "Go",
				},
				{
					DisplayName: "TestParent/subtest_1",
					SuiteName:   "TestParent",
					CaseName:    "subtest_1",
					Status:      runtests.TestSuccess,
					Format:      "Go",
				},
				{
					DisplayName: "TestParent/subtest_2/nested",
					SuiteName:   "TestParent",
					CaseName:    "subtest_2/nested",
					Status:      runtests.TestSuccess,
					Format:      "Go",
				},
				{
					DisplayName: "TestSSHRmDir//some/dir",
					SuiteName:   "TestSSHRmDir",
					CaseName:    "/some/dir",
					Status:      runtests.TestSuccess,
					Format:      "Go",
				},
				{
					DisplayName: "TestParse/trailing/\\s+.../",
					SuiteName:   "TestParse",
					CaseName:    "trailing/\\s+.../",
					Status:      runtests.TestSuccess,
					Format:      "Go",
				},
				{
					DisplayName: " #1/!2",
					SuiteName:   "TestPunctuation",
					CaseName:    " #1/!2",
					Status:      runtests.TestSuccess,
					Format:      "Go",
				},
				{
					SuiteName: "TestFallback",
					CaseName:  "FallbackCase",
					Status:    runtests.TestSuccess,
					Format:    "Go",
				},
			},
		},
	}
	results, _, _ := testCaseToResultSink(detail.Cases, []*resultpb.StringPair{}, detail, outputRoot)
	if len(results) != 7 {
		t.Fatalf("expected 7 results, got %d", len(results))
	}

	// 1. Top-level test: package fine_name, test function as single case component.
	want0 := &sinkpb.TestIdentifier{
		FineName:           "tools/testing/resultdb",
		CaseNameComponents: []string{"TestTopLevel"},
	}
	if !proto.Equal(results[0].TestIdStructured, want0) {
		t.Errorf("results[0].TestIdStructured diff: got %+v, want %+v", results[0].TestIdStructured, want0)
	}

	// 2. Subtest: package fine_name, full DisplayName as single case component without splitting.
	want1 := &sinkpb.TestIdentifier{
		FineName:           "tools/testing/resultdb",
		CaseNameComponents: []string{"TestParent/subtest_1"},
	}
	if !proto.Equal(results[1].TestIdStructured, want1) {
		t.Errorf("results[1].TestIdStructured diff: got %+v, want %+v", results[1].TestIdStructured, want1)
	}

	// 3. Nested subtest: package fine_name, full DisplayName preserved verbatim.
	want2 := &sinkpb.TestIdentifier{
		FineName:           "tools/testing/resultdb",
		CaseNameComponents: []string{"TestParent/subtest_2/nested"},
	}
	if !proto.Equal(results[2].TestIdStructured, want2) {
		t.Errorf("results[2].TestIdStructured diff: got %+v, want %+v", results[2].TestIdStructured, want2)
	}

	// 4. Leading / consecutive slashes in subtest name (e.g. t.Run("/some/dir")): slashes preserved verbatim.
	want3 := &sinkpb.TestIdentifier{
		FineName:           "tools/testing/resultdb",
		CaseNameComponents: []string{"TestSSHRmDir//some/dir"},
	}
	if !proto.Equal(results[3].TestIdStructured, want3) {
		t.Errorf("results[3].TestIdStructured diff: got %+v, want %+v", results[3].TestIdStructured, want3)
	}

	// 5. Trailing slash subtest (e.g. t.Run(".../\\s+.../")): slashes preserved verbatim.
	want4 := &sinkpb.TestIdentifier{
		FineName:           "tools/testing/resultdb",
		CaseNameComponents: []string{"TestParse/trailing/\\s+.../"},
	}
	if !proto.Equal(results[4].TestIdStructured, want4) {
		t.Errorf("results[4].TestIdStructured diff: got %+v, want %+v", results[4].TestIdStructured, want4)
	}

	// 6. Leading reserved character: reserved char <= ',' wrapped in brackets, slashes preserved verbatim.
	want5 := &sinkpb.TestIdentifier{
		FineName:           "tools/testing/resultdb",
		CaseNameComponents: []string{"[ #1/!2]"},
	}
	if !proto.Equal(results[5].TestIdStructured, want5) {
		t.Errorf("results[5].TestIdStructured diff: got %+v, want %+v", results[5].TestIdStructured, want5)
	}

	// 7. Fallback to CaseName when DisplayName is empty.
	want6 := &sinkpb.TestIdentifier{
		FineName:           "tools/testing/resultdb",
		CaseNameComponents: []string{"FallbackCase"},
	}
	if !proto.Equal(results[6].TestIdStructured, want6) {
		t.Errorf("results[6].TestIdStructured diff: got %+v, want %+v", results[6].TestIdStructured, want6)
	}
}

func TestTestCaseToResultSink_StructuredTestID_Exoneration(t *testing.T) {
	outputRoot := t.TempDir()
	detail := &runtests.TestDetails{
		Name:      "fuchsia-pkg://fuchsia.com/my-package#meta/my-test.cm",
		Status:    runtests.TestFailure,
		StartTime: time.Now(),
		TestResult: runtests.TestResult{
			Cases: []runtests.TestCaseResult{
				{
					DisplayName: "Suite.ExoneratedCase",
					SuiteName:   "Suite",
					CaseName:    "ExoneratedCase",
					Status:      runtests.TestExonerated,
					Format:      "GoogleTest",
				},
			},
		},
	}
	_, exonerations, _ := testCaseToResultSink(detail.Cases, []*resultpb.StringPair{}, detail, outputRoot)
	if len(exonerations) != 1 {
		t.Fatalf("expected 1 exoneration, got %d", len(exonerations))
	}
	exon := exonerations[0]
	if exon.TestIdStructured == nil {
		t.Fatalf("expected TestIdStructured to be set on exoneration")
	}
	want := &sinkpb.TestIdentifier{
		FineName:           "Suite",
		CaseNameComponents: []string{"ExoneratedCase"},
	}
	if !proto.Equal(exon.TestIdStructured, want) {
		t.Errorf("Exoneration TestIdStructured diff: got %+v, want %+v", exon.TestIdStructured, want)
	}
	if wantID := "fuchsia-pkg://fuchsia.com/my-package#meta/my-test.cm/Suite:ExoneratedCase"; exon.TestId != wantID {
		t.Errorf("Exoneration TestId diff: got %q, want %q", exon.TestId, wantID)
	}
}

func TestTestDetailsToResultSink_StructuredTestID(t *testing.T) {
	outputRoot := t.TempDir()

	t.Run("target test", func(t *testing.T) {
		detail := &runtests.TestDetails{
			Name:      "fuchsia-pkg://fuchsia.com/my-package#meta/my-test.cm",
			Status:    runtests.TestExonerated,
			StartTime: time.Now(),
		}
		res, exon, _, err := testDetailsToResultSink([]*resultpb.StringPair{}, detail, outputRoot)
		if err != nil {
			t.Fatalf("unexpected error: %v", err)
		}
		want := &sinkpb.TestIdentifier{
			CaseNameComponents: []string{"*fixture"},
		}
		if !proto.Equal(res.TestIdStructured, want) {
			t.Errorf("TestIdStructured diff: got %+v, want %+v", res.TestIdStructured, want)
		}
		if exon == nil || exon.TestIdStructured == nil {
			t.Fatalf("expected exoneration with TestIdStructured")
		}
		if !proto.Equal(exon.TestIdStructured, want) {
			t.Errorf("Exoneration TestIdStructured diff: got %+v, want %+v", exon.TestIdStructured, want)
		}
	})

	t.Run("host test", func(t *testing.T) {
		detail := &runtests.TestDetails{
			Name:      "host_x64/standalone_tool_test",
			Status:    runtests.TestSuccess,
			StartTime: time.Now(),
		}
		res, _, _, err := testDetailsToResultSink([]*resultpb.StringPair{}, detail, outputRoot)
		if err != nil {
			t.Fatalf("unexpected error: %v", err)
		}
		want := &sinkpb.TestIdentifier{
			CaseNameComponents: []string{"*fixture"},
		}
		if !proto.Equal(res.TestIdStructured, want) {
			t.Errorf("TestIdStructured diff: got %+v, want %+v", res.TestIdStructured, want)
		}
	})

	t.Run("test with cases has nil TestIdStructured", func(t *testing.T) {
		detail := &runtests.TestDetails{
			Name:      "fuchsia-pkg://fuchsia.com/my-package#meta/my-test.cm",
			Status:    runtests.TestSuccess,
			StartTime: time.Now(),
			TestResult: runtests.TestResult{
				Cases: []runtests.TestCaseResult{
					{
						SuiteName: "Suite",
						CaseName:  "Case",
						Status:    runtests.TestSuccess,
					},
				},
			},
		}
		res, _, _, err := testDetailsToResultSink([]*resultpb.StringPair{}, detail, outputRoot)
		if err != nil {
			t.Fatalf("unexpected error: %v", err)
		}
		if res.TestId != detail.Name {
			t.Errorf("TestId diff: got %q, want %q", res.TestId, detail.Name)
		}
		if res.TestIdStructured != nil {
			t.Errorf("expected TestIdStructured to be nil when cases exist, got: %+v", res.TestIdStructured)
		}
	})
}

func TestTestCaseToResultSink_StructuredTestID_LeadingDisallowedChars(t *testing.T) {
	outputRoot := t.TempDir()

	testCases := []struct {
		name         string
		suiteName    string
		caseName     string
		wantFine     string
		wantCase     string
		wantLegacyID string
	}{
		{
			name:         "component URL fragment case name starting with hash",
			suiteName:    "",
			caseName:     "#meta/test.cm",
			wantFine:     "",
			wantCase:     "[#meta/test.cm]",
			wantLegacyID: "fuchsia-pkg://fuchsia.com/my-pkg#meta/my-test.cm/:#meta/test.cm",
		},
		{
			name:         "quoted case name starting with double quote",
			suiteName:    "TestParseValues",
			caseName:     "\"hello\"",
			wantFine:     "TestParseValues",
			wantCase:     "[\"hello\"]",
			wantLegacyID: "fuchsia-pkg://fuchsia.com/my-pkg#meta/my-test.cm/TestParseValues:\"hello\"",
		},
		{
			name:         "case name with multiple leading disallowed characters and spaces",
			suiteName:    "my-suite",
			caseName:     "   #!test_case",
			wantFine:     "my-suite",
			wantCase:     "[   #!test_case]",
			wantLegacyID: "fuchsia-pkg://fuchsia.com/my-pkg#meta/my-test.cm/my-suite:   #!test_case",
		},
		{
			name:         "leading asterisk is wrapped in brackets",
			suiteName:    "MySuite",
			caseName:     "*fixture",
			wantFine:     "MySuite",
			wantCase:     "[*fixture]",
			wantLegacyID: "fuchsia-pkg://fuchsia.com/my-pkg#meta/my-test.cm/MySuite:*fixture",
		},
		{
			name:         "regular case name without disallowed prefix is unchanged",
			suiteName:    "MySuite",
			caseName:     "MyCase",
			wantFine:     "MySuite",
			wantCase:     "MyCase",
			wantLegacyID: "fuchsia-pkg://fuchsia.com/my-pkg#meta/my-test.cm/MySuite:MyCase",
		},
		{
			name:         "empty case name produces empty case component",
			suiteName:    "MySuite",
			caseName:     "",
			wantFine:     "MySuite",
			wantCase:     "",
			wantLegacyID: "fuchsia-pkg://fuchsia.com/my-pkg#meta/my-test.cm/MySuite:",
		},
		{
			name:         "case name starting with hash is wrapped in brackets",
			suiteName:    "MySuite",
			caseName:     "###",
			wantFine:     "MySuite",
			wantCase:     "[###]",
			wantLegacyID: "fuchsia-pkg://fuchsia.com/my-pkg#meta/my-test.cm/MySuite:###",
		},
		{
			name:         "case name with single double quote is wrapped in brackets",
			suiteName:    "TestFailsParseValues",
			caseName:     "\"",
			wantFine:     "TestFailsParseValues",
			wantCase:     "[\"]",
			wantLegacyID: "fuchsia-pkg://fuchsia.com/my-pkg#meta/my-test.cm/TestFailsParseValues:\"",
		},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			detail := &runtests.TestDetails{
				Name:      "fuchsia-pkg://fuchsia.com/my-pkg#meta/my-test.cm",
				Status:    runtests.TestSuccess,
				StartTime: time.Now(),
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{
						{
							SuiteName: tc.suiteName,
							CaseName:  tc.caseName,
							Status:    runtests.TestSuccess,
							Format:    "GoogleTest",
						},
					},
				},
			}
			results, _, _ := testCaseToResultSink(detail.Cases, []*resultpb.StringPair{}, detail, outputRoot)
			if len(results) != 1 {
				t.Fatalf("expected 1 result, got %d", len(results))
			}
			res := results[0]
			if res.TestIdStructured == nil {
				t.Fatalf("expected TestIdStructured to be set")
			}
			want := &sinkpb.TestIdentifier{
				FineName:           tc.wantFine,
				CaseNameComponents: []string{tc.wantCase},
			}
			if !proto.Equal(res.TestIdStructured, want) {
				t.Errorf("TestIdStructured diff: got %+v, want %+v", res.TestIdStructured, want)
			}
			if res.TestId != tc.wantLegacyID {
				t.Errorf("legacy TestId diff: got %q, want %q", res.TestId, tc.wantLegacyID)
			}
		})
	}
}

func TestTestCaseToResultSink_StructuredTestID_FlatScheme(t *testing.T) {
	outputRoot := t.TempDir()
	detail := &runtests.TestDetails{
		Name:      "//scripts/shac/tests:cml_test",
		Status:    runtests.TestSuccess,
		StartTime: time.Now(),
		TestResult: runtests.TestResult{
			Cases: []runtests.TestCaseResult{
				{
					DisplayName: "test_cml_format",
					SuiteName:   ":shac",
					CaseName:    "test_cml_format",
					Status:      runtests.TestSuccess,
					Format:      "shac",
				},
				{
					DisplayName: "InlineDirTest.InlineDirPino",
					SuiteName:   "",
					CaseName:    "InlineDirTest.InlineDirPino",
					Status:      runtests.TestSuccess,
					Format:      "FTF",
				},
				{
					SuiteName: "some_suite",
					CaseName:  "some_case",
					Status:    runtests.TestSuccess,
					Format:    "flat",
				},
			},
		},
	}
	results, _, _ := testCaseToResultSink(detail.Cases, []*resultpb.StringPair{}, detail, outputRoot)
	if len(results) != 3 {
		t.Fatalf("expected 3 results, got %d", len(results))
	}
	// ResultDB scheme "flat" defines only the case level; fine_name must be empty.
	want0 := &sinkpb.TestIdentifier{
		FineName:           "",
		CaseNameComponents: []string{"test_cml_format"},
	}
	if !proto.Equal(results[0].TestIdStructured, want0) {
		t.Errorf("results[0].TestIdStructured diff: got %+v, want %+v", results[0].TestIdStructured, want0)
	}

	want1 := &sinkpb.TestIdentifier{
		FineName:           "",
		CaseNameComponents: []string{"InlineDirTest.InlineDirPino"},
	}
	if !proto.Equal(results[1].TestIdStructured, want1) {
		t.Errorf("results[1].TestIdStructured diff: got %+v, want %+v", results[1].TestIdStructured, want1)
	}

	want2 := &sinkpb.TestIdentifier{
		FineName:           "",
		CaseNameComponents: []string{"some_case"},
	}
	if !proto.Equal(results[2].TestIdStructured, want2) {
		t.Errorf("results[2].TestIdStructured diff: got %+v, want %+v", results[2].TestIdStructured, want2)
	}
}

func TestDetermineScheme(t *testing.T) {
	testCases := []struct {
		name       string
		detail     *runtests.TestDetails
		wantScheme string
	}{
		{
			name:       "empty cases defaults to single",
			detail:     &runtests.TestDetails{},
			wantScheme: "single",
		},
		{
			name: "cases present with empty format defaults to flat",
			detail: &runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{
						{CaseName: "test_something"},
					},
				},
			},
			wantScheme: "flat",
		},
		{
			name: "googletest format maps to gtest",
			detail: &runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{Format: "GoogleTest"}},
				},
			},
			wantScheme: "gtest",
		},
		{
			name: "gtest format maps to gtest",
			detail: &runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{Format: "gtest"}},
				},
			},
			wantScheme: "gtest",
		},
		{
			name: "rust format maps to rust",
			detail: &runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{Format: "Rust"}},
				},
			},
			wantScheme: "rust",
		},
		{
			name: "go format maps to go",
			detail: &runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{Format: "Go"}},
				},
			},
			wantScheme: "go",
		},
		{
			name: "mobly format maps to mobly",
			detail: &runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{Format: "Mobly"}},
				},
			},
			wantScheme: "mobly",
		},
		{
			name: "junit format maps to junit",
			detail: &runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{Format: "JUnit"}},
				},
			},
			wantScheme: "junit",
		},
		{
			name: "ftf format maps to flat",
			detail: &runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{Format: "FTF"}},
				},
			},
			wantScheme: "flat",
		},
		{
			name: "shac format maps to flat",
			detail: &runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{Format: "shac"}},
				},
			},
			wantScheme: "flat",
		},
		{
			name: "unknown format defaults to flat",
			detail: &runtests.TestDetails{
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{{Format: "Unknown"}},
				},
			},
			wantScheme: "flat",
		},
		{
			name: "cases with conflicting formats falls back to flat",
			detail: &runtests.TestDetails{
				Name: "conflicting-test",
				TestResult: runtests.TestResult{
					Cases: []runtests.TestCaseResult{
						{Format: "FTF"},
						{Format: "GoogleTest"},
					},
				},
			},
			wantScheme: "flat",
		},
	}

	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			got := DetermineScheme(tc.detail)
			if got != tc.wantScheme {
				t.Errorf("DetermineScheme() = %q, want %q", got, tc.wantScheme)
			}
		})
	}
}

func TestParentInvocationLevelArtifacts(t *testing.T) {
	tempDir := t.TempDir()
	logFile := filepath.Join(tempDir, "syslog.txt")
	if err := os.WriteFile(logFile, []byte("syslog"), 0644); err != nil {
		t.Fatal(err)
	}
	artifacts := parentInvocationLevelArtifacts(tempDir, []string{"syslog.txt"})
	if _, ok := artifacts["parent/syslog.txt"]; !ok {
		t.Errorf("expected key parent/syslog.txt, got keys %v", artifacts)
	}
}

func TestTestToUploadRequests(t *testing.T) {
	outputRoot := t.TempDir()
	detail := &runtests.TestDetails{
		Name:      "fuchsia-pkg://fuchsia.com/my-pkg#meta/my-test.cm",
		Status:    runtests.TestSuccess,
		StartTime: time.Now(),
		TestResult: runtests.TestResult{
			Cases: []runtests.TestCaseResult{
				{
					SuiteName: "MySuite",
					CaseName:  "MyCase",
					Status:    runtests.TestSuccess,
					Format:    "Rust",
				},
				{
					SuiteName: "MySuite",
					CaseName:  "MyExoneratedCase",
					Status:    runtests.TestExonerated,
					Format:    "Rust",
				},
			},
		},
	}

	reqs, skipped, err := TestToUploadRequests(detail, []*resultpb.StringPair{}, outputRoot, nil)
	if err != nil {
		t.Fatalf("TestToUploadRequests failed: %v", err)
	}
	if len(skipped) != 0 {
		t.Errorf("Unexpected skipped tests: %v", skipped)
	}
	if len(reqs) != 2 {
		t.Fatalf("Got %d upload requests, want 2 (results + exonerations)", len(reqs))
	}

	if reqs[0].Endpoint != "ReportTestResults" {
		t.Errorf("reqs[0].Endpoint = %q, want ReportTestResults", reqs[0].Endpoint)
	}
	var testResultsReq sinkpb.ReportTestResultsRequest
	if err := protojson.Unmarshal(reqs[0].Data, &testResultsReq); err != nil {
		t.Fatalf("Failed to unmarshal ReportTestResultsRequest: %v", err)
	}
	if len(testResultsReq.TestResults) != 3 {
		// 2 test cases + 1 suite-level test detail (suite-level results are omitted in stacked CL per b/527958757)
		t.Errorf("Got %d test results in request, want 3", len(testResultsReq.TestResults))
	}

	if reqs[1].Endpoint != "ReportTestExonerations" {
		t.Errorf("reqs[1].Endpoint = %q, want ReportTestExonerations", reqs[1].Endpoint)
	}
	var testExonerationsReq sinkpb.ReportTestExonerationsRequest
	if err := protojson.Unmarshal(reqs[1].Data, &testExonerationsReq); err != nil {
		t.Fatalf("Failed to unmarshal ReportTestExonerationsRequest: %v", err)
	}
	if len(testExonerationsReq.TestExonerations) != 1 {
		t.Errorf("Got %d test exonerations in request, want 1", len(testExonerationsReq.TestExonerations))
	}

	// 2. With OutputFiles: sub-invocation-level artifacts request is appended.
	outputDir := "my_output_dir"
	if err := os.MkdirAll(filepath.Join(outputRoot, outputDir), 0755); err != nil {
		t.Fatal(err)
	}
	logFile := filepath.Join(outputRoot, outputDir, "stdout-and-stderr.txt")
	if err := os.WriteFile(logFile, []byte("log content"), 0644); err != nil {
		t.Fatal(err)
	}
	detail.OutputDir = outputDir
	detail.OutputFiles = []string{"stdout-and-stderr.txt"}

	reqsWithArtifacts, skipped, err := TestToUploadRequests(detail, []*resultpb.StringPair{}, outputRoot, nil)
	if err != nil {
		t.Fatalf("TestToUploadRequests with artifacts failed: %v", err)
	}
	if len(skipped) != 0 {
		t.Errorf("Unexpected skipped tests: %v", skipped)
	}
	if len(reqsWithArtifacts) != 3 {
		t.Fatalf("Got %d upload requests, want 3 (results + exonerations + artifacts)", len(reqsWithArtifacts))
	}
	if reqsWithArtifacts[2].Endpoint != "ReportInvocationLevelArtifacts" {
		t.Errorf("reqsWithArtifacts[2].Endpoint = %q, want ReportInvocationLevelArtifacts", reqsWithArtifacts[2].Endpoint)
	}
	var invArtifactsReq sinkpb.ReportInvocationLevelArtifactsRequest
	if err := protojson.Unmarshal(reqsWithArtifacts[2].Data, &invArtifactsReq); err != nil {
		t.Fatalf("Failed to unmarshal ReportInvocationLevelArtifactsRequest: %v", err)
	}
	if len(invArtifactsReq.Artifacts) != 1 {
		t.Errorf("Got %d artifacts in request, want 1", len(invArtifactsReq.Artifacts))
	}
	if artifact, ok := invArtifactsReq.Artifacts["stdout-and-stderr.txt"]; !ok {
		t.Errorf("Artifact stdout-and-stderr.txt not found in request: %+v", invArtifactsReq.Artifacts)
	} else if artifact.GetFilePath() != logFile {
		t.Errorf("Artifact FilePath = %q, want %q", artifact.GetFilePath(), logFile)
	}

	// 3. With parent invocation artifacts: sub-invocation includes artifacts with "parent/" prefix.
	parentLogFile := filepath.Join(outputRoot, "infra_and_test_std_and_klog.txt")
	if err := os.WriteFile(parentLogFile, []byte("parent log content"), 0644); err != nil {
		t.Fatal(err)
	}
	reqsWithParent, skipped, err := TestToUploadRequests(detail, []*resultpb.StringPair{}, outputRoot, []string{"infra_and_test_std_and_klog.txt"})
	if err != nil {
		t.Fatalf("TestToUploadRequests with parent artifacts failed: %v", err)
	}
	if len(skipped) != 0 {
		t.Errorf("Unexpected skipped tests: %v", skipped)
	}
	if len(reqsWithParent) != 3 {
		t.Fatalf("Got %d upload requests, want 3", len(reqsWithParent))
	}
	var parentInvArtifactsReq sinkpb.ReportInvocationLevelArtifactsRequest
	if err := protojson.Unmarshal(reqsWithParent[2].Data, &parentInvArtifactsReq); err != nil {
		t.Fatalf("Failed to unmarshal ReportInvocationLevelArtifactsRequest: %v", err)
	}
	if len(parentInvArtifactsReq.Artifacts) != 2 {
		t.Errorf("Got %d artifacts in request, want 2", len(parentInvArtifactsReq.Artifacts))
	}
	if artifact, ok := parentInvArtifactsReq.Artifacts["parent/infra_and_test_std_and_klog.txt"]; !ok {
		t.Errorf("parent/infra_and_test_std_and_klog.txt not found in request: %+v", parentInvArtifactsReq.Artifacts)
	} else if artifact.GetFilePath() != parentLogFile {
		t.Errorf("parent artifact FilePath = %q, want %q", artifact.GetFilePath(), parentLogFile)
	}
}
