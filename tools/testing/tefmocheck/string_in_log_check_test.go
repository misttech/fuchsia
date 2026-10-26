// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package tefmocheck

import (
	"fmt"
	"regexp"
	"strings"
	"testing"
	"time"

	"github.com/google/go-cmp/cmp"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

func TestCheck(t *testing.T) {
	const killerString = "KILLER STRING"
	const exceptString = "Don't die!"
	const line = "KILLER STRING: Something failed."
	exceptBlock := &logBlock{
		startString: "skip_test",
		endString:   "end_test",
	}
	exceptBlock2 := &logBlock{
		startString: "block_start",
		endString:   "block_end",
	}
	c := stringInLogCheck{
		String:        killerString,
		OnlyOnStates:  []string{},
		ExceptStrings: []string{exceptString},
		ExceptBlocks:  []*logBlock{exceptBlock, exceptBlock2},
		Type:          swarmingOutputType,
	}
	gotName := c.Name()
	wantName := "string_in_log/infra_and_test_std_and_klog.txt/KILLER_STRING"
	if gotName != wantName {
		t.Errorf("c.Name() returned %q, want %q", gotName, wantName)
	}

	for _, tc := range []struct {
		name                string
		attributeToTest     bool
		addTag              bool
		testingOutputs      TestingOutputs
		typeToCheck         logType
		skipPassedTask      bool
		skipAllPassedTests  bool
		skipPassedTest      bool
		ignoreFlakes        bool
		alwaysFlake         bool
		states              []string
		swarmingResultState string
		shouldMatch         bool
		wantName            string
		wantFlake           bool
		wantLine            string
		wantTags            []build.TestTag
		wantOutputFiles     []string
		setWantOutputFiles  bool
	}{
		{
			name: "should match simple",
			testingOutputs: TestingOutputs{
				SwarmingOutput: fmt.Appendf(nil, "PREFIX %s SUFFIX", killerString),
			},
			shouldMatch: true,
		},
		{
			name: "should match with failure reason",
			testingOutputs: TestingOutputs{
				SwarmingOutput: fmt.Appendf(nil, "1\n2\n%s\n4\n5\n", line),
			},
			shouldMatch: true,
			wantLine:    line,
		},
		{
			name: "should match if string before except_block and return failure reason",
			testingOutputs: TestingOutputs{
				SwarmingOutput: fmt.Appendf(nil, "%s\n%s\n%s", exceptBlock.startString, exceptBlock.endString, line),
			},
			shouldMatch: true,
			wantLine:    line,
		},
		{
			name: "exceptSuccessfulSwarmingResult",
			testingOutputs: TestingOutputs{
				SwarmingOutput: fmt.Appendf(nil, "PREFIX %s SUFFIX", killerString),
			},
			skipPassedTask:      true,
			swarmingResultState: "COMPLETED",
		},
		{
			name: "should not match if string in other log",
			testingOutputs: TestingOutputs{
				SerialLogs:     [][]byte{[]byte(killerString)},
				SwarmingOutput: []byte("gentle string"),
			},
		},
		{
			name: "should not match if except_string in log",
			testingOutputs: TestingOutputs{
				SwarmingOutput: []byte(killerString + exceptString),
			},
		},
		{
			name: "should match if string before except_block",
			testingOutputs: TestingOutputs{
				SwarmingOutput: fmt.Appendf(nil, "PREFIX %s ... %s output %s SUFFIX", killerString, exceptBlock.startString, exceptBlock.endString),
			},
			shouldMatch: true,
		},
		{
			name: "should match if string after except_block",
			testingOutputs: TestingOutputs{
				SwarmingOutput: fmt.Appendf(nil, "PREFIX %s output %s ... %s SUFFIX", exceptBlock.startString, exceptBlock.endString, killerString),
			},
			shouldMatch: true,
		},
		{
			name: "should not match if string in except_block",
			testingOutputs: TestingOutputs{
				SwarmingOutput: fmt.Appendf(nil,
					"PREFIX %s %s output %s SUFFIX %s %s %s",
					exceptBlock.startString, killerString, exceptBlock.endString,
					exceptBlock2.startString, killerString, exceptBlock2.endString),
			},
		},
		{
			name: "should match if string in both except_block and outside except_block",
			testingOutputs: TestingOutputs{
				SwarmingOutput: fmt.Appendf(nil,
					"PREFIX %s ... %s %s %s %s %s %s SUFFIX",
					killerString, exceptBlock.startString, killerString, exceptBlock.endString,
					exceptBlock2.startString, killerString, exceptBlock2.endString,
				),
			},
			shouldMatch: true,
		},
		{
			name: "should not match if string in except_block with missing end string",
			testingOutputs: TestingOutputs{
				SwarmingOutput: fmt.Appendf(nil,
					"PREFIX ... %s %s %s %s %s",
					exceptBlock.startString, killerString, exceptBlock.endString,
					exceptBlock2.startString, killerString,
				),
			},
		},
		{
			name: "should match if swarming task state is in expected states",
			testingOutputs: TestingOutputs{
				SwarmingOutput: []byte(killerString),
			},
			states:              []string{"STATE_1", "STATE_2"},
			swarmingResultState: "STATE_1",
			shouldMatch:         true,
		},
		{
			name: "should not match if swarming task state is not in expected states",
			testingOutputs: TestingOutputs{
				SwarmingOutput: []byte(killerString),
			},
			states:              []string{"STATE_1", "STATE_2"},
			swarmingResultState: "NO_STATE",
		},
		{
			name:            "should match per test swarming output",
			attributeToTest: true,
			testingOutputs: TestingOutputs{
				SwarmingOutput: []byte(killerString),
				SwarmingOutputPerTest: []TestLog{
					{
						TestName: "foo-test",
						Bytes:    []byte(killerString),
						FilePath: "foo/log.txt",
						Index:    0,
					},
				},
			},
			shouldMatch: true,
			wantName:    wantName + "/foo-test",
		},
		{
			name:            "should match per test swarming output with double slash in test name",
			attributeToTest: true,
			testingOutputs: TestingOutputs{
				SwarmingOutput: []byte(killerString),
				SwarmingOutputPerTest: []TestLog{
					{
						TestName: "@@//foo/bar:baz",
						Bytes:    []byte(killerString),
						FilePath: "foo/log.txt",
						Index:    0,
					},
				},
			},
			shouldMatch: true,
			wantName:    wantName + "/@@//foo/bar:baz",
		},
		{
			name:            "should add test name to tag",
			attributeToTest: true,
			addTag:          true,
			testingOutputs: TestingOutputs{
				SwarmingOutput: []byte(killerString),
				SwarmingOutputPerTest: []TestLog{
					{
						TestName: "foo-test",
						Bytes:    []byte(killerString),
						FilePath: "foo/log.txt",
						Index:    0,
					},
				},
			},
			shouldMatch: true,
			wantName:    wantName,
			wantTags:    []build.TestTag{{Key: "test_name", Value: "foo-test"}},
		},
		{
			name:            "should respect except block even if split across tests",
			attributeToTest: true,
			testingOutputs: TestingOutputs{
				SwarmingOutput: fmt.Appendf(nil, "%s %s %s %s", exceptBlock.startString, killerString, exceptBlock.endString, killerString),
				SwarmingOutputPerTest: []TestLog{
					{
						TestName: "foo-test",
						Bytes:    fmt.Appendf(nil, "%s %s", exceptBlock.startString, killerString),
						FilePath: "foo/log.txt",
						Index:    0,
					},
					{
						TestName: "bar-test",
						Bytes:    fmt.Appendf(nil, "%s %s", exceptBlock.endString, killerString),
						FilePath: "bar/log.txt",
						Index:    len(exceptBlock.startString) + len(killerString) + 2,
					},
				},
			},
			shouldMatch: true,
			wantName:    wantName + "/bar-test",
		},
		{
			name:            "should not match when killer string in second test is within except block started in first test",
			attributeToTest: true,
			testingOutputs: TestingOutputs{
				SwarmingOutput: fmt.Appendf(nil, "%s %s %s", exceptBlock.startString, killerString, exceptBlock.endString),
				SwarmingOutputPerTest: []TestLog{
					{
						TestName: "foo-test",
						Bytes:    fmt.Appendf(nil, "%s ", exceptBlock.startString),
						FilePath: "foo/log.txt",
						Index:    0,
					},
					{
						TestName: "bar-test",
						Bytes:    fmt.Appendf(nil, "%s %s", killerString, exceptBlock.endString),
						FilePath: "bar/log.txt",
						Index:    len(exceptBlock.startString) + 1,
					},
				},
			},
			shouldMatch: false,
		},
		{
			name:            "should match correct FailureReason for non-zero SwarmingOutputPerTest slice index",
			attributeToTest: true,
			testingOutputs: TestingOutputs{
				SwarmingOutput: []byte("first test line\nsecond test " + line + "\n"),
				SwarmingOutputPerTest: []TestLog{
					{
						TestName: "foo-test",
						Bytes:    []byte("first test line\n"),
						FilePath: "foo/log.txt",
						Index:    0,
					},
					{
						TestName: "bar-test",
						Bytes:    []byte("second test " + line + "\n"),
						FilePath: "bar/log.txt",
						Index:    len("first test line\n"),
					},
				},
			},
			shouldMatch: true,
			wantName:    wantName + "/bar-test",
			wantLine:    "second test " + line,
		},
		{
			name:            "should check all syslogs",
			attributeToTest: true,
			testingOutputs: TestingOutputs{
				Syslogs: [][]byte{[]byte("gentle string"), []byte(killerString)},
			},
			shouldMatch: true,
			typeToCheck: syslogType,
			wantName:    "string_in_log/syslog.txt/KILLER_STRING",
		},
		{
			name: "should skip if all tests passed",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test1",
							Status: runtests.TestSuccess,
						},
					},
				},
				SerialLogs: [][]byte{[]byte("gentle string"), []byte(killerString)},
			},
			skipAllPassedTests:  true,
			swarmingResultState: "COMPLETED",
			typeToCheck:         serialLogType,
		},
		{
			name: "should skip if ignore flakes",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test1",
							Status: runtests.TestFailure,
						},
						{
							Name:   "test1",
							Status: runtests.TestSuccess,
						},
					},
				},
				SerialLogs: [][]byte{[]byte("gentle string"), []byte(killerString)},
			},
			skipAllPassedTests:  true,
			ignoreFlakes:        true,
			swarmingResultState: "COMPLETED",
			typeToCheck:         serialLogType,
		},
		{
			name: "should not skip if a test failed",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test1",
							Status: runtests.TestFailure,
						},
					},
				},
				SerialLogs: [][]byte{[]byte("gentle string"), []byte(killerString)},
			},
			shouldMatch:         true,
			skipAllPassedTests:  true,
			swarmingResultState: "COMPLETED",
			typeToCheck:         serialLogType,
		},
		{
			name: "should not skip if not ignore flakes",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test1",
							Status: runtests.TestFailure,
						},
						{
							Name:   "test1",
							Status: runtests.TestSuccess,
						},
					},
				},
				SerialLogs: [][]byte{[]byte("gentle string"), []byte(killerString)},
			},
			shouldMatch:         true,
			skipAllPassedTests:  true,
			swarmingResultState: "COMPLETED",
			typeToCheck:         serialLogType,
		},
		{
			name: "should skip if passed test",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test1",
							Status: runtests.TestFailure,
						},
						{
							Name:   "test2",
							Status: runtests.TestSuccess,
						},
					},
				},
				SwarmingOutput: []byte(killerString),
				SwarmingOutputPerTest: []TestLog{
					{
						TestName: "test1",
						Bytes:    []byte{},
						FilePath: "foo/log.txt",
						Index:    0,
					},
					{
						TestName: "test2",
						Bytes:    []byte(killerString),
						FilePath: "foo/log.txt",
						Index:    0,
					},
				},
			},
			skipPassedTest: true,
		},
		{
			name: "should report flake if ignore flakes",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test1",
							Status: runtests.TestFailure,
						},
						{
							Name:   "test1",
							Status: runtests.TestSuccess,
						},
						{
							Name:   "test2",
							Status: runtests.TestFailure,
						},
					},
				},
				SwarmingOutput: []byte(killerString + killerString),
				SwarmingOutputPerTest: []TestLog{
					{
						TestName: "test1",
						Bytes:    []byte(killerString),
						FilePath: "foo/log.txt",
						Index:    0,
					},
					{
						TestName: "test1",
						Bytes:    []byte(killerString),
						FilePath: "foo/log.txt",
						Index:    len(killerString),
					},

					{
						TestName: "test2",
						Bytes:    []byte{},
						FilePath: "foo/log.txt",
						Index:    len(killerString) * 2,
					},
				},
			},
			shouldMatch:    true,
			skipPassedTest: true,
			ignoreFlakes:   true,
			wantFlake:      true,
		},
		{
			name: "should report failure over flake",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test1",
							Status: runtests.TestFailure,
						},
						{
							Name:   "test1",
							Status: runtests.TestSuccess,
						},
						{
							Name:   "test2",
							Status: runtests.TestFailure,
						},
					},
				},
				SwarmingOutput: []byte(killerString + killerString + killerString),
				SwarmingOutputPerTest: []TestLog{
					{
						TestName: "test1",
						Bytes:    []byte(killerString),
						FilePath: "foo/log.txt",
						Index:    0,
					},
					{
						TestName: "test1",
						Bytes:    []byte(killerString),
						FilePath: "foo/log.txt",
						Index:    len(killerString),
					},

					{
						TestName: "test2",
						Bytes:    []byte(killerString),
						FilePath: "foo/log.txt",
						Index:    len(killerString) * 2,
					},
				},
			},
			shouldMatch:    true,
			skipPassedTest: true,
			ignoreFlakes:   true,
		},
		{
			name: "should not skip if not ignore flakes and skipPassedTest",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test1",
							Status: runtests.TestFailure,
						},
						{
							Name:   "test1",
							Status: runtests.TestSuccess,
						},
					},
				},
				SwarmingOutput: []byte(killerString + killerString),
				SwarmingOutputPerTest: []TestLog{
					{
						TestName: "test1",
						Bytes:    []byte(killerString),
						FilePath: "foo/log.txt",
						Index:    0,
					},
					{
						TestName: "test1",
						Bytes:    []byte(killerString),
						FilePath: "foo/log.txt",
						Index:    len(killerString),
					},
				},
			},
			shouldMatch:    true,
			skipPassedTest: true,
		},
		{
			name: "should skip if skipPassedTask and a test failed",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test1",
							Status: runtests.TestFailure,
						},
					},
				},
				SerialLogs: [][]byte{[]byte("gentle string"), []byte(killerString)},
			},
			skipPassedTask:      true,
			swarmingResultState: "COMPLETED",
			typeToCheck:         serialLogType,
		},
		{
			name: "should always flake if not attributed to test",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test1",
							Status: runtests.TestFailure,
						},
						{
							Name:   "test2",
							Status: runtests.TestSuccess,
						},
					},
				},
				SwarmingOutput: []byte(killerString),
				SwarmingOutputPerTest: []TestLog{
					{
						TestName: "test1",
						Bytes:    []byte{},
						FilePath: "foo/log.txt",
						Index:    0,
					},
					{
						TestName: "test2",
						Bytes:    []byte(killerString),
						FilePath: "foo/log.txt",
						Index:    0,
					},
				},
			},
			shouldMatch: true,
			alwaysFlake: true,
			wantFlake:   true,
		},
		{
			name: "should always flake if skip passed test and failed test",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test1",
							Status: runtests.TestSuccess,
						},
						{
							Name:   "test2",
							Status: runtests.TestFailure,
						},
					},
				},
				SwarmingOutput: []byte(killerString),
				SwarmingOutputPerTest: []TestLog{
					{
						TestName: "test1",
						Bytes:    []byte{},
						FilePath: "foo/log.txt",
						Index:    0,
					},
					{
						TestName: "test2",
						Bytes:    []byte(killerString),
						FilePath: "foo/log.txt",
						Index:    0,
					},
				},
			},
			shouldMatch:    true,
			skipPassedTest: true,
			alwaysFlake:    true,
			wantFlake:      true,
		},
		{
			name: "should not flake if skip passed test",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test1",
							Status: runtests.TestFailure,
						},
						{
							Name:   "test2",
							Status: runtests.TestSuccess,
						},
					},
				},
				SwarmingOutput: []byte(killerString),
				SwarmingOutputPerTest: []TestLog{
					{
						TestName: "test1",
						Bytes:    []byte{},
						FilePath: "foo/log.txt",
						Index:    0,
					},
					{
						TestName: "test2",
						Bytes:    []byte(killerString),
						FilePath: "foo/log.txt",
						Index:    0,
					},
				},
			},
			skipPassedTest: true,
			alwaysFlake:    true,
		},
		{
			name: "should skip if killer string in syslog is from passed test",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:           "test1",
							Status:         runtests.TestFailure,
							StartTime:      time.Date(2026, 10, 5, 14, 0, 0, 0, time.UTC),
							DurationMillis: 60000,
						},
						{
							Name:           "test2",
							Status:         runtests.TestSuccess,
							StartTime:      time.Date(2026, 10, 5, 14, 5, 0, 0, time.UTC),
							DurationMillis: 60000,
						},
					},
				},
				Syslogs: [][]byte{
					[]byte("2026-10-05 14:00:00.000000 test1 output\n" +
						"2026-10-05 14:05:00.000000 test2 output " + killerString + "\n"),
				},
			},
			skipPassedTest: true,
			typeToCheck:    syslogType,
			shouldMatch:    false,
		},
		{
			name: "should match if killer string in syslog is from failed test and attribute to test",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:           "test1",
							Status:         runtests.TestFailure,
							StartTime:      time.Date(2026, 10, 5, 14, 0, 0, 0, time.UTC),
							DurationMillis: 60000,
						},
						{
							Name:           "test2",
							Status:         runtests.TestSuccess,
							StartTime:      time.Date(2026, 10, 5, 14, 5, 0, 0, time.UTC),
							DurationMillis: 60000,
						},
					},
				},
				Syslogs: [][]byte{
					[]byte("2026-10-05 14:00:00.000000 test1 output " + killerString + "\n" +
						"2026-10-05 14:05:00.000000 test2 output\n"),
				},
			},
			skipPassedTest:  true,
			attributeToTest: true,
			typeToCheck:     syslogType,
			shouldMatch:     true,
			wantName:        "string_in_log/syslog.txt/KILLER_STRING/test1",
		},
		{
			name: "should match when killer string in failed test syslog but not in passed test syslog",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test0",
							Status: runtests.TestSuccess,
						},
						{
							Name:   "test1",
							Status: runtests.TestFailure,
						},
					},
				},
				Syslogs: [][]byte{
					[]byte("test0 output\ntest1 output " + killerString + "\n"),
				},
				SyslogsPerTest: [][]TestLog{
					{
						{
							TestName: "test0",
							Bytes:    []byte("test0 output\n"),
							FilePath: "test0/syslog.txt",
							Index:    0,
						},
						{
							TestName: "test1",
							Bytes:    []byte("test1 output " + killerString + "\n"),
							FilePath: "test1/syslog.txt",
							Index:    len("test0 output\n"),
						},
					},
				},
			},
			skipPassedTest:     true,
			attributeToTest:    true,
			typeToCheck:        syslogType,
			shouldMatch:        true,
			wantName:           "string_in_log/syslog.txt/KILLER_STRING/test1",
			wantOutputFiles:    []string{"test1/syslog.txt"},
			setWantOutputFiles: true,
		},
		{
			name: "should not match when killer string is in passed test syslog but absent in failed test syslog",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test0",
							Status: runtests.TestSuccess,
						},
						{
							Name:   "test1",
							Status: runtests.TestFailure,
						},
					},
				},
				Syslogs: [][]byte{
					[]byte("test0 output " + killerString + "\ntest1 output clean\n"),
				},
				SyslogsPerTest: [][]TestLog{
					{
						{
							TestName: "test0",
							Bytes:    []byte("test0 output " + killerString + "\n"),
							FilePath: "test0/syslog.txt",
							Index:    0,
						},
						{
							TestName: "test1",
							Bytes:    []byte("test1 output clean\n"),
							FilePath: "test1/syslog.txt",
							Index:    len("test0 output " + killerString + "\n"),
						},
					},
				},
			},
			skipPassedTest:     true,
			attributeToTest:    true,
			typeToCheck:        syslogType,
			shouldMatch:        false,
			setWantOutputFiles: true,
			wantOutputFiles:    []string{},
		},
		{
			name: "should not match when killer string is in full syslog but outside all per-test windows",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test0",
							Status: runtests.TestFailure,
						},
						{
							Name:   "test1",
							Status: runtests.TestFailure,
						},
					},
				},
				Syslogs: [][]byte{
					[]byte("pre-test log " + killerString + "\ntest0 output clean\ntest1 output clean\n"),
				},
				SyslogsPerTest: [][]TestLog{
					{
						{
							TestName: "test0",
							Bytes:    []byte("test0 output clean\n"),
							FilePath: "test0/syslog.txt",
							Index:    len("pre-test log " + killerString + "\n"),
						},
						{
							TestName: "test1",
							Bytes:    []byte("test1 output clean\n"),
							FilePath: "test1/syslog.txt",
							Index:    len("pre-test log " + killerString + "\ntest0 output clean\n"),
						},
					},
				},
			},
			skipPassedTest:     true,
			attributeToTest:    true,
			typeToCheck:        syslogType,
			shouldMatch:        false,
			setWantOutputFiles: true,
			wantOutputFiles:    []string{},
		},
		{
			name: "should not match when killer string in second test syslog is within except block started in first test syslog",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:   "test0",
							Status: runtests.TestFailure,
						},
						{
							Name:   "test1",
							Status: runtests.TestFailure,
						},
					},
				},
				Syslogs: [][]byte{
					fmt.Appendf(nil, "%s\n%s\n%s\n", exceptBlock.startString, killerString, exceptBlock.endString),
				},
				SyslogsPerTest: [][]TestLog{
					{
						{
							TestName: "test0",
							Bytes:    fmt.Appendf(nil, "%s\n", exceptBlock.startString),
							FilePath: "test0/syslog.txt",
							Index:    0,
						},
						{
							TestName: "test1",
							Bytes:    fmt.Appendf(nil, "%s\n%s\n", killerString, exceptBlock.endString),
							FilePath: "test1/syslog.txt",
							Index:    len(fmt.Appendf(nil, "%s\n", exceptBlock.startString)),
						},
					},
				},
			},
			skipPassedTest:     true,
			attributeToTest:    true,
			typeToCheck:        syslogType,
			shouldMatch:        false,
			setWantOutputFiles: true,
			wantOutputFiles:    []string{},
		},
		{
			name: "should not attribute to test for serial logs even if attribute_to_test is true",
			testingOutputs: TestingOutputs{
				TestSummary: &runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{
							Name:           "test0",
							Status:         runtests.TestSuccess,
							StartTime:      time.Date(2026, 10, 5, 14, 0, 0, 0, time.UTC),
							DurationMillis: 60000,
						},
						{
							Name:           "test1",
							Status:         runtests.TestFailure,
							StartTime:      time.Date(2026, 10, 5, 14, 5, 0, 0, time.UTC),
							DurationMillis: 60000,
						},
					},
				},
				SerialLogs: [][]byte{
					[]byte("2026-10-05 14:05:00.000000 test1 output " + killerString + "\n"),
				},
			},
			attributeToTest: true,
			typeToCheck:     serialLogType,
			shouldMatch:     true,
			wantName:        "string_in_log/serial_log.txt/KILLER_STRING",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			c := c // Make a copy to avoid modifying shared state.
			c.AttributeToTest = tc.attributeToTest
			c.AddTag = tc.addTag
			c.SkipPassedTask = tc.skipPassedTask
			c.SkipAllPassedTests = tc.skipAllPassedTests
			c.SkipPassedTest = tc.skipPassedTest
			c.IgnoreFlakes = tc.ignoreFlakes
			c.AlwaysFlake = tc.alwaysFlake
			// It accesses this field for DebugText().
			tc.testingOutputs.SwarmingSummary = &SwarmingTaskSummary{
				Results: &SwarmingRpcsTaskResult{
					TaskId: "abc", State: tc.swarmingResultState,
				},
			}
			c.OnlyOnStates = tc.states
			if tc.typeToCheck != "" {
				c.Type = tc.typeToCheck
			}
			if c.Check(&tc.testingOutputs) != tc.shouldMatch {
				t.Errorf("c.Check(%q) returned %t, expected %t",
					tc.name, !tc.shouldMatch, tc.shouldMatch)
			}
			gotName := c.Name()
			if tc.wantName != "" && gotName != tc.wantName {
				t.Errorf("c.Name() returned %q, want %q", gotName, tc.wantName)
			}
			if tc.attributeToTest && tc.addTag && tc.shouldMatch {
				if diff := cmp.Diff(tc.wantTags, c.Tags()); diff != "" {
					t.Errorf("c.Tags() returned diff (-want +got): %s", diff)
				}
			}
			c.DebugText() // minimal coverage, check it doesn't crash.
			var perTestLogs []TestLog
			if len(tc.testingOutputs.SwarmingOutputPerTest) > 0 {
				perTestLogs = tc.testingOutputs.SwarmingOutputPerTest
			} else if len(tc.testingOutputs.SyslogsPerTest) > 0 {
				perTestLogs = tc.testingOutputs.SyslogsPerTest[0]
			}
			gotOutputFiles := c.OutputFiles()
			if tc.setWantOutputFiles {
				if diff := cmp.Diff(tc.wantOutputFiles, gotOutputFiles); diff != "" {
					t.Errorf("c.OutputFiles() returned diff (-want +got): %s", diff)
				}
			} else {
				if len(gotOutputFiles) > 1 {
					t.Errorf("c.OutputFiles() returned %d files (%v), want at most 1", len(gotOutputFiles), gotOutputFiles)
				}
				if len(perTestLogs) == 0 && len(gotOutputFiles) != 0 {
					t.Errorf("c.OutputFiles() returned %s, want []", gotOutputFiles)
				}
				if len(perTestLogs) == 1 && (len(gotOutputFiles) != 1 || perTestLogs[0].FilePath != gotOutputFiles[0]) {
					t.Errorf("c.OutputFiles() returned %s, want %q", gotOutputFiles, perTestLogs[0].FilePath)
				}
			}
			if tc.wantFlake != c.IsFlake() {
				t.Errorf("c.IsFlake() returned %t, expected %t", c.IsFlake(), tc.wantFlake)
			}
			if tc.wantLine != "" && tc.wantLine != c.line {
				t.Errorf("Got line %s, expected %s", c.line, tc.wantLine)
			}
		})
	}
}

