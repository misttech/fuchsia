// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package gn2gone

import (
	"context"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	"go.fuchsia.dev/fuchsia/build/tools/bazel2gn"
)

// findGN locates gn: from $GN2GONE_TEST_GN (e.g. when running `go test`
// directly), or as test data staged next to the test binary by
// //build/tools/gn2gone:gn2gone_test_data.
func findGN(t *testing.T) string {
	t.Helper()
	if p := os.Getenv("GN2GONE_TEST_GN"); p != "" {
		return p
	}
	hostDir := map[string]string{"arm64": "host_arm64", "amd64": "host_x64"}[runtime.GOARCH]
	p, err := filepath.Abs(filepath.Join(hostDir, "test_data", "gn2gone", "gn"))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(p); err != nil {
		t.Fatalf("can't find gn test data (%v); set $GN2GONE_TEST_GN", err)
	}
	return p
}

func writeTree(t *testing.T, root string, files map[string]string) {
	t.Helper()
	for p, content := range files {
		full := filepath.Join(root, p)
		if err := os.MkdirAll(filepath.Dir(full), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(full, []byte(content), 0o644); err != nil {
			t.Fatal(err)
		}
	}
}

const sentinel = bazel2gn.SentinelComment + "\n\nimport(\"//build/tools/bazel2gn/bazel_migration.gni\")\n\n"

func fixtureTree() map[string]string {
	return map[string]string{
		".gn": `buildconfig = "//build/BUILDCONFIG.gn"
default_args = {
  dir_pw = "//"
}
`,
		// READY, DELETE_BUILD_GN: only an assert and a test group above, and
		// the test is wired through //tools:host_tests.
		"lib/ready/BUILD.gn": `assert(is_host, "These targets are only compiled on host.")

group("tests") {
  testonly = true
  deps = [ ":ready_test($host_toolchain)" ]
}

` + sentinel + `go_library("ready") {
  sources = [ "ready.go" ]
}

go_test("ready_test") {
  library = ":ready"
}
`,
		"lib/ready/BUILD.bazel": `go_library(name = "ready")

go_test(name = "ready_test")
`,
		// No sentinel: parent test group.
		"lib/BUILD.gn": `group("tests") {
  testonly = true
  deps = [
    "other:tests",
    "ready:tests",
    "unwired:tests",
  ]
}
`,
		// BLOCKED through a template subtarget.
		"lib/blocked/BUILD.gn": sentinel + `rustc_library("blocked") {
  sources = [ "lib.rs" ]
}
`,
		"app/BUILD.gn": `# A comment mentioning "//lib/ready:ready" must not count.
executable("app") {
  deps = [ "//lib/blocked:blocked_rust" ]
}
`,
		// BLOCKED_TESTS_UNWIRED: no bazel_test_suite reaches the test.
		"lib/unwired/BUILD.gn": `group("tests") {
  testonly = true
  deps = [ ":unwired_test" ]
}

` + sentinel + `go_library("unwired") {
}

go_test("unwired_test") {
}
`,
		"tools/BUILD.gn": `bazel_test_suite("bazel_tests") {
  target_tests = [
    "//lib/dual:d_tests",
    "//lib/opaque:o_test",
  ]
  host_tests = [
    "//lib/dt/tests:dt_test",
    "//lib/manualt:all",
    "//lib/tagged:all",
    "//lib/varsuite:suite",
    "//tools:host_tests",
  ]
}
`,
		"tools/BUILD.bazel": `test_suite(
    name = "host_tests",
    tests = ["//lib/ready:ready_test"],
)
`,
		// READY, STRIP_BELOW_SENTINEL: a live host tool shim above.
		"lib/strip/BUILD.gn": `bazel_host_tool("strip") {
  bazel_target = ":strip_bin"
}

` + sentinel + `go_binary("strip_bin") {
}
`,
		"bundles/BUILD.gn": `group("tools") {
  deps = [ "//lib/strip" ]
}
`,
		// EMPTY_SENTINEL.
		"lib/empty/BUILD.gn": `source_set("empty") {
}

` + sentinel,
		// BLOCKED through a variable from //.gn default_args.
		"lib/viavar/BUILD.gn": sentinel + `source_set("viavar") {
}
`,
		"third_party/pw/BUILD.gn": `group("x") {
  public_deps = [ "$dir_pw/lib/viavar" ]
}
`,
		// BLOCKED by a test group that bazel2gn generated (from a Bazel
		// test_suite) below another directory's sentinel. That entry can't be
		// edited, so //lib/suite must be unsynced first.
		"lib/suite/BUILD.gn": sentinel + `group("tests") {
  testonly = true
  deps = [ "//lib/chained:chained_test" ]
}
`,
		"lib/chained/BUILD.gn": sentinel + `go_test("chained_test") {
}
`,
		// BLOCKED_TESTS_UNWIRED: a test_suite whose `tests` is a variable
		// doesn't count as "every test in the package".
		"lib/varsuite/BUILD.gn": `group("tests") {
  testonly = true
  deps = [ ":vs_test" ]
}

` + sentinel + `go_test("vs_test") {
}
`,
		"lib/varsuite/BUILD.bazel": `TESTS = [":other_test"]

test_suite(
    name = "suite",
    tests = TESTS,
)

go_test(name = "vs_test")
`,
		// BLOCKED through a relative label in a .gni, used by the BUILD.gn
		// next to it (like //src/developer/ffx/lib/ffx_libraries_list.gni).
		"lib/ffx/libs.gni": `all_libs = [ "termio" ]
`,
		"lib/ffx/BUILD.gn": `import("libs.gni")
group("lib") {
  deps = []
  foreach(lib, all_libs) {
    deps += [ lib ]
  }
}
`,
		"lib/ffx/termio/BUILD.gn": sentinel + `rustc_library("termio") {
}
`,
		// BLOCKED_TESTS_UNWIRED: Bazel's implicit "every test in the
		// package" skips tests tagged "manual".
		"lib/manualt/BUILD.gn": `group("tests") {
  testonly = true
  deps = [ ":m_test" ]
}

` + sentinel + `go_test("m_test") {
}
`,
		"lib/manualt/BUILD.bazel": `test_suite(name = "all")

go_test(
    name = "m_test",
    tags = ["manual"],
)
`,
		// BLOCKED_TESTS_UNWIRED: a test_suite with a `tags` filter selects a
		// subset, not every test.
		"lib/tagged/BUILD.gn": `group("tests") {
  testonly = true
  deps = [ ":t_test" ]
}

` + sentinel + `go_test("t_test") {
}
`,
		"lib/tagged/BUILD.bazel": `test_suite(
    name = "all",
    tags = ["-flaky"],
)

go_test(name = "t_test")
`,
		// READY, but STRIP_BELOW_SENTINEL: the test group also pulls in tests
		// from a variable, which deleting BUILD.gn would drop from CQ.
		"lib/opaque/BUILD.gn": `import("//build/extra_tests.gni")

group("tests") {
  testonly = true
  deps = [ ":o_test" ] + extra_tests
}

` + sentinel + `go_test("o_test") {
}
`,
		// BLOCKED_DUAL_TESTED: //lib/dt/tests has no GN consumers, but GN
		// runs its test of //lib/dt, which GN still builds (//app2 uses it).
		"lib/dt/BUILD.gn": sentinel + `go_library("dt") {
}
`,
		"app2/BUILD.gn": `executable("app2") {
  deps = [ "//lib/dt" ]
}
`,
		"lib/dt/tests/BUILD.gn": `group("tests") {
  testonly = true
  deps = [ ":dt_test" ]
}

` + sentinel + `go_test("dt_test") {
  deps = [ "//lib/dt" ]
}
`,
		// BLOCKED_TESTS_UNWIRED: the same test runs on host and on device in
		// GN, but Bazel only runs the device package (target_tests). The host
		// copy must not count as wired.
		"lib/dual/BUILD.gn": `group("tests") {
  testonly = true
  deps = [
    ":d_test($host_toolchain)",
    ":d_tests",
  ]
}

` + sentinel + `rustc_test("d_test") {
}

fuchsia_unittest_package("d_tests") {
  deps = [ ":d_test" ]
}
`,
		"lib/dual/BUILD.bazel": `rustc_test(name = "d_test")

fx_unittest_package(
    name = "d_tests",
    unit_tests = [":d_test"],
)

wrap_host_rust_test(
    name = "d_host_test",
    binary_name = "d_test",
)
`,
		// BLOCKED_DUAL_TESTED without a GN test group: the synced test of
		// //lib/media (still built by GN) isn't run by GN yet because its GN
		// package is still skipped, but its GN copy must stay.
		"lib/media/BUILD.gn": sentinel + `source_set("media") {
}
`,
		"app3/BUILD.gn": `executable("app3") {
  deps = [ "//lib/media" ]
}
`,
		"lib/media/unit_tests/BUILD.gn": sentinel + `executable("m_unit_test_bin") {
  testonly = true
  deps = [ "//lib/media" ]
}
`,
		// BLOCKED: a same-directory relative label (".:target").
		"lib/samedir/BUILD.gn": `group("samedir") {
  deps = [ ".:samedir_bin" ]
}

` + sentinel + `executable("samedir_bin") {
}
`,
		// Dynamic labels with no usable static prefix are reported.
		"build/dynamic.gni": `template("dyn") {
  group(target_name) {
    deps = [ "//$invoker.dir" ]
  }
}
`,
	}
}

func load(t *testing.T, root string) *Index {
	t.Helper()
	idx, stats, err := Load(context.Background(), LoadOptions{Root: root, GNBin: findGN(t), BatchSize: 3, Workers: 2})
	if err != nil {
		t.Fatalf("Load() error: %v", err)
	}
	if len(stats.ParseErrors) > 0 {
		t.Fatalf("Load() parse errors: %v", stats.ParseErrors)
	}
	return idx
}

func TestScan(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, fixtureTree())
	idx := load(t, root)

	type result struct {
		State     State
		Mode      Mode
		Unwired   []string
		UnderTest []string
		Blockers  []string
	}
	got := map[string]result{}
	for _, r := range idx.ScanAll() {
		res := result{State: r.State, Mode: r.Mode, Unwired: r.UnwiredTests, UnderTest: r.UnderTest}
		for _, b := range r.Blockers {
			res.Blockers = append(res.Blockers, b.String())
		}
		got[r.Dir] = res
	}
	want := map[string]result{
		"lib/ready":   {State: StateReady, Mode: ModeDeleteBuildGN},
		"lib/strip":   {State: StateReady, Mode: ModeStripBelowSentinel},
		"lib/empty":   {State: StateEmptySentinel},
		"lib/unwired": {State: StateBlockedTestsUnwired, Unwired: []string{"unwired_test"}},
		"lib/blocked": {State: StateBlocked, Blockers: []string{
			`app/BUILD.gn:3 executable("app") deps "//lib/blocked:blocked_rust" -> :blocked [subtarget]`,
		}},
		"lib/viavar": {State: StateBlocked, Blockers: []string{
			`third_party/pw/BUILD.gn:2 group("x") public_deps "$dir_pw/lib/viavar" -> :viavar [var_substituted]`,
		}},
		"lib/chained": {State: StateBlocked, Blockers: []string{
			`lib/suite/BUILD.gn:7 group("tests") deps "//lib/chained:chained_test" -> :chained_test (synced in //lib/suite)`,
		}},
		"lib/suite":            {State: StateReady, Mode: ModeDeleteBuildGN},
		"lib/varsuite":         {State: StateBlockedTestsUnwired, Unwired: []string{"vs_test"}},
		"lib/opaque":           {State: StateReady, Mode: ModeStripBelowSentinel},
		"lib/dt/tests":         {State: StateBlockedDualTested, UnderTest: []string{"//lib/dt:dt"}},
		"lib/dual":             {State: StateBlockedTestsUnwired, Unwired: []string{"d_test (host)"}},
		"lib/media/unit_tests": {State: StateBlockedDualTested, UnderTest: []string{"//lib/media:media"}},
		"lib/media": {State: StateBlocked, Blockers: []string{
			`app3/BUILD.gn:2 executable("app3") deps "//lib/media" -> :media`,
			`lib/media/unit_tests/BUILD.gn:7 executable("m_unit_test_bin") deps "//lib/media" -> :media (synced in //lib/media/unit_tests)`,
		}},
		"lib/samedir": {State: StateBlocked, Blockers: []string{
			`lib/samedir/BUILD.gn:2 group("samedir") deps ".:samedir_bin" -> :samedir_bin`,
		}},
		"lib/dt": {State: StateBlocked, Blockers: []string{
			`app2/BUILD.gn:2 executable("app2") deps "//lib/dt" -> :dt`,
			`lib/dt/tests/BUILD.gn:11 go_test("dt_test") deps "//lib/dt" -> :dt (synced in //lib/dt/tests)`,
		}},
		"lib/manualt": {State: StateBlockedTestsUnwired, Unwired: []string{"m_test"}},
		"lib/tagged":  {State: StateBlockedTestsUnwired, Unwired: []string{"t_test"}},
		"lib/ffx/termio": {State: StateBlocked, Blockers: []string{
			`lib/ffx/libs.gni:1 all_libs "termio" -> (any target)`,
		}},
	}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("ScanAll() diff (-want +got):\n%s", diff)
	}

	var unattributed []string
	for _, l := range idx.Resolver.Unattributed() {
		unattributed = append(unattributed, l.File+" "+l.Value)
	}
	if diff := cmp.Diff([]string{"build/dynamic.gni //$invoker.dir"}, unattributed); diff != "" {
		t.Errorf("Unattributed() diff (-want +got):\n%s", diff)
	}
}

