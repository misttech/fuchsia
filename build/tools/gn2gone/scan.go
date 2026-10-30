// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package gn2gone

import (
	"fmt"
	"path"
	"sort"
	"strings"
)

// State is the outcome for one sentinel directory.
type State string

const (
	// StateReady: nothing in GN references the synced targets, except test
	// groups whose tests are also run through a `bazel_test_suite()`.
	StateReady State = "READY"
	// StateEmptySentinel: nothing is synced below the sentinel (e.g. every
	// rule in BUILD.bazel is `# @bazel2gn:skip`), so there's nothing to
	// unsync.
	StateEmptySentinel State = "EMPTY_SENTINEL"
	// StateBlockedDualTested: no GN consumers, but synced tests that GN test
	// groups run test code in another directory that GN still builds. Per the
	// dual-run decision (//docs/development/build/bazel_concepts/tests.md,
	// "Remove the GN copy with the library"), the GN test copies go away
	// together with the GN copy of the code under test, not before.
	StateBlockedDualTested State = "BLOCKED_DUAL_TESTED"
	// StateBlockedTestsUnwired: no GN consumers, but some synced tests that
	// GN test groups run are not reachable from any `bazel_test_suite()`, so
	// unsyncing would silently drop them from CQ.
	StateBlockedTestsUnwired State = "BLOCKED_TESTS_UNWIRED"
	// StateBlocked: GN still references synced targets.
	StateBlocked State = "BLOCKED"
)

// Mode is the edit that unsyncs a READY directory.
type Mode string

const (
	// ModeDeleteBuildGN: nothing but imports, asserts, and test groups would
	// remain above the sentinel, so BUILD.gn is deleted.
	ModeDeleteBuildGN Mode = "DELETE_BUILD_GN"
	// ModeStripBelowSentinel: hand-written targets remain above the
	// sentinel, so only the sentinel and what's below it are removed.
	ModeStripBelowSentinel Mode = "STRIP_BELOW_SENTINEL"
)

// Reference is a GN reference reported to users.
type Reference struct {
	File string `json:"file"`
	Line int    `json:"line"`
	// Column is the 1-based column of the string literal.
	Column int `json:"column"`
	// Enclosing is `<template>("<name>")` of the enclosing target, or "".
	Enclosing string `json:"enclosing,omitempty"`
	Attr      string `json:"attr,omitempty"`
	Literal   string `json:"literal"`
	// Target is the matched synced target, or "" if the reference may point
	// at any target in the directory.
	Target string `json:"target,omitempty"`
	How    How    `json:"how"`
	Tier   Tier   `json:"tier"`
	// FromSentinelDir is set when the reference is itself below the sentinel
	// of another directory, i.e. that directory must be unsynced first.
	FromSentinelDir string `json:"from_sentinel_dir,omitempty"`
}

func (r Reference) String() string {
	var sb strings.Builder
	fmt.Fprintf(&sb, "%s:%d", r.File, r.Line)
	if r.Enclosing != "" {
		fmt.Fprintf(&sb, " %s", r.Enclosing)
	}
	if r.Attr != "" {
		fmt.Fprintf(&sb, " %s", r.Attr)
	}
	fmt.Fprintf(&sb, " %q", r.Literal)
	if r.Target != "" {
		fmt.Fprintf(&sb, " -> :%s", r.Target)
	} else {
		sb.WriteString(" -> (any target)")
	}
	if r.How != HowAbsolute && r.How != HowLocal && r.How != HowRelative {
		fmt.Fprintf(&sb, " [%s]", r.How)
	} else if r.Tier == TierPrefix {
		sb.WriteString(" [subtarget]")
	}
	if r.FromSentinelDir != "" {
		fmt.Fprintf(&sb, " (synced in //%s)", r.FromSentinelDir)
	}
	return sb.String()
}

