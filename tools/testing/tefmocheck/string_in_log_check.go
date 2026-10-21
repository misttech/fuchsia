// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package tefmocheck

import (
	"bytes"
	"fmt"
	"path"
	"path/filepath"
	"slices"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/botanist/constants"
	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

const botanistErrorPrefix = "botanist ERROR: "

// stringInLogCheck checks if String is found in the log named LogName.
type stringInLogCheck struct {
	baseCheck
	// String that will be searched for.
	String string
	// OnlyOnStates will cause Check() to return false if the swarming task
	// state doesn't match with one of these states.
	OnlyOnStates []string
	// ExceptStrings will cause Check() to return false if present.
	ExceptStrings []string
	// ExceptBlocks will cause Check() to return false if the string is only
	// within these blocks. The start string and end string should be unique
	// strings that only appear around the except block. A stray start string
	// will cause everything after it to be included in the except block even
	// if the end string is missing.
	ExceptBlocks []*logBlock
	// SkipPassedTask will cause Check() to return false if the
	// Swarming task succeeded.
	SkipPassedTask bool
	// SkipAllPassedTests will cause Check() to return false if all tests
	// in the Swarming task passed.
	SkipAllPassedTests bool
	// SkipPassedTest will cause Check() to return true only if it finds the
	// log in the per-test output (swarming or syslog) of a failed test.
	SkipPassedTest bool
	// IgnoreFlakes will cause Check() to behave in the following ways when
	// combined with other options:
	//   SkipAllPassedTests: Check() will ignore flakes when determining if all
	//     tests passed.
	//   SkipPassedTest: Check() will return the failure as a flake if the
	//     associated test later passes.
	IgnoreFlakes bool
	// AlwaysFlake will always return the failure as a flake so that it doesn't
	// fail the build, but will still be reported as a flake.
	AlwaysFlake bool
	// Type of log that will be checked.
	Type logType
	// Whether to check the per-test output for this log and emit a
	// check that's specific to the test during which the log appeared.
	AttributeToTest bool
	// If combined with AttributeToTest, it'll list the test name as a tag instead
	// of appending it to the tefmo check name.
	AddTag bool
	// InfraFailure is true if the check is related to infra.
	InfraFailure bool
	// If true, add a synthetic test case to every failed test. If AttributeToTest is
	// set, the synthetic test case will be added to the specified test instead
	// of every failed test.
	emitSyntheticTestCase bool

	swarmingResult *SwarmingRpcsTaskResult
	testName       string
	outputFile     string
	isFlake        bool
	// line that contains the string being searched for. Can be empty.
	line string
}

// stringInLogCheckConfig represents the YAML schema for a stringInLogCheck.
// Note we are only mapping "public" fields. If a field is added/modified/removed
// in stringInLogCheck, the same will need to be done in this struct as well.
type stringInLogCheckConfig struct {
	String        string   `yaml:"string"`
	LogTypes      []string `yaml:"log_types"`
	OnlyOnStates  []string `yaml:"only_on_states"`
	ExceptStrings []string `yaml:"except_strings"`
	ExceptBlocks  []struct {
		Start string `yaml:"start_string"`
		End   string `yaml:"end_string"`
	} `yaml:"except_blocks"`
	SkipPassedTask        bool `yaml:"skip_passed_task"`
	SkipAllPassedTests    bool `yaml:"skip_all_passed_tests"`
	SkipPassedTest        bool `yaml:"skip_passed_test"`
	IgnoreFlakes          bool `yaml:"ignore_flakes"`
	AlwaysFlake           bool `yaml:"always_flake"`
	AttributeToTest       bool `yaml:"attribute_to_test"`
	AddTag                bool `yaml:"add_tag"`
	InfraFailure          bool `yaml:"infra_failure"`
	EmitSyntheticTestCase bool `yaml:"emit_synthetic_test_case"`
}

// toChecks converts a stringInLogCheckConfig to a slice of FailureModeCheck.
func (c *stringInLogCheckConfig) toChecks() ([]FailureModeCheck, error) {
	if c.String == "" {
		return nil, fmt.Errorf("string_in_log_check 'string' field is required")
	}
	if len(c.LogTypes) == 0 {
		return nil, fmt.Errorf("string_in_log_check 'log_types' field is required and must be a list of strings; given: %v", c.LogTypes)
	}
	var exceptBlocks []*logBlock
	for _, b := range c.ExceptBlocks {
		exceptBlocks = append(exceptBlocks, &logBlock{
			startString: b.Start,
			endString:   b.End,
		})
	}
	var ret []FailureModeCheck
	for _, t := range c.LogTypes {
		lType, err := parseLogType(t)
		if err != nil {
			return nil, fmt.Errorf("for string %q: %w", c.String, err)
		}
		check := &stringInLogCheck{
			String:                c.String,
			OnlyOnStates:          c.OnlyOnStates,
			ExceptStrings:         c.ExceptStrings,
			ExceptBlocks:          exceptBlocks,
			SkipPassedTask:        c.SkipPassedTask,
			SkipAllPassedTests:    c.SkipAllPassedTests,
			SkipPassedTest:        c.SkipPassedTest,
			IgnoreFlakes:          c.IgnoreFlakes,
			AlwaysFlake:           c.AlwaysFlake,
			Type:                  lType,
			AttributeToTest:       c.AttributeToTest,
			AddTag:                c.AddTag,
			InfraFailure:          c.InfraFailure,
			emitSyntheticTestCase: c.EmitSyntheticTestCase,
		}
		ret = append(ret, check)
	}
	return ret, nil
}

func (c *stringInLogCheck) Check(to *TestingOutputs) bool {
	c.swarmingResult = to.SwarmingSummary.Results
	if !c.swarmingResult.Failure && c.swarmingResult.State == "COMPLETED" {
		if c.SkipPassedTask {
			return false
		}
		if c.SkipAllPassedTests {
			failedTests := make(map[string]struct{})
			for _, test := range to.TestSummary.Tests {
				if test.Status != runtests.TestSuccess {
					failedTests[test.Name] = struct{}{}
				} else if c.IgnoreFlakes {
					// If a later run of a failed test passed,
					// remove it from the list of failed tests.
					if _, ok := failedTests[test.Name]; ok {
						delete(failedTests, test.Name)
					}
				}
			}
			if len(failedTests) == 0 {
				return false
			}
		}
	}
	matchedState := slices.Contains(c.OnlyOnStates, c.swarmingResult.State)
	if len(c.OnlyOnStates) != 0 && !matchedState {
		return false
	}

	// Only swarming and syslog outputs support per-test log splitting and attribution.
	// Serial logs are deliberately excluded to avoid false positives caused by
	// UART buffering latency and sequential runner command interleaving across
	// test boundaries.
	if (c.Type == swarmingOutputType || c.Type == syslogType) && (c.AttributeToTest || c.SkipPassedTest) {
		rawLogs, perTestLogs := to.allLogs(c.Type)
		type testdata struct {
			name       string
			outputFile string
			isFlake    bool
			index      int
			line       string
		}
		matchedTestsMap := make(map[string]testdata)
		for fileIdx, testLogs := range perTestLogs {
			var fullLog []byte
			if fileIdx < len(rawLogs) {
				fullLog = rawLogs[fileIdx]
			}
			for i, testLog := range testLogs {
				var testResult runtests.TestStatus
				if to.TestSummary != nil && i < len(to.TestSummary.Tests) {
					testResult = to.TestSummary.Tests[i].Status
				}
				if c.SkipPassedTest && testResult == runtests.TestSuccess {
					if c.IgnoreFlakes {
						if test, ok := matchedTestsMap[testLog.TestName]; ok {
							test.isFlake = true
							matchedTestsMap[testLog.TestName] = test
						}
					}
					continue
				}
				if len(testLog.Bytes) == 0 {
					continue
				}
				toCheck := fullLog
				start := testLog.Index
				end := testLog.Index + len(testLog.Bytes)
				if len(toCheck) < end {
					toCheck = testLog.Bytes
					start = 0
					end = len(testLog.Bytes)
				}
				found, line := c.checkBytes(toCheck, start, end)
				if found {
					matchedTestsMap[testLog.TestName] = testdata{testLog.TestName, testLog.FilePath, c.AlwaysFlake, i, string(line)}
				}
			}
		}
		var unflakedMatches []testdata
		var flakedTests []testdata
		for _, data := range matchedTestsMap {
			if data.isFlake {
				flakedTests = append(flakedTests, data)
			} else {
				unflakedMatches = append(unflakedMatches, data)
			}
		}
		// Prioritize returning the first failure. If there are no failures,
		// then return the first flake.
		if len(unflakedMatches) > 0 {
			slices.SortFunc(unflakedMatches, func(a, b testdata) int {
				return a.index - b.index
			})
			if c.AttributeToTest {
				c.testName = unflakedMatches[0].name
				c.outputFile = unflakedMatches[0].outputFile
				c.line = unflakedMatches[0].line
			}
			return true
		}
		if len(flakedTests) > 0 {
			slices.SortFunc(flakedTests, func(a, b testdata) int {
				return a.index - b.index
			})
			if c.AttributeToTest {
				c.testName = flakedTests[0].name
				c.outputFile = flakedTests[0].outputFile
				c.line = flakedTests[0].line
			}
			c.isFlake = true
			return true
		}
		if c.SkipPassedTest {
			return false
		}
	}

	for _, file := range to.logs(c.Type) {
		found, line := c.checkBytes(file, 0, len(file))
		if found {
			if c.AlwaysFlake {
				c.isFlake = true
			}
			c.line = string(line)
			return true
		}
	}
	return false
}

func (c *stringInLogCheck) checkBytes(toCheck []byte, start int, end int) (bool, []byte) {
	toCheckBlock := toCheck[start:end]
	for _, s := range c.ExceptStrings {
		if bytes.Contains(toCheckBlock, []byte(s)) {
			return false, nil
		}
	}
	if found, line := c.checkBytesForString(toCheck, start, end, c.String); found {
		return true, line
	}
	if after, ok := strings.CutPrefix(c.String, botanistErrorPrefix); ok {
		wrapped := botanistErrorPrefix + constants.BotanistFailedMsg + ": " + after
		return c.checkBytesForString(toCheck, start, end, wrapped)
	}
	return false, nil
}

func (c *stringInLogCheck) checkBytesForString(toCheck []byte, start int, end int, s string) (bool, []byte) {
	toCheckBlock := toCheck[start:end]
	stringBytes := []byte(s)
	if len(c.ExceptBlocks) == 0 {
		if idx := bytes.Index(toCheckBlock, stringBytes); idx != -1 {
			line := getLine(toCheckBlock, idx)
			return true, line
		} else {
			return false, nil
		}
	}
	index := bytes.Index(toCheckBlock, stringBytes) + start
	for index >= start && index < end {
		foundString := true
		// beforeBlock and afterBlock intentionally slice across the full log
		// (toCheck[:index] and toCheck[nextStartIndex:]) rather than being restricted
		// to [start:end]. This is because EXCEPT blocks, such as
		// [===LSAN EXCEPT BLOCK START===] from lsan tests sometimes wind up spilling past
		// the log boundaries of the test for which they are intended.
		// Slicing the log specifically to the boundaries of the test runs caused false
		// positive failures in serial logs and swarming output (b/573272606) as well as
		// syslogs (b/575658434).
		beforeBlock := toCheck[:index]
		nextStartIndex := index + len(stringBytes)
		afterBlock := toCheck[nextStartIndex:]
		for _, block := range c.ExceptBlocks {
			closestStartIndex := bytes.LastIndex(beforeBlock, []byte(block.startString))
			if closestStartIndex < 0 {
				// There is no start string before this occurrence, so it must not be
				// included in this exceptBlock. Check the next exceptBlock.
				continue
			}
			closestEndIndex := bytes.LastIndex(beforeBlock, []byte(block.endString))
			if closestEndIndex < closestStartIndex {
				// There is no end string between the start string and the string to
				// check, so check if end string appears after. If so, then this
				// occurrence is included in this exceptBlock, so we can break and
				// check the next occurrence of the string.
				if bytes.Contains(afterBlock, []byte(block.endString)) {
					foundString = false
					break
				} else {
					// If the end string doesn't appear after the string to check,
					// it may have been truncated out of the log. In that case, we
					// assume every occurrence of the string to check between the
					// start string and the end of the block are included in the
					// exceptBlock.
					return false, nil
				}
			}
		}
		if foundString {
			line := getLine(toCheck, index)
			return true, line
		}
		index = bytes.Index(afterBlock, stringBytes)
		if index >= 0 {
			index += nextStartIndex
		}
	}
	return false, nil
}

func getLine(toCheck []byte, index int) []byte {
	if index >= len(toCheck) {
		return nil
	}
	lineStart := bytes.LastIndexByte(toCheck[:index], '\n') + 1
	lineEnd := bytes.IndexByte(toCheck[index:], '\n')
	if lineEnd == -1 {
		lineEnd = len(toCheck)
	} else {
		lineEnd += index
	}
	return toCheck[lineStart:lineEnd]
}

func (c *stringInLogCheck) Name() string {
	// TODO(https://fxbug.dev/42150891): With multi-device logs, the file names may be different than
	// the log type. Consider using the actual filename of the log.
	name := path.Join("string_in_log", string(c.Type), strings.ReplaceAll(c.String, " ", "_"))
	if c.testName != "" && !c.AddTag {
		name = name + "/" + c.testName
	}
	return name
}

func (c *stringInLogCheck) DebugText() string {
	var debugStr strings.Builder
	debugStr.WriteString(fmt.Sprintf("Found the string \"%s\" in ", c.String))
	if c.outputFile != "" && c.testName != "" {
		debugStr.WriteString(fmt.Sprintf("%s of test %s.", filepath.Base(c.outputFile), c.testName))
	} else {
		debugStr.WriteString(fmt.Sprintf("%s for task %s.", c.Type, c.swarmingResult.TaskId))
	}
	debugStr.WriteString("\nThat file should be accessible from the build result page or Sponge.\n")
	for _, s := range c.ExceptStrings {
		debugStr.WriteString(fmt.Sprintf("\nDid not find the exception string \"%s\"", s))
	}
	for _, block := range c.ExceptBlocks {
		debugStr.WriteString(fmt.Sprintf("\nDid not occur inside a block delimited by:\nSTART: %s\nEND: %s", block.startString, block.endString))
	}
	return debugStr.String()
}

func (c *stringInLogCheck) OutputFiles() []string {
	if c.outputFile == "" {
		return []string{}
	}
	return []string{c.outputFile}
}

func (c *stringInLogCheck) IsFlake() bool {
	return c.isFlake
}

func (c *stringInLogCheck) Tags() []build.TestTag {
	if c.AddTag && c.testName != "" {
		return []build.TestTag{{Key: "test_name", Value: c.testName}}
	}
	return nil
}

func (c *stringInLogCheck) IsInfraFailure() bool {
	return c.InfraFailure
}

func (c *stringInLogCheck) FailureReason() string {
	return c.line
}

func (c *stringInLogCheck) EmitSyntheticTestCase() bool {
	return c.emitSyntheticTestCase
}

func (c *stringInLogCheck) TestName() string {
	return c.testName
}

// NOTE: stringInLogCheck (FailureModeCheck) definitions have been migrated over to the
// check-configs.yaml file in this directory (https://fxbug.dev/535311264).
// The fields from stringInLogCheck are still supported in check-configs.yaml
// (see the "stringInLogCheckConfig" struct above for the mapping of field names to yaml values).