func TestDynamicLabels(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, map[string]string{
		"lib/ready/BUILD.gn": sentinel + `source_set("ready") {
}
`,
		"lib/other/BUILD.gn": sentinel + `source_set("other") {
}
`,
		"app/BUILD.gn": `group("app") {
  deps = [
    # The dynamic part continues the last path component.
    "//lib/rea$x",
    # Only the target name is dynamic.
    "//lib/other:$y",
  ]
  data_deps = [ "//lib/re" + z ]
}
`,
		// Relative labels with a dynamic part.
		"lib/BUILD.gn": `group("g") {
  deps = [
    "rea$x:tests",
    "oth" + y,
  ]
}

# A list of directories turned into "$t:bin" labels: the bare entry may
# name any target in the directory, not just the default one.
foreach(t, [ "viafor" ]) {
  print("$t:bin")
}
`,
		"lib/viafor/BUILD.gn": `group("viafor") {
}

` + sentinel + `executable("bin") {
}
`,
	})
	idx := load(t, root)
	got := map[string][]string{}
	for _, r := range idx.ScanAll() {
		for _, b := range r.Blockers {
			got[r.Dir] = append(got[r.Dir], b.String())
		}
	}
	want := map[string][]string{
		"lib/ready": {
			`app/BUILD.gn:4 group("app") deps "//lib/rea$x" -> (any target) [dynamic_prefix]`,
			`app/BUILD.gn:8 group("app") data_deps "//lib/re" -> (any target) [dynamic_prefix]`,
			`lib/BUILD.gn:3 group("g") deps "rea$x:tests" -> (any target) [dynamic_prefix]`,
		},
		"lib/other": {
			`app/BUILD.gn:6 group("app") deps "//lib/other:$y" -> (any target) [dynamic_prefix]`,
			`lib/BUILD.gn:4 group("g") deps "oth" -> (any target) [dynamic_prefix]`,
		},
		"lib/viafor": {
			`lib/BUILD.gn:10 "viafor" -> (any target)`,
		},
	}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("blockers diff (-want +got):\n%s", diff)
	}
	var unattributed []string
	for _, l := range idx.Resolver.Unattributed() {
		unattributed = append(unattributed, l.Value)
	}
	if diff := cmp.Diff([]string(nil), unattributed); diff != "" {
		t.Errorf("Unattributed() diff (-want +got):\n%s", diff)
	}
}