// DirReport is the result for one sentinel directory.
type DirReport struct {
	Dir   string `json:"dir"`
	State State  `json:"state"`
	// Mode is set for READY directories.
	Mode Mode `json:"mode,omitempty"`
	// Blockers are GN references to synced targets.
	Blockers []Reference `json:"blockers,omitempty"`
	// Cleanup are test group entries that unsyncing removes.
	Cleanup []Reference `json:"cleanup,omitempty"`
	// UnwiredTests are synced tests run by GN test groups but not reachable
	// from any `bazel_test_suite()`.
	UnwiredTests []string `json:"unwired_tests,omitempty"`
	// UnderTest are GN-built synced targets in other directories that this
	// directory's GN-run synced tests test (BLOCKED_DUAL_TESTED).
	UnderTest []string `json:"under_test,omitempty"`
}

// Index is the result of parsing and resolving the whole tree.
type Index struct {
	Root     string
	Files    []*FileIndex
	Resolver *Resolver
	Wiring   *TestWiring
	// Refs are all resolved references, grouped by sentinel directory.
	Refs map[string][]Ref
	// Sentinel holds the index of each sentinel directory's BUILD.gn.
	Sentinel map[string]*FileIndex
}

// BuildIndex resolves every string literal in `files`.
func BuildIndex(root string, files []*FileIndex) *Index {
	idx := &Index{
		Root:     root,
		Files:    files,
		Resolver: NewResolver(files),
		Refs:     map[string][]Ref{},
		Sentinel: map[string]*FileIndex{},
	}
	for _, fi := range files {
		if fi.IsSentinelFile() && path.Base(fi.Path) == "BUILD.gn" {
			idx.Sentinel[fi.Dir()] = fi
		}
		for _, l := range fi.Literals {
			// Bazel labels look like GN labels but GN never resolves them, so
			// they are not references at all (see IsBazelLabel).
			if IsBazelLabel(l) {
				continue
			}
			for _, ref := range idx.Resolver.Resolve(l) {
				idx.Refs[ref.Dir] = append(idx.Refs[ref.Dir], ref)
			}
		}
	}
	idx.Wiring = NewTestWiring(root, files)
	return idx
}

func enclosing(t *Target) string {
	if t == nil {
		return ""
	}
	if t.Name == "" {
		return t.Template + "(...)"
	}
	return fmt.Sprintf("%s(%q)", t.Template, t.Name)
}

func (idx *Index) toReference(r Ref) Reference {
	ref := Reference{
		File:      r.Lit.File,
		Line:      r.Lit.Line,
		Column:    r.Lit.Column,
		Enclosing: enclosing(r.Lit.Target),
		Attr:      r.Lit.Attr,
		Literal:   r.Lit.Value,
		Target:    r.Target,
		How:       r.How,
		Tier:      r.Tier,
	}
	if r.Lit.Region == RegionBelow {
		ref.FromSentinelDir = path.Dir(r.Lit.File)
	}
	return ref
}

// IsBazelLabel reports whether `l` is assigned to an attribute whose values
// are Bazel labels rather than GN labels, e.g. `bazel_target` of
// `bazel_host_tool()` or `host_tests` of `bazel_test_suite()`. Such strings
// often look exactly like GN labels of synced targets, but GN never resolves
// them.
func IsBazelLabel(l *Literal) bool {
	switch l.Attr {
	case "bazel_target", "bazel_targets":
		return l.Target != nil
	case "host_tests", "target_tests":
		return l.Target != nil && l.Target.Template == "bazel_test_suite"
	}
	return false
}

var testGroupAttrs = map[string]bool{"testonly": true, "deps": true, "public_deps": true, "visibility": true}

// IsTestGroup reports whether `t` is a group that only aggregates tests:
// a `testonly` group named "tests" or "*_tests" that sets nothing but
// `deps`, `public_deps` and `visibility`.
func IsTestGroup(t *Target) bool {
	if t == nil || t.Template != "group" || !t.TestOnly {
		return false
	}
	if t.Name != "tests" && !strings.HasSuffix(t.Name, "_tests") {
		return false
	}
	for _, a := range t.Attrs {
		if !testGroupAttrs[a] {
			return false
		}
	}
	return true
}

