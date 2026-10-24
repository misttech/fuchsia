// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package resultdb

import (
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/google/go-cmp/cmp"
	resultpb "go.chromium.org/luci/resultdb/proto/v1"
	sinkpb "go.chromium.org/luci/resultdb/sink/proto/v1"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/testing/protocmp"

	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

var testDataDir = flag.String("test_data_dir", "testdata", "Path to testdata/; only used in GN build")

func TestGetLUCICtx(t *testing.T) {
	old := os.Getenv("LUCI_CONTEXT")
	defer os.Setenv("LUCI_CONTEXT", old)
	os.Setenv("LUCI_CONTEXT", filepath.Join(*testDataDir, "lucictx.json"))
	client, err := NewClient()
	if err != nil {
		t.Errorf("Cannot parse LUCI_CONTEXT: %v", err)
	}
	if client.resultSink.ResultSinkAddr != "result.sink" {
		t.Errorf("Incorrect value parsed for result_sink address. Got %s", client.resultSink.ResultSinkAddr)
	}
	if client.resultSink.AuthToken != "token" {
		t.Errorf("Incorrect value parsed for result_sink auth_token field. Got %s", client.resultSink.AuthToken)
	}
}

func TestParse2Summary(t *testing.T) {
	t.Parallel()
	for _, name := range []string{"summary.json", "summary2.json"} {
		summary, err := ParseSummaryTests(filepath.Join(*testDataDir, name))
		if err != nil {
			t.Fatal(err)
		}
		testResults, _, skipped := SummaryToResultSink(summary, []*resultpb.StringPair{}, name)
		for _, testResult := range testResults {
			if len(testResult.TestId) == 0 {
				t.Errorf("Empty testId is not allowed.")
			}
		}
		if len(skipped) != 0 {
			t.Errorf("Tests got skipped %v, expect no skip", skipped)
		}
	}
}

func TestFailWithLongTestName(t *testing.T) {
	summary, err := ParseSummaryTests(filepath.Join(*testDataDir, "summary_long_name.json"))
	if err != nil {
		t.Fatal(err)
	}
	_, _, testsSkipped := SummaryToResultSink(summary, []*resultpb.StringPair{}, "")

	skippedTestName := "fuchsia-pkg://fuchsia.com/netstack-integration-tests#meta/netstack-inspect-integration-test.cm/:fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_inspect_dhcp_netdevice::_multiple_invalid_port_and_single_invalid_trans_proto_vec_packetattributes_ip_proto_packet_formats_ip_ipv4proto_proto_packet_formats_ip_ipproto_udp_port_invalid_port_packetattributes_ip_proto_packet_formats_ip_ipv4proto_proto_packet_formats_ip_ipproto_udp_port_invalid_port_packetattributes_ip_proto_packet_formats_ip_ipv4proto_proto_packet_formats_ip_ipproto_tcp_port_dhcp_client_port_"

	if len(testsSkipped) != 1 {
		t.Errorf("Incorrect number of skipped tests got: %d want %d", len(testsSkipped), 1)
	}
	if testsSkipped[0] != skippedTestName {
		t.Errorf("Incorrect skipped test, got: %s want %s", testsSkipped[0], skippedTestName)
	}
}

func TestParseExoneratedSummary(t *testing.T) {
	// 1. Load and map the custom summary containing exonerations
	summary, err := ParseSummaryTests(filepath.Join(*testDataDir, "summary_exonerated.json"))
	if err != nil {
		t.Fatal(err)
	}

	// Assert total count of target items in summary returned by ParseSummaryTests
	if len(summary.Tests) != 5 {
		t.Fatalf("Unexpected parsed tests count inside summary: got %d, want 5", len(summary.Tests))
	}

	results, exonerations, skipped := SummaryToResultSink(summary, []*resultpb.StringPair{}, "summary_exonerated.json")

	// Expect exactly 6 results:
	// - 1 from prefix/exonerated_target_only (exonerated target is mapped as FAILED)
	// - 3 from prefix/failed_target_with_exonerated_case (1 exonerated case (FAILED) + 1 passed case (PASSED) + 1 failed target details (FAILED))
	// - 1 from prefix/passed_target (passed target itself)
	// - 1 from prefix/skipped_target (skipped target itself)
	// - 0 from prefix/unloaded_target_exceeding_limit_... (skipped upload target due to name length exceeds 512 limit)
	if len(results) != 6 {
		t.Fatalf("Unexpected number of mapped results: got %d, want 6", len(results))
	}
	// Expect exactly 2 exonerations:
	// - 1 target exoneration from prefix/exonerated_target_only
	// - 1 case exoneration from prefix/failed_target_with_exonerated_case/SuiteX:CaseExonerated
	if len(exonerations) != 2 {
		t.Fatalf("Unexpected number of mapped exonerations: got %d, want 2", len(exonerations))
	}

	// Expect exactly 1 skipped target run exceeding the 512-byte limit
	if len(skipped) != 1 {
		t.Fatalf("Got %d skipped tests, want 1", len(skipped))
	}
	expectedSkippedName := "prefix/unloaded_target_exceeding_limit_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia_fuchsia"
	if skipped[0] != expectedSkippedName {
		t.Errorf("Unexpected skipped target returned: got %q, want %q", skipped[0], expectedSkippedName)
	}

	// 2. Verify mapped payload details
	// results[0]: prefix/exonerated_target_only (FAILED)
	if results[0].TestId != "prefix/exonerated_target_only" || results[0].StatusV2 != resultpb.TestResult_FAILED {
		t.Errorf("Unexpected mapped result 0: %+v", results[0])
	}

	// results[1]: prefix/failed_target_with_exonerated_case/SuiteX:CaseExonerated (FAILED)
	if results[1].TestId != "prefix/failed_target_with_exonerated_case/SuiteX:CaseExonerated" || results[1].StatusV2 != resultpb.TestResult_FAILED {
		t.Errorf("Unexpected mapped result 1: %+v", results[1])
	}
	if results[1].FailureReason == nil || len(results[1].FailureReason.Errors) == 0 || results[1].FailureReason.Errors[0].Message != "Flaky run instance" {
		t.Errorf("Unexpected FailureReason for result 1: %+v", results[1].FailureReason)
	}

	// results[2]: prefix/failed_target_with_exonerated_case/SuiteX:CasePassed (PASSED)
	if results[2].TestId != "prefix/failed_target_with_exonerated_case/SuiteX:CasePassed" || results[2].StatusV2 != resultpb.TestResult_PASSED {
		t.Errorf("Unexpected mapped result 2: %+v", results[2])
	}

	// results[3]: prefix/failed_target_with_exonerated_case (FAILED)
	if results[3].TestId != "prefix/failed_target_with_exonerated_case" || results[3].StatusV2 != resultpb.TestResult_FAILED {
		t.Errorf("Unexpected mapped result 3: %+v", results[3])
	}
	expectedErrorMessage := "CaseExonerated: Flaky run instance"
	if results[3].FailureReason == nil || len(results[3].FailureReason.Errors) == 0 || results[3].FailureReason.Errors[0].Message != expectedErrorMessage {
		t.Errorf("Target failure reason was not populated with the failed cases. got err: %q", results[3].FailureReason.GetErrors()[0].GetMessage())
	}

	// results[4]: prefix/passed_target (PASSED)
	if results[4].TestId != "prefix/passed_target" || results[4].StatusV2 != resultpb.TestResult_PASSED {
		t.Errorf("Unexpected mapped result 4: %+v", results[4])
	}

	// results[5]: prefix/skipped_target (SKIPPED)
	if results[5].TestId != "prefix/skipped_target" || results[5].StatusV2 != resultpb.TestResult_SKIPPED {
		t.Errorf("Unexpected mapped result 5: %+v", results[5])
	}
	if results[5].SkippedReason == nil || results[5].SkippedReason.ReasonMessage != "skipped because unaffected" {
		t.Errorf("Skipped target mapped incorrect skipped reason, got: %+v", results[5].SkippedReason)
	}

	// Expect the exonerations mapped to:
	// - Case exoneration: prefix/failed_target_with_exonerated_case/SuiteX:CaseExonerated
	// - Target exoneration: prefix/exonerated_target_only
	exoneratedIDs := []string{exonerations[0].TestId, exonerations[1].TestId}
	sort.Strings(exoneratedIDs)
	expectedIDs := []string{
		"prefix/exonerated_target_only",
		"prefix/failed_target_with_exonerated_case/SuiteX:CaseExonerated",
	}
	if diff := cmp.Diff(exoneratedIDs, expectedIDs); diff != "" {
		t.Errorf("Exoneration IDs differ (-got +want):\n%s", diff)
	}
}

func TestSendData(t *testing.T) {
	var receivedBody string
	var receivedAuthHeader string
	var receivedPath string

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		receivedAuthHeader = r.Header.Get("Authorization")
		receivedPath = r.URL.Path
		b, err := io.ReadAll(r.Body)
		if err != nil {
			w.WriteHeader(http.StatusInternalServerError)
			return
		}
		receivedBody = string(b)
		w.WriteHeader(http.StatusOK)
		w.Write([]byte("{}"))
	}))
	defer server.Close()

	client := &Client{
		resultSink: &resultSink{
			AuthToken:      "test-token",
			ResultSinkAddr: server.Listener.Addr().String(),
		},
		httpClient: &http.Client{},
		semaphore:  make(chan struct{}, 1),
	}
	client.semaphore <- struct{}{}

	payload := `{"testResults":[{"testId":"foo"}]}`
	if err := client.SendData(payload, "ReportTestResults"); err != nil {
		t.Fatalf("SendData failed: %v", err)
	}

	if receivedAuthHeader != "ResultSink test-token" {
		t.Errorf("got auth header %q, want %q", receivedAuthHeader, "ResultSink test-token")
	}
	if receivedPath != "/prpc/luci.resultsink.v1.Sink/ReportTestResults" {
		t.Errorf("got path %q, want %q", receivedPath, "/prpc/luci.resultsink.v1.Sink/ReportTestResults")
	}
	if receivedBody != payload {
		t.Errorf("got body %q, want %q", receivedBody, payload)
	}
}

