// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package gn2gone

import (
	"path"
	"regexp"
	"sort"
	"strings"
)

// How says how a string literal was resolved to a sentinel directory.
type How string

const (
	// HowAbsolute: "//dir:target" or "//dir".
	HowAbsolute How = "absolute"
	// HowRelative: "sub/dir:target" or "../dir", resolved against the
	// containing BUILD.gn.
	HowRelative How = "relative"
	// HowLocal: ":target" in the same BUILD.gn.
	HowLocal How = "local"
	// HowVarSubstituted: "$var/dir:target", with `var` from //.gn.
	HowVarSubstituted How = "var_substituted"
	// HowDynamicPrefix: "//dir/$x...", "//dir:$x", or a "//dir/" + ...
	// concatenation, which might name any sentinel directory whose path starts
	// with the static text (or, after ":", any target in that directory).
	HowDynamicPrefix How = "dynamic_prefix"
	// HowDynamicLocal: ":$x..." or ":" + ... in the same BUILD.gn.
	HowDynamicLocal How = "dynamic_local"
)

// Tier says how precisely the reference was matched to a target.
type Tier string

const (
	// TierExact: the name matches a target defined in the directory.
	TierExact Tier = "exact"
	// TierPrefix: the name is a defined target name followed by "_", "-" or
	// ".", e.g. a template subtarget like ":foo_rust" for ":foo".
	TierPrefix Tier = "prefix"
	// TierDir: no specific target; conservatively refers to every synced
	// target in the directory.
	TierDir Tier = "dir"
)

// Ref is a string literal resolved to a sentinel directory.
type Ref struct {
	Lit *Literal
	// Dir is the sentinel directory, relative to the Fuchsia root.
	Dir string
	How How
	// RefName is the target name as written (after resolution), or "".
	RefName string
	Tier    Tier
	// Target is the matched target name (TierExact/TierPrefix), or "".
	Target string
	// Side is RegionAbove or RegionBelow for matched targets, "" for TierDir.
	Side Region
}

// HitsSynced reports whether the reference may point at a synced target.
func (r *Ref) HitsSynced() bool { return r.Side == RegionBelow || r.Tier == TierDir }

// maxDynamicDirs caps how many sentinel directories one dynamic string may
// block. Broader prefixes (like "//src/") are counted but not attributed.
const maxDynamicDirs = 50

var (
	labelChars   = regexp.MustCompile(`^[A-Za-z0-9_.+\-/:@]+$`)
	leadingVarRE = regexp.MustCompile(`^\$\{?([A-Za-z_][A-Za-z0-9_]*)\}?(.*)$`)
)

// Resolver maps string literals to sentinel directories and targets.
type Resolver struct {
	sentinelDirs map[string]bool
	sortedDirs   []string
	below        map[string]map[string]bool
	above        map[string]map[string]bool
	vars         map[string]string
	// unattributed holds dynamic strings that may name a sentinel directory
	// but are not attributed to any: their static prefix is empty (e.g.
	// "//$x") or matches more than maxDynamicDirs sentinel directories.
	unattributed map[*Literal]bool
}

// Unattributed returns the dynamic strings that may name sentinel
// directories but block none, sorted by file and line.
func (r *Resolver) Unattributed() []*Literal {
	var out []*Literal
	for l := range r.unattributed {
		out = append(out, l)
	}
	sort.Slice(out, func(i, j int) bool {
		if out[i].File != out[j].File {
			return out[i].File < out[j].File
		}
		return out[i].Line < out[j].Line
	})
	return out
}

// NewResolver builds a resolver from the indexes of all files. Variables
// assigned a "//..." string literal in //.gn (typically in default_args) are
// substituted when a label starts with them.
func NewResolver(files []*FileIndex) *Resolver {
	r := &Resolver{
		sentinelDirs: map[string]bool{},
		below:        map[string]map[string]bool{},
		above:        map[string]map[string]bool{},
		vars:         map[string]string{},
		unattributed: map[*Literal]bool{},
	}
	for _, fi := range files {
		if fi.IsSentinelFile() && path.Base(fi.Path) == "BUILD.gn" {
			d := fi.Dir()
			r.sentinelDirs[d] = true
			r.below[d] = map[string]bool{}
			r.above[d] = map[string]bool{}
		}
	}
	for _, fi := range files {
		d := fi.Dir()
		if !r.sentinelDirs[d] || path.Base(fi.Path) != "BUILD.gn" {
			continue
		}
		for _, t := range fi.Targets {
			if t.Name == "" {
				continue
			}
			if t.Region == RegionBelow {
				r.below[d][t.Name] = true
			} else if t.Region == RegionAbove {
				r.above[d][t.Name] = true
			}
		}
	}
	for _, fi := range files {
		if fi.Path != ".gn" {
			continue
		}
		for _, l := range fi.Literals {
			key := l.InnerAttr
			if key == "" {
				key = l.Attr
			}
			if key != "" && strings.HasPrefix(l.Value, "//") && !l.InConcat {
				r.vars[key] = l.Value
			}
		}
	}
	for d := range r.sentinelDirs {
		r.sortedDirs = append(r.sortedDirs, d)
	}
	sort.Strings(r.sortedDirs)
	return r
}

