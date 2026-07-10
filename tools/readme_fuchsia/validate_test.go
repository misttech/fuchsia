// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme_fuchsia

import (
	"strings"
	"testing"
)

func TestValidate_ErrorLinks(t *testing.T) {
	tests := []struct {
		name         string
		readme       Readme
		expectedLink string
	}{
		{
			name: "missing name",
			readme: Readme{
				URL:              "https://example.com",
				Revision:         "1234",
				SecurityCritical: "no",
				Licenses:         []string{"MIT"},
				LicenseFiles:     []string{"LICENSE"},
			},
			expectedLink: "http://go/readme_fuchsia#name",
		},
		{
			name: "missing url and cpe",
			readme: Readme{
				Name:             "test",
				SecurityCritical: "no",
				Licenses:         []string{"MIT"},
				LicenseFiles:     []string{"LICENSE"},
			},
			expectedLink: "http://go/readme_fuchsia#url",
		},
		{
			name: "missing security critical",
			readme: Readme{
				Name:         "test",
				URL:          "https://example.com",
				Revision:     "1234",
				Licenses:     []string{"MIT"},
				LicenseFiles: []string{"LICENSE"},
			},
			expectedLink: "http://go/readme_fuchsia#security-critical",
		},
		{
			name: "invalid security critical",
			readme: Readme{
				Name:             "test",
				URL:              "https://example.com",
				Revision:         "1234",
				SecurityCritical: "maybe",
				Licenses:         []string{"MIT"},
				LicenseFiles:     []string{"LICENSE"},
			},
			expectedLink: "http://go/readme_fuchsia#security-critical",
		},
		{
			name: "missing license",
			readme: Readme{
				Name:             "test",
				URL:              "https://example.com",
				Revision:         "1234",
				SecurityCritical: "no",
				LicenseFiles:     []string{"LICENSE"},
			},
			expectedLink: "http://go/readme_fuchsia#license",
		},
		{
			name: "missing license file",
			readme: Readme{
				Name:             "test",
				URL:              "https://example.com",
				Revision:         "1234",
				SecurityCritical: "no",
				Licenses:         []string{"MIT"},
			},
			expectedLink: "http://go/readme_fuchsia#license-file",
		},
		{
			name: "unknown fields",
			readme: Readme{
				Name:             "test",
				URL:              "https://example.com",
				Revision:         "1234",
				SecurityCritical: "no",
				Licenses:         []string{"MIT"},
				LicenseFiles:     []string{"LICENSE"},
				UnknownFields:    []UnknownField{{Key: "Foo", Value: "Bar"}},
			},
			expectedLink: "http://go/readme_fuchsia#unknown-fields",
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			errs := Validate("", []*Readme{&tc.readme})
			if len(errs) == 0 {
				t.Fatalf("expected validation error, got none")
			}
			found := false
			for _, err := range errs {
				if strings.Contains(err.Error(), tc.expectedLink) {
					found = true
					break
				}
			}
			if !found {
				t.Errorf("expected error containing %q, got: %v", tc.expectedLink, errs)
			}
		})
	}
}
