// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"encoding/json"
	"fmt"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	resultpb "go.chromium.org/luci/resultdb/proto/v1"
	"google.golang.org/protobuf/testing/protocmp"

	"go.fuchsia.dev/fuchsia/tools/testing/resultdb"
)

func TestStringPairConvert(t *testing.T) {
	tests := []struct {
		tag  string
		want *resultpb.StringPair
	}{
		{
			tag:  "swarming_id   :   foo_bar",
			want: &resultpb.StringPair{Key: "swarming_id", Value: "foo_bar"},
		},
		{
			tag:  "swarming-bot-id: abc-def",
			want: &resultpb.StringPair{Key: "swarming-bot-id", Value: "abc-def"},
		},
	}
	for _, tc := range tests {
		got, err := stringPairFromString(tc.tag)
		if err != nil {
			t.Errorf("stringPairFromString(%s) errored %v", tc.tag, err)
		}
		if diff := cmp.Diff(tc.want, got, protocmp.Transform()); diff != "" {
			t.Errorf("stringPairFromString diff (-want +got):\n%s", diff)
		}
	}
}

func TestConvertTags(t *testing.T) {
	tags := []string{"tag1:value1", "  tag2  :  value2  ", "tag3:"}
	out, err := convertTags(tags)
	if err != nil {
		t.Errorf("convertTags(%v) errored %v", tags, err)
	}
	if len(out) != len(tags) {
		t.Errorf("convertTags(%v) did not convert to the correct number of keypairs %v", tags, len(tags))
	}
	for _, pair := range out {
		if strings.Contains(pair.Key, " ") || strings.Contains(pair.Value, " ") {
			t.Errorf("convertTags(%v) = %v did not did not remove extra space", tags, pair)
		}
	}
}

func TestReadRealmFromLUCIContext(t *testing.T) {
	t.Run("valid realm", func(t *testing.T) {
		lucictxFile := filepath.Join(t.TempDir(), "lucictx.json")
		if err := os.WriteFile(lucictxFile, []byte(`{"realm": {"name": "fuchsia:ci"}}`), 0600); err != nil {
			t.Fatal(err)
		}
		oldCtx := os.Getenv("LUCI_CONTEXT")
		defer os.Setenv("LUCI_CONTEXT", oldCtx)
		os.Setenv("LUCI_CONTEXT", lucictxFile)

		realm, err := readRealmFromLUCIContext()
		if err != nil {
			t.Fatalf("readRealmFromLUCIContext failed: %v", err)
		}
		if realm != "fuchsia:ci" {
			t.Errorf("readRealmFromLUCIContext got %q, want fuchsia:ci", realm)
		}
	})

	t.Run("missing realm name", func(t *testing.T) {
		lucictxFile := filepath.Join(t.TempDir(), "lucictx.json")
		if err := os.WriteFile(lucictxFile, []byte(`{"realm": {}}`), 0600); err != nil {
			t.Fatal(err)
		}
		oldCtx := os.Getenv("LUCI_CONTEXT")
		defer os.Setenv("LUCI_CONTEXT", oldCtx)
		os.Setenv("LUCI_CONTEXT", lucictxFile)

		if _, err := readRealmFromLUCIContext(); err == nil {
			t.Errorf("readRealmFromLUCIContext expected error for missing realm name, got nil")
		}
	})
}

func TestRunPost(t *testing.T) {
	var receivedEndpoints []string
	var receivedAuthHeader string

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		receivedAuthHeader = r.Header.Get("Authorization")
		receivedEndpoints = append(receivedEndpoints, r.URL.Path)
		w.WriteHeader(http.StatusOK)
		w.Write([]byte("{}"))
	}))
	defer server.Close()

	lucictxFile := filepath.Join(t.TempDir(), "lucictx.json")
	if err := os.WriteFile(lucictxFile, []byte(fmt.Sprintf(`{
		"result_sink": {
			"address": "%s",
			"auth_token": "test-token"
		},
		"realm": {
			"name": "fuchsia:ci"
		}
	}`, server.Listener.Addr().String())), 0600); err != nil {
		t.Fatal(err)
	}

	oldCtx := os.Getenv("LUCI_CONTEXT")
	defer os.Setenv("LUCI_CONTEXT", oldCtx)
	os.Setenv("LUCI_CONTEXT", lucictxFile)

	oldStdin := os.Stdin
	defer func() { os.Stdin = oldStdin }()
	r, w, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	os.Stdin = r
	go func() {
		defer w.Close()
		_ = json.NewEncoder(w).Encode([]resultdb.UploadRequest{
			{
				Endpoint: "ReportTestResults",
				Data:     json.RawMessage(`{"testResults":[{"testId":"foo"}]}`),
			},
			{
				Endpoint: "ReportTestExonerations",
				Data:     json.RawMessage(`{"testExonerations":[{"testId":"foo"}]}`),
			},
		})
	}()

	if err := runPost(); err != nil {
		t.Fatalf("runPost failed: %v", err)
	}

	if receivedAuthHeader != "ResultSink test-token" {
		t.Errorf("got auth header %q, want %q", receivedAuthHeader, "ResultSink test-token")
	}
	if diff := cmp.Diff([]string{
		"/prpc/luci.resultsink.v1.Sink/ReportTestResults",
		"/prpc/luci.resultsink.v1.Sink/ReportTestExonerations",
	}, receivedEndpoints); diff != "" {
		t.Errorf("endpoints diff (-want +got):\n%s", diff)
	}
}

func TestRunParent(t *testing.T) {
	var receivedEndpoints []string
	var receivedAuthHeader string

	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		receivedAuthHeader = r.Header.Get("Authorization")
		receivedEndpoints = append(receivedEndpoints, r.URL.Path)
		w.WriteHeader(http.StatusOK)
		w.Write([]byte("{}"))
	}))
	defer server.Close()

	lucictxFile := filepath.Join(t.TempDir(), "lucictx.json")
	if err := os.WriteFile(lucictxFile, []byte(fmt.Sprintf(`{
		"result_sink": {
			"address": "%s",
			"auth_token": "test-token"
		}
	}`, server.Listener.Addr().String())), 0600); err != nil {
		t.Fatal(err)
	}

	oldCtx := os.Getenv("LUCI_CONTEXT")
	defer os.Setenv("LUCI_CONTEXT", oldCtx)
	os.Setenv("LUCI_CONTEXT", lucictxFile)

	summaryFile := filepath.Join(t.TempDir(), "summary.json")
	if err := os.WriteFile(summaryFile, []byte(`{
		"tests": [
			{
				"name": "host_x64/test1",
				"result": "PASS"
			}
		]
	}`), 0600); err != nil {
		t.Fatal(err)
	}

	summaries = []string{summaryFile}
	tags = []string{"builder:test-builder"}
	outputRoot = t.TempDir()
	invocationArtifacts = nil

	if err := runParent(); err != nil {
		t.Fatalf("runParent failed: %v", err)
	}

	if receivedAuthHeader != "ResultSink test-token" {
		t.Errorf("got auth header %q, want %q", receivedAuthHeader, "ResultSink test-token")
	}
	if diff := cmp.Diff([]string{
		"/prpc/luci.resultsink.v1.Sink/ReportTestResults",
		"/prpc/luci.resultsink.v1.Sink/ReportInvocationLevelArtifacts",
	}, receivedEndpoints); diff != "" {
		t.Errorf("endpoints diff (-want +got):\n%s", diff)
	}
}