// Like //sdk/lib/c/libc.gni's `".$suffix"`: a trailing "." is the directory
// itself, so the string must not match a sibling sharing the name as a prefix
// (//sdk/lib/component), only directories under it.
func TestTrailingDot(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, map[string]string{
		"sdk/c/libc.gni": `subtarget = ".$suffix"
`,
		"sdk/c/sub/BUILD.gn": sentinel + `source_set("sub") {
}
`,
		"sdk/component/BUILD.gn": sentinel + `source_set("component") {
}
`,
	})
	idx := load(t, root)
	got := map[string]State{}
	for _, r := range idx.ScanAll() {
		got[r.Dir] = r.State
	}
	want := map[string]State{"sdk/c/sub": StateBlocked, "sdk/component": StateReady}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("states diff (-want +got):\n%s", diff)
	}
}

func TestQuery(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, fixtureTree())
	idx := load(t, root)

	refs, err := idx.Query("//lib/ready:ready_test")
	if err != nil {
		t.Fatalf("Query() error: %v", err)
	}
	var got []string
	for _, r := range refs {
		got = append(got, r.String())
	}
	want := []string{`lib/ready/BUILD.gn:5 group("tests") deps ":ready_test($host_toolchain)" -> :ready_test`}
	if diff := cmp.Diff(want, got); diff != "" {
		t.Errorf("Query() diff (-want +got):\n%s", diff)
	}

	if _, err := idx.Query("//app"); err == nil {
		t.Errorf("Query(//app) succeeded, want error for a directory without the sentinel")
	}
	if _, err := idx.Query("//lib/ready:nope"); err == nil {
		t.Errorf("Query(//lib/ready:nope) succeeded, want error for an unknown target")
	}
}

