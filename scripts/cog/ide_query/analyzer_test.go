// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"encoding/json"
	"os"
	"path/filepath"
	"reflect"
	"testing"
)

func TestSplitCommand(t *testing.T) {
	tests := []struct {
		cmd      string
		expected []string
	}{
		{
			cmd:      "clang++ -o obj/file.o -c file.cc",
			expected: []string{"clang++", "-o", "obj/file.o", "-c", "file.cc"},
		},
		{
			cmd:      `clang++ "-DQUOTED_ARG" -o "obj/path with spaces.o"`,
			expected: []string{"clang++", "-DQUOTED_ARG", "-o", "obj/path with spaces.o"},
		},
		{
			cmd:      `clang++ 'single quoted' -DFOO=\"bar\"`,
			expected: []string{"clang++", "single quoted", "-DFOO=\"bar\""},
		},
		{
			cmd:      "arg1\\ with\\ space arg2",
			expected: []string{"arg1 with space", "arg2"},
		},
	}

	for _, test := range tests {
		result := splitCommand(test.cmd)
		if !reflect.DeepEqual(result, test.expected) {
			t.Errorf("splitCommand(%q) = %v; want %v", test.cmd, result, test.expected)
		}
	}
}

func TestExtractOutput(t *testing.T) {
	tests := []struct {
		cmd      CompileCommand
		expected string
	}{
		{
			cmd: CompileCommand{
				Directory: "/build",
				Arguments: []string{"clang++", "-o", "out.o", "in.cc"},
			},
			expected: "/build/out.o",
		},
		{
			cmd: CompileCommand{
				Directory: "/build",
				Command:   "clang++ -o /abs/out.o in.cc",
			},
			expected: "/abs/out.o",
		},
	}

	for _, test := range tests {
		result, err := ExtractOutput(test.cmd)
		if err != nil {
			t.Errorf("ExtractOutput failed: %v", err)
		}
		if result != test.expected {
			t.Errorf("ExtractOutput = %q; want %q", result, test.expected)
		}
	}
}

func TestPopulateTargets(t *testing.T) {
	// Create a temporary business logic context.
	tmpDir, err := os.MkdirTemp("", "ide-query-test")
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(tmpDir)

	fuchsiaDir := filepath.Join(tmpDir, "fuchsia")
	buildDir := filepath.Join(fuchsiaDir, "out/default")
	os.MkdirAll(buildDir, 0755)

	sourceFile := filepath.Join(fuchsiaDir, "src/main.cc")
	os.MkdirAll(filepath.Dir(sourceFile), 0755)
	os.WriteFile(sourceFile, []byte("void main() {}"), 0644)

	// Create a mock compile_commands.json
	compDb := []CompileCommand{
		{
			Directory: buildDir,
			File:      "../../src/main.cc",
			Arguments: []string{"clang++", "-o", "obj/main.o", "-c", "../../src/main.cc"},
		},
	}
	dbContent, _ := json.Marshal(compDb)
	os.WriteFile(filepath.Join(buildDir, "compile_commands.json"), dbContent, 0644)

	// Mock the Build API resolver.
	oldResolve := resolveNinjaPath
	defer func() { resolveNinjaPath = oldResolve }()
	resolveNinjaPath = func(ctx *WorkspaceContext, ninjaPath string) (string, error) {
		if ninjaPath == "obj/main.o" {
			return "//src:main_target", nil
		}
		return "", nil
	}

	ctx := &WorkspaceContext{
		FuchsiaDir: fuchsiaDir,
		BuildDir:   buildDir,
		Files: []FileEntry{
			{
				AbsPath: sourceFile,
				Status:  StatusFound,
			},
		},
	}

	if err := ctx.PopulateTargets(); err != nil {
		t.Fatalf("PopulateTargets failed: %v", err)
	}

	if len(ctx.Files[0].BuildTargets) != 1 || ctx.Files[0].BuildTargets[0] != "//src:main_target" {
		t.Errorf("expected //src:main_target, got %v", ctx.Files[0].BuildTargets)
	}
}