func isTestTarget(t *Target) bool {
	if t == nil {
		return false
	}
	n := strings.ToLower(t.Name)
	return t.TestOnly || strings.Contains(strings.ToLower(t.Template), "test") ||
		strings.HasSuffix(n, "_test") || strings.HasSuffix(n, "_tests") ||
		strings.HasSuffix(n, "-test") || strings.HasSuffix(n, "-tests")
}

// isTestRef reports whether `r` points at a synced test: a test target, or a
// test subtarget such as `:foo_test` for `rustc_library("foo")` with
// `with_unit_tests = true`.
func (idx *Index) isTestRef(r Ref, sentinel *FileIndex) bool {
	if r.Tier == TierDir || r.Side != RegionBelow {
		return false
	}
	if r.Tier == TierPrefix && strings.Contains(r.RefName[len(r.Target):], "test") {
		return true
	}
	return isTestTarget(sentinel.target(r.Target, RegionBelow))
}

// isHostRef reports whether test group entry `r` runs a test on host: the
// label names the host toolchain (":foo_test($host_toolchain)"), or the test
// group or the synced test is defined in an `if (is_host)` block.
func isHostRef(r Ref, sentinel *FileIndex) bool {
	if strings.Contains(r.Lit.Value, "host_toolchain") || strings.Contains(r.Lit.Value, "toolchain:host") {
		return true
	}
	if r.Lit.Target != nil && r.Lit.Target.Condition == "is_host" {
		return true
	}
	if t := sentinel.target(r.Target, RegionBelow); t != nil && t.Condition == "is_host" {
		return true
	}
	return false
}

func (fi *FileIndex) target(name string, region Region) *Target {
	for _, t := range fi.Targets {
		if t.Name == name && t.Region == region {
			return t
		}
	}
	return nil
}

// Scan computes the report for one sentinel directory.
func (idx *Index) Scan(dir string) *DirReport {
	rep := &DirReport{Dir: dir}
	sentinel := idx.Sentinel[dir]
	gnFile := path.Join(dir, "BUILD.gn")
	var synced int
	for _, t := range sentinel.Targets {
		if t.Region == RegionBelow {
			synced++
		}
	}
	if synced == 0 {
		rep.State = StateEmptySentinel
		return rep
	}

	type testRef struct {
		name string
		host bool
	}
	var testsToCheck []testRef
	seenTest := map[testRef]bool{}
	for _, r := range idx.Refs[dir] {
		fromOwnBelow := r.Lit.File == gnFile && r.Lit.Region == RegionBelow
		if fromOwnBelow || !r.HitsSynced() {
			continue
		}
		// A reference from another directory's below-sentinel section is
		// always a blocker: bazel2gn owns that section and would regenerate
		// the entry, so that directory must be unsynced first.
		if r.Lit.Region != RegionBelow && IsTestGroup(r.Lit.Target) && (r.Lit.Attr == "deps" || r.Lit.Attr == "public_deps") && idx.isTestRef(r, sentinel) {
			rep.Cleanup = append(rep.Cleanup, idx.toReference(r))
			tr := testRef{r.RefName, isHostRef(r, sentinel)}
			if !seenTest[tr] {
				seenTest[tr] = true
				testsToCheck = append(testsToCheck, tr)
			}
			continue
		}
		rep.Blockers = append(rep.Blockers, idx.toReference(r))
	}
	sortRefs(rep.Blockers)
	if len(rep.Blockers) > 0 {
		rep.State = StateBlocked
		sortRefs(rep.Cleanup)
		return rep
	}

	if rep.UnderTest = idx.gnBuiltUnderTest(dir, sentinel, rep.Cleanup); len(rep.UnderTest) > 0 {
		rep.State = StateBlockedDualTested
		sortRefs(rep.Cleanup)
		return rep
	}

	for _, tr := range testsToCheck {
		if !idx.Wiring.Wired(dir, tr.name, tr.host) {
			name := tr.name
			if tr.host {
				name += " (host)"
			}
			rep.UnwiredTests = append(rep.UnwiredTests, name)
		}
	}
	sort.Strings(rep.UnwiredTests)
	if len(rep.UnwiredTests) > 0 {
		rep.State = StateBlockedTestsUnwired
		sortRefs(rep.Cleanup)
		return rep
	}

	rep.State = StateReady
	rep.Mode = idx.mode(dir, sentinel)
	if rep.Mode == ModeDeleteBuildGN {
		// Deleting BUILD.gn also deletes the test groups above the sentinel,
		// so external test group references to them must go too.
		for _, r := range idx.Refs[dir] {
			if r.Side == RegionAbove && r.Lit.File != gnFile {
				rep.Cleanup = append(rep.Cleanup, idx.toReference(r))
			}
		}
	}
	sortRefs(rep.Cleanup)
	return rep
}

