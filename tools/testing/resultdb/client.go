// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package resultdb

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"path/filepath"

	resultpb "go.chromium.org/luci/resultdb/proto/v1"
	sinkpb "go.chromium.org/luci/resultdb/sink/proto/v1"
	"google.golang.org/protobuf/encoding/protojson"
	"google.golang.org/protobuf/proto"

	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

// luciContext corresponds to the schema of the file identified by the
// LUCI_CONTEXT env var. See
// https://crsrc.org/i/go/src/go.chromium.org/luci/lucictx/sections.proto for
// the whole structure.
type luciContext struct {
	ResultDB   resultDB   `json:"resultdb"`
	ResultSink resultSink `json:"result_sink"`
}

// resultSink holds the result_sink information parsed from LUCI_CONTEXT.
type resultSink struct {
	AuthToken      string `json:"auth_token"`
	ResultSinkAddr string `json:"address"`
}

type Client struct {
	resultSink *resultSink
	httpClient *http.Client
	semaphore  chan struct{}
}

type resultDB struct {
	CurrentInvocation resultDBInvocation `json:"current_invocation"`
}

type resultDBInvocation struct {
	Name string `json:"name"`
}

func (c *Client) sendMessage(req proto.Message, endpoint string) error {
	data, err := protojson.Marshal(req)
	if err != nil {
		return err
	}
	return c.SendData(data, endpoint)
}

// ReportSummaries parses each summary.json file and streams test results and
// exonerations to ResultSink in batches of MaxBatchSize. Streaming per test
// entry avoids holding all converted TestResult protobufs and unmarshaled
// TestCaseResult slices in memory at once for large multiplied test runs.
func (c *Client) ReportSummaries(summaries []string, tags []*resultpb.StringPair, outputRoot string) ([]string, error) {
	if len(outputRoot) == 0 {
		outputRoot, _ = os.Getwd()
	}
	rootPath, _ := filepath.Abs(outputRoot)

	var pendingResults []*sinkpb.TestResult
	var pendingExonerations []*sinkpb.TestExoneration
	var allTestsSkipped []string

	flushResults := func(batch []*sinkpb.TestResult) error {
		return c.sendMessage(&sinkpb.ReportTestResultsRequest{TestResults: batch}, "ReportTestResults")
	}
	flushExonerations := func(batch []*sinkpb.TestExoneration) error {
		return c.sendMessage(&sinkpb.ReportTestExonerationsRequest{TestExonerations: batch}, "ReportTestExonerations")
	}

	for _, summaryFile := range summaries {
		if err := forEachSummaryTest(summaryFile, func(test *runtests.TestDetails) error {
			results, exonerations, skipped := testToResultSink(test, tags, rootPath)
			allTestsSkipped = append(allTestsSkipped, skipped...)

			pendingResults = append(pendingResults, results...)
			for len(pendingResults) >= MaxBatchSize {
				if err := flushResults(pendingResults[:MaxBatchSize:MaxBatchSize]); err != nil {
					return err
				}
				clear(pendingResults[:MaxBatchSize])
				pendingResults = pendingResults[MaxBatchSize:]
			}

			pendingExonerations = append(pendingExonerations, exonerations...)
			for len(pendingExonerations) >= MaxBatchSize {
				if err := flushExonerations(pendingExonerations[:MaxBatchSize:MaxBatchSize]); err != nil {
					return err
				}
				clear(pendingExonerations[:MaxBatchSize])
				pendingExonerations = pendingExonerations[MaxBatchSize:]
			}
			return nil
		}); err != nil {
			return nil, err
		}
	}

	if len(pendingResults) > 0 {
		if err := flushResults(pendingResults); err != nil {
			return nil, err
		}
	}
	if len(pendingExonerations) > 0 {
		if err := flushExonerations(pendingExonerations); err != nil {
			return nil, err
		}
	}

	return allTestsSkipped, nil
}

func (c *Client) ReportInvocationLevelArtifacts(outputRoot string, invocationArtifacts []string) error {
	invocationRequest := &sinkpb.ReportInvocationLevelArtifactsRequest{
		Artifacts: InvocationLevelArtifacts(outputRoot, invocationArtifacts),
	}

	return c.sendMessage(invocationRequest, "ReportInvocationLevelArtifacts")
}

// SendData sends a raw JSON payload to the specified endpoint on ResultSink.
func (c *Client) SendData(data []byte, endpoint string) error {
	<-c.semaphore
	defer func() { c.semaphore <- struct{}{} }()

	url := fmt.Sprintf("http://%s/prpc/luci.resultsink.v1.Sink/%s", c.resultSink.ResultSinkAddr, endpoint)
	req, err := http.NewRequest("POST", url, bytes.NewReader(data))
	if err != nil {
		return err
	}
	// ResultSink HTTP authorization scheme is documented at
	// https://fuchsia.googlesource.com/third_party/luci-go/+/HEAD/resultdb/sink/proto/v1/sink.proto#29
	req.Header.Set("Authorization", "ResultSink "+c.resultSink.AuthToken)
	req.Header.Set("Accept", "application/json")
	req.Header.Set("Content-Type", "application/json")
	resp, err := c.httpClient.Do(req)
	if err != nil {
		return err
	}
	// Drain the response body before closing so net/http reuses the persistent
	// loopback TCP connection across batches.
	defer func() {
		_, _ = io.Copy(io.Discard, resp.Body)
		_ = resp.Body.Close()
	}()
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("ResultDB Http Request errored with status code %s (%d)", http.StatusText(resp.StatusCode), resp.StatusCode)
	}
	return nil
}

func NewClient() (*Client, error) {
	b, err := os.ReadFile(os.Getenv("LUCI_CONTEXT"))
	if err != nil {
		return nil, err
	}
	var ctx luciContext
	if err = json.Unmarshal(b, &ctx); err != nil {
		return nil, err
	}
	// We are clearly running inside a LUCI_CONTEXT luciexe environment but rdb
	// stream was not started.
	if ctx.ResultSink.AuthToken == "" || ctx.ResultSink.ResultSinkAddr == "" {
		return nil, fmt.Errorf("resultdb is enabled but not resultsink for invocation. Make sure swarming is run under \"rdb stream\"")
	}

	client := &Client{
		resultSink: &ctx.ResultSink,
		httpClient: &http.Client{},
		semaphore:  make(chan struct{}, 64),
	}

	for i := 0; i < cap(client.semaphore); i++ {
		client.semaphore <- struct{}{}
	}
	return client, nil
}