func TestSendDataError(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusBadRequest)
	}))
	defer server.Close()

	client := &Client{
		resultSink: &resultSink{
			AuthToken:      "test-token",
			ResultSinkAddr: server.Listener.Addr().String(),
		},
		httpClient: &http.Client{},
		semaphore:  make(chan struct{}, 1),
	}
	client.semaphore <- struct{}{}

	if err := client.SendData(`{}`, "ReportTestResults"); err == nil {
		t.Errorf("expected error for HTTP 400, got nil")
	}
}

func TestReportSummariesMatchesSummaryToResultSink(t *testing.T) {
	summaries := []string{
		filepath.Join(*testDataDir, "summary.json"),
		filepath.Join(*testDataDir, "summary2.json"),
		filepath.Join(*testDataDir, "summary_exonerated.json"),
	}
	tags := []*resultpb.StringPair{
		{Key: "builder", Value: "core.x64-release"},
	}

	var wantResults []*sinkpb.TestResult
	var wantExonerations []*sinkpb.TestExoneration
	var wantSkipped []string
	for _, summaryFile := range summaries {
		summary, err := ParseSummaryTests(summaryFile)
		if err != nil {
			t.Fatalf("ParseSummaryTests(%s) failed: %s", summaryFile, err)
		}
		results, exonerations, skipped := SummaryToResultSink(summary, tags, *testDataDir)
		wantResults = append(wantResults, results...)
		wantExonerations = append(wantExonerations, exonerations...)
		wantSkipped = append(wantSkipped, skipped...)
	}

	var mu sync.Mutex
	var gotResults []*sinkpb.TestResult
	var gotExonerations []*sinkpb.TestExoneration
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			t.Errorf("failed reading request body: %s", err)
			w.WriteHeader(http.StatusInternalServerError)
			return
		}
		mu.Lock()
		defer mu.Unlock()
		switch {
		case strings.HasSuffix(r.URL.Path, "/ReportTestResults"):
			var req sinkpb.ReportTestResultsRequest
			if err := protojson.Unmarshal(body, &req); err != nil {
				t.Errorf("failed unmarshaling ReportTestResultsRequest: %s", err)
			}
			if len(req.TestResults) > MaxBatchSize {
				t.Errorf("batch size %d exceeds MaxBatchSize %d", len(req.TestResults), MaxBatchSize)
			}
			gotResults = append(gotResults, req.TestResults...)
		case strings.HasSuffix(r.URL.Path, "/ReportTestExonerations"):
			var req sinkpb.ReportTestExonerationsRequest
			if err := protojson.Unmarshal(body, &req); err != nil {
				t.Errorf("failed unmarshaling ReportTestExonerationsRequest: %s", err)
			}
			if len(req.TestExonerations) > MaxBatchSize {
				t.Errorf("batch size %d exceeds MaxBatchSize %d", len(req.TestExonerations), MaxBatchSize)
			}
			gotExonerations = append(gotExonerations, req.TestExonerations...)
		default:
			t.Errorf("unexpected endpoint %s", r.URL.Path)
		}
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("{}"))
	}))
	defer srv.Close()

	client := &Client{
		resultSink: &resultSink{
			AuthToken:      "token",
			ResultSinkAddr: strings.TrimPrefix(srv.URL, "http://"),
		},
		httpClient: srv.Client(),
		semaphore:  make(chan struct{}, 64),
	}
	for i := 0; i < cap(client.semaphore); i++ {
		client.semaphore <- struct{}{}
	}

	gotSkipped, err := client.ReportSummaries(summaries, tags, *testDataDir)
	if err != nil {
		t.Fatalf("ReportSummaries failed: %s", err)
	}

	if diff := cmp.Diff(wantSkipped, gotSkipped); diff != "" {
		t.Errorf("skipped tests differ (-want +got):\n%s", diff)
	}
	sortTags := protocmp.SortRepeatedFields(&sinkpb.TestResult{}, "tags")
	if diff := cmp.Diff(wantResults, gotResults, protocmp.Transform(), sortTags); diff != "" {
		t.Errorf("reported results differ (-want +got):\n%s", diff)
	}
	if diff := cmp.Diff(wantExonerations, gotExonerations, protocmp.Transform()); diff != "" {
		t.Errorf("reported exonerations differ (-want +got):\n%s", diff)
	}
}

