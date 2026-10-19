// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package tefmocheck

import (
	_ "embed"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
	"time"

	"github.com/google/go-cmp/cmp"

	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

const validLog = `2020/03/20 22:47:38 Attempting to powercycle device upper-bacon-clock-snort
2020/03/20 22:47:38 Using AMT to powercycle.
[0.000] 00000.00000>
[0.000] 00000.00000> welcome to Zircon
[0.000] 00000.00000>
TAP version 13
1..5
Test/1 output line 1
Test/1 output line 2
ok 1 Test/1 (12.6s)
not ok 2 Test2 (44.511025786s)
Test3 output line 1
Test3 output line 2
ok 3 fakeTest (1)
Tests took 41 seconds.
ok 3 Test3 (27.90584341s)
Test4 output line 1
Test4 output line 2
Test4 output mixed with testrunner output not ok 4 Test4 (88.8s)
Test5 output line 1
fail message
Test5 failed
not ok 5 Test5 (27.90584341s)
Test5 output
2020/03/20 22:57:53.569095 botanist attempting to close SSH session due to: context canceled
ok 6 Test5 (9s)
2020/03/20 22:57:53.569333 botanist DEBUG: stopping or rebooting the node "upper-bacon-clock-snort"
`

var validLogTests = []string{"Test/1", "Test2", "Test3", "Test4", "Test5", "Test5"}

const validFFXLog = `2020/03/20 22:47:38 Attempting to powercycle device upper-bacon-clock-snort
2020/03/20 22:47:38 Using AMT to powercycle.
[0.000] 00000.00000>
[0.000] 00000.00000> welcome to Zircon
[0.000] 00000.00000>
TAP version 13
1..5
testrunner output
Running test 'ffxTest1'
ffxTest1 output line 1
Running test 'ffxTest1.testcase1'
ffxTest1 output line 2
ffxTest1 completed with result: PASSED
Running test 'ffxTest2'
ffxTest2 completed with result: FAILED
ok 1 ffxTest1 (12.6s)
not ok 2 ffxTest2 (44.511025786s)
testrunner output
Running test 'v1Test3'
v1Test3 output line 1
ok 3 fakelog
v1Test3 output line 2
Tests took 41 seconds.
ok 3 v1Test3 (27.90584341s)
hostTest4 output line 1
hostTest4 output line 2
Running test 'test5' inside host test
test5 completed with result: PASSED
hostTest4 output mixed with testrunner output not ok 4 hostTest4 (88.8s)
Running test 'test5'
ok 5 test5 (7s)
Running test 'notRecordedTest'
ok 6 notRecordedTest (234.234s)
2020/03/20 22:57:53.569333 botanist DEBUG: stopping or rebooting the node "upper-bacon-clock-snort"
`

func TestSplitLogByTest(t *testing.T) {
	cases := []struct {
		name           string
		input          string
		inputTests     []string
		expectedOutput []TestLog
	}{
		{
			name:       "log without ffx tests",
			input:      validLog,
			inputTests: validLogTests,
			expectedOutput: []TestLog{
				{"Test/1", "Test/1", []byte("Test/1 output line 1\n" +
					"Test/1 output line 2\n" +
					"ok 1 Test/1 (12.6s)\n"), "", 222},
				{"Test2", "Test2", []byte("not ok 2 Test2 (44.511025786s)\n"), "", 284},
				{"Test3", "Test3", []byte("Test3 output line 1\n" +
					"Test3 output line 2\n" +
					"ok 3 fakeTest (1)\n" +
					"Tests took 41 seconds.\n" +
					"ok 3 Test3 (27.90584341s)\n"), "", 315},
				{"Test4", "Test4", []byte("Test4 output line 1\n" +
					"Test4 output line 2\n" +
					"Test4 output mixed with testrunner output not ok 4 Test4 (88.8s)\n"), "", 422},
				{"Test5", "Test5", []byte("Test5 output line 1\n" +
					"fail message\n" +
					"Test5 failed\n" +
					"not ok 5 Test5 (27.90584341s)\n"), "", 527},
				{"Test5", "Test5", []byte("Test5 output\n" +
					"2020/03/20 22:57:53.569095 botanist attempting to close SSH session due to: context canceled\n" +
					"ok 6 Test5 (9s)\n"), "", 603},
			},
		}, {
			name:       "log with ffx tests",
			input:      validFFXLog,
			inputTests: []string{"ffxTest1", "ffxTest2", "v1Test3", "hostTest4", "test5"},
			expectedOutput: []TestLog{
				{"ffxTest1", "ffxTest1", []byte("testrunner output\n" +
					"Running test 'ffxTest1'\n" +
					"ffxTest1 output line 1\n" +
					"Running test 'ffxTest1.testcase1'\n" +
					"ffxTest1 output line 2\n" +
					"ffxTest1 completed with result: PASSED\n"), "", 222},
				{"ffxTest2", "ffxTest2", []byte("Running test 'ffxTest2'\n" +
					"ffxTest2 completed with result: FAILED\n" +
					"ok 1 ffxTest1 (12.6s)\n" +
					"not ok 2 ffxTest2 (44.511025786s)\n"), "", 383},
				{"v1Test3", "v1Test3", []byte("testrunner output\n" +
					"Running test 'v1Test3'\n" +
					"v1Test3 output line 1\n" +
					"ok 3 fakelog\n" +
					"v1Test3 output line 2\n" +
					"Tests took 41 seconds.\n" +
					"ok 3 v1Test3 (27.90584341s)\n"), "", 502},
				{"hostTest4", "hostTest4", []byte("hostTest4 output line 1\n" +
					"hostTest4 output line 2\n" +
					"Running test 'test5' inside host test\n" +
					"test5 completed with result: PASSED\n" +
					"hostTest4 output mixed with testrunner output not ok 4 hostTest4 (88.8s)\n"), "", 651},
				{"test5", "test5", []byte("Running test 'test5'\n" +
					"ok 5 test5 (7s)\n"), "", 846},
			},
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			output, err := splitLogByTest([]byte(tc.input), testDetailsForTests(tc.inputTests))
			if err != nil {
				t.Fatal("splitLogByTest() returned unexpected error:", err)
			}
			if diff := cmp.Diff(tc.expectedOutput, output); diff != "" {
				t.Errorf("splitLogByTest() returned wrong output (-want +got):\n%s", diff)
			}
		})
	}
}

