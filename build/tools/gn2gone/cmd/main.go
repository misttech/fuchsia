// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// gn2gone finds directories whose bazel2gn-synced GN targets are no longer
// used by GN, explains what still uses the others, and unsyncs ready ones.
package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strings"

	"go.fuchsia.dev/fuchsia/build/tools/gn2gone"
)

const usage = `Usage: gn2gone [flags] <command> [args]

Commands:
  scan                 Report every bazel2gn sentinel directory: READY,
                       EMPTY_SENTINEL, BLOCKED_DUAL_TESTED,
                       BLOCKED_TESTS_UNWIRED, or BLOCKED.
  query <label>        List GN references to synced targets: //dir, //dir:*,
                       or //dir:target.
  unsync [<dir>...]    Unsync READY directories (all of them by default):
                       delete BUILD.gn or strip the sentinel and everything
                       below it, and remove test group entries. Edits the
                       working tree only; nothing is committed.

Flags:
`

func main() {
	fs := flag.NewFlagSet("gn2gone", flag.ExitOnError)
	fuchsiaDir := fs.String("fuchsia_dir", os.Getenv("FUCHSIA_DIR"), "Fuchsia root directory. Defaults to $FUCHSIA_DIR.")
	gnBin := fs.String("gn_bin", "", "Path to gn. Defaults to the prebuilt under --fuchsia_dir.")
	jsonOut := fs.Bool("json", false, "Print JSON instead of text.")
	verbose := fs.Bool("v", false, "scan: also list blockers of BLOCKED directories.")
	workers := fs.Int("workers", runtime.NumCPU(), "Number of gn processes to run in parallel.")
	batch := fs.Int("batch", 50, "Number of files per gn process.")
	fs.Usage = func() {
		fmt.Fprint(fs.Output(), usage)
		fs.PrintDefaults()
	}
	// Accept flags anywhere: `gn2gone --json scan`, `gn2gone scan --json`,
	// `gn2gone query //foo --json`.
	var positional []string
	argv := os.Args[1:]
	for {
		fs.Parse(argv)
		if fs.NArg() == 0 {
			break
		}
		positional = append(positional, fs.Arg(0))
		argv = fs.Args()[1:]
	}
	cmd, args := "", []string(nil)
	if len(positional) > 0 {
		cmd, args = positional[0], positional[1:]
	}
	if cmd == "" {
		fs.Usage()
		os.Exit(2)
	}
	if *fuchsiaDir == "" {
		fatalf("--fuchsia_dir is required (or set $FUCHSIA_DIR)")
	}
	root, err := filepath.Abs(*fuchsiaDir)
	if err != nil {
		fatalf("%v", err)
	}
	if *gnBin == "" {
		*gnBin = filepath.Join(root, "prebuilt", "third_party", "gn", hostTag(), "gn")
	}

	ctx := context.Background()
	idx, stats, err := gn2gone.Load(ctx, gn2gone.LoadOptions{Root: root, GNBin: *gnBin, BatchSize: *batch, Workers: *workers})
	if err != nil {
		fatalf("%v", err)
	}
	// A file that failed to parse may hold references to synced targets, so
	// results are incomplete: report it, exit non-zero, and never unsync.
	for _, e := range stats.ParseErrors {
		fmt.Fprintf(os.Stderr, "error: %v\n", e)
	}
	incomplete := len(stats.ParseErrors) > 0
	if incomplete && cmd == "unsync" {
		fatalf("refusing to unsync: %d GN files failed to parse, so their references are unknown", len(stats.ParseErrors))
	}
	defer func() {
		if incomplete {
			fatalf("results are incomplete: %d GN files failed to parse", len(stats.ParseErrors))
		}
	}()

	switch cmd {
	case "scan":
		reps := idx.ScanAll()
		if *jsonOut {
			out := scanJSON{Reports: reps, ParseErrors: []string{}, Unattributed: []string{}}
			for _, e := range stats.ParseErrors {
				out.ParseErrors = append(out.ParseErrors, e.Error())
			}
			for _, l := range stats.Unattributed {
				out.Unattributed = append(out.Unattributed, fmt.Sprintf("%s:%d %q", l.File, l.Line, l.Value))
			}
			writeJSON(os.Stdout, out)
			return
		}
		printScan(os.Stdout, reps, stats, *verbose)
	case "query":
		if len(args) != 1 {
			fatalf("query takes exactly one label")
		}
		refs, err := idx.Query(args[0])
		if err != nil {
			fatalf("%v", err)
		}
		if *jsonOut {
			writeJSON(os.Stdout, refs)
			return
		}
		if len(refs) == 0 {
			fmt.Printf("No GN references to synced targets of %s.\n", args[0])
		}
		for _, r := range refs {
			fmt.Println(r.String())
		}
	case "unsync":
		var reps []*gn2gone.DirReport
		if len(args) == 0 {
			for _, r := range idx.ScanAll() {
				if r.State == gn2gone.StateReady {
					reps = append(reps, r)
				}
			}
		} else {
			for _, a := range args {
				d := strings.TrimSuffix(strings.TrimPrefix(a, "//"), "/")
				if !idx.Resolver.IsSentinelDir(d) {
					fatalf("//%s has no BUILD.gn with the bazel2gn sentinel", d)
				}
				reps = append(reps, idx.Scan(d))
			}
		}
		if n := len(stats.Unattributed); n > 0 {
			fmt.Fprintf(os.Stderr, "note: %d dynamic strings may name sentinel directories but block none; review them with `gn2gone scan -v` before landing.\n", n)
		}
		plan := idx.PlanUnsync(reps)
		for _, d := range sortedKeys(plan.Skipped) {
			fmt.Fprintf(os.Stderr, "skipping //%s: %s (run `gn2gone query //%s` for details)\n", d, plan.Skipped[d], d)
		}
		if len(plan.Dirs) == 0 {
			fmt.Println("Nothing to unsync.")
			return
		}
		if err := plan.Apply(ctx, root, *gnBin); err != nil {
			fatalf("%v", err)
		}
		fmt.Printf("Unsynced %d directories:\n", len(plan.Dirs))
		for _, d := range plan.Dirs {
			fmt.Printf("  //%s\n", d)
		}
		printFilesByRepo(os.Stdout, root, plan.Files())
	default:
		fs.Usage()
		os.Exit(2)
	}
}

