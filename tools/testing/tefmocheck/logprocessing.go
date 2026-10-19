// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package tefmocheck

import (
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strconv"
	"strings"
	"time"

	"golang.org/x/sync/errgroup"

	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

const finalTestGracePeriod = 10 * time.Second

// SplitTestLogs splits logBytes into per-test logs,
// writes those per-test logs to files in outDir, and returns a slice of TestLogs.
// The logs will be written to the parent directory of path.
func SplitTestLogs(logBytes []byte, logBaseName, outDir string, testNames []runtests.TestDetails) ([]TestLog, error) {
	testLogs, err := splitLogByTest(logBytes, testNames)
	if err != nil {
		return nil, err
	}
	if outDir == "" {
		return testLogs, nil
	}
	if err := writeTestLogs(testLogs, logBaseName, outDir); err != nil {
		return nil, err
	}
	return testLogs, nil
}

// SplitTestLogsByTime splits logBytes into per-test logs based on timestamps,
// writes those per-test logs to files in outDir if outDir is not empty, and
// returns a slice of TestLogs.
func SplitTestLogsByTime(logBytes []byte, logBaseName, outDir string, tests []runtests.TestDetails) ([]TestLog, error) {
	testLogs := splitLogByTestTime(logBytes, tests)
	if outDir == "" {
		return testLogs, nil
	}
	if err := writeTestLogs(testLogs, logBaseName, outDir); err != nil {
		return nil, err
	}
	return testLogs, nil
}

func writeTestLogs(testLogs []TestLog, logBaseName, outDir string) error {
	var g errgroup.Group
	for i := range testLogs {
		testLog := &testLogs[i]
		if len(testLog.Bytes) == 0 {
			continue
		}
		g.Go(func() error {
			destPath := filepath.Join(outDir, testLog.TestOutputDir, logBaseName)
			// If the destination file already exists (e.g. emitted natively by the test runner),
			// skip overwriting it so native test logs take precedence.
			if _, err := os.Stat(destPath); err == nil {
				testLog.FilePath = destPath
				return nil
			}
			if err := os.MkdirAll(filepath.Dir(destPath), 0o766); err != nil {
				return err
			}
			if err := os.WriteFile(destPath, testLog.Bytes, 0o666); err != nil {
				return err
			}
			testLog.FilePath = destPath
			return nil
		})
	}
	return g.Wait()
}

// ReadAndSplitLogs reads each file at the given paths and, if a test summary is provided,
// splits the logs into per-test logs based on test time windows.
// If multiple log paths share the same base name, subsequent files are disambiguated
// with a numeric suffix (e.g. syslog.txt, syslog_1.txt) to avoid overwriting files.
func ReadAndSplitLogs(paths []string, summary *runtests.TestSummary, outputsDir string, baseNameCounts map[string]int) ([][]byte, [][]TestLog, error) {
	var tests []runtests.TestDetails
	if summary != nil {
		tests = summary.Tests
	}
	var raw [][]byte
	var perTest [][]TestLog
	for _, path := range paths {
		content, err := os.ReadFile(path)
		if err != nil {
			return nil, nil, fmt.Errorf("failed to read log from %s: %w", path, err)
		}
		raw = append(raw, content)
		if len(tests) > 0 {
			base := filepath.Base(path)
			if baseNameCounts != nil {
				count := baseNameCounts[base]
				baseNameCounts[base] = count + 1
				if count > 0 {
					ext := filepath.Ext(base)
					stem := strings.TrimSuffix(base, ext)
					base = fmt.Sprintf("%s_%d%s", stem, count, ext)
				}
			}
			pt, err := SplitTestLogsByTime(content, base, outputsDir, tests)
			if err != nil {
				return nil, nil, fmt.Errorf("failed to split log %s: %w", path, err)
			}
			perTest = append(perTest, pt)
		}
	}
	return raw, perTest, nil
}

// RecordTestOutputFiles registers the relative paths of the given per-test logs
// in each test's OutputFiles slice within the summary.
func RecordTestOutputFiles(summary *runtests.TestSummary, logGroups [][]TestLog, outputsDir string) error {
	if summary == nil {
		return nil
	}
	tests := summary.Tests
	for _, perTest := range logGroups {
		logName := "log"
		for _, l := range perTest {
			if l.FilePath != "" {
				logName = filepath.Base(l.FilePath)
				break
			}
		}
		for i := range perTest {
			if i >= len(tests) {
				return fmt.Errorf("per-test logs count (%d) for %s exceeds tests count (%d)", len(perTest), logName, len(tests))
			}
			test := &tests[i]
			testLog := &perTest[i]
			if testLog.FilePath == "" {
				continue
			}
			// TODO(olivernewman): This check fails when a unit test for
			// testrunner fails and the failure output contains the TAP output
			// referencing fake tests used for test data. Make this check less
			// strict by having `SplitTestLogs` only create a split log for
			// tests that actually appear in summary.json.
			if testLog.TestName != "" && test.Name != testLog.TestName {
				return fmt.Errorf("per-test log %s name %q does not match test name %q", filepath.Base(testLog.FilePath), testLog.TestName, test.Name)
			}
			relPath, err := filepath.Rel(filepath.Join(outputsDir, test.OutputDir), testLog.FilePath)
			if err != nil {
				return err
			}
			test.OutputFiles = append(test.OutputFiles, relPath)
		}
	}
	return nil
}

var testPlanRE = regexp.MustCompile(`1\.\.(\d+)\n`)

// For ffx tests run with the -test-file flag, the testFinishedRE shows up
// for all tests at the end after all tests are run, so we split on the test
// start log instead.
var ffxTestStartedRE = regexp.MustCompile(`Running test '(\S+)'`)

const testPlanSubmatches = 2

// TestLog represents an individual test's slice of a larger log file.
type TestLog struct {
	TestName      string
	TestOutputDir string
	Bytes         []byte
	FilePath      string
	// Index represents the start index of the log within the larger log.
	Index int
}

// splitLogsByTest expects the standard output of a Fuchsia test Swarming task
// as input. We expect it to look like:
// * Prefix that we ignore
// * TAP version line (TAP = Test Anything Protocol, https://testanything.org/)
// * Alternating log lines and TAP lines.
// * Suffix we that we ignore
//
// NOTE: This is not a correct TAP consumer. There are many things in TAP
// that it does not handle, and there are things not in TAP that it does handle.
// It is only tested against the output of the fuchsia testrunner:
// https://fuchsia.googlesource.com/fuchsia/+/HEAD/tools/testing/testrunner
//
// Technical debt: this should probably be moved or combined with code in tools/testing/tap/.
// It exists separately because this code was originally written in google3.
//
// Returns a slice of TestLogs.
func splitLogByTest(input []byte, testNames []runtests.TestDetails) ([]TestLog, error) {
	var ret []TestLog
	// Everything up to and including tapVersionBytes is a prefix that we skip.
	tapVersionBytes := []byte("TAP version 13\n")
	tapVersionIndex := bytes.Index(input, tapVersionBytes)
	if tapVersionIndex == -1 {
		return ret, nil
	}

	var totalTests, testsSeen uint64
	var advance int
	for i := tapVersionIndex + len(tapVersionBytes); i < len(input); i += advance {
		if int(testsSeen) == len(testNames) {
			break
		}
		advance = 0
		data := input[i:]
		// Do not require this to be at start-of-line as it sometimes appears in the middle.
		testFinishedRE, err := regexp.Compile(fmt.Sprintf(`(not )?ok %d (\S+) \(\d+`, testsSeen+1))
		if err != nil {
			return ret, fmt.Errorf("failed to compile regexp: %v", err)
		}
		var testName string
		var testOutputDir string
		const testFinishedSubmatches = 3
		for len(data) > 0 {
			advanceForLine, line := len(data), data
			if j := bytes.IndexByte(data, '\n'); j >= 0 {
				advanceForLine, line = j+1, data[0:j+1]
			}
			data = data[advanceForLine:]
			advance += advanceForLine
			if totalTests == 0 {
				matches := testPlanRE.FindSubmatch(line)
				if len(matches) != testPlanSubmatches {
					continue
				}
				totalTests, err = strconv.ParseUint(string(matches[testPlanSubmatches-1]), 10, 64)
				if err != nil {
					return ret, fmt.Errorf("failed to parse totalTests: %v", err)
				}
				// We've seen everything before the first line of the first test
				break
			} else {
				matches := ffxTestStartedRE.FindSubmatch(line)
				if len(matches) != 2 {
					matches = testFinishedRE.FindSubmatch(line)
					if len(matches) != testFinishedSubmatches {
						continue
					} else if testName == "" {
						newTestName := string(matches[testFinishedSubmatches-1])
						if newTestName == testNames[testsSeen].Name {
							testName = newTestName
							testOutputDir = testNames[testsSeen].OutputDir
						} else {
							continue
						}
					}
				} else {
					newTestName := string(matches[1])
					if testName == "" && newTestName == testNames[testsSeen].Name {
						testName = newTestName
						testOutputDir = testNames[testsSeen].OutputDir
						continue
					} else if testName != "" && int(testsSeen+1) < len(testNames) && newTestName == testNames[testsSeen+1].Name {
						// A new test started, so reset the data so this line gets
						// included in the next TestLog.
						advance -= advanceForLine
						data = append(line, data...)
					} else {
						// We found a match but it doesn't match either the
						// current or next test, so just continue to the next line.
						continue
					}
				}
				ret = append(ret, TestLog{testName, testOutputDir, input[i : i+advance], "", i})
				testsSeen += 1
				break
			}
		}
	}
	return ret, nil
}

var logTimestampRE = regexp.MustCompile(`^\[?(\d{4}[-/]\d{2}[-/]\d{2}[ T]\d{2}:\d{2}:\d{2}(?:\.\d+)?(?:Z|[+-]\d{2}:?\d{2})?)\]?`)

// Layouts are ordered by decreasing frequency in Fuchsia test environments:
// Botanist timestamped logs ("2006-01-02 15:04:05.999999999"), Go standard
// logger timestamps ("2006/01/02 15:04:05.999999999"), ISO 8601, and RFC3339.
// Note: layouts specifying fractional seconds (".999999999" and RFC3339Nano) also
// parse timestamps without fractional seconds in time.Parse.
var logTimeLayouts = []string{
	"2006-01-02 15:04:05.999999999",
	"2006/01/02 15:04:05.999999999",
	"2006-01-02T15:04:05.999999999",
	time.RFC3339Nano,
}

// parseLogTimestamp parses host wall-clock timestamps prepended to device logs
// (e.g. by Botanist NewTimestampWriter: "YYYY-MM-DD HH:MM:SS.ffffff "),
// standard Go log timestamps ("YYYY/MM/DD HH:MM:SS"), bracketed formats,
// and RFC3339 formats. Monotonic uptime timestamps (such as Zircon uptime
// "[00012.345]") are ignored and return false.
func parseLogTimestamp(line []byte, loc *time.Location) (time.Time, bool) {
	if loc == nil {
		loc = time.UTC
	}
	matches := logTimestampRE.FindSubmatch(line)
	if len(matches) < 2 {
		return time.Time{}, false
	}
	str := string(matches[1])
	for _, layout := range logTimeLayouts {
		if t, err := time.ParseInLocation(layout, str, loc); err == nil {
			return t, true
		}
	}
	return time.Time{}, false
}

// findTestForTime finds the index of the test that was active at time t.
// It assumes tests is sorted chronologically by StartTime.
//
// For non-terminal tests (i < len(tests)-1), any log emitted between
// tests[i].StartTime and tests[i+1].StartTime is attributed to test i,
// accounting for test teardown and inter-test latency.
// For the final test in tests, the active window is bounded by
// StartTime + DurationMillis + finalTestGracePeriod (or unbounded if DurationMillis
// is not set).
func findTestForTime(t time.Time, tests []runtests.TestDetails) int {
	for i, test := range tests {
		if test.StartTime.IsZero() {
			continue
		}
		if t.Before(test.StartTime) {
			continue
		}
		if i+1 < len(tests) && !tests[i+1].StartTime.IsZero() && tests[i+1].StartTime.After(test.StartTime) {
			if t.Before(tests[i+1].StartTime) {
				return i
			}
			continue
		}
		if test.DurationMillis > 0 {
			end := test.StartTime.Add(time.Duration(test.DurationMillis)*time.Millisecond + finalTestGracePeriod)
			if !t.After(end) {
				return i
			}
			continue
		}
		return i
	}
	return -1
}

// splitLogByTestTime splits input log bytes into per-test logs based on the
// timestamp at the beginning of each line and the StartTime and Duration of each test.
// Returns a slice of TestLogs with one entry per test in tests.
// Lines without timestamps are attributed to the currently active test, preserving
// multi-line stack traces and continuations.
//
// Operational assumptions:
// 1. Tests in tests must be executed sequentially and ordered chronologically by
// StartTime (parallel test execution is unsupported for time-window splitting).
// 2. Host log timestamps (e.g. from Botanist) are assumed to be in UTC,
// matching LUCI builder host configurations.
func splitLogByTestTime(input []byte, tests []runtests.TestDetails) []TestLog {
	if len(tests) == 0 {
		return nil
	}
	ret := make([]TestLog, len(tests))
	for i, test := range tests {
		ret[i] = TestLog{
			TestName:      test.Name,
			TestOutputDir: test.OutputDir,
		}
	}
	if len(input) == 0 {
		return ret
	}

	loc := time.UTC
	hasValidStartTime := false
	for _, test := range tests {
		if !test.StartTime.IsZero() {
			if test.StartTime.Location() != nil {
				loc = test.StartTime.Location()
			}
			hasValidStartTime = true
			break
		}
	}
	if !hasValidStartTime {
		return ret
	}

	type byteRange struct {
		start int
		end   int
		set   bool
	}
	ranges := make([]byteRange, len(tests))

	currentTest := -1
	completedAllTests := false
	lineStart := 0
	for lineStart < len(input) {
		lineEnd := bytes.IndexByte(input[lineStart:], '\n')
		if lineEnd < 0 {
			lineEnd = len(input)
		} else {
			lineEnd = lineStart + lineEnd + 1
		}
		lineBytes := input[lineStart:lineEnd]

		if t, ok := parseLogTimestamp(lineBytes, loc); ok {
			candidateTest := findTestForTime(t, tests)
			if completedAllTests {
				// Once all tests have completed, ignore subsequent logs even if
				// they contain older timestamps.
				currentTest = -1
			} else if candidateTest == -1 {
				// Time has moved outside/past active test windows.
				currentTest = -1
				if ranges[len(tests)-1].set {
					completedAllTests = true
				}
			} else if candidateTest >= currentTest {
				currentTest = candidateTest
			}
			// If candidateTest < currentTest, treat it as an out-of-order log
			// and keep currentTest unchanged without rewinding to an earlier test.
		}

		if currentTest >= 0 && currentTest < len(tests) {
			if !ranges[currentTest].set {
				ranges[currentTest].start = lineStart
				ranges[currentTest].set = true
			}
			ranges[currentTest].end = lineEnd
		}

		lineStart = lineEnd
	}

	for i := range ret {
		if ranges[i].set && ranges[i].end > ranges[i].start {
			ret[i].Bytes = input[ranges[i].start:ranges[i].end]
			ret[i].Index = ranges[i].start
		}
	}
	return ret
}
