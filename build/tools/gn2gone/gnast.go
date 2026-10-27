// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package gn2gone

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os/exec"
	"path/filepath"
	"sync"
)

// Location is a node's position in a GN file, as reported by
// `gn format --dump-tree=json`. Lines and columns are 1-based.
type Location struct {
	BeginLine   int `json:"begin_line"`
	BeginColumn int `json:"begin_column"`
	EndLine     int `json:"end_line"`
	EndColumn   int `json:"end_column"`
}

// Node is a GN parse tree node, as reported by `gn format --dump-tree=json`.
// The tree is syntax only: GN does not evaluate variables, conditions, or
// imports when dumping it.
type Node struct {
	// Type is the node type, e.g. "BLOCK", "FUNCTION", "BINARY", "LITERAL",
	// "IDENTIFIER", "LIST", "CONDITION", "ACCESSOR", "BLOCK_COMMENT".
	Type string `json:"type"`
	// Value is the function name for FUNCTION, the operator for BINARY,
	// the identifier for IDENTIFIER, and the source text (including quotes
	// for strings) for LITERAL.
	Value    string    `json:"value"`
	Child    []*Node   `json:"child"`
	Location *Location `json:"location"`
}

// Line returns the node's first line, or 0 if unknown.
func (n *Node) Line() int {
	if n == nil || n.Location == nil {
		return 0
	}
	return n.Location.BeginLine
}

// Column returns the node's first column, or 0 if unknown.
func (n *Node) Column() int {
	if n == nil || n.Location == nil {
		return 0
	}
	return n.Location.BeginColumn
}

// ParseOptions configures ParseFiles.
type ParseOptions struct {
	// GNBin is the path to the gn binary.
	GNBin string
	// Root is the directory that file paths are relative to.
	Root string
	// BatchSize is the number of files passed to one gn process. One
	// unreadable or unparsable file makes gn fail the whole batch, in which
	// case the batch is retried one file at a time.
	BatchSize int
	// Workers is the number of gn processes to run in parallel.
	Workers int
}

// ParseError records a file that gn could not parse.
type ParseError struct {
	Path string
	Err  error
}

func (e *ParseError) Error() string { return fmt.Sprintf("parsing %s: %v", e.Path, e.Err) }

// ParseFiles parses `files` (relative to opts.Root) with
// `gn format --dump-tree=json`. It returns the parse trees by path, and
// errors for files that could not be parsed.
func ParseFiles(ctx context.Context, files []string, opts ParseOptions) (map[string]*Node, []*ParseError) {
	if opts.BatchSize <= 0 {
		opts.BatchSize = 50
	}
	if opts.Workers <= 0 {
		opts.Workers = 1
	}
	var batches [][]string
	for i := 0; i < len(files); i += opts.BatchSize {
		batches = append(batches, files[i:min(i+opts.BatchSize, len(files))])
	}

	var mu sync.Mutex
	trees := make(map[string]*Node, len(files))
	var errs []*ParseError
	sem := make(chan struct{}, opts.Workers)
	var wg sync.WaitGroup
	for _, b := range batches {
		wg.Add(1)
		sem <- struct{}{}
		go func(batch []string) {
			defer wg.Done()
			defer func() { <-sem }()
			got, err := dumpTrees(ctx, opts, batch)
			if err == nil {
				mu.Lock()
				for i, f := range batch {
					trees[f] = got[i]
				}
				mu.Unlock()
				return
			}
			// Retry one at a time to isolate the bad file(s).
			for _, f := range batch {
				got, err := dumpTrees(ctx, opts, []string{f})
				mu.Lock()
				if err != nil {
					errs = append(errs, &ParseError{Path: f, Err: err})
				} else {
					trees[f] = got[0]
				}
				mu.Unlock()
			}
		}(b)
	}
	wg.Wait()
	return trees, errs
}

// dumpTrees runs gn once over `batch` and decodes one tree per file.
func dumpTrees(ctx context.Context, opts ParseOptions, batch []string) ([]*Node, error) {
	args := []string{"format", "--dump-tree=json"}
	for _, f := range batch {
		args = append(args, filepath.Join(opts.Root, f))
	}
	cmd := exec.CommandContext(ctx, opts.GNBin, args...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return nil, fmt.Errorf("gn format --dump-tree=json failed (%v): %s", err, bytes.TrimSpace(stderr.Bytes()))
	}
	dec := json.NewDecoder(&stdout)
	var trees []*Node
	for {
		var n Node
		err := dec.Decode(&n)
		if errors.Is(err, io.EOF) {
			break
		}
		if err != nil {
			return nil, fmt.Errorf("decoding gn parse tree: %v", err)
		}
		trees = append(trees, &n)
	}
	if len(trees) != len(batch) {
		return nil, fmt.Errorf("gn returned %d parse trees for %d files", len(trees), len(batch))
	}
	return trees, nil
}