// IsSentinelDir reports whether `dir` has a BUILD.gn with the sentinel.
func (r *Resolver) IsSentinelDir(dir string) bool { return r.sentinelDirs[dir] }

// SentinelDirs returns all sentinel directories, sorted.
func (r *Resolver) SentinelDirs() []string { return r.sortedDirs }

// dirsUnder returns the sentinel directories that a label starting with the
// static text "//" + `prefix` may name. The dynamic part may continue the
// last path component ("//sdk/fidl/fuchsia.$x"), so this matches by plain
// string prefix, not by whole path components.
func (r *Resolver) dirsUnder(prefix string) []string {
	if strings.Trim(prefix, "/") == "" {
		return nil
	}
	var out []string
	for i := sort.SearchStrings(r.sortedDirs, prefix); i < len(r.sortedDirs); i++ {
		d := r.sortedDirs[i]
		if !strings.HasPrefix(d, prefix) {
			break
		}
		out = append(out, d)
	}
	return out
}

func (r *Resolver) match(dir, name string) (Tier, string, Region) {
	if r.below[dir][name] {
		return TierExact, name, RegionBelow
	}
	if r.above[dir][name] {
		return TierExact, name, RegionAbove
	}
	best, side := "", Region("")
	try := func(names map[string]bool, s Region) {
		for n := range names {
			if len(name) > len(n) && strings.HasPrefix(name, n) && strings.ContainsRune("_-.", rune(name[len(n)])) && len(n) > len(best) {
				best, side = n, s
			}
		}
	}
	try(r.below[dir], RegionBelow)
	try(r.above[dir], RegionAbove)
	if best != "" {
		return TierPrefix, best, side
	}
	return TierDir, "", ""
}

func (r *Resolver) ref(l *Literal, dir string, how How, name string) Ref {
	ref := Ref{Lit: l, Dir: dir, How: how, RefName: name, Tier: TierDir}
	if name != "" {
		ref.Tier, ref.Target, ref.Side = r.match(dir, name)
	}
	return ref
}

// stripToolchain removes a trailing "(toolchain)" from a label.
func stripToolchain(s string) string {
	if strings.HasSuffix(s, ")") {
		if i := strings.LastIndexByte(s, '('); i >= 0 {
			return s[:i]
		}
	}
	return s
}