func TestBotanistErrorPrefixCheck(t *testing.T) {
	for _, tc := range []struct {
		name        string
		checkString string
		log         string
		shouldMatch bool
	}{
		{
			name:        "matches catch-all botanist failed check",
			checkString: "botanist ERROR: botanist failed",
			log:         "2026-10-01 19:53:55.445836 botanist: run.go:839: botanist ERROR: botanist failed: some error\n",
			shouldMatch: true,
		},
		{
			name:        "matches bare botanist error line",
			checkString: "botanist ERROR: botanist failed",
			log:         "botanist ERROR: botanist failed: some error\n",
			shouldMatch: true,
		},
		{
			name:        "specific botanist ERROR check matches unwrapped error",
			checkString: "botanist ERROR: start target error",
			log:         "2026-10-01 19:53:55.445836 botanist: run.go:839: botanist ERROR: start target error: FFX command failed\n",
			shouldMatch: true,
		},
		{
			name:        "specific botanist ERROR check matches when wrapped by botanist failed",
			checkString: "botanist ERROR: start target error",
			log:         "2026-10-01 19:53:55.445836 botanist: run.go:839: botanist ERROR: botanist failed: start target error: FFX command failed\n",
			shouldMatch: true,
		},
		{
			name:        "specific botanist ERROR check does not match inner wrapped error",
			checkString: "botanist ERROR: timed out trying to connect to ssh",
			log:         "2026-10-01 19:53:55.445836 botanist: run.go:839: botanist ERROR: botanist failed: failed to run snapshot over ssh: timed out trying to connect to ssh\n",
			shouldMatch: false,
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			c := stringInLogCheck{
				String: tc.checkString,
				Type:   swarmingOutputType,
			}
			to := TestingOutputs{
				SwarmingOutput: []byte(tc.log),
				SwarmingSummary: &SwarmingTaskSummary{
					Results: &SwarmingRpcsTaskResult{
						TaskId: "abc",
						State:  "COMPLETED",
					},
				},
			}
			if got := c.Check(&to); got != tc.shouldMatch {
				t.Errorf("c.Check() for %q on %q = %t, want %t", tc.checkString, tc.log, got, tc.shouldMatch)
			}
		})
	}
}

