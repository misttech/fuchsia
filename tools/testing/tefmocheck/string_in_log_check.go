// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package tefmocheck

import (
	"bytes"
	"fmt"
	"path"
	"path/filepath"
	"regexp"
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
	// Name override used instead of String when constructing Name() (required
	// when matching via regex; optional otherwise).
	CheckName string
	// String that will be searched for.
	String string
	// PrefilterPrefix is an optional literal string used as a pre-filter before
	// evaluating regex. Can only be set when regex is provided and must appear
	// in the regex pattern.
	PrefilterPrefix string
	// prefilterPrefixBytes is the byte representation of PrefilterPrefix, cached
	// to avoid repeated string-to-[]byte allocations during log scanning.
	prefilterPrefixBytes []byte
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

	regex          *regexp.Regexp
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
	// Custom name for the check; required when is_regex is true, otherwise
	// optional (defaults to String if omitted).
	Name    string `yaml:"name"`
	String  string `yaml:"string"`
	IsRegex bool   `yaml:"is_regex"`
	// PrefilterPrefix is an optional literal string used to accelerate regex
	// evaluation across large logs using fast byte searching (bytes.Index).
	// When set:
	// - is_regex must be true, and the prefix must appear in the regex pattern.
	// - Instead of evaluating the regex across the full log, the engine jumps
	//   directly to candidate prefix occurrences and evaluates the regex in a
	//   bounded window around the match (including a small lookback buffer of
	//   256 bytes to check preceding word/line boundaries, and a forward window
	//   of up to 8KB for multi-line patterns).
	PrefilterPrefix string   `yaml:"prefilter_prefix"`
	LogTypes        []string `yaml:"log_types"`
	OnlyOnStates    []string `yaml:"only_on_states"`
	ExceptStrings   []string `yaml:"except_strings"`
	ExceptBlocks    []struct {
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
	var re *regexp.Regexp
	if c.IsRegex {
		if c.Name == "" {
			return nil, fmt.Errorf("string_in_log_check 'name' field is required when 'is_regex' is true")
		}
		var err error
		re, err = regexp.Compile(c.String)
		if err != nil {
			return nil, fmt.Errorf("for regex %q: %w", c.String, err)
		}
	}
	if c.PrefilterPrefix != "" {
		if !c.IsRegex {
			return nil, fmt.Errorf("string_in_log_check 'prefilter_prefix' field can only be set when 'is_regex' is true")
		}
		if !strings.Contains(c.String, c.PrefilterPrefix) && !strings.Contains(c.String, regexp.QuoteMeta(c.PrefilterPrefix)) {
			return nil, fmt.Errorf("string_in_log_check 'prefilter_prefix' %q must appear in regex pattern %q", c.PrefilterPrefix, c.String)
		}
	}
	var exceptBlocks []*logBlock
	for _, b := range c.ExceptBlocks {
		exceptBlocks = append(exceptBlocks, &logBlock{
			startString: b.Start,
			endString:   b.End,
		})
	}
	var prefixBytes []byte
	if c.PrefilterPrefix != "" {
		prefixBytes = []byte(c.PrefilterPrefix)
	}
	var ret []FailureModeCheck
	for _, t := range c.LogTypes {
		lType, err := parseLogType(t)
		if err != nil {
			return nil, fmt.Errorf("for string %q: %w", c.String, err)
		}
		check := &stringInLogCheck{
			CheckName:             c.Name,
			String:                c.String,
			PrefilterPrefix:       c.PrefilterPrefix,
			prefilterPrefixBytes:  prefixBytes,
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
			regex:                 re,
		}
		ret = append(ret, check)
	}
	return ret, nil
}

func (c *stringInLogCheck) Check(to *TestingOutputs) bool {
	c.swarmingResult = to.SwarmingSummary.Results
	c.testName = ""
	c.outputFile = ""
	c.isFlake = false
	c.line = ""
	if !c.swarmingResult.Failure && c.swarmingResult.State == "COMPLETED" {
		if c.SkipPassedTask {
			return false
		}
		if c.SkipAllPassedTests {
			failedTests := make(map[string]struct{})
			if to.TestSummary != nil {
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

// Constants for regex prefilter prefix lookback and forward window.
//
// Searching entire multi-megabyte log files with complex multiline regexes is slow.
// To optimize, callers can specify a literal PrefilterPrefix (e.g. "panic: ") which
// is searched using fast bytes.Index.
//
// Once a prefix match is found:
//   - regexPrefixLookback defines how many bytes before the prefix are included in the
//     regex evaluation window. This allows regexes to verify preceding tokens or word
//     boundaries (e.g. `(?:^|[^-\w\n])panic:` or log timestamps/prefixes) without
//     triggering false positives on words like "non-panic:".
//   - regexPrefixWindowSize defines how many bytes after the prefix are included in the
//     window. This bounds the regex search while providing ample room (8KB) for multi-line
//     stack header matching (such as `goroutine \d+ [` within 8-10 lines), even when
//     intervening lines contain long panic messages, arguments, or URLs.
const (
	regexPrefixLookback   = 256
	regexPrefixWindowSize = 8 * 1024
)

func (c *stringInLogCheck) checkBytes(toCheck []byte, start int, end int) (bool, []byte) {
	toCheckBlock := toCheck[start:end]
	for _, s := range c.ExceptStrings {
		if bytes.Contains(toCheckBlock, []byte(s)) {
			return false, nil
		}
	}
	if c.regex != nil {
		return c.checkBytesMatch(toCheck, start, end, c.findFirstRegex)
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

func (c *stringInLogCheck) findFirstRegex(b []byte) (int, int) {
	if c.PrefilterPrefix == "" {
		loc := c.regex.FindIndex(b)
		if loc == nil {
			return -1, -1
		}
		return loc[0], loc[1]
	}
	prefixBytes := c.prefilterPrefixBytes
	if len(prefixBytes) == 0 {
		prefixBytes = []byte(c.PrefilterPrefix)
	}
	for offset := 0; offset < len(b); {
		idx := bytes.Index(b[offset:], prefixBytes)
		if idx == -1 {
			return -1, -1
		}
		pos := offset + idx
		winStart := max(0, pos-regexPrefixLookback)
		winEnd := min(len(b), pos+len(prefixBytes)+regexPrefixWindowSize)
		window := b[winStart:winEnd]

		locs := c.regex.FindAllIndex(window, -1)
		for _, loc := range locs {
			relPos := pos - winStart
			if loc[0] <= relPos && loc[1] >= relPos+len(prefixBytes) {
				return winStart + loc[0], winStart + loc[1]
			}
		}
		offset = pos + 1
	}
	return -1, -1
}

func (c *stringInLogCheck) checkBytesForString(toCheck []byte, start int, end int, s string) (bool, []byte) {
	stringBytes := []byte(s)
	return c.checkBytesMatch(toCheck, start, end, func(b []byte) (int, int) {
		idx := bytes.Index(b, stringBytes)
		if idx == -1 {
			return -1, -1
		}
		return idx, idx + len(stringBytes)
	})
}

func (c *stringInLogCheck) checkBytesMatch(toCheck []byte, start int, end int, findFirst func([]byte) (int, int)) (bool, []byte) {
	toCheckBlock := toCheck[start:end]
	relStart, relEnd := findFirst(toCheckBlock)
	if relStart == -1 {
		return false, nil
	}
	if len(c.ExceptBlocks) == 0 {
		line := getLine(toCheck, start+relStart)
		return true, line
	}
	index := start + relStart
	nextStartIndex := start + relEnd
	if nextStartIndex <= index {
		nextStartIndex = index + 1
	}
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
		relStart, relEnd := findFirst(afterBlock)
		if relStart < 0 {
			break
		}
		index = nextStartIndex + relStart
		nextStartIndex = nextStartIndex + relEnd
		if nextStartIndex <= index {
			nextStartIndex = index + 1
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
	checkName := c.String
	if c.CheckName != "" {
		checkName = c.CheckName
	}
	name := path.Join("string_in_log", string(c.Type), strings.ReplaceAll(checkName, " ", "_"))
	if c.testName != "" && !c.AddTag {
		name = name + "/" + c.testName
	}
	return name
}

func (c *stringInLogCheck) DebugText() string {
	var debugStr strings.Builder
	if c.regex != nil {
		debugStr.WriteString(fmt.Sprintf("Found match for pattern %q in ", c.regex.String()))
	} else {
		debugStr.WriteString(fmt.Sprintf("Found the string \"%s\" in ", c.String))
	}
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