func testDetailsForTests(tests []string) []runtests.TestDetails {
	var testDetails []runtests.TestDetails
	for _, testName := range tests {
		testDetails = append(testDetails, runtests.TestDetails{Name: testName, TestResult: runtests.TestResult{OutputDir: testName}})
	}
	return testDetails
}

const truncatedLog = `TAP version 13
1..2
Test1 output line 1
ok 1 Test1 (12.6s)
foo
foo
foo
foo
foo
foo
foo
foo
foo`

func TestSplitTruncatedLog(t *testing.T) {
	input := []byte(truncatedLog)
	output, err := splitLogByTest(input, testDetailsForTests([]string{"Test1", "Test2"}))
	if err != nil {
		t.Fatal("splitLogByTest() returned unexpected error:", err)
	}
	wantOutput := []TestLog{
		0: {"Test1", "Test1", []byte("Test1 output line 1\n" +
			"ok 1 Test1 (12.6s)\n"), "", 20},
	}
	if diff := cmp.Diff(wantOutput, output); diff != "" {
		t.Errorf("splitLogByTest() returned wrong output (-want +got):\n%s", diff)
	}
}

func TestSplitNonTAPLog(t *testing.T) {
	var input []byte
	// Input does not contain tapVersionBytes.
	for i := 0; i < 200; i++ {
		input = append(input, []byte("foo\n")...)
	}
	output, err := splitLogByTest(input, testDetailsForTests([]string{"foo"}))
	if err != nil {
		t.Fatal("splitLogByTest() returned unexpected error:", err)
	}
	if len(output) != 0 {
		t.Errorf("splitLogByTest() returned %d keys, want 0", len(output))
	}
}

func lastLine(input []byte) string {
	ret := strings.TrimRight(string(input), "\n")
	i := strings.LastIndexByte(ret, '\n')
	if i >= 0 {
		ret = ret[i+1:]
	}
	return ret
}

