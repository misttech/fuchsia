// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package util

import (
	"encoding/json"
	"fmt"
	"os"
	"regexp"
	"strings"
)

type Gen struct {
	Targets   map[string]*Target `json:"targets"`
	IsCleaned bool               `json:"cleaned"`

	re *regexp.Regexp `json:"-"`
}

func LoadGen(path string) (*Gen, error) {
	b, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("Failed to read [%s] gen file: %w\n", path, err)
	}

	gen := Gen{
		Targets: make(map[string]*Target),

		// Many rust_crate projects have a suffix in the label name that
		// doesn't map to a directory. We use a regular expression to
		// strip that part of the label text away. We store the regexp
		// in this GN struct so we don't have to recompile the regex on
		// each loop.
		re: regexp.MustCompile(`(.*)-v(\d+)_(\d+)_(\d+)(.*)`),
	}
	d := json.NewDecoder(strings.NewReader(string(b)))
	if err := d.Decode(&gen); err != nil {
		return nil, fmt.Errorf("Failed to decode [%s] into struct object: %w", path, err)
	}
	return &gen, nil
}
func (g *Gen) clean() error {
	if g.IsCleaned {
		return fmt.Errorf("gen file is already cleaned.")
	}

	toAdd := make(map[string]*Target, 0)
	for name, t := range g.Targets {
		t.Name = name
		if err := t.Clean(g.re); err != nil {
			return fmt.Errorf("Failed to clean target %v: %w", t, err)
		}
		for _, n := range t.CleanNames {
			toAdd[n] = t
		}
	}

	for k, v := range toAdd {
		if _, ok := g.Targets[k]; !ok {
			g.Targets[k] = v
		}
	}

	g.IsCleaned = true
	return nil
}
