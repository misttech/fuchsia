// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme_fuchsia

import (
	"reflect"
	"testing"
)

func TestGetField(t *testing.T) {
	r := &Readme{
		Name:             "test_project",
		URL:              "http://test",
		SecurityCritical: "yes",
		Licenses:         []string{"MIT", "Apache-2.0"},
		LicenseFiles:     []string{"LICENSE", "NOTICE"},
		Description:      "Line 1\nLine 2",
		UnknownFields: []UnknownField{
			{Key: "Custom Key", Value: "Custom Value"},
		},
	}

	tests := []struct {
		key     string
		wantVal string
		wantOk  bool
	}{
		{"Name", "test_project", true},
		{"URL", "http://test", true},
		{"Security Critical", "yes", true},
		{"License", "MIT, Apache-2.0", true}, // Slice joined by separator (comma)
		{"License File", "LICENSE, NOTICE", true},
		{"Description", "Line 1\nLine 2", true},
		{"Custom Key", "Custom Value", true},
		{"Nonexistent Key", "", false},
	}

	for _, tc := range tests {
		gotVal, gotOk := r.GetField(tc.key)
		if gotOk != tc.wantOk {
			t.Errorf("GetField(%q) ok = %v, want %v", tc.key, gotOk, tc.wantOk)
		}
		if gotVal != tc.wantVal {
			t.Errorf("GetField(%q) = %q, want %q", tc.key, gotVal, tc.wantVal)
		}
	}
}

func TestSetField(t *testing.T) {
	r := &Readme{}

	// 1. Set standard field
	if err := r.SetField("Name", "new_name"); err != nil {
		t.Fatalf("SetField(Name) failed: %v", err)
	}
	if r.Name != "new_name" {
		t.Errorf("Expected Name to be 'new_name', got %q", r.Name)
	}

	// 2. Set slice field (should split, sort, and deduplicate)
	if err := r.SetField("License File", "NOTICE, LICENSE, LICENSE"); err != nil {
		t.Fatalf("SetField(License File) failed: %v", err)
	}
	expectedLFs := []string{"LICENSE", "NOTICE"}
	if !reflect.DeepEqual(r.LicenseFiles, expectedLFs) {
		t.Errorf("Expected LicenseFiles to be %v, got %v", expectedLFs, r.LicenseFiles)
	}

	// 3. Set unknown field (should add to UnknownFields)
	if err := r.SetField("New Custom Key", "New Value"); err != nil {
		t.Fatalf("SetField(New Custom Key) failed: %v", err)
	}
	if len(r.UnknownFields) != 1 {
		t.Fatalf("Expected 1 unknown field, got %d", len(r.UnknownFields))
	}
	if r.UnknownFields[0].Key != "New Custom Key" || r.UnknownFields[0].Value != "New Value" {
		t.Errorf("Unexpected unknown field: %+v", r.UnknownFields[0])
	}

	// 4. Update existing unknown field
	if err := r.SetField("New Custom Key", "Updated Value"); err != nil {
		t.Fatalf("SetField(New Custom Key) update failed: %v", err)
	}
	if len(r.UnknownFields) != 1 {
		t.Fatalf("Expected still 1 unknown field, got %d", len(r.UnknownFields))
	}
	if r.UnknownFields[0].Value != "Updated Value" {
		t.Errorf("Expected updated value, got %q", r.UnknownFields[0].Value)
	}
}

func TestGetField_Aliases(t *testing.T) {
	r := &Readme{
		UpstreamRevision:   "rev123",
		LocalModifications: "none",
	}

	// Test Upstream Revision aliases
	val, ok := r.GetField("Upstream Revision")
	if !ok || val != "rev123" {
		t.Errorf("GetField(Upstream Revision) = %q, %v; want 'rev123', true", val, ok)
	}
	val, ok = r.GetField("Upstream revision")
	if !ok || val != "rev123" {
		t.Errorf("GetField(Upstream revision) = %q, %v; want 'rev123', true", val, ok)
	}

	// Test Local Modifications aliases
	val, ok = r.GetField("Local Modifications")
	if !ok || val != "none" {
		t.Errorf("GetField(Local Modifications) = %q, %v; want 'none', true", val, ok)
	}
	val, ok = r.GetField("Modifications")
	if !ok || val != "none" {
		t.Errorf("GetField(Modifications) = %q, %v; want 'none', true", val, ok)
	}
}
