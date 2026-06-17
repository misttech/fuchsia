// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package readme_fuchsia

import (
	"bufio"
	"bytes"
	"fmt"
	"os"
	"reflect"
	"sort"
	"strings"
)

const dependencyDivider = "-------------------- DEPENDENCY DIVIDER --------------------"

type fieldMeta struct {
	Index     int
	Multiline bool
	Separator string
	IsSlice   bool
}

var directiveMap map[string]fieldMeta

func init() {
	directiveMap = make(map[string]fieldMeta)
	t := reflect.TypeOf(Readme{})
	for i := 0; i < t.NumField(); i++ {
		f := t.Field(i)
		readmeTag := f.Tag.Get("readme")
		if readmeTag == "" || readmeTag == "-" {
			continue
		}

		multiline := f.Tag.Get("multiline") == "true"
		separator := f.Tag.Get("separator")
		isSlice := f.Type.Kind() == reflect.Slice

		// The readme tag can be comma-separated aliases, e.g. "Local Modifications,Modifications"
		aliases := strings.Split(readmeTag, ",")
		for _, alias := range aliases {
			directiveMap[alias] = fieldMeta{
				Index:     i,
				Multiline: multiline,
				Separator: separator,
				IsSlice:   isSlice,
			}
		}
	}
}

// ParseFile reads a README.fuchsia file from disk and parses it into a slice of Readme structs.
func ParseFile(path string) ([]*Readme, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	return Parse(data)
}

// deduplicateAndSort takes a slice of strings, trims whitespace, removes empties and duplicates, and sorts them.
func deduplicateAndSort(items []string) []string {
	seen := make(map[string]bool)
	var result []string
	for _, item := range items {
		trimmed := strings.TrimSpace(item)
		if trimmed != "" && !seen[trimmed] {
			seen[trimmed] = true
			result = append(result, trimmed)
		}
	}
	sort.Strings(result)
	return result
}

// Parse extracts a slice of Readme structs from the given byte array,
// splitting by the standard DEPENDENCY DIVIDER if present.
func Parse(data []byte) ([]*Readme, error) {
	var readmes []*Readme

	blocks := bytes.Split(data, []byte(dependencyDivider))

	for _, block := range blocks {
		readme := &Readme{}
		readmeVal := reflect.ValueOf(readme).Elem()

		var currentKey string
		var currentValue strings.Builder

		scanner := bufio.NewScanner(bytes.NewReader(block))
		for scanner.Scan() {
			line := scanner.Text()
			trimmed := strings.TrimSpace(line)

			// Check if we are currently inside a multi-line field
			inMultiline := false
			if currentKey != "" {
				if meta, ok := directiveMap[currentKey]; ok && meta.Multiline {
					inMultiline = true
				}
			}

			// Skip empty lines or comments ONLY if we are not actively parsing a multi-line field.
			if (trimmed == "" || strings.HasPrefix(trimmed, "#")) && !inMultiline {
				continue
			}

			isRootDirective := false
			cleanLine := strings.TrimLeft(line, " \t->")
			parts := strings.SplitN(cleanLine, ":", 2)

			if len(parts) == 2 {
				key := strings.TrimSpace(parts[0])
				value := strings.TrimSpace(parts[1])

				if meta, ok := directiveMap[key]; ok {
					isRootDirective = true
					fieldVal := readmeVal.Field(meta.Index)

					if meta.IsSlice {
						// Split by separator (usually comma)
						items := strings.Split(value, meta.Separator)
						for _, item := range items {
							itemStr := strings.TrimSpace(item)
							if itemStr != "" {
								fieldVal.Set(reflect.Append(fieldVal, reflect.ValueOf(itemStr)))
							}
						}
						currentKey = ""
					} else {
						currentKey = key
						currentValue.Reset()
						currentValue.WriteString(value)
						fieldVal.SetString(currentValue.String())
					}
					continue
				} else if !inMultiline {
					// We hit a Key: Value pair, it's not a known directive, and we are NOT
					// currently inside a multi-line field. This is an unknown field.
					// We only record it if it's not a legacy ignored field.
					if key != "License Type" && key != "License File URL" && key != "License Reference" && key != "Non-License File Explanation" && key != "Notes" {
						readme.UnknownFields = append(readme.UnknownFields, UnknownField{
							Key:   key,
							Value: value,
						})
					}
					currentKey = ""
					isRootDirective = true
					continue
				}
			}

			// Continuation of a multi-line value
			if !isRootDirective && inMultiline {
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
				readmeVal.Field(directiveMap[currentKey].Index).SetString(currentValue.String())
			}
		}

		if err := scanner.Err(); err != nil {
			return nil, fmt.Errorf("error scanning README bytes: %w", err)
		}

		// Deduplicate and sort all list fields (e.g., License Files) dynamically.
		// We use reflection to find any field of type []string (except UnknownFields).
		for i := 0; i < readmeVal.NumField(); i++ {
			f := readmeVal.Field(i)
			if f.Kind() == reflect.Slice && f.Type().Elem().Kind() == reflect.String && readmeVal.Type().Field(i).Name != "UnknownFields" {
				if f.Len() > 0 {
					var strSlice []string
					for j := 0; j < f.Len(); j++ {
						strSlice = append(strSlice, f.Index(j).String())
					}
					sorted := deduplicateAndSort(strSlice)
					f.Set(reflect.MakeSlice(f.Type(), len(sorted), len(sorted)))
					for j, s := range sorted {
						f.Index(j).SetString(s)
					}
				}
			}
		}

		// Only append if we actually parsed something useful
		if readme.Name != "" || len(readme.LicenseFiles) > 0 || len(readme.SourceFiles) > 0 || len(readme.NonLicenseFiles) > 0 {
			readmes = append(readmes, readme)
		}
	}

	return readmes, nil
}
