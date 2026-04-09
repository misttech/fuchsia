// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package file

import (
	"bytes"
	"testing"
)

func TestForceUTF8(t *testing.T) {
	tests := []struct {
		name     string
		input    []byte
		expected []byte
	}{
		{
			name:     "Valid UTF-8 ASCII",
			input:    []byte("hello world"),
			expected: []byte("hello world"),
		},
		{
			name:     "Valid UTF-8 Smart Quotes",
			input:    []byte("left \u201c right \u201d"),
			expected: []byte("left \u201c right \u201d"),
		},
		{
			name: "Windows-1252 Smart Quotes",
			// 0x93 is “ and 0x94 is ” in Windows-1252
			input:    []byte{'l', 'e', 'f', 't', ' ', 0x93, ' ', 'r', 'i', 'g', 'h', 't', ' ', 0x94},
			expected: []byte("left \u201c right \u201d"),
		},
		{
			name: "Windows-1252 Copyright Symbol",
			// 0xA9 is © in Windows-1252
			input:    []byte{0xA9, ' ', '2', '0', '2', '6'},
			expected: []byte("© 2026"),
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			result := forceUTF8(tc.input)
			if !bytes.Equal(result, tc.expected) {
				t.Errorf("forceUTF8 failed for %s.\nExpected: %q\nGot:      %q", tc.name, string(tc.expected), string(result))
			}
		})
	}
}