func TestPopulateTargets_NestedHeader(t *testing.T) {
	tmpDir, err := os.MkdirTemp("", "ide-query-test-nested")
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(tmpDir)

	fuchsiaDir := filepath.Join(tmpDir, "fuchsia")
	buildDir := filepath.Join(fuchsiaDir, "out/default")
	os.MkdirAll(buildDir, 0755)

	sourceFile := filepath.Join(fuchsiaDir, "zircon/kernel/lib/arch/x86/standard-segments.cc")
	headerFile := filepath.Join(fuchsiaDir, "zircon/kernel/lib/arch/x86/include/lib/arch/x86/standard-segments.h")
	os.MkdirAll(filepath.Dir(sourceFile), 0755)
	os.MkdirAll(filepath.Dir(headerFile), 0755)
	os.WriteFile(sourceFile, []byte("void f() {}"), 0644)
	os.WriteFile(headerFile, []byte("void f();"), 0644)

	compDb := []CompileCommand{
		{
			Directory: buildDir,
			File:      "../../zircon/kernel/lib/arch/x86/standard-segments.cc",
			Arguments: []string{"clang++", "-o", "obj/zircon/kernel/lib/arch/x86/standard-segments.o", "-c", "../../zircon/kernel/lib/arch/x86/standard-segments.cc"},
		},
	}
	dbContent, _ := json.Marshal(compDb)
	os.WriteFile(filepath.Join(buildDir, "compile_commands.json"), dbContent, 0644)

	oldResolve := resolveNinjaPath
	defer func() { resolveNinjaPath = oldResolve }()
	resolveNinjaPath = func(ctx *WorkspaceContext, ninjaPath string) (string, error) {
		if ninjaPath == "obj/zircon/kernel/lib/arch/x86/standard-segments.o" {
			return "//zircon/kernel/lib/arch/x86:x86", nil
		}
		return "", nil
	}

	ctx := &WorkspaceContext{
		FuchsiaDir: fuchsiaDir,
		BuildDir:   buildDir,
		Files: []FileEntry{
			{
				AbsPath: headerFile,
				Status:  StatusFound,
			},
		},
	}

	if err := ctx.PopulateTargets(); err != nil {
		t.Fatalf("PopulateTargets failed: %v", err)
	}

	if len(ctx.Files[0].BuildTargets) != 1 || ctx.Files[0].BuildTargets[0] != "//zircon/kernel/lib/arch/x86:x86" {
		t.Errorf("expected //zircon/kernel/lib/arch/x86:x86, got %v", ctx.Files[0].BuildTargets)
	}
}