func TestSplitTestLogs(t *testing.T) {
	logDir := t.TempDir()
	if err := os.MkdirAll(logDir, 0o700); err != nil {
		t.Fatal("failed to create unifiedLogDir:", err)
	}

	const baseName = "valid.txt"
	testLogs, err := SplitTestLogs([]byte(validLog), baseName, logDir, testDetailsForTests(validLogTests))
	if err != nil {
		t.Fatal("SplitTestLogs() failed:", err)
	}
	if len(testLogs) == 0 {
		t.Fatal("SplitTestLogs() returned an empty slice")
	}
	for testIndex, testLog := range testLogs {
		if !strings.HasPrefix(testLog.FilePath, logDir) {
			t.Errorf("expected per-test log path to start with baseDir. got: %s\nwant: %s", testLog.FilePath, logDir)
		}
		if filepath.Base(testLog.FilePath) != baseName {
			t.Errorf("expected per-test log path to end with baseName. got: %s\n want: %s", testLog.FilePath, baseName)
		}
		actualLastLine := lastLine(testLog.Bytes)
		expectedRE, err := regexp.Compile(fmt.Sprintf("(not )?ok %d ", testIndex+1))
		if err != nil {
			t.Errorf("failed to compile regexp for expected last line: %v", err)
		} else if !expectedRE.Match([]byte(actualLastLine)) {
			t.Errorf("wrong final log line. got:%s\nwant:%s", actualLastLine, expectedRE.String())
		}
	}
}

func TestParseLogTimestamp(t *testing.T) {
	utc := time.UTC
	cases := []struct {
		line   string
		want   time.Time
		wantOk bool
	}{
		{
			line:   "2026-10-05 14:18:56.123456 [klog] message",
			want:   time.Date(2026, 10, 5, 14, 18, 56, 123456000, utc),
			wantOk: true,
		},
		{
			line:   "2026-10-05 14:18:56.123 [klog] message",
			want:   time.Date(2026, 10, 5, 14, 18, 56, 123000000, utc),
			wantOk: true,
		},
		{
			line:   "2026-10-05 14:18:56 [klog] message",
			want:   time.Date(2026, 10, 5, 14, 18, 56, 0, utc),
			wantOk: true,
		},
		{
			line:   "2020/03/20 22:47:38 Attempting to powercycle",
			want:   time.Date(2020, 3, 20, 22, 47, 38, 0, utc),
			wantOk: true,
		},
		{
			line:   "2020/03/20 22:47:38.654321 Attempting to powercycle",
			want:   time.Date(2020, 3, 20, 22, 47, 38, 654321000, utc),
			wantOk: true,
		},
		{
			line:   "[2026-10-05 14:18:56.123456] [klog] message",
			want:   time.Date(2026, 10, 5, 14, 18, 56, 123456000, utc),
			wantOk: true,
		},
		{
			line:   "2026-10-05T14:18:56.123456Z message",
			want:   time.Date(2026, 10, 5, 14, 18, 56, 123456000, utc),
			wantOk: true,
		},
		{
			line:   "2026-10-05T14:18:56Z message",
			want:   time.Date(2026, 10, 5, 14, 18, 56, 0, utc),
			wantOk: true,
		},
		{
			line:   "2026-10-05T14:18:56.123456-07:00 message",
			want:   time.Date(2026, 10, 5, 14, 18, 56, 123456000, time.FixedZone("", -7*3600)),
			wantOk: true,
		},
		{
			line:   "[0.000] 00000.00000> welcome to Zircon",
			wantOk: false,
		},
		{
			line:   "just a regular log line",
			wantOk: false,
		},
		{
			line:   "",
			wantOk: false,
		},
	}

	for _, tc := range cases {
		got, ok := parseLogTimestamp([]byte(tc.line), utc)
		if ok != tc.wantOk {
			t.Errorf("parseLogTimestamp(%q) ok = %v, want %v", tc.line, ok, tc.wantOk)
			continue
		}
		if tc.wantOk && !got.Equal(tc.want) {
			t.Errorf("parseLogTimestamp(%q) = %v, want %v", tc.line, got, tc.want)
		}
	}
}

