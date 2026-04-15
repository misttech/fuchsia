// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"encoding/hex"
	"encoding/json"
	"flag"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	"go.fuchsia.dev/fuchsia/tools/botanist/constants"
	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/debug/covargs/api/llvm"
	"go.fuchsia.dev/fuchsia/tools/integration/testsharder"
	"go.fuchsia.dev/fuchsia/tools/lib/ffxutil"
	"go.fuchsia.dev/fuchsia/tools/lib/osmisc"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
	"go.fuchsia.dev/fuchsia/tools/testing/tap"
	"go.fuchsia.dev/fuchsia/tools/testing/testrunner"
)

var (
	covargs            = flag.String("covargs", "", "Path to covargs binary")
	coverageTestBinary = flag.String("coverage-test-binary", "", "Path to instrumented coverage test binary")
	coverageTestName   = flag.String("coverage-test-name", "", "Name of coverage test")
	goldenCoverageFile = flag.String("golden-coverage", "", "Path to golden coverage file")
	host               = flag.Bool("host", false, "If set, run coverage test on host")
	ffxPath            = flag.String("ffx", "", "Path to the ffx tool")
	llvmCov            = flag.String("llvm-cov", "", "Path to llvm-cov tool")
	llvmProfData       = flag.String("llvm-profdata", "", "Path to version of llvm-profdata tool")
)

func TestCoverage(t *testing.T) {
	testOutDir := t.TempDir()
	rawProfile := runCoverageTest(t, testOutDir)

	// Read golden coverage.
	goldenCoverage, err := os.ReadFile(*goldenCoverageFile)
	if err != nil {
		t.Fatalf("cannot find golden coverage file: %s", err)
	}
	var goldenCoverageExport llvm.Export
	if err := json.Unmarshal(goldenCoverage, &goldenCoverageExport); err != nil {
		t.Fatalf("cannot unmarshal golden coverage: %s", err)
	}

	//Verify raw profiles and extract the build ID.
	args := []string{
		"show",
		"-binary-ids",
		rawProfile,
	}
	showCmd := exec.Command(*llvmProfData, args...)
	showCmdOutput, err := showCmd.CombinedOutput()
	if err != nil {
		t.Fatalf("cannot read raw profile %s: %s", rawProfile, string(showCmdOutput))
	}

	splittedOutput := strings.Split(strings.TrimSpace(string(showCmdOutput)), "\n")
	if len(splittedOutput) < 2 {
		t.Fatalf("invalid build id in profile %q: %s", rawProfile, string(showCmdOutput))
	}
	embeddedBuildId := splittedOutput[len(splittedOutput)-1]
	if _, err = hex.DecodeString(embeddedBuildId); err != nil {
		t.Fatalf("invalid build id in profile %q: %s", rawProfile, embeddedBuildId)
	}

	// Subtest 1: Verify coverage using raw LLVM commands.
	t.Run("LLVM", func(t *testing.T) {
		indexedProfile := filepath.Join(testOutDir, "llvm.profdata")
		args = []string{
			"merge",
			rawProfile,
			"-o",
			indexedProfile,
			"--binary-file", *coverageTestBinary,
		}
		if output, err := exec.Command(*llvmProfData, args...).CombinedOutput(); err != nil {
			t.Fatalf("llvm-profdata merge failed: %s", string(output))
		}

		args = []string{
			"export",
			"-summary-only",
			"-format=text",
			"-instr-profile", indexedProfile,
			*coverageTestBinary,
		}
		exportOutput, err := exec.Command(*llvmCov, args...).CombinedOutput()
		if err != nil {
			t.Fatalf("llvm-cov export failed: %s", string(exportOutput))
		}

		var generatedCoverage llvm.Export
		if err := json.Unmarshal(exportOutput, &generatedCoverage); err != nil {
			t.Fatalf("failed to unmarshal coverage: %s", err)
		}

		if diff := cmp.Diff(goldenCoverageExport.Data, generatedCoverage.Data); diff != "" {
			t.Fatalf("unexpected coverage (-golden +generated): %s", diff)
		}
	})

	// Subtest 2: Verify coverage using covargs.
	t.Run("Covargs", func(t *testing.T) {
		buildIdDir := filepath.Join(testOutDir, ".build-id")
		debugFile := filepath.Join(buildIdDir, embeddedBuildId[:2], embeddedBuildId[2:]+".debug")
		if err := osmisc.CopyFile(*coverageTestBinary, debugFile); err != nil {
			t.Fatalf("failed to create a debug file: %s", err)
		}

		summaryFile := filepath.Join(testOutDir, "summary.json")
		if _, err := os.Stat(summaryFile); err != nil {
			t.Fatalf("failed to find summary.json: %s", err)
		}

		args = []string{
			"-build-id-dir", buildIdDir,
			"-llvm-cov", *llvmCov,
			"-llvm-profdata", *llvmProfData,
			"-output-dir", testOutDir,
			"-coverage-report=false",
			"-report-dir", testOutDir,
			"-save-temps", testOutDir,
			"-summary", summaryFile,
		}

		covargsCmd := exec.Command(*covargs, args...)
		if output, err := covargsCmd.CombinedOutput(); err != nil {
			t.Fatalf("covargs failed: %s", string(output))
		}

		coverageFile := filepath.Join(testOutDir, "coverage.json")
		generatedCoverageJson, err := os.ReadFile(coverageFile)
		if err != nil {
			t.Fatalf("cannot read coverage.json: %s", err)
		}

		var generatedCoverage llvm.Export
		if err := json.Unmarshal(generatedCoverageJson, &generatedCoverage); err != nil {
			t.Fatalf("cannot unmarshal covargs output: %s", err)
		}

		if len(goldenCoverageExport.Data) != 1 || len(generatedCoverage.Data) != 1 {
			t.Fatalf("unexpected data length")
		}

		diff := cmp.Diff(goldenCoverageExport.Data[0].Totals, generatedCoverage.Data[0].Totals)
		if diff != "" {
			t.Fatalf("unexpected coverage summary (-golden +generated): %s", diff)
		}
	})
}

