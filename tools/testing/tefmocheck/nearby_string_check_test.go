// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package tefmocheck

import (
	"testing"
)

func TestNearbyStringCheck(t *testing.T) {
	tests := []struct {
		name            string
		log             string
		check           *NearbyStringCheck
		expectedCheck   bool
		expectedName    string
		expectedFailure string
		expectedLine1   string
		expectedLine2   string
	}{
		{
			name: "strings within distance",
			log: `line 1
string1
line 3
string2
line 5`,
			check: &NearbyStringCheck{
				String1:          "string1",
				String2:          "string2",
				MaxDistanceLines: 2,
				Type:             serialLogType,
			},
			expectedCheck:   true,
			expectedName:    "nearby_string/serial_log.txt/string1/string2",
			expectedFailure: "Found lines nearby:\nstring1\nstring2",
			expectedLine1:   "string1",
			expectedLine2:   "string2",
		},
		{
			name: "strings outside distance",
			log: `string1
line 2
line 3
line 4
string2`,
			check: &NearbyStringCheck{
				String1:          "string1",
				String2:          "string2",
				MaxDistanceLines: 2,
				Type:             serialLogType,
			},
			expectedCheck: false,
		},
		{
			name: "strings in reverse order within distance",
			log: `string2
line 2
string1`,
			check: &NearbyStringCheck{
				String1:          "string1",
				String2:          "string2",
				MaxDistanceLines: 2,
				Type:             serialLogType,
			},
			expectedCheck:   true,
			expectedName:    "nearby_string/serial_log.txt/string1/string2",
			expectedFailure: "Found lines nearby:\nstring1\nstring2",
			expectedLine1:   "string1",
			expectedLine2:   "string2",
		},
	}

	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			outputs := &TestingOutputs{
				SerialLogs: [][]byte{[]byte(test.log)},
			}
			if got := test.check.Check(outputs); got != test.expectedCheck {
				t.Errorf("Check() = %v, want %v", got, test.expectedCheck)
			}
			if test.expectedCheck {
				if name := test.check.Name(); name != test.expectedName {
					t.Errorf("Name() = %q, want %q", name, test.expectedName)
				}
				if reason := test.check.FailureReason(); reason != test.expectedFailure {
					t.Errorf("FailureReason() = %q, want %q", reason, test.expectedFailure)
				}
				if test.check.line1 != test.expectedLine1 {
					t.Errorf("line1 = %q, want %q", test.check.line1, test.expectedLine1)
				}
				if test.check.line2 != test.expectedLine2 {
					t.Errorf("line2 = %q, want %q", test.check.line2, test.expectedLine2)
				}
			}
		})
	}
}