func TestFindTestForTime(t *testing.T) {
	utc := time.UTC
	t0 := time.Date(2026, 10, 5, 14, 0, 0, 0, utc)
	t1 := time.Date(2026, 10, 5, 14, 5, 0, 0, utc)
	t2 := time.Date(2026, 10, 5, 14, 10, 0, 0, utc)

	tests := []runtests.TestDetails{
		{Name: "test0", StartTime: t0, DurationMillis: int64((5 * time.Minute) / time.Millisecond)},
		{Name: "test1", StartTime: t1, DurationMillis: int64((5 * time.Minute) / time.Millisecond)},
		{Name: "test2", StartTime: t2, DurationMillis: int64((1 * time.Minute) / time.Millisecond)},
	}

	cases := []struct {
		time time.Time
		want int
	}{
		{time: t0.Add(-1 * time.Second), want: -1},
		{time: t0, want: 0},
		{time: t0.Add(2 * time.Minute), want: 0},
		{time: t1, want: 1},
		{time: t1.Add(3 * time.Minute), want: 1},
		{time: t2, want: 2},
		{time: t2.Add(30 * time.Second), want: 2},
		{time: t2.Add(70 * time.Second), want: 2},  // within 10s grace
		{time: t2.Add(80 * time.Second), want: -1}, // past 10s grace
	}

	for _, tc := range cases {
		got := findTestForTime(tc.time, tests)
		if got != tc.want {
			t.Errorf("findTestForTime(%v) = %d, want %d", tc.time, got, tc.want)
		}
	}
}

func TestSplitLogByTestTime(t *testing.T) {
	utc := time.UTC
	t0 := time.Date(2026, 10, 5, 14, 0, 0, 0, utc)
	t1 := time.Date(2026, 10, 5, 14, 5, 0, 0, utc)

	tests := []runtests.TestDetails{
		{Name: "test0", TestResult: runtests.TestResult{OutputDir: "test0_out"}, StartTime: t0, DurationMillis: 300000},
		{Name: "test1", TestResult: runtests.TestResult{OutputDir: "test1_out"}, StartTime: t1, DurationMillis: 300000},
	}

	log := "2026-10-05 13:59:00.000000 pre-test line\n" +
		"2026-10-05 14:00:00.000000 test0 line 1\n" +
		"continuation of test0 line 1 without timestamp\n" +
		"2026-10-05 14:01:00.000000 test0 line 2\n" +
		"2026-10-05 14:05:00.000000 test1 line 1\n" +
		"2026-10-05 14:06:00.000000 test1 line 2\n" +
		"2026-10-05 14:20:00.000000 post-test line\n"

	got := splitLogByTestTime([]byte(log), tests)
	if len(got) != 2 {
		t.Fatalf("splitLogByTestTime returned %d logs, want 2", len(got))
	}

	wantTest0 := "2026-10-05 14:00:00.000000 test0 line 1\n" +
		"continuation of test0 line 1 without timestamp\n" +
		"2026-10-05 14:01:00.000000 test0 line 2\n"
	if string(got[0].Bytes) != wantTest0 {
		t.Errorf("got[0].Bytes = %q, want %q", string(got[0].Bytes), wantTest0)
	}
	if got[0].TestName != "test0" {
		t.Errorf("got[0].TestName = %q, want %q", got[0].TestName, "test0")
	}
	if got[0].TestOutputDir != "test0_out" {
		t.Errorf("got[0].TestOutputDir = %q, want %q", got[0].TestOutputDir, "test0_out")
	}
	if got[0].Index != len("2026-10-05 13:59:00.000000 pre-test line\n") {
		t.Errorf("got[0].Index = %d, want %d", got[0].Index, len("2026-10-05 13:59:00.000000 pre-test line\n"))
	}

	wantTest1 := "2026-10-05 14:05:00.000000 test1 line 1\n" +
		"2026-10-05 14:06:00.000000 test1 line 2\n"
	if string(got[1].Bytes) != wantTest1 {
		t.Errorf("got[1].Bytes = %q, want %q", string(got[1].Bytes), wantTest1)
	}
	if got[1].TestName != "test1" {
		t.Errorf("got[1].TestName = %q, want %q", got[1].TestName, "test1")
	}
}