func TestPopulateTargets_DeterministicNeighbor(t *testing.T) {
	tmpDir, err := os.MkdirTemp("", "ide-query-test-determinism")
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(tmpDir)

	fuchsiaDir := filepath.Join(tmpDir, "fuchsia")
	buildDir := filepath.Join(fuchsiaDir, "out/default")
	os.MkdirAll(buildDir, 0755)

	fileA := filepath.Join(fuchsiaDir, "src/a.cc")
	fileB := filepath.Join(fuchsiaDir, "src/b.cc")
	headerFile := filepath.Join(fuchsiaDir, "src/header.h")
	os.MkdirAll(filepath.Dir(fileA), 0755)
	os.WriteFile(fileA, []byte(""), 0644)
	os.WriteFile(fileB, []byte(""), 0644)
	os.WriteFile(headerFile, []byte(""), 0644)

	// In the compdb, A comes before B.
	// dirToCmd["src"] should be A's command.
	compDb := []CompileCommand{
		{
			Directory: buildDir,
			File:      "../../src/a.cc",
			Arguments: []string{"clang++", "-o", "obj/a.o", "-c", "../../src/a.cc"},
		},
		{
			Directory: buildDir,
			File:      "../../src/b.cc",
			Arguments: []string{"clang++", "-o", "obj/b.o", "-c", "../../src/b.cc"},
		},
	}
	dbContent, _ := json.Marshal(compDb)
	os.WriteFile(filepath.Join(buildDir, "compile_commands.json"), dbContent, 0644)

	oldResolve := resolveNinjaPath
	defer func() { resolveNinjaPath = oldResolve }()
	resolveNinjaPath = func(ctx *WorkspaceContext, ninjaPath string) (string, error) {
		if ninjaPath == "obj/a.o" {
			return "//src:a", nil
		}
		if ninjaPath == "obj/b.o" {
			return "//src:b", nil
		}
		return "", nil
	}

	ctx := &WorkspaceContext{
		FuchsiaDir: fuchsiaDir,
		BuildDir:   buildDir,
		Files: []FileEntry{
			{
				AbsPath: headerFile,
				Status:  StatusFound,
			},
		},
	}

	if err := ctx.PopulateTargets(); err != nil {
		t.Fatalf("PopulateTargets failed: %v", err)
	}

	// Should favor the first one in the list (//src:a) because we only
	// set the dirToCmd map if it's not already present.
	if len(ctx.Files[0].BuildTargets) != 1 || ctx.Files[0].BuildTargets[0] != "//src:a" {
		t.Errorf("expected //src:a, got %v", ctx.Files[0].BuildTargets)
	}
}

func TestPopulateTargets_MultipleTargets(t *testing.T) {
	tmpDir, err := os.MkdirTemp("", "ide-query-test-multi")
	if err != nil {
		t.Fatal(err)
	}
	defer os.RemoveAll(tmpDir)

	fuchsiaDir := filepath.Join(tmpDir, "fuchsia")
	buildDir := filepath.Join(fuchsiaDir, "out/default")
	os.MkdirAll(buildDir, 0755)

	sourceFile := filepath.Join(fuchsiaDir, "src/main.cc")
	os.MkdirAll(filepath.Dir(sourceFile), 0755)
	os.WriteFile(sourceFile, []byte(""), 0644)

	// Create a mock compile_commands.json with TWO entries for the same file.
	compDb := []CompileCommand{
		{
			Directory: buildDir,
			File:      "../../src/main.cc",
			Arguments: []string{"clang++", "-o", "obj/foo.o", "-c", "../../src/main.cc"},
		},
		{
			Directory: buildDir,
			File:      "../../src/main.cc",
			Arguments: []string{"clang++", "-o", "obj/bar.o", "-c", "../../src/main.cc"},
		},
	}
	dbContent, _ := json.Marshal(compDb)
	os.WriteFile(filepath.Join(buildDir, "compile_commands.json"), dbContent, 0644)

	oldResolve := resolveNinjaPath
	defer func() { resolveNinjaPath = oldResolve }()
	resolveNinjaPath = func(ctx *WorkspaceContext, ninjaPath string) (string, error) {
		if ninjaPath == "obj/foo.o" {
			return "//src:foo", nil
		}
		if ninjaPath == "obj/bar.o" {
			return "//src:bar", nil
		}
		return "", nil
	}

	ctx := &WorkspaceContext{
		FuchsiaDir: fuchsiaDir,
		BuildDir:   buildDir,
		Files: []FileEntry{
			{
				AbsPath: sourceFile,
				Status:  StatusFound,
			},
		},
	}

	if err := ctx.PopulateTargets(); err != nil {
		t.Fatalf("PopulateTargets failed: %v", err)
	}

	// Should pick //src:bar because it's alphabetically before //src:foo
	if len(ctx.Files[0].BuildTargets) != 1 || ctx.Files[0].BuildTargets[0] != "//src:bar" {
		t.Errorf("expected //src:bar, got %v", ctx.Files[0].BuildTargets)
	}
}
