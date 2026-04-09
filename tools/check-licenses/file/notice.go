// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package file

import (
	"bytes"
)

type Data struct {
	LibraryName string `json:"libraryName"`
	LicenseText []byte `json:"licenseText"`
	LineNumber  int    `json:"lineNumber"`
}

func mergeDuplicates(licenses []*Data) []*Data {
	set := make(map[string]map[string]bool)
	dedupedLicenses := make([]*Data, 0, len(licenses))

	for _, l := range licenses {
		if _, ok := set[l.LibraryName]; !ok {
			set[l.LibraryName] = make(map[string]bool)
		}

		// Map string keys naturally perform allocation-free lookups for byte slices
		// under the hood in Go via a compiler optimization, saving memory.
		if !set[l.LibraryName][string(l.LicenseText)] {
			set[l.LibraryName][string(l.LicenseText)] = true
			dedupedLicenses = append(dedupedLicenses, l)
		}
	}

	return dedupedLicenses
}

// ParseAndroid extracts licenses from Android NOTICE files.
func ParseAndroid(path string, content []byte) ([]*Data, error) {
	var licenses []*Data
	content = bytes.ReplaceAll(content, []byte("\r\n"), []byte("\n"))
	blocks := bytes.Split(content, []byte("==========================================================="))

	for i := 0; i < len(blocks)-1; i++ {
		block := bytes.TrimSpace(blocks[i])
		if bytes.Contains(block, []byte("The notices is included for the library:")) {
			text := bytes.TrimSpace(blocks[i+1])

			libName := ""
			for _, line := range bytes.Split(block, []byte("\n")) {
				if bytes.Contains(line, []byte("The notices is included for the library:")) {
					parts := bytes.SplitN(line, []byte(":"), 2)
					if len(parts) >= 2 {
						libName = string(bytes.TrimSpace(parts[1]))
					}
				}
			}

			licenses = append(licenses, &Data{
				LibraryName: libName,
				LicenseText: text,
				LineNumber:  i,
			})
		}
	}
	return mergeDuplicates(licenses), nil
}

// ParseChromium extracts licenses from Chromium NOTICE files.
func ParseChromium(path string, content []byte) ([]*Data, error) {
	var licenses []*Data
	content = bytes.ReplaceAll(content, []byte("\r\n"), []byte("\n"))
	blocks := bytes.Split(content, []byte("--------------------"))

	if len(blocks) > 0 {
		text := bytes.TrimSpace(blocks[0])
		if len(text) > 0 {
			licenses = append(licenses, &Data{
				LibraryName: "Chromium",
				LicenseText: text,
				LineNumber:  0,
			})
		}
	}

	for i := 1; i < len(blocks)-1; i += 2 {
		libName := string(bytes.TrimSpace(blocks[i]))
		text := bytes.TrimSpace(blocks[i+1])
		if len(text) > 0 {
			licenses = append(licenses, &Data{
				LibraryName: libName,
				LicenseText: text,
				LineNumber:  i,
			})
		}
	}
	return mergeDuplicates(licenses), nil
}

// ParseFlutter extracts licenses from Flutter NOTICE files.
func ParseFlutter(path string, content []byte) ([]*Data, error) {
	var licenses []*Data
	content = bytes.ReplaceAll(content, []byte("\r\n"), []byte("\n"))
	blocks := bytes.Split(content, []byte("===================================================================================================="))

	for i, block := range blocks {
		block = bytes.TrimSpace(block)
		if len(block) == 0 {
			continue
		}

		parts := bytes.Split(block, []byte("----------------------------------------------------------------------------------------------------"))
		if len(parts) < 2 {
			continue
		}

		header := bytes.TrimSpace(parts[0])
		text := bytes.TrimSpace(parts[1])

		libName := ""
		for _, line := range bytes.Split(header, []byte("\n")) {
			if bytes.HasPrefix(line, []byte("LIBRARY:")) {
				libName = string(bytes.TrimSpace(bytes.TrimPrefix(line, []byte("LIBRARY:"))))
			}
		}

		textParts := bytes.SplitN(text, []byte("END OF TERMS AND CONDITIONS"), 2)
		text = bytes.TrimSpace(textParts[0])

		licenses = append(licenses, &Data{
			LibraryName: libName,
			LicenseText: text,
			LineNumber:  i,
		})
	}
	return mergeDuplicates(licenses), nil
}

// ParseGoogle extracts licenses from Google NOTICE files.
func ParseGoogle(path string, content []byte) ([]*Data, error) {
	var licenses []*Data
	content = bytes.ReplaceAll(content, []byte("\r\n"), []byte("\n"))
	blocks := bytes.Split(content, []byte("================="))

	for i, block := range blocks {
		block = bytes.TrimSpace(block)
		if len(block) == 0 {
			continue
		}
		parts := bytes.SplitN(block, []byte("\n"), 2)
		libName := string(bytes.TrimSpace(parts[0]))
		text := []byte("")
		if len(parts) > 1 {
			text = bytes.TrimSpace(parts[1])
		}
		licenses = append(licenses, &Data{
			LibraryName: libName,
			LicenseText: text,
			LineNumber:  i,
		})
	}
	return mergeDuplicates(licenses), nil
}

// ParseOneDelimiter extracts licenses from generic single-delimiter NOTICE files.
func ParseOneDelimiter(path string, content []byte) ([]*Data, error) {
	var licenses []*Data
	content = bytes.ReplaceAll(content, []byte("\r\n"), []byte("\n"))
	blocks := bytes.Split(content, []byte("--------------------------------------------------------------------------------"))

	for i, block := range blocks {
		block = bytes.TrimSpace(block)
		if len(block) == 0 {
			continue
		}

		parts := bytes.SplitN(block, []byte("\n\n"), 2)
		libName := string(bytes.TrimSpace(parts[0]))
		text := []byte("")
		if len(parts) > 1 {
			text = bytes.TrimSpace(parts[1])
		} else {
			text = parts[0]
			libName = ""
		}

		licenses = append(licenses, &Data{
			LibraryName: libName,
			LicenseText: text,
			LineNumber:  i,
		})
	}
	return mergeDuplicates(licenses), nil
}
