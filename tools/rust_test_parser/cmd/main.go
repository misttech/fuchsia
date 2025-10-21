// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bytes"
	"context"
	"flag"
	"fmt"
	"io"
	"os"

	"go.fuchsia.dev/fuchsia/tools/lib/jsonutil"
	"go.fuchsia.dev/fuchsia/tools/lib/subprocess"
	"go.fuchsia.dev/fuchsia/tools/rust_test_parser"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
	"go.fuchsia.dev/fuchsia/tools/testing/testrunner/constants"
)

func usage() {
	fmt.Printf(`rust_test_parser [rust test command]

Reads stdout from the rust test command, and writes a JSON formatted summary to stdout
of any error messages parsed from the logs.
`)
}

func mainImpl() error {
	flag.Usage = usage

	// Parse any global flags (e.g. those for glog)
	flag.Parse()

	args := flag.Args()

	var testErr error
	stdoutForParsing := new(bytes.Buffer)
	testStdout := io.MultiWriter(os.Stdout, stdoutForParsing)
	r := &subprocess.Runner{Env: os.Environ()}
	fmt.Fprintf(os.Stdout, "Running %s\n", args[0])
	if err := r.Run(context.Background(), args, subprocess.RunOptions{Stdout: testStdout}); err != nil {
		testErr = fmt.Errorf("Error running test: %w", err)
	}

	if outputSummaryPath := os.Getenv(constants.TestOutputSummaryPathEnvKey); outputSummaryPath != "" {
		cases := rust_test_parser.Parse(stdoutForParsing.Bytes())
		result := runtests.TestResult{
			Cases: cases,
		}
		if err := jsonutil.WriteToFile(outputSummaryPath, result); err != nil {
			return fmt.Errorf("Error writing output: %w", err)
		}
	}
	return testErr
}

func main() {
	if err := mainImpl(); err != nil {
		fmt.Fprintf(os.Stderr, "%s\n", err)
		os.Exit(1)
	}
}