// scanJSON is the `scan --json` output.
type scanJSON struct {
	Reports []*gn2gone.DirReport `json:"reports"`
	// ParseErrors lists GN files that failed to parse. If non-empty, the
	// reports are incomplete.
	ParseErrors []string `json:"parse_errors"`
	// Unattributed lists dynamic strings ("file:line literal") that may name
	// sentinel directories but block none, because their static prefix is
	// empty or too broad.
	Unattributed []string `json:"unattributed_dynamic_labels"`
}

func hostTag() string {
	arch := runtime.GOARCH
	if arch == "amd64" {
		arch = "x64"
	}
	osName := runtime.GOOS
	if osName == "darwin" {
		osName = "mac"
	}
	return osName + "-" + arch
}

func printScan(w io.Writer, reps []*gn2gone.DirReport, stats *gn2gone.Stats, verbose bool) {
	counts := map[gn2gone.State]int{}
	for _, r := range reps {
		counts[r.State]++
	}
	fmt.Fprintf(w, "Scanned %d GN files (%d string literals) in %.1fs (parsing %.1fs).\n",
		stats.Files, stats.Literals, stats.TotalTime.Seconds(), stats.ParseTime.Seconds())
	fmt.Fprintf(w, "%d sentinel directories: %d READY, %d BLOCKED_DUAL_TESTED, %d BLOCKED_TESTS_UNWIRED, %d EMPTY_SENTINEL, %d BLOCKED.\n",
		len(reps), counts[gn2gone.StateReady], counts[gn2gone.StateBlockedDualTested], counts[gn2gone.StateBlockedTestsUnwired],
		counts[gn2gone.StateEmptySentinel], counts[gn2gone.StateBlocked])
	fmt.Fprintf(w, "%d dynamic strings may name sentinel directories but block none (static prefix empty or matching more than 50); -v lists them.\n\n",
		len(stats.Unattributed))
	if verbose && len(stats.Unattributed) > 0 {
		fmt.Fprintln(w, "Unattributed dynamic strings:")
		for _, l := range stats.Unattributed {
			fmt.Fprintf(w, "  %s:%d %q\n", l.File, l.Line, l.Value)
		}
		fmt.Fprintln(w)
	}
	for _, st := range []gn2gone.State{gn2gone.StateReady, gn2gone.StateBlockedDualTested, gn2gone.StateBlockedTestsUnwired, gn2gone.StateEmptySentinel} {
		if counts[st] == 0 {
			continue
		}
		fmt.Fprintf(w, "%s:\n", st)
		for _, r := range reps {
			if r.State != st {
				continue
			}
			switch st {
			case gn2gone.StateReady:
				fmt.Fprintf(w, "  //%s  (%s, %d cleanup entries)\n", r.Dir, r.Mode, len(r.Cleanup))
			case gn2gone.StateBlockedDualTested:
				fmt.Fprintf(w, "  //%s  tests GN-built: %s\n", r.Dir, strings.Join(r.UnderTest, ", "))
			case gn2gone.StateBlockedTestsUnwired:
				fmt.Fprintf(w, "  //%s  unwired: %s\n", r.Dir, strings.Join(r.UnwiredTests, ", "))
			default:
				fmt.Fprintf(w, "  //%s\n", r.Dir)
			}
		}
		fmt.Fprintln(w)
	}
	// Blocked directories, fewest blockers first: the closest to READY.
	var blocked []*gn2gone.DirReport
	for _, r := range reps {
		if r.State == gn2gone.StateBlocked {
			blocked = append(blocked, r)
		}
	}
	sort.SliceStable(blocked, func(i, j int) bool {
		return referencingTargets(blocked[i]) < referencingTargets(blocked[j])
	})
	if len(blocked) > 0 {
		fmt.Fprintf(w, "BLOCKED (fewest referencing targets first):\n")
	}
	for _, r := range blocked {
		fmt.Fprintf(w, "  //%s  %d referencing targets\n", r.Dir, referencingTargets(r))
		if verbose {
			for _, b := range r.Blockers {
				fmt.Fprintf(w, "      %s\n", b.String())
			}
		}
	}
}

