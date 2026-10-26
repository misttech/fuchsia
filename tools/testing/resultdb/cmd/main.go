// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bytes"
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"os/exec"
	"strings"

	resultpb "go.chromium.org/luci/resultdb/proto/v1"

	"go.fuchsia.dev/fuchsia/tools/lib/flagmisc"
	"go.fuchsia.dev/fuchsia/tools/testing/resultdb"
)

var (
	summaries           flagmisc.StringsValue
	tags                flagmisc.StringsValue
	outputRoot          string
	invocationArtifacts flagmisc.StringsValue
	isPost              bool

	// runRdbStream executes an rdb stream sub-invocation.
	// Exposed as a variable so unit tests can mock sub-invocation dispatch
	// without spawning external processes.
	runRdbStream = func(targetRealm, moduleName, scheme string, payload []byte) error {
		selfExe, err := os.Executable()
		if err != nil {
			return fmt.Errorf("failed to get self executable path: %w", err)
		}
		cmd := exec.Command("rdb", "stream",
			"-new",
			"-include",
			"-inherit-sources",
			"-realm="+targetRealm,
			"-module-name="+moduleName,
			"-module-scheme="+scheme,
			"-previous-test-id-prefix=",
			"--",
			selfExe,
			"-post",
		)
		cmd.Stdin = bytes.NewReader(payload)
		cmd.Stdout = os.Stdout
		cmd.Stderr = os.Stderr
		if err := cmd.Run(); err != nil {
			return fmt.Errorf("rdb stream failed for module %s (scheme %s): %w", moduleName, scheme, err)
		}
		return nil
	}

	// These functions and variables are currently unused in this base change, but
	// will be used when sub-invocations are created, e.g. https://fxrev.dev/1878106
	// or https://fxrev.dev/1842967.
	_ = runRdbStream
	_ = readRealmFromLUCIContext
)

func main() {
	if err := mainImpl(); err != nil {
		fmt.Fprintf(os.Stderr, "ResultDB upload errored: %v\n", err)
		os.Exit(1)
	}
}

func mainImpl() error {
	flag.Var(&summaries, "summary", "Repeated flag, file location to summary.json."+
		" To pass in multiple files do '--summary file1.json --summary file2.json'")
	flag.Var(&tags, "tag", "Repeated flag, add tag to all test case and test suites."+
		" Uses the format key:value. To pass in multiple tags do '--tag key1:val1 --tag key2:val2'")
	flag.StringVar(&outputRoot, "output", "",
		"Output root path to be joined with 'output_file' field in summary.json. If not set, current directory will be used.")
	flag.Var(&invocationArtifacts, "invocation-artifact", "Repeated flag, path of file to upload as an invocation-level artifact.")
	flag.BoolVar(&isPost, "post", false, "Post test results read from stdin directly to ResultSink inside rdb stream.")

	flag.Parse()

	if isPost {
		return runPost()
	}
	return runParent()
}

func runPost() error {
	client, err := resultdb.NewClient()
	if err != nil {
		return err
	}
	var requests []resultdb.UploadRequest
	if err := json.NewDecoder(os.Stdin).Decode(&requests); err != nil {
		return fmt.Errorf("failed to decode upload requests from stdin: %w", err)
	}
	for _, req := range requests {
		if err := client.SendData(req.Data, req.Endpoint); err != nil {
			return err
		}
	}
	return nil
}

type luciRealmContext struct {
	Realm struct {
		Name string `json:"name"`
	} `json:"realm"`
}

// readRealmFromLUCIContext extracts the realm name from $LUCI_CONTEXT.
// This is extracted into a standalone function primarily to enable unit testing.
func readRealmFromLUCIContext() (string, error) {
	ctxPath := os.Getenv("LUCI_CONTEXT")
	if ctxPath == "" {
		return "", fmt.Errorf("LUCI_CONTEXT environment variable is not set")
	}
	b, err := os.ReadFile(ctxPath)
	if err != nil {
		return "", err
	}
	var ctx luciRealmContext
	if err = json.Unmarshal(b, &ctx); err != nil {
		return "", err
	}
	if ctx.Realm.Name == "" {
		return "", fmt.Errorf("realm.name is missing or empty in LUCI_CONTEXT (%s)", ctxPath)
	}
	return ctx.Realm.Name, nil
}

func runParent() error {
	tagPairs, err := convertTags(tags)
	if err != nil {
		return err
	}

	client, err := resultdb.NewClient()
	if err != nil {
		return err
	}

	// Enqueue invocation-level artifacts (such as 100+ MB Swarming and device
	// logs) before streaming summaries so ResultSink uploads them in the
	// background while test results are converted.
	if err := client.ReportInvocationLevelArtifacts(outputRoot, invocationArtifacts); err != nil {
		return err
	}

	allTestsSkipped, err := client.ReportSummaries(summaries, tagPairs, outputRoot)
	if err != nil {
		return err
	}

	if len(allTestsSkipped) > 0 {
		return fmt.Errorf("Some tests could not be uploaded due to testname exceeding byte limit %d.", resultdb.MaxTestIDLength)
	}
	return nil
}

func convertTags(tags []string) ([]*resultpb.StringPair, error) {
	t := []*resultpb.StringPair{}
	for _, tag := range tags {
		pair, err := stringPairFromString(tag)
		if err != nil {
			return nil, err
		}
		t = append(t, pair)
	}
	return t, nil
}

// stringPairFromString creates a pb.StringPair from the given key:val string.
func stringPairFromString(s string) (*resultpb.StringPair, error) {
	p := strings.SplitN(s, ":", 2)
	if len(p) != 2 {
		return nil, fmt.Errorf("cannot match tag content %s in the format key:value", s)
	}
	return &resultpb.StringPair{Key: strings.TrimSpace(p[0]), Value: strings.TrimSpace(p[1])}, nil
}
