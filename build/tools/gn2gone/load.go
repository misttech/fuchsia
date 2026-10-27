// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package gn2gone

import (
	"context"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"go.fuchsia.dev/fuchsia/build/tools/bazel2gn"
)

// LoadOptions configures Load.
type LoadOptions struct {
	// Root is the Fuchsia root directory.
	Root      string
	GNBin     string
	BatchSize int
	Workers   int
}

// Stats summarizes a Load.
type Stats struct {
	Files       int
	Literals    int
	ParseTime   time.Duration
	TotalTime   time.Duration
	ParseErrors []*ParseError
	// Unattributed are dynamic strings that may name sentinel directories
	// but block none (see Resolver.Unattributed).
	Unattributed []*Literal
}

// ListGNFiles returns every .gn and .gni file under `root`, relative to it.
// It skips the same directories as bazel2gn's sentinel discovery
// (bazel2gn.SkipDiscovery) and doesn't follow symlinks.
//
// That includes prebuilt/. GN does load a few BUILD.gn files there (e.g.
// //prebuilt/third_party/pydantic-core), but they come from CIPD packages,
// and none references a synced target. The only references to sentinel
// directories under prebuilt/ are in the Go toolchain's copy of Fuchsia
// BUILD.gn files, which GN never loads; scanning them only added false
// blockers.
func ListGNFiles(root string) ([]string, error) {
	var files []string
	err := filepath.WalkDir(root, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(root, p)
		if err != nil {
			return err
		}
		if d.IsDir() {
			if bazel2gn.SkipDiscovery(rel) {
				return filepath.SkipDir
			}
			return nil
		}
		if !d.Type().IsRegular() {
			return nil
		}
		if strings.HasSuffix(d.Name(), ".gn") || strings.HasSuffix(d.Name(), ".gni") {
			files = append(files, filepath.ToSlash(rel))
		}
		return nil
	})
	sort.Strings(files)
	return files, err
}

// Load parses and indexes every GN file under opts.Root.
func Load(ctx context.Context, opts LoadOptions) (*Index, *Stats, error) {
	start := time.Now()
	files, err := ListGNFiles(opts.Root)
	if err != nil {
		return nil, nil, err
	}
	parseStart := time.Now()
	trees, perrs := ParseFiles(ctx, files, ParseOptions{
		GNBin: opts.GNBin, Root: opts.Root, BatchSize: opts.BatchSize, Workers: opts.Workers,
	})
	parseTime := time.Since(parseStart)
	var indexes []*FileIndex
	lits := 0
	for _, f := range files {
		tree, ok := trees[f]
		if !ok {
			continue
		}
		sl := 0
		// Files are listed with bazel2gn.SkipDiscovery, so sentinel directories
		// are exactly the ones bazel2gn syncs.
		if filepath.Base(f) == "BUILD.gn" {
			content, err := os.ReadFile(filepath.Join(opts.Root, f))
			if err != nil {
				return nil, nil, err
			}
			sl = FindSentinelLine(content)
		}
		fi := IndexFile(f, sl, tree)
		lits += len(fi.Literals)
		indexes = append(indexes, fi)
	}
	idx := BuildIndex(opts.Root, indexes)
	return idx, &Stats{
		Files:        len(files),
		Literals:     lits,
		ParseTime:    parseTime,
		TotalTime:    time.Since(start),
		ParseErrors:  perrs,
		Unattributed: idx.Resolver.Unattributed(),
	}, nil
}