func TestSplitTestLogsByTime(t *testing.T) {
	utc := time.UTC
	t0 := time.Date(2026, 10, 5, 14, 0, 0, 0, utc)
	t1 := time.Date(2026, 10, 5, 14, 5, 0, 0, utc)

	tests := []runtests.TestDetails{
		{Name: "test0", TestResult: runtests.TestResult{OutputDir: "test0_out"}, StartTime: t0, DurationMillis: 300000},
		{Name: "test1", TestResult: runtests.TestResult{OutputDir: "test1_out"}, StartTime: t1, DurationMillis: 300000},
	}

	log := "2026-10-05 14:00:00.000000 test0 line 1\n" +
		"2026-10-05 14:05:00.000000 test1 line 1\n"

	logDir := t.TempDir()
	const baseName = "syslog.txt"
	testLogs, err := SplitTestLogsByTime([]byte(log), baseName, logDir, tests)
	if err != nil {
		t.Fatalf("SplitTestLogsByTime() failed: %v", err)
	}
	if len(testLogs) != 2 {
		t.Fatalf("SplitTestLogsByTime() returned %d logs, want 2", len(testLogs))
	}
	for _, testLog := range testLogs {
		if !strings.HasPrefix(testLog.FilePath, logDir) {
			t.Errorf("expected per-test log path to start with logDir. got: %s\nwant: %s", testLog.FilePath, logDir)
		}
		if filepath.Base(testLog.FilePath) != baseName {
			t.Errorf("expected per-test log path to end with baseName. got: %s\n want: %s", testLog.FilePath, baseName)
		}
		data, err := os.ReadFile(testLog.FilePath)
		if err != nil {
			t.Errorf("failed to read written file %s: %v", testLog.FilePath, err)
		} else if diff := cmp.Diff(testLog.Bytes, data); diff != "" {
			t.Errorf("written file diff (-want +got): %s", diff)
		}
	}
}

func TestReadAndSplitLogs(t *testing.T) {
	utc := time.UTC
	t0 := time.Date(2026, 10, 5, 14, 0, 0, 0, utc)
	summary := &runtests.TestSummary{
		Tests: []runtests.TestDetails{
			{Name: "test0", TestResult: runtests.TestResult{OutputDir: "test0_out"}, StartTime: t0, DurationMillis: 60000},
		},
	}

	tempDir := t.TempDir()
	logPath := filepath.Join(tempDir, "syslog.txt")
	logContent := []byte("2026-10-05 14:00:00.000000 test0 line 1\n")
	if err := os.WriteFile(logPath, logContent, 0o666); err != nil {
		t.Fatal(err)
	}

	outputsDir := filepath.Join(tempDir, "out")
	raw, perTest, err := ReadAndSplitLogs([]string{logPath}, summary, outputsDir, nil)
	if err != nil {
		t.Fatalf("ReadAndSplitLogs failed: %v", err)
	}
	if len(raw) != 1 || string(raw[0]) != string(logContent) {
		t.Errorf("raw logs mismatch: got %s, want %s", raw, logContent)
	}
	if len(perTest) != 1 || len(perTest[0]) != 1 {
		t.Fatalf("perTest logs length mismatch: got %d groups", len(perTest))
	}
	if string(perTest[0][0].Bytes) != string(logContent) {
		t.Errorf("perTest[0][0].Bytes got %s, want %s", perTest[0][0].Bytes, logContent)
	}

	// Multiple logs with identical basenames should be disambiguated with numeric suffixes.
	multiOutDir := filepath.Join(tempDir, "multi_out")
	logPath2 := filepath.Join(tempDir, "sub", "syslog.txt")
	if err := os.MkdirAll(filepath.Dir(logPath2), 0o766); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(logPath2, logContent, 0o666); err != nil {
		t.Fatal(err)
	}
	counts := make(map[string]int)
	_, perTestMulti, err := ReadAndSplitLogs([]string{logPath, logPath2}, summary, multiOutDir, counts)
	if err != nil {
		t.Fatalf("ReadAndSplitLogs with duplicates failed: %v", err)
	}
	if len(perTestMulti) != 2 {
		t.Fatalf("perTestMulti got %d groups, want 2", len(perTestMulti))
	}
	if filepath.Base(perTestMulti[0][0].FilePath) != "syslog.txt" {
		t.Errorf("first log filename = %q, want 'syslog.txt'", filepath.Base(perTestMulti[0][0].FilePath))
	}
	if filepath.Base(perTestMulti[1][0].FilePath) != "syslog_1.txt" {
		t.Errorf("second log filename = %q, want 'syslog_1.txt'", filepath.Base(perTestMulti[1][0].FilePath))
	}

	// Nil summary should read logs without error and return nil perTest.
	rawNil, perTestNil, err := ReadAndSplitLogs([]string{logPath}, nil, outputsDir, nil)
	if err != nil {
		t.Fatalf("ReadAndSplitLogs with nil summary failed: %v", err)
	}
	if len(rawNil) != 1 || len(perTestNil) != 0 {
		t.Errorf("ReadAndSplitLogs with nil summary got raw=%v, perTest=%v", rawNil, perTestNil)
	}
}