func TestUnsync(t *testing.T) {
	root := t.TempDir()
	writeTree(t, root, fixtureTree())
	idx := load(t, root)

	plan := idx.PlanUnsync(idx.ScanAll())
	if diff := cmp.Diff([]string{"lib/opaque", "lib/ready", "lib/strip", "lib/suite"}, plan.Dirs); diff != "" {
		t.Errorf("plan.Dirs diff (-want +got):\n%s", diff)
	}
	if err := plan.Apply(context.Background(), root, findGN(t)); err != nil {
		t.Fatalf("Apply() error: %v", err)
	}

	if _, err := os.Stat(filepath.Join(root, "lib/ready/BUILD.gn")); !os.IsNotExist(err) {
		t.Errorf("lib/ready/BUILD.gn still exists, want deleted (err=%v)", err)
	}
	read := func(p string) string {
		b, err := os.ReadFile(filepath.Join(root, p))
		if err != nil {
			t.Fatal(err)
		}
		return string(b)
	}
	if diff := cmp.Diff(`group("tests") {
  testonly = true
  deps = [
    "other:tests",
    "unwired:tests",
  ]
}
`, read("lib/BUILD.gn")); diff != "" {
		t.Errorf("lib/BUILD.gn diff (-want +got):\n%s", diff)
	}
	if diff := cmp.Diff(`bazel_host_tool("strip") {
  bazel_target = ":strip_bin"
}
`, read("lib/strip/BUILD.gn")); diff != "" {
		t.Errorf("lib/strip/BUILD.gn diff (-want +got):\n%s", diff)
	}

	// Unsyncing //lib/suite unblocks //lib/chained on the next run.
	// Unsynced directories are no longer sentinel directories, so they're
	// not planned again.
	idx = load(t, root)
	plan = idx.PlanUnsync(idx.ScanAll())
	if diff := cmp.Diff([]string{"lib/chained"}, plan.Dirs); diff != "" {
		t.Errorf("second PlanUnsync() dirs diff (-want +got):\n%s", diff)
	}
}

func TestRemoveEntry(t *testing.T) {
	for _, tc := range []struct {
		line, literal, want string
		drop, wantErr       bool
	}{
		{line: `    ":foo",`, literal: ":foo", drop: true},
		{line: `    ":foo"  # comment`, literal: ":foo", drop: true},
		{line: `  deps = [ ":foo($host_toolchain)" ]`, literal: ":foo($host_toolchain)", want: `  deps = [ ]`},
		{line: `  deps = [ ":foo", ":bar" ]`, literal: ":foo", wantErr: true},
	} {
		got, drop, err := removeEntry(tc.line, tc.literal)
		if (err != nil) != tc.wantErr {
			t.Errorf("removeEntry(%q, %q) error = %v, wantErr %v", tc.line, tc.literal, err, tc.wantErr)
			continue
		}
		if drop != tc.drop || strings.TrimSpace(got) != strings.TrimSpace(strings.ReplaceAll(tc.want, "[ ]", "[]")) {
			t.Errorf("removeEntry(%q, %q) = (%q, %v), want (%q, %v)", tc.line, tc.literal, got, drop, tc.want, tc.drop)
		}
	}
}