// referencingTargets counts distinct targets (or files, for file-scope
// references) that block the directory.
func referencingTargets(r *gn2gone.DirReport) int {
	set := map[string]bool{}
	for _, b := range r.Blockers {
		set[b.File+" "+b.Enclosing] = true
	}
	return len(set)
}

func printFilesByRepo(w io.Writer, root string, files []string) {
	byRepo := map[string][]string{}
	for _, f := range files {
		dir := filepath.Dir(filepath.Join(root, f))
		for {
			if _, err := os.Stat(dir); err == nil {
				break
			}
			dir = filepath.Dir(dir)
		}
		repo := "(unknown repository)"
		if out, err := exec.Command("git", "-C", dir, "rev-parse", "--show-toplevel").Output(); err == nil {
			repo = strings.TrimSpace(string(out))
		}
		byRepo[repo] = append(byRepo[repo], f)
	}
	fmt.Fprintln(w, "\nTouched files by repository (commit each repository separately):")
	for _, repo := range sortedKeys(byRepo) {
		fmt.Fprintf(w, "  %s\n", repo)
		for _, f := range byRepo[repo] {
			fmt.Fprintf(w, "    %s\n", f)
		}
	}
}

func sortedKeys[V any](m map[string]V) []string {
	keys := make([]string, 0, len(m))
	for k := range m {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	return keys
}

func writeJSON(w io.Writer, v any) {
	enc := json.NewEncoder(w)
	enc.SetIndent("", "  ")
	if err := enc.Encode(v); err != nil {
		fatalf("%v", err)
	}
}

func fatalf(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "gn2gone: "+format+"\n", args...)
	os.Exit(1)
}