func TestRecordTestOutputFiles(t *testing.T) {
	outputsDir := t.TempDir()
	summary := &runtests.TestSummary{
		Tests: []runtests.TestDetails{
			{Name: "test0", TestResult: runtests.TestResult{OutputDir: "test0_out"}},
		},
	}
	testLogPath := filepath.Join(outputsDir, "test0_out", "syslog.txt")
	if err := os.MkdirAll(filepath.Dir(testLogPath), 0o766); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(testLogPath, []byte("data"), 0o666); err != nil {
		t.Fatal(err)
	}

	logGroups := [][]TestLog{
		{
			{TestName: "test0", FilePath: testLogPath},
		},
	}
	if err := RecordTestOutputFiles(summary, logGroups, outputsDir); err != nil {
		t.Fatalf("RecordTestOutputFiles failed: %v", err)
	}
	if len(summary.Tests[0].OutputFiles) != 1 || summary.Tests[0].OutputFiles[0] != "syslog.txt" {
		t.Errorf("summary.Tests[0].OutputFiles got %v, want ['syslog.txt']", summary.Tests[0].OutputFiles)
	}

	// Nil summary should succeed without doing anything.
	if err := RecordTestOutputFiles(nil, logGroups, outputsDir); err != nil {
		t.Errorf("RecordTestOutputFiles with nil summary failed: %v", err)
	}

	// Mismatched test name should return an error.
	badGroups := [][]TestLog{
		{
			{TestName: "different_test", FilePath: testLogPath},
		},
	}
	if err := RecordTestOutputFiles(summary, badGroups, outputsDir); err == nil || !strings.Contains(err.Error(), "syslog.txt") {
		t.Errorf("expected error containing 'syslog.txt' for mismatched test name, got: %v", err)
	}

	// Too many per-test logs should return an error.
	tooManyGroups := [][]TestLog{
		{
			{TestName: "test0", FilePath: testLogPath},
			{TestName: "test1", FilePath: testLogPath},
		},
	}
	if err := RecordTestOutputFiles(summary, tooManyGroups, outputsDir); err == nil || !strings.Contains(err.Error(), "syslog.txt") {
		t.Errorf("expected error containing 'syslog.txt' when per-test logs count exceeds tests count, got: %v", err)
	}
}

func TestSplitLogByTestTime_OutOfOrderTimestamps(t *testing.T) {
	utc := time.UTC
	t0 := time.Date(2026, 10, 5, 14, 0, 0, 0, utc)
	t1 := time.Date(2026, 10, 5, 14, 5, 0, 0, utc)

	tests := []runtests.TestDetails{
		{Name: "test0", StartTime: t0, DurationMillis: 300000},
		{Name: "test1", StartTime: t1, DurationMillis: 300000},
	}

	// Line 3 has a historical timestamp (earlier than test1's start time),
	// but appears chronologically during test1. It should NOT cause test0's
	// range to jump forward or stretch across test1.
	// Line 6 has a historical timestamp that appears after all tests have completed;
	// it should be ignored and not revive test0 or test1.
	log := "2026-10-05 14:00:00.000000 test0 line\n" +
		"2026-10-05 14:05:00.000000 test1 start\n" +
		"2026-10-05 14:01:00.000000 historical log during test1\n" +
		"2026-10-05 14:06:00.000000 test1 line\n" +
		"2026-10-05 14:15:00.000000 post-test line after grace period\n" +
		"2026-10-05 14:02:00.000000 late historical log after completion\n"

	got := splitLogByTestTime([]byte(log), tests)
	if len(got) != 2 {
		t.Fatalf("got %d test logs, want 2", len(got))
	}
	wantTest0 := "2026-10-05 14:00:00.000000 test0 line\n"
	if string(got[0].Bytes) != wantTest0 {
		t.Errorf("got[0].Bytes = %q, want %q", string(got[0].Bytes), wantTest0)
	}
	wantTest1 := "2026-10-05 14:05:00.000000 test1 start\n" +
		"2026-10-05 14:01:00.000000 historical log during test1\n" +
		"2026-10-05 14:06:00.000000 test1 line\n"
	if string(got[1].Bytes) != wantTest1 {
		t.Errorf("got[1].Bytes = %q, want %q", string(got[1].Bytes), wantTest1)
	}
}