func TestReportSummariesBatching(t *testing.T) {
	// Generate a summary with > 2*MaxBatchSize results and exonerations to
	// exercise multi-batch flushing and final tail flushing.
	const caseCount = MaxBatchSize*2 + 10
	cases := make([]runtests.TestCaseResult, caseCount)
	for i := range cases {
		cases[i] = runtests.TestCaseResult{
			DisplayName: fmt.Sprintf("suite/case_%d", i),
			SuiteName:   "suite",
			CaseName:    fmt.Sprintf("case_%d", i),
			Status:      runtests.TestExonerated,
			Format:      "Rust",
		}
	}
	summary := runtests.TestSummary{
		Outputs: map[string]string{"syslog.txt": "syslog.txt"},
		Tests: []runtests.TestDetails{
			{
				Name:        "host_x64/multiplied_test",
				SourceLabel: "//src/foo:multiplied_test",
				Status:      runtests.TestFailure,
				StartTime:   time.Unix(1700000000, 0),
				TestResult:  runtests.TestResult{Cases: cases},
			},
		},
	}
	summaryBytes, err := json.Marshal(summary)
	if err != nil {
		t.Fatalf("failed marshaling summary: %s", err)
	}
	summaryPath := filepath.Join(t.TempDir(), "summary.json")
	if err := os.WriteFile(summaryPath, summaryBytes, 0o600); err != nil {
		t.Fatalf("failed writing summary: %s", err)
	}

	var resultBatchSizes []int
	var exonerationBatchSizes []int
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			t.Errorf("failed reading body: %s", err)
			w.WriteHeader(http.StatusInternalServerError)
			return
		}
		switch {
		case strings.HasSuffix(r.URL.Path, "/ReportTestResults"):
			var req sinkpb.ReportTestResultsRequest
			if err := protojson.Unmarshal(body, &req); err != nil {
				t.Errorf("failed unmarshaling ReportTestResultsRequest: %s", err)
			}
			resultBatchSizes = append(resultBatchSizes, len(req.TestResults))
		case strings.HasSuffix(r.URL.Path, "/ReportTestExonerations"):
			var req sinkpb.ReportTestExonerationsRequest
			if err := protojson.Unmarshal(body, &req); err != nil {
				t.Errorf("failed unmarshaling ReportTestExonerationsRequest: %s", err)
			}
			exonerationBatchSizes = append(exonerationBatchSizes, len(req.TestExonerations))
		}
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("{}"))
	}))
	defer srv.Close()

	client := &Client{
		resultSink: &resultSink{
			AuthToken:      "token",
			ResultSinkAddr: strings.TrimPrefix(srv.URL, "http://"),
		},
		httpClient: srv.Client(),
		semaphore:  make(chan struct{}, 64),
	}
	for i := 0; i < cap(client.semaphore); i++ {
		client.semaphore <- struct{}{}
	}

	skipped, err := client.ReportSummaries([]string{summaryPath}, nil, t.TempDir())
	if err != nil {
		t.Fatalf("ReportSummaries failed: %s", err)
	}
	if len(skipped) != 0 {
		t.Errorf("got skipped %v, want empty", skipped)
	}

	// caseCount cases + 1 top-level test result.
	wantResultBatches := []int{MaxBatchSize, MaxBatchSize, 11}
	if diff := cmp.Diff(wantResultBatches, resultBatchSizes); diff != "" {
		t.Errorf("result batch sizes differ (-want +got):\n%s", diff)
	}
	// caseCount exonerated cases (top-level test has Status=TestFailure, not TestExonerated).
	wantExonerationBatches := []int{MaxBatchSize, MaxBatchSize, 10}
	if diff := cmp.Diff(wantExonerationBatches, exonerationBatchSizes); diff != "" {
		t.Errorf("exoneration batch sizes differ (-want +got):\n%s", diff)
	}
}
