// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package resultdb

import (
	"fmt"
	"html"
	"log"
	"slices"
	"strings"
	"unicode/utf8"

	resultpb "go.chromium.org/luci/resultdb/proto/v1"
	sinkpb "go.chromium.org/luci/resultdb/sink/proto/v1"
	"google.golang.org/protobuf/types/known/timestamppb"

	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

// FixtureCaseName is the ResultDB-reserved case name used to report the outcome
// of the setup and teardown shared by a group of test cases under the same
// fine_name.
// //third_party/luci-go/resultdb/proto/v1/common.proto:214
const FixtureCaseName = "*fixture"

// MaxSummaryHTMLLength is the maximum size of a test result's summary_html.
// //third_party/luci-go/resultdb/proto/v1/test_result.proto:203
const MaxSummaryHTMLLength = 4096

// classifyFixture determines whether to synthesize a *fixture result for a test
// suite execution and what its StatusV2 should be.
//
// A *fixture result is only synthesized when setup or teardown fails; a clean
// setup and teardown does not produce one:
//   - FAIL (intrinsic fixture failure) -> FAILED
//   - INFRA_FAIL (environmental or infrastructure failure) -> EXECUTION_ERRORED
func classifyFixture(t *runtests.TestDetails) (status resultpb.TestResult_Status, reason string, ok bool) {
	if t.Status == runtests.TestFailure {
		if t.SetupSucceeded != nil && !*t.SetupSucceeded {
			return resultpb.TestResult_FAILED, "Test setup failed before test cases could run.", true
		}
		if t.TeardownSucceeded != nil && !*t.TeardownSucceeded {
			return resultpb.TestResult_FAILED, "Test teardown failed after test cases executed.", true
		}
	} else if t.Status == runtests.TestInfraFailure {
		if t.SetupSucceeded != nil && !*t.SetupSucceeded {
			return resultpb.TestResult_EXECUTION_ERRORED, "Test setup failed due to an infrastructure or environmental error.", true
		}
		if t.TeardownSucceeded != nil && !*t.TeardownSucceeded {
			return resultpb.TestResult_EXECUTION_ERRORED, "Test teardown failed due to an infrastructure or environmental error.", true
		}
	}
	return resultpb.TestResult_STATUS_UNSPECIFIED, "", false
}

// hasFixture reports whether results already contain a *fixture under
// fineName, e.g. the top-level result of a test without test cases, which
// testDetailsToResultSink reports with the *fixture case name.
func hasFixture(results []*sinkpb.TestResult, fineName string) bool {
	for _, r := range results {
		id := r.GetTestIdStructured()
		if id.GetFineName() == fineName && slices.Equal(id.GetCaseNameComponents(), []string{FixtureCaseName}) {
			return true
		}
	}
	return false
}

// appendFixtureResult appends the *fixture result synthesized for test to
// results, or its test ID to skipped if the ID exceeds MaxTestIDLength.
// results must already hold all of test's other results, so that the *fixture
// shares their fine_name and is not reported twice under the same fine_name.
func appendFixtureResult(results []*sinkpb.TestResult, skipped []string, test *runtests.TestDetails) ([]*sinkpb.TestResult, []string) {
	fixture, testID := synthesizeFixtureResult(results, test)
	switch {
	case fixture != nil:
		if !hasFixture(results, fixture.TestIdStructured.GetFineName()) {
			results = append(results, fixture)
		}
	case testID != "":
		skipped = append(skipped, testID)
	}
	return results, skipped
}

// fixtureFineName returns the fine_name of the *fixture result of a test under
// the given scheme, given the test's other results.
//
// ResultDB defines *fixture as the setup and teardown common to the tests in
// the same fine_name, so if all of results share a non-empty fine_name, the
// *fixture is reported under it. Otherwise, e.g. when the test cases span
// multiple fine_names, it falls back to a fine_name that only depends on the
// scheme. Returns "" for schemes without a fine_name ("single" and "flat").
func fixtureFineName(scheme string, results []*sinkpb.TestResult) string {
	switch scheme {
	case "single", "flat":
		return ""
	}

	shared, found := "", false
	for _, r := range results {
		id := r.GetTestIdStructured()
		if id == nil {
			// The top-level result of a test with test cases has no
			// structured test ID.
			continue
		}
		if !found {
			shared, found = id.GetFineName(), true
		} else if id.GetFineName() != shared {
			shared = ""
			break
		}
	}
	if shared != "" {
		return shared
	}

	if scheme == "rust" {
		return rustCrateRootModule
	}
	return "test"
}

// synthesizeFixtureResult builds the <fine_name>/*fixture TestResult for a
// test suite execution when setup or teardown failed, or nil if no fixture
// should be reported. If the fixture's test ID exceeds MaxTestIDLength, it
// returns nil and the test ID so that the caller can report it as skipped.
//
// SetupSucceeded and TeardownSucceeded describe the suite execution as a
// whole, so at most one *fixture is synthesized per execution. It is reported
// under the fine_name shared by the test's other results if there is one, so
// it is grouped with the tests whose setup and teardown it reports, and under
// a scheme-dependent fine_name otherwise (see fixtureFineName). Callers must
// not report it if another result already reports a *fixture under the same
// fine_name (see hasFixture).
func synthesizeFixtureResult(results []*sinkpb.TestResult, testDetail *runtests.TestDetails) (*sinkpb.TestResult, string) {
	if testDetail.IsTestingFailureMode {
		return nil, ""
	}

	status, reason, ok := classifyFixture(testDetail)
	if !ok {
		return nil, ""
	}

	fineName := fixtureFineName(DetermineScheme(testDetail), results)
	testID := fmt.Sprintf("%s/%s:%s", testDetail.Name, fineName, FixtureCaseName)
	if len(testID) > MaxTestIDLength {
		log.Printf("[ERROR] Skip uploading *fixture to ResultDB due to test_id exceeding %d bytes max limit: %q", MaxTestIDLength, testID)
		return nil, testID
	}

	r := &sinkpb.TestResult{
		TestIdStructured: &sinkpb.TestIdentifier{
			FineName:           fineName,
			CaseNameComponents: []string{FixtureCaseName},
		},
		TestId:    testID,
		StatusV2:  status,
		StartTime: timestamppb.New(testDetail.StartTime),
	}

	switch status {
	case resultpb.TestResult_FAILED:
		r.FailureReason = fixtureFailureReason(testDetail, reason)
	case resultpb.TestResult_EXECUTION_ERRORED:
		r.SummaryHtml = fixtureSummaryHTML(testDetail)
	}
	return r, ""
}

// fixtureFailureReason describes why the suite's setup or teardown failed.
// If testDetail.FailureReason contains non-empty error messages, those are used
// directly so LUCI Analysis clusters on the actual setup/teardown error;
// otherwise the synthesized reason is used as a fallback.
func fixtureFailureReason(testDetail *runtests.TestDetails, reason string) *resultpb.FailureReason {
	kind := resultpb.FailureReason_ORDINARY
	if testDetail.SetupSucceeded != nil && !*testDetail.SetupSucceeded {
		kind = resultpb.FailureReason_CRASH
	}
	if testDetail.FailureReason != nil && len(testDetail.FailureReason.Errors) > 0 {
		res := toResultDBFailureReason(testDetail.FailureReason, kind)
		if len(res.Errors) > 0 {
			return res
		}
	}
	fr := &runtests.FailureReason{
		Errors: []*runtests.FailureReasonError{{Message: reason}},
	}
	return toResultDBFailureReason(fr, kind)
}

// fixtureSummaryHTML renders the exit code and error messages of an
// environmental fixture failure (StatusV2 = EXECUTION_ERRORED).
func fixtureSummaryHTML(testDetail *runtests.TestDetails) string {
	var b strings.Builder
	if testDetail.ExitCode != nil {
		fmt.Fprintf(&b, "<p>Exit code: %d</p>", *testDetail.ExitCode)
	}
	if testDetail.FailureReason != nil {
		const preWrapperLen = len("<pre></pre>")
		for _, e := range testDetail.FailureReason.Errors {
			if e == nil || e.Message == "" {
				continue
			}
			remaining := MaxSummaryHTMLLength - b.Len() - preWrapperLen
			if remaining <= 3 {
				break
			}
			escaped := truncateEscapedHTML(e.Message, remaining)
			if escaped == "..." {
				// Nothing of the message fits.
				break
			}
			b.WriteString("<pre>")
			b.WriteString(escaped)
			b.WriteString("</pre>")
		}
	}
	return b.String()
}

// truncateEscapedHTML HTML-escapes str, truncating str (with a trailing "...")
// if needed so the returned escaped string is at most max bytes. It always
// truncates str rather than the escaped string, and only at rune boundaries,
// so it never splits an HTML entity or a multibyte UTF-8 character.
func truncateEscapedHTML(str string, max int) string {
	origin := str
	target := max - 3 // reserve room for "..."
	if target < 0 {
		target = 0
	}
	// Escaping never shrinks a string, so a prefix longer than target bytes can
	// never fit. Clip to a rune boundary up front to bound the loop below.
	if len(origin) > target {
		cut := target
		for cut > 0 && !utf8.RuneStart(origin[cut]) {
			cut--
		}
		origin = origin[:cut]
	}
	for {
		escaped := html.EscapeString(origin)
		if len(escaped) <= target {
			if len(origin) < len(str) {
				return escaped + "..."
			}
			return escaped
		}
		// Drop the last rune and try again.
		_, size := utf8.DecodeLastRuneInString(origin)
		if size == 0 {
			return "..."
		}
		origin = origin[:len(origin)-size]
	}
}
