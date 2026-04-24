// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme

import (
	"bufio"
	"bytes"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

const dependencyDivider = "-------------------- DEPENDENCY DIVIDER --------------------"

// ParseFile reads a README.fuchsia file from disk and parses it into a slice of Readme structs.
func ParseFile(path string) ([]*Readme, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	return Parse(data)
}

var knownDirectives = map[string]bool{
	"Name":                true,
	"URL":                 true,
	"Version":             true,
	"Security Critical":   true,
	"Location":            true,
	"License File":        true,
	"Source File":         true,
	"Non-License File":    true,
	"Upstream Git":        true,
	"Description":         true,
	"Local Modifications": true,
	"Modifications":       true, // Legacy alias
	"Deprecated":          true, // Legacy
}

var knownFileDirectives = map[string]bool{
	"License":                      true,
	"License Type":                 true,
	"License File URL":             true,
	"Non-License File Explanation": true,
}

// isMultiLineField returns true if the given key is allowed to span multiple lines.
func isMultiLineField(key string) bool {
	return key == "Description" || key == "Local Modifications" || key == "Modifications" || key == "Deprecated"
}

// Parse extracts a slice of Readme structs from the given byte array,
// splitting by the standard DEPENDENCY DIVIDER if present.
func Parse(data []byte) ([]*Readme, error) {
	var readmes []*Readme

	blocks := bytes.Split(data, []byte(dependencyDivider))

	for _, block := range blocks {
		readme := &Readme{}
		var currentLicenseEntry *LicenseEntry
		var currentSourceEntry *LicenseEntry
		var currentNonLicenseEntry *NonLicenseEntry

		var currentKey string
		var currentValue strings.Builder

		scanner := bufio.NewScanner(bytes.NewReader(block))
		for scanner.Scan() {
			line := scanner.Text()
			trimmed := strings.TrimSpace(line)

			// Skip empty lines or comments ONLY if we are not actively parsing a multi-line field.
			if (trimmed == "" || strings.HasPrefix(trimmed, "#")) && !isMultiLineField(currentKey) {
				continue
			}

			// 1. Check if the line is a known file-level directive
			// These usually have leading whitespace or an arrow (e.g., " -> License Type: Android")
			isFileDirective := false
			if strings.HasPrefix(line, " ") || strings.HasPrefix(line, "\t") || strings.HasPrefix(line, "->") {
				cleanLine := strings.TrimLeft(line, " \t->")
				parts := strings.SplitN(cleanLine, ":", 2)
				if len(parts) == 2 {
					key := strings.TrimSpace(parts[0])
					if knownFileDirectives[key] {
						isFileDirective = true
						if currentLicenseEntry != nil {
							value := strings.TrimSpace(parts[1])
							switch key {
							case "License":
								currentLicenseEntry.License = value
							case "License Type":
								currentLicenseEntry.LicenseType = value
							case "License File URL":
								currentLicenseEntry.LicenseFileURL = value
							}
						} else if currentSourceEntry != nil {
							value := strings.TrimSpace(parts[1])
							switch key {
							case "License":
								currentSourceEntry.License = value
							case "License Type":
								currentSourceEntry.LicenseType = value
							case "License File URL":
								currentSourceEntry.LicenseFileURL = value
							}
						} else if currentNonLicenseEntry != nil {
							value := strings.TrimSpace(parts[1])
							switch key {
							case "Non-License File Explanation":
								currentNonLicenseEntry.Explanation = value
							}
						}
						currentKey = "" // File-level metadata interrupts any active multi-line field
						continue
					}
				}
			}
			if isFileDirective {
				continue
			}

			// 2. Check if the line is a new directive (Key: Value pair)
			isRootDirective := false
			parts := strings.SplitN(line, ":", 2)
			if len(parts) == 2 {
				key := strings.TrimSpace(parts[0])
				value := strings.TrimSpace(parts[1])

				if knownDirectives[key] {
					isRootDirective = true

					if key == "License File" || key == "Source File" || key == "Non-License File" {
						if currentLicenseEntry != nil {
							readme.LicenseFiles = append(readme.LicenseFiles, *currentLicenseEntry)
							currentLicenseEntry = nil
						}
						if currentSourceEntry != nil {
							readme.SourceFiles = append(readme.SourceFiles, *currentSourceEntry)
							currentSourceEntry = nil
						}
						if currentNonLicenseEntry != nil {
							readme.NonLicenseFiles = append(readme.NonLicenseFiles, *currentNonLicenseEntry)
							currentNonLicenseEntry = nil
						}

						if key == "License File" {
							currentLicenseEntry = &LicenseEntry{Path: value}
							if readme.LicenseFile == "" {
								readme.LicenseFile = value
							}
						} else if key == "Source File" {
							currentSourceEntry = &LicenseEntry{Path: value}
						} else if key == "Non-License File" {
							currentNonLicenseEntry = &NonLicenseEntry{Path: value}
						}
						currentKey = "" // reset multi-line tracking
						continue
					}

					currentKey = key
					currentValue.Reset()
					currentValue.WriteString(value)
					assignValue(readme, currentKey, currentValue.String())
					continue
				} else if !isMultiLineField(currentKey) {
					// We hit a Key: Value pair, it's not a known directive, and we are NOT
					// currently inside a multi-line field. This is an unknown field.
					readme.UnknownFields = append(readme.UnknownFields, UnknownField{
						Key:   key,
						Value: value,
					})
					currentKey = ""        // Ensure we don't accidentally treat the next line as part of this unknown field
					isRootDirective = true // Mark as handled
					continue
				}
			}

			// 3. Continuation of a multi-line value
			if !isRootDirective && currentKey != "" {
				// Only specific fields are allowed to be multi-line
				if !isMultiLineField(currentKey) {
					currentKey = ""
					continue
				}

				// If the line looks like a new unknown directive (Key: Value with no leading indent), stop.
				// However, lines like "Note: something" or "http://foo" can have colons.
				// We only consider it a new directive if it matches known patterns or is capitalized without spaces.
				parts := strings.SplitN(line, ":", 2)
				if len(parts) == 2 && !strings.HasPrefix(line, " ") && !strings.HasPrefix(line, "\t") {
					keyStr := strings.TrimSpace(parts[0])
					if keyStr != "" {
						// Heuristic: Is this actually a new field?
						// It is a new field if it's a known directive, OR if it has no spaces (like 'LicenseFile'),
						// OR if it's explicitly capitalized like a proper noun 'Unknown Field'.
						isField := false
						if knownDirectives[keyStr] || knownFileDirectives[keyStr] {
							isField = true
						} else if !strings.Contains(keyStr, " ") {
							isField = true
						} else {
							// E.g., "Unknown Field:" -> check if words are capitalized
							words := strings.Split(keyStr, " ")
							if len(words) > 0 && len(words[0]) > 0 && words[0][0] >= 'A' && words[0][0] <= 'Z' {
								// We assume it's an unknown field.
								// However, "Note: It has colons!" would trigger this.
								// Let's only assume it's a field if it has a space after the colon.
								if strings.HasPrefix(parts[1], " ") {
									isField = true
								}
							}
						}

						// "Note:" is a common false positive in descriptions
						if keyStr == "Note" {
							isField = false
						}

						if isField {
							// It's a new unknown field. Store it.
							readme.UnknownFields = append(readme.UnknownFields, UnknownField{
								Key:   keyStr,
								Value: strings.TrimSpace(parts[1]),
							})
							currentKey = ""
							continue
						}
					}
				}

				if trimmed == "" {
					currentValue.WriteString("\n")
				} else {
					// Strip up to 2 spaces of indentation from continuation lines
					unindented := line
					if strings.HasPrefix(unindented, "  ") {
						unindented = unindented[2:]
					} else if strings.HasPrefix(unindented, " ") {
						unindented = unindented[1:]
					}
					currentValue.WriteString("\n" + strings.TrimRight(unindented, " \t\r\n"))
				}
				assignValue(readme, currentKey, currentValue.String())
			}
		}

		if err := scanner.Err(); err != nil {
			return nil, fmt.Errorf("error scanning README bytes: %w", err)
		}

		// Commit the final License File entry
		if currentLicenseEntry != nil {
			readme.LicenseFiles = append(readme.LicenseFiles, *currentLicenseEntry)
		}
		if currentSourceEntry != nil {
			readme.SourceFiles = append(readme.SourceFiles, *currentSourceEntry)
		}
		if currentNonLicenseEntry != nil {
			readme.NonLicenseFiles = append(readme.NonLicenseFiles, *currentNonLicenseEntry)
		}

		// Only append if we actually parsed something useful (ignores empty trailing blocks)
		if readme.Name != "" || len(readme.LicenseFiles) > 0 || len(readme.SourceFiles) > 0 || len(readme.NonLicenseFiles) > 0 {
			readmes = append(readmes, readme)
		}
	}

	return readmes, nil
}

func assignValue(r *Readme, key, value string) {
	switch key {
	case "Name":
		r.Name = value
	case "URL":
		r.URL = value
	case "Version":
		r.Version = value
	case "Security Critical":
		r.SecurityCritical = value
	case "Location":
		r.Location = value
	case "Upstream Git":
		r.UpstreamGit = value
	case "Description":
		r.Description = value
	case "Local Modifications", "Modifications":
		r.LocalModifications = value
	}
}

// ParseAnyMetadata parses a metadata file based on its filename (README.fuchsia, go.mod, Cargo.toml, pubspec.yaml)
// and returns the root Readme (if any) and a slice of sub-project Readmes.
// For go.mod, there is no root readme, only sub-projects.
func ParseAnyMetadata(path string) (rootReadmes []*Readme, subReadmes []*Readme, err error) {
	base := filepath.Base(path)
	var readmes []*Readme

	if base == "go.mod" {
		readmes, err = ParseGoMod(path)
	} else if base == "Cargo.toml" {
		readmes, err = ParseCargoToml(path)
	} else if base == "pubspec.yaml" {
		readmes, err = ParsePubspecYaml(path)
	} else {
		readmes, err = ParseFile(path)
	}

	if err != nil || len(readmes) == 0 {
		return nil, nil, err
	}

	if base != "go.mod" {
		rootReadmes = append(rootReadmes, readmes[0])
	}

	startIdx := 1
	if base == "go.mod" || (base == "Cargo.toml" && readmes[0].Location != ".") {
		startIdx = 0
	}

	if startIdx < len(readmes) {
		subReadmes = append(subReadmes, readmes[startIdx:]...)
	}

	return rootReadmes, subReadmes, nil
}