func TestRegexCheck(t *testing.T) {
	const panicPattern = `(?m)(?:\[worker-\d+\]\s+)?(?:^|[^-\w\n])panic: .*\n(?:.*\n){0,8}.*goroutine \d+ \[`
	const panicPrefix = "panic: "

	exceptBlocks := []*logBlock{
		{
			startString: "=== RUN   TestIgnoredPanic",
			endString:   "--- PASS: TestIgnoredPanic",
		},
	}

	crossTestLog0 := "=== RUN   TestIgnoredPanic\nsetup\n"
	crossTestLog1 := "panic: test panic\n\ngoroutine 5 [running]:\n" + strings.Repeat("frame()\n", 9) + "--- PASS: TestIgnoredPanic\n"
	crossTestLog2 := "panic: unhandled panic\n\ngoroutine 1 [running]:\n"
	crossTestLog := crossTestLog0 + crossTestLog1 + crossTestLog2
	crossTestPerTest := []TestLog{
		{
			TestName: "test0",
			Bytes:    []byte(crossTestLog0),
			Index:    0,
		},
		{
			TestName: "test1",
			Bytes:    []byte(crossTestLog1),
			Index:    len(crossTestLog0),
		},
		{
			TestName: "test2",
			Bytes:    []byte(crossTestLog2),
			Index:    len(crossTestLog0) + len(crossTestLog1),
		},
	}
	crossTestPerTestIgnoredOnly := []TestLog{
		{
			TestName: "test0",
			Bytes:    []byte(crossTestLog0),
			Index:    0,
		},
		{
			TestName: "test1",
			Bytes:    []byte(crossTestLog1),
			Index:    len(crossTestLog0),
		},
	}

	for _, tc := range []struct {
		name            string
		prefilterPrefix string
		log             string
		perTest         []TestLog
		shouldMatch     bool
		wantLine        string
	}{
		{
			name:            "without prefilter matches multiline pattern and attributes first line",
			prefilterPrefix: "",
			log:             "2026-10-20 10:00:00.000000 test: panic: sync: negative WaitGroup counter\n\ngoroutine 9 [running]:\n",
			shouldMatch:     true,
			wantLine:        "2026-10-20 10:00:00.000000 test: panic: sync: negative WaitGroup counter",
		},
		{
			name:            "without prefilter ignores match inside except_block",
			prefilterPrefix: "",
			log:             "=== RUN   TestIgnoredPanic\npanic: test panic\n\ngoroutine 5 [running]:\n--- PASS: TestIgnoredPanic\n",
			shouldMatch:     false,
		},
		{
			name:            "without prefilter ignores match in second test within except block started in first test",
			prefilterPrefix: "",
			log:             crossTestLog0 + crossTestLog1,
			perTest:         crossTestPerTestIgnoredOnly,
			shouldMatch:     false,
		},
		{
			name:            "with prefilter matches when prefix present and lookback matches single delimiter",
			prefilterPrefix: panicPrefix,
			log:             "2026-10-20 10:00:00.000000 testrunner: panic: sync: negative WaitGroup counter\n\ngoroutine 9 [running]:\n",
			shouldMatch:     true,
			wantLine:        "2026-10-20 10:00:00.000000 testrunner: panic: sync: negative WaitGroup counter",
		},
		{
			name:            "with prefilter matches when pattern includes preceding token in lookback",
			prefilterPrefix: panicPrefix,
			log:             "[worker-42]   panic: sync: negative WaitGroup counter\n\ngoroutine 9 [running]:\n",
			shouldMatch:     true,
			wantLine:        "[worker-42]   panic: sync: negative WaitGroup counter",
		},
		{
			name:            "with prefilter prevents false positive on non-matching prefix boundary",
			prefilterPrefix: panicPrefix,
			log:             "2026-10-20 10:00:00.000000 test: non-panic: synthetic message\n\ngoroutine 1 [running]:\n",
			shouldMatch:     false,
		},
		{
			name:            "with prefilter fast path returns false when prefix is absent",
			prefilterPrefix: panicPrefix,
			log:             "healthy test output\nmore healthy test output\n",
			shouldMatch:     false,
		},
		{
			name:            "with prefilter matches subsequent valid prefix occurrence when earlier occurrence fails regex",
			prefilterPrefix: panicPrefix,
			log:             "non-panic: ignore this\nfiller line\npanic: real failure\n\ngoroutine 1 [running]:\n",
			shouldMatch:     true,
			wantLine:        "panic: real failure",
		},
		{
			name:            "with prefilter ignores match in except block and finds next match",
			prefilterPrefix: panicPrefix,
			log:             "=== RUN   TestIgnoredPanic\npanic: test panic\n\ngoroutine 5 [running]:\n" + strings.Repeat("frame()\n", 9) + "--- PASS: TestIgnoredPanic\npanic: unhandled panic\n\ngoroutine 1 [running]:\n",
			shouldMatch:     true,
			wantLine:        "panic: unhandled panic",
		},
		{
			name:            "with prefilter ignores match in second test within except block started in first test",
			prefilterPrefix: panicPrefix,
			log:             crossTestLog0 + crossTestLog1,
			perTest:         crossTestPerTestIgnoredOnly,
			shouldMatch:     false,
		},
		{
			name:            "with prefilter matches in subsequent test outside except block started in earlier test",
			prefilterPrefix: panicPrefix,
			log:             crossTestLog,
			perTest:         crossTestPerTest,
			shouldMatch:     true,
			wantLine:        "panic: unhandled panic",
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			c := stringInLogCheck{
				CheckName:       "regex_check",
				String:          panicPattern,
				PrefilterPrefix: tc.prefilterPrefix,
				Type:            swarmingOutputType,
				AttributeToTest: true,
				regex:           regexp.MustCompile(panicPattern),
				ExceptBlocks:    exceptBlocks,
			}
			to := TestingOutputs{
				SwarmingOutput:        []byte(tc.log),
				SwarmingOutputPerTest: tc.perTest,
				SwarmingSummary: &SwarmingTaskSummary{
					Results: &SwarmingRpcsTaskResult{TaskId: "abc", State: "COMPLETED", Failure: true},
				},
			}
			if got := c.Check(&to); got != tc.shouldMatch {
				t.Fatalf("c.Check() = %t, want %t", got, tc.shouldMatch)
			}
			if tc.wantLine != "" && c.FailureReason() != tc.wantLine {
				t.Errorf("FailureReason() = %q, want %q", c.FailureReason(), tc.wantLine)
			}
		})
	}
}

func BenchmarkRegexCheckPrefilter(b *testing.B) {
	const panicPattern = `(?m)(?:^|[^-\w\n])panic: .*\n(?:.*\n){0,8}.*goroutine \d+ \[`
	log := []byte(strings.Repeat("2026-10-20 10:00:00.000000 normal log output line from running test suite without panics\n", 10000))
	to := TestingOutputs{
		SwarmingOutput: log,
		SwarmingSummary: &SwarmingTaskSummary{
			Results: &SwarmingRpcsTaskResult{TaskId: "abc", State: "COMPLETED", Failure: true},
		},
	}

	for _, tc := range []struct {
		name            string
		prefilterPrefix string
	}{
		{name: "without_prefilter", prefilterPrefix: ""},
		{name: "with_prefilter", prefilterPrefix: "panic: "},
	} {
		b.Run(tc.name, func(b *testing.B) {
			c := stringInLogCheck{
				CheckName:       "go_panic",
				String:          panicPattern,
				PrefilterPrefix: tc.prefilterPrefix,
				Type:            swarmingOutputType,
				regex:           regexp.MustCompile(panicPattern),
			}
			b.ResetTimer()
			for i := 0; i < b.N; i++ {
				c.Check(&to)
			}
		})
	}
}
