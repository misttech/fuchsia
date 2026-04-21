// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme

import (
	"strings"
)

// Format takes a slice of Readme structs and serializes them into the
// canonical README.fuchsia text format, inserting the DEPENDENCY DIVIDER
// between sub-projects if multiple are present.
func Format(readmes []*Readme) string {
	var b strings.Builder

	for i, r := range readmes {
		if i > 0 {
			b.WriteString("\n" + dependencyDivider + "\n\n")
		}
		b.WriteString(formatSingle(r))
	}

	return b.String()
}

func formatSingle(r *Readme) string {
	var b strings.Builder

	writeField(&b, "Name", r.Name)
	writeField(&b, "URL", r.URL)
	writeField(&b, "Version", r.Version)
	writeField(&b, "Security Critical", r.SecurityCritical)
	writeField(&b, "Location", r.Location)
	writeField(&b, "Upstream Git", r.UpstreamGit)

	hasLicenses := len(r.LicenseFiles) > 0 || r.LicenseFile != "" || len(r.SourceFiles) > 0 || len(r.NonLicenseFiles) > 0
	if hasLicenses {
		b.WriteString("\n")
	}

	for i, lf := range r.LicenseFiles {
		if lf.Path != "" {
			if i > 0 {
				b.WriteString("\n")
			}
			b.WriteString("License File: " + lf.Path + "\n")
			if lf.License != "" {
				b.WriteString("  License: " + lf.License + "\n")
			}
			if lf.LicenseType != "" && lf.LicenseType != "Single License" {
				b.WriteString("  License Type: " + lf.LicenseType + "\n")
			}
			if lf.LicenseFileURL != "" {
				b.WriteString("  License File URL: " + lf.LicenseFileURL + "\n")
			}
		}
	}

	for i, sf := range r.SourceFiles {
		if sf.Path != "" {
			if i > 0 || len(r.LicenseFiles) > 0 {
				b.WriteString("\n")
			}
			b.WriteString("Source File: " + sf.Path + "\n")
			if sf.License != "" {
				b.WriteString("  License: " + sf.License + "\n")
			}
			if sf.LicenseType != "" && sf.LicenseType != "Single License" {
				b.WriteString("  License Type: " + sf.LicenseType + "\n")
			}
			if sf.LicenseFileURL != "" {
				b.WriteString("  License File URL: " + sf.LicenseFileURL + "\n")
			}
		}
	}

	for i, nlf := range r.NonLicenseFiles {
		if nlf.Path != "" {
			if i > 0 || len(r.LicenseFiles) > 0 || len(r.SourceFiles) > 0 {
				b.WriteString("\n")
			}
			b.WriteString("Non-License File: " + nlf.Path + "\n")
			if nlf.Explanation != "" {
				b.WriteString("  Non-License File Explanation: " + nlf.Explanation + "\n")
			}
		}
	}

	// Fallback to legacy single file if LicenseFiles array is empty
	if len(r.LicenseFiles) == 0 && r.LicenseFile != "" {
		if len(r.NonLicenseFiles) > 0 || len(r.SourceFiles) > 0 {
			b.WriteString("\n")
		}
		b.WriteString("License File: " + r.LicenseFile + "\n")
	}

	writeMultiLineField(&b, "Description", r.Description)
	writeMultiLineField(&b, "Local Modifications", r.LocalModifications)

	// Append any unknown fields at the very bottom so they aren't lost
	if len(r.UnknownFields) > 0 {
		b.WriteString("\n")
	}
	for _, unknown := range r.UnknownFields {
		writeField(&b, unknown.Key, unknown.Value)
	}

	return strings.TrimRight(b.String(), "\n") + "\n"
}

func writeField(b *strings.Builder, key, value string) {
	if value != "" {
		b.WriteString(key + ": " + value + "\n")
	}
}

func writeMultiLineField(b *strings.Builder, key, value string) {
	if value != "" {
		b.WriteString("\n" + key + ":\n")

		// Remove all leading and trailing whitespace/newlines from the value itself
		value = strings.TrimSpace(value)

		// Add a 2-space indentation to every line in the multiline string
		lines := strings.Split(value, "\n")
		for _, line := range lines {
			if line == "" || strings.TrimSpace(line) == "" {
				b.WriteString("\n")
			} else {
				b.WriteString("  " + line + "\n")
			}
		}
	}
}
