// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package file

import (
	"bytes"
	"embed"
	"os"
	"path/filepath"
	"testing"

	"github.com/google/go-cmp/cmp"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/testutil"
)

//go:embed testdata/example_*
var testDataFS embed.FS

var standardExpected = []*Data{
	{
		LibraryName: "First Library Name",
		LicenseText: []byte("First license text\nLorum Ipsum Dolor"),
	},
	{
		LibraryName: "Second Library Name",
		LicenseText: []byte("/* More License Text\n * All rights reserved.\n */"),
	},
}

var chromiumExpected = []*Data{
	{
		LibraryName: "Chromium",
		LicenseText: []byte("// Copyright 2015 The Example Authors."),
	},
	{
		LibraryName: "First Library Name",
		LicenseText: []byte("First license text\nLorum Ipsum Dolor"),
	},
	{
		LibraryName: "Second Library Name",
		LicenseText: []byte("/* More License Text\n * All rights reserved.\n */"),
	},
}

func TestParsers(t *testing.T) {
	tempDir := t.TempDir()
	testutil.DumpTestData(t, testDataFS, tempDir)
	testDataDir := filepath.Join(tempDir, "testdata")

	tests := []struct {
		name     string
		filename string
		parser   func(string, []byte) ([]*Data, error)
		expected []*Data
	}{
		{
			name:     "Android",
			filename: "example_android",
			parser:   ParseAndroid,
			expected: standardExpected,
		},
		{
			name:     "Chromium",
			filename: "example_chromium",
			parser:   ParseChromium,
			expected: chromiumExpected,
		},
		{
			name:     "Flutter",
			filename: "example_flutter",
			parser:   ParseFlutter,
			expected: standardExpected,
		},
		{
			name:     "Google",
			filename: "example_google",
			parser:   ParseGoogle,
			expected: standardExpected,
		},
		{
			name:     "OneDelimiter",
			filename: "example_onedelimiter",
			parser:   ParseOneDelimiter,
			expected: standardExpected,
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			path := filepath.Join(testDataDir, tt.filename)
			content, err := os.ReadFile(path)
			if err != nil {
				t.Fatalf("Failed to read testdata file: %v", err)
			}

			got, err := tt.parser(path, content)
			if err != nil {
				t.Fatalf("Parser returned error: %v", err)
			}

			// Clean up output for robust comparison
			for _, d := range got {
				d.LicenseText = bytes.TrimSpace(d.LicenseText)
				d.LibraryName = string(bytes.TrimSpace([]byte(d.LibraryName)))
				d.LineNumber = 0
			}
			for _, d := range tt.expected {
				d.LineNumber = 0
			}

			if diff := cmp.Diff(tt.expected, got); diff != "" {
				t.Errorf("Parsed data mismatch (-want +got):\n%s", diff)
			}
		})
	}
}