// gnBuiltUnderTest returns the synced targets in other directories that the
// synced tests (those behind `cleanup`, the test group entries in GN, and
// every synced test target) depend on,
// directly or through other synced targets in `dir`, and that GN still
// builds. Only targets in an ancestor directory of `dir` (//lib/foo for
// //lib/foo/tests) count as code under test, plus, for a directory with only
// test targets, a sibling directory (//a/foo for //a/foo-tests). A directory
// with its own library is assumed to test that library, and shared test
// frameworks such as gtest_main are dependencies, not the code being tested.
func (idx *Index) gnBuiltUnderTest(dir string, sentinel *FileIndex, cleanup []Reference) []string {
	// Synced targets in `dir` and the references in their bodies.
	edges := map[string][]Ref{}
	for _, l := range sentinel.Literals {
		if l.Region != RegionBelow || l.Target == nil || l.Target.Name == "" || IsBazelLabel(l) {
			continue
		}
		edges[l.Target.Name] = append(edges[l.Target.Name], idx.Resolver.Resolve(l)...)
	}
	testOnly := true
	for _, t := range sentinel.Targets {
		if t.Region == RegionBelow && !isTestTarget(t) {
			testOnly = false
		}
	}
	// Start from the tests GN test groups run, and from every synced test
	// target: a synced test whose GN package is still `# @bazel2gn:skip`
	// (waiting to "Run in GN too") isn't in a GN test group yet, but its GN
	// copy must stay until the code under test leaves GN.
	var queue []string
	for _, c := range cleanup {
		if c.Target != "" {
			queue = append(queue, c.Target)
		}
	}
	for _, t := range sentinel.Targets {
		if t.Region == RegionBelow && t.Name != "" && isTestTarget(t) {
			queue = append(queue, t.Name)
		}
	}
	seen := map[string]bool{}
	found := map[string]bool{}
	for len(queue) > 0 {
		name := queue[0]
		queue = queue[1:]
		if seen[name] {
			continue
		}
		seen[name] = true
		for _, r := range edges[name] {
			switch {
			case r.Dir == dir:
				if r.Target != "" {
					queue = append(queue, r.Target)
				}
			case relatedDir(dir, r.Dir, testOnly) && r.HitsSynced() && idx.builtByGN(r.Dir, dir):
				label := "//" + r.Dir
				if r.Target != "" {
					label += ":" + r.Target
				}
				found[label] = true
			}
		}
	}
	var out []string
	for l := range found {
		out = append(out, l)
	}
	sort.Strings(out)
	return out
}

// relatedDir reports whether `other` is an ancestor of `dir`, or, if
// `siblings`, a sibling of it.
func relatedDir(dir, other string, siblings bool) bool {
	return strings.HasPrefix(dir, other+"/") || (siblings && path.Dir(dir) == path.Dir(other))
}

// builtByGN reports whether anything in GN other than `dir`'s synced section
// still references the synced targets of sentinel directory `other`.
func (idx *Index) builtByGN(other, dir string) bool {
	otherGN := path.Join(other, "BUILD.gn")
	for _, r := range idx.Refs[other] {
		if !r.HitsSynced() {
			continue
		}
		if r.Lit.Region == RegionBelow && (r.Lit.File == otherGN || path.Dir(r.Lit.File) == dir) {
			continue
		}
		return true
	}
	return false
}

