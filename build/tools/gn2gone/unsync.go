// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package gn2gone

import (
	"context"
	"fmt"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
)

// Plan is the set of file edits that unsyncs some READY directories.
type Plan struct {
	// Deletes are BUILD.gn files to delete, relative to the Fuchsia root.
	Deletes []string
	// Truncates maps a BUILD.gn file to the sentinel line: that line and
	// everything after it are removed.
	Truncates map[string]int
	// RemoveEntries maps a file to the list entries (string literals) to
	// remove from it.
	RemoveEntries map[string][]Reference
	// Dirs are the directories the plan unsyncs.
	Dirs []string
	// Skipped maps directories that can't be unsynced to the reason.
	Skipped map[string]string
}

// PlanUnsync plans the edits for `reports`. Directories that are not READY
// are skipped.
func (idx *Index) PlanUnsync(reports []*DirReport) *Plan {
	p := &Plan{Truncates: map[string]int{}, RemoveEntries: map[string][]Reference{}, Skipped: map[string]string{}}
	deleted := map[string]bool{}
	for _, rep := range reports {
		if rep.State == StateReady && rep.Mode == ModeDeleteBuildGN {
			deleted[path.Join(rep.Dir, "BUILD.gn")] = true
		}
	}
	for _, rep := range reports {
		if rep.State != StateReady {
			p.Skipped[rep.Dir] = string(rep.State)
			continue
		}
		gnFile := path.Join(rep.Dir, "BUILD.gn")
		switch rep.Mode {
		case ModeDeleteBuildGN:
			p.Deletes = append(p.Deletes, gnFile)
		case ModeStripBelowSentinel:
			p.Truncates[gnFile] = idx.Sentinel[rep.Dir].SentinelLine
		}
		for _, c := range rep.Cleanup {
			if deleted[c.File] {
				continue
			}
			if sl, ok := p.Truncates[c.File]; ok && c.Line >= sl {
				continue
			}
			p.RemoveEntries[c.File] = append(p.RemoveEntries[c.File], c)
		}
		p.Dirs = append(p.Dirs, rep.Dir)
	}
	sort.Strings(p.Deletes)
	sort.Strings(p.Dirs)
	return p
}

// Files returns every file the plan touches, sorted.
func (p *Plan) Files() []string {
	set := map[string]bool{}
	for _, f := range p.Deletes {
		set[f] = true
	}
	for f := range p.Truncates {
		set[f] = true
	}
	for f := range p.RemoveEntries {
		set[f] = true
	}
	var out []string
	for f := range set {
		out = append(out, f)
	}
	sort.Strings(out)
	return out
}

// Apply performs the plan under `root`, then runs `gn format` on every
// edited (not deleted) file. Entries are removed by line: the literal must be
// the only list entry on its line (the `gn format` style for multi-entry
// lists), or the only entry of a single-line list. Apply checks every entry
// before changing any file.
func (p *Plan) Apply(ctx context.Context, root, gnBin string) error {
	edited := map[string][]string{}
	read := func(f string) ([]string, error) {
		if lines, ok := edited[f]; ok {
			return lines, nil
		}
		b, err := os.ReadFile(filepath.Join(root, f))
		if err != nil {
			return nil, err
		}
		lines := strings.Split(string(b), "\n")
		edited[f] = lines
		return lines, nil
	}
	// Truncate first: removed entries are above the sentinel, so their line
	// numbers are unaffected, but removing them would shift the sentinel.
	for f, sl := range p.Truncates {
		lines, err := read(f)
		if err != nil {
			return err
		}
		if sl < 1 || sl > len(lines) {
			return fmt.Errorf("%s: sentinel line %d out of range", f, sl)
		}
		edited[f] = append(lines[:sl-1:sl-1], "")
	}
	for f, entries := range p.RemoveEntries {
		lines, err := read(f)
		if err != nil {
			return err
		}
		// Edit bottom-up so earlier line numbers stay valid.
		sort.Slice(entries, func(i, j int) bool { return entries[i].Line > entries[j].Line })
		for _, e := range entries {
			if e.Line < 1 || e.Line > len(lines) {
				return fmt.Errorf("%s:%d: line out of range", f, e.Line)
			}
			newLine, drop, err := removeEntry(lines[e.Line-1], e.Literal)
			if err != nil {
				return fmt.Errorf("%s:%d: %v", f, e.Line, err)
			}
			if drop {
				lines = append(lines[:e.Line-1], lines[e.Line:]...)
			} else {
				lines[e.Line-1] = newLine
			}
		}
		edited[f] = lines
	}
	for _, f := range p.Deletes {
		delete(edited, f)
	}

	var toFormat []string
	for f, lines := range edited {
		if err := os.WriteFile(filepath.Join(root, f), []byte(strings.Join(lines, "\n")), 0o644); err != nil {
			return err
		}
		toFormat = append(toFormat, filepath.Join(root, f))
	}
	for _, f := range p.Deletes {
		if err := os.Remove(filepath.Join(root, f)); err != nil {
			return err
		}
	}
	if len(toFormat) == 0 || gnBin == "" {
		return nil
	}
	sort.Strings(toFormat)
	out, err := exec.CommandContext(ctx, gnBin, append([]string{"format"}, toFormat...)...).CombinedOutput()
	if err != nil {
		return fmt.Errorf("gn format failed (%v): %s", err, strings.TrimSpace(string(out)))
	}
	return nil
}

// removeEntry removes the list entry `"literal"` from `line`. It returns
// drop=true if the whole line should be removed.
func removeEntry(line, literal string) (string, bool, error) {
	quoted := regexp.QuoteMeta(`"` + literal + `"`)
	if regexp.MustCompile(`^\s*` + quoted + `\s*,?\s*(#.*)?$`).MatchString(line) {
		return "", true, nil
	}
	single := regexp.MustCompile(`\[\s*` + quoted + `\s*,?\s*\]`)
	if single.MatchString(line) {
		return single.ReplaceAllString(line, "[]"), false, nil
	}
	return "", false, fmt.Errorf("can't remove %q automatically: it shares the line with other code", literal)
}