func runCoverageTest(t *testing.T, testOutDir string) string {
	var test testsharder.Test
	var tester testrunner.Tester
	if *host {
		test = getHostTest()
		tester = getHostTester(t, testOutDir)
	} else {
		test = getTargetTest()
		tester = getTargetTester(t, testOutDir)
	}

	defer tester.Close()
	ctx := context.Background()
	outDir := t.TempDir()
	result, err := tester.Test(ctx, test, os.Stdout, os.Stdout, filepath.Join(testOutDir, outDir))
	result, err = tester.ProcessResult(ctx, test, outDir, result, err)
	if err != nil {
		t.Fatalf("failed to run the test: %s", err)
	}
	outputs, err := testrunner.CreateTestOutputs(tap.NewProducer(io.Discard), testOutDir)
	if err != nil {
		t.Fatalf("failed to create test outputs: %s", err)
	}
	// Record data sinks.
	if err := outputs.Record(ctx, *result); err != nil {
		t.Fatalf("failed to record data sinks: %s", err)
	}

	if !*host {
		var sinks []runtests.DataSinkMap
		// Copy profiles to the host.
		err = tester.EnsureSinks(ctx, sinks, outputs)
		if err != nil {
			t.Fatalf("failed to collect data sinks: %s", err)
		}
	}

	// Close recording of test outputs.
	if err := outputs.Close(); err != nil {
		t.Fatalf("failed to save test outputs: %s", err)
	}

	var rawProfile string
	outputTest := outputs.Summary.Tests[0]
	for _, sinks := range outputTest.DataSinks {
		// There should be one sink per test.
		if len(sinks) != 1 {
			t.Fatalf("there should be one sink per test")
		}
		rawProfile = filepath.Join(testOutDir, sinks[0].File)
	}
	if rawProfile == "" {
		t.Fatalf("failed to find a raw profile")
	}

	return rawProfile
}

func getHostTest() testsharder.Test {
	test := testsharder.Test{
		Test: build.Test{
			Path: *coverageTestBinary,
		},
		RunAlgorithm: testsharder.StopOnFailure,
		Runs:         1,
	}

	return test
}

func getHostTester(t *testing.T, testOutDir string) testrunner.Tester {
	wd, err := os.Getwd()
	if err != nil {
		t.Fatalf("failed to get current working directory: %s", err)
	}
	tester, err := testrunner.NewSubprocessTester(testrunner.SubprocessTesterOptions{Dir: wd, Env: os.Environ(), OutputDir: testOutDir})
	if err != nil {
		t.Fatalf("failed to initialize fuchsia tester: %s", err)
	}

	return tester
}

func getTargetTest() testsharder.Test {
	test := testsharder.Test{
		Test: build.Test{
			Name:       *coverageTestName,
			PackageURL: *coverageTestName,
		},
		RunAlgorithm: testsharder.StopOnFailure,
		Runs:         1,
		// The ffx tester uses ffx test with --test-file which
		// errors if the tags are empty.
		Tags: []build.TestTag{{Key: "key", Value: "value"}},
	}
	return test
}

func getTargetTester(t *testing.T, testOutDir string) testrunner.Tester {
	ctx := context.Background()
	// Create a new fuchsia tester that is responsible for executing the test.
	sshPriv := os.Getenv(constants.SSHKeyEnvKey)
	sshKeys := ffxutil.SSHInfo{SshPriv: sshPriv}
	ffx, err := ffxutil.NewFFXInstance(ctx, *ffxPath, "", os.Environ(), os.Getenv(constants.DeviceAddrEnvKey), &sshKeys, testOutDir, ffxutil.UseFFXLegacy, ffxutil.ConfigSettings{})
	if err != nil {
		t.Fatalf("failed to initialize ffx instance: %s", err)
	}
	tester, err := testrunner.NewFFXTester(ctx, testrunner.FFXTesterOptions{Ffx: ffx, OutputDir: testOutDir, LLVMProfdata: *llvmProfData})
	if err != nil {
		t.Fatalf("failed to initialize ffx tester: %s", err)
	}

	return tester
}