// mode decides whether a READY directory's BUILD.gn can be deleted.
func (idx *Index) mode(dir string, sentinel *FileIndex) Mode {
	gnFile := path.Join(dir, "BUILD.gn")
	if sentinel.AboveOther > 0 {
		return ModeStripBelowSentinel
	}
	for _, t := range sentinel.Targets {
		if t.Region == RegionAbove && !IsTestGroup(t) {
			return ModeStripBelowSentinel
		}
		if t.Region == RegionAbove && (t.InCondition || t.OpaqueDeps) {
			// Entries we can't see (from an `if` or a variable) might be other
			// tests that deleting BUILD.gn would drop from CQ.
			return ModeStripBelowSentinel
		}
	}
	// Every entry of the test groups above must be a synced test in this
	// directory; anything else (e.g. "integration:tests") would be lost.
	for _, l := range sentinel.Literals {
		if l.Region != RegionAbove || l.Target == nil || l.CallArgOf == "import" {
			continue
		}
		if l.Attr != "deps" && l.Attr != "public_deps" {
			continue
		}
		ok := false
		for _, r := range idx.Resolver.Resolve(l) {
			if r.Dir == dir && idx.isTestRef(r, sentinel) {
				ok = true
			}
		}
		if !ok {
			return ModeStripBelowSentinel
		}
	}
	// Anything other than a test group referencing a target above the
	// sentinel keeps BUILD.gn alive.
	for _, r := range idx.Refs[dir] {
		if r.Side != RegionAbove || r.Lit.File == gnFile {
			continue
		}
		// References from another directory's below-sentinel section can't
		// be edited (bazel2gn owns it), so BUILD.gn must stay.
		if r.Lit.Region == RegionBelow || !IsTestGroup(r.Lit.Target) || (r.Lit.Attr != "deps" && r.Lit.Attr != "public_deps") {
			return ModeStripBelowSentinel
		}
	}
	return ModeDeleteBuildGN
}

// ScanAll computes reports for every sentinel directory, sorted by directory.
func (idx *Index) ScanAll() []*DirReport {
	var reps []*DirReport
	for _, d := range idx.Resolver.SentinelDirs() {
		reps = append(reps, idx.Scan(d))
	}
	return reps
}

// Query returns every GN reference to synced targets matching `label`, which
// is "//dir", "//dir:*" or "//dir:target". References from the directory's
// own below-sentinel section, and Bazel labels (see IsBazelLabel), are
// omitted.
func (idx *Index) Query(label string) ([]Reference, error) {
	if !strings.HasPrefix(label, "//") {
		return nil, fmt.Errorf("label %q must start with //", label)
	}
	dir, name, hasColon := strings.Cut(stripToolchain(label)[2:], ":")
	dir = strings.TrimSuffix(dir, "/")
	if !idx.Resolver.IsSentinelDir(dir) {
		return nil, fmt.Errorf("//%s has no BUILD.gn with the bazel2gn sentinel; gn2gone only indexes references to synced targets", dir)
	}
	all := !hasColon || name == "*"
	sentinel := idx.Sentinel[dir]
	if !all && sentinel.target(name, RegionBelow) == nil {
		return nil, fmt.Errorf("//%s:%s is not a synced (below-sentinel) target", dir, name)
	}
	gnFile := path.Join(dir, "BUILD.gn")
	var out []Reference
	for _, r := range idx.Refs[dir] {
		if r.Lit.File == gnFile && r.Lit.Region == RegionBelow {
			continue
		}
		if !r.HitsSynced() {
			continue
		}
		if all || r.Target == name || r.Tier == TierDir {
			out = append(out, idx.toReference(r))
		}
	}
	sortRefs(out)
	return out, nil
}

func sortRefs(refs []Reference) {
	sort.Slice(refs, func(i, j int) bool {
		a, b := refs[i], refs[j]
		if a.File != b.File {
			return a.File < b.File
		}
		if a.Line != b.Line {
			return a.Line < b.Line
		}
		return a.Column < b.Column
	})
}