// Resolve returns the sentinel directories and targets that `l` may name.
//
// Relative (and ":local") labels are resolved against the containing file's
// directory. For a .gni file this is an approximation: GN evaluates it in the
// directory of the importing BUILD.gn, which is almost always the .gni's own
// directory (e.g. a list of subdirectory names next to the BUILD.gn that
// iterates over it). Resolving there can only add blockers, never hide one
// that an importer in the same directory would see.
func (r *Resolver) Resolve(l *Literal) []Ref {
	s := l.Value
	if s == "" || strings.ContainsAny(s, " \t") {
		return nil
	}
	srcDir := path.Dir(l.File)
	relativeOK := l.Region != RegionDotGN
	s = stripToolchain(s)
	how := How("")
	if m := leadingVarRE.FindStringSubmatch(s); m != nil {
		if v, ok := r.vars[m[1]]; ok {
			v = strings.TrimSuffix(v, "/")
			rest := strings.TrimPrefix(m[2], "/")
			if v == "" || v == "/" {
				s = "//" + rest
			} else {
				s = v + "/" + rest
			}
			how = HowVarSubstituted
		}
	}
	// Labels with a dynamic part: "$x" interpolation, or an operand of `+`.
	if dyn := strings.IndexByte(s, '$'); dyn >= 0 || l.InConcat {
		static := s
		if dyn >= 0 {
			static = s[:dyn]
		}
		// A string starting with the dynamic part ("$dir:bin", "$x/foo") can't
		// be placed. When it iterates over a list of directories, the list
		// entries themselves block those directories (see bareDirName).
		// Counting these as unattributed would drown the signal: thousands
		// are `${target_name}:...` in templates.
		if !labelChars.MatchString(static) {
			return nil
		}
		switch {
		case strings.HasPrefix(static, "//"):
			return r.dynamicPrefix(l, static[2:])
		case !relativeOK || strings.HasPrefix(static, "/"):
			// "/..." is a file system path, not a label.
			return nil
		case strings.HasPrefix(static, ":"):
			if r.sentinelDirs[srcDir] {
				return []Ref{r.ref(l, srcDir, HowDynamicLocal, "")}
			}
			return nil
		}
		// A relative static start ("sub/$x:tests", "tests/" + name). A string
		// that starts with the dynamic part ("$x", "$x/foo") can't be placed.
		return r.dynamicPrefix(l, relativeStatic(srcDir, static))
	}
	if !labelChars.MatchString(s) {
		return nil
	}
	if how == "" {
		how = HowAbsolute
	}
	switch {
	case strings.HasPrefix(s, "//"):
		d, t, hasColon := strings.Cut(s[2:], ":")
		d = strings.TrimSuffix(d, "/")
		if !r.sentinelDirs[d] {
			return nil
		}
		if !hasColon {
			t = bareDirName(l, d)
		}
		return []Ref{r.ref(l, d, how, t)}
	case !relativeOK:
		return nil
	case strings.HasPrefix(s, ":"):
		if !r.sentinelDirs[srcDir] {
			return nil
		}
		return []Ref{r.ref(l, srcDir, HowLocal, s[1:])}
	}
	d, t, hasColon := strings.Cut(s, ":")
	nd := path.Clean(path.Join(srcDir, d))
	if !r.sentinelDirs[nd] {
		return nil
	}
	if !hasColon {
		t = bareDirName(l, nd)
	}
	// Same-directory labels such as ".:target", "../<this dir>:target" and
	// deps = [ "." ] are real references; a bare "." elsewhere (e.g.
	// include_dirs = [ "." ]) is a path, not a label.
	if nd == srcDir && t == "" {
		return nil
	}
	return []Ref{r.ref(l, nd, HowRelative, t)}
}

// bareDirName returns the target name for a label without ":" ("//dir" or
// "dir"). Used directly as a dependency, it means the directory's default
// target (named like the directory). Anywhere else, typically a list of
// directories that a `foreach` turns into "${dir}:tests" or "$dir:bin", it
// may stand for any target in the directory, so "" (TierDir) is returned.
func bareDirName(l *Literal, dir string) string {
	if l.Target != nil && l.InnerAttr == "" {
		switch l.Attr {
		case "deps", "public_deps", "data_deps":
			return path.Base(dir)
		}
	}
	return ""
}

// relativeStatic turns the static start of a relative label into a
// root-relative static start for dynamicPrefix, keeping a trailing partial
// path component ("rea" in "rea$x") or a trailing "/" or ":".
func relativeStatic(srcDir, static string) string {
	dirPart, rest, hasColon := strings.Cut(static, ":")
	if hasColon {
		return path.Clean(path.Join(srcDir, dirPart)) + ":" + rest
	}
	if static == "" {
		return ""
	}
	joined := path.Clean(path.Join(srcDir, static))
	// A trailing "." or ".." is a whole directory, not a partial component:
	// ".$suffix" must not turn into the parent directory's name as a prefix.
	if b := path.Base(static); strings.HasSuffix(static, "/") || b == "." || b == ".." {
		if joined == "." {
			return ""
		}
		joined += "/"
	}
	return joined
}

// dynamicPrefix handles a label of which only the start, "//" + `static`, is
// known. If `static` contains ":", the directory is fully known and only the
// target name is dynamic. Otherwise the label may name any sentinel directory
// starting with `static`.
func (r *Resolver) dynamicPrefix(l *Literal, static string) []Ref {
	if d, _, hasColon := strings.Cut(static, ":"); hasColon {
		d = strings.TrimSuffix(d, "/")
		if !r.sentinelDirs[d] {
			return nil
		}
		return []Ref{r.ref(l, d, HowDynamicPrefix, "")}
	}
	dirs := r.dirsUnder(static)
	if strings.Trim(static, "/") == "" || len(dirs) > maxDynamicDirs {
		r.unattributed[l] = true
		return nil
	}
	var refs []Ref
	for _, d := range dirs {
		refs = append(refs, r.ref(l, d, HowDynamicPrefix, ""))
	}
	return refs
}
