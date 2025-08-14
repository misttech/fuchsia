// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package tefmocheck

import (
	"path"
	"strings"
)

// NearbyStringCheck checks for two strings that appear close to each other in a log.
type NearbyStringCheck struct {
	baseCheck
	// String1 to search for.
	String1 string
	// String2 to search for.
	String2 string
	// MaxDistanceLines is the maximum number of lines apart the two strings can be.
	MaxDistanceLines int
	// Type of log to search in.
	Type logType
	// InfraFailure is true if the check is related to infra.
	InfraFailure bool
	line1        string
	line2        string
}

func (c *NearbyStringCheck) Check(outputs *TestingOutputs) bool {
	var logs [][]byte
	switch c.Type {
	case serialLogType:
		logs = outputs.SerialLogs
	case swarmingOutputType:
		logs = [][]byte{outputs.SwarmingOutput}
	case syslogType:
		logs = outputs.Syslogs
	default:
		return false
	}

	for _, log := range logs {
		lines := strings.Split(string(log), "\n")
		var string1Lines []int
		var string2Lines []int
		for i, line := range lines {
			if strings.Contains(line, c.String1) {
				string1Lines = append(string1Lines, i)
			}
			if strings.Contains(line, c.String2) {
				string2Lines = append(string2Lines, i)
			}
		}

		for _, line1 := range string1Lines {
			for _, line2 := range string2Lines {
				distance := line1 - line2
				if distance < 0 {
					distance = -distance
				}
				if distance <= c.MaxDistanceLines {
					c.line1 = lines[line1]
					c.line2 = lines[line2]
					return true
				}
			}
		}
	}
	return false
}

func (c *NearbyStringCheck) Name() string {
	return path.Join("nearby_string",
		string(c.Type),
		strings.ReplaceAll(c.String1, " ", "_"),
		strings.ReplaceAll(c.String2, " ", "_"))
}

func (c *NearbyStringCheck) IsInfraFailure() bool {
	return c.InfraFailure
}

func (c *NearbyStringCheck) FailureReason() string {
	return "Found lines nearby:\n" + c.line1 + "\n" + c.line2
}

func NearbyStringsChecks() []FailureModeCheck {
	return []FailureModeCheck{
		// For https://fxbug.dev/433753567
		&NearbyStringCheck{
			String1:          "WARN: Command failure occurred: ZX_ERR_IO_REFUSED: command failure",
			String2:          "Format: Log Type - Time(microsec) - Message - Optional Info",
			MaxDistanceLines: 20,
			Type:             serialLogType,
		},
	}
}
