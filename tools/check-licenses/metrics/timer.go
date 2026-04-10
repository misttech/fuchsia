// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package metrics

import (
	"sync"
	"time"
)

// Timer tracks durations, call counts, and max durations for a given phase or operation.
type Timer struct {
	Name        string `json:"name"`
	Description string `json:"description"`

	mu            sync.RWMutex
	TotalDuration time.Duration `json:"total_duration"`
	CallCount     int64         `json:"call_count"`
	MaxDuration   time.Duration `json:"max_duration"`
}

// RegisterTimer adds a new Timer to the global registry.
func RegisterTimer(name, description string) *Timer {
	registry.mu.Lock()
	defer registry.mu.Unlock()

	t := &Timer{
		Name:        name,
		Description: description,
	}
	registry.Timers[name] = t
	return t
}

// Track is meant to be deferred at the start of a function.
// e.g. `defer metrics.PhaseDuration.Track()()`
func (t *Timer) Track() func() {
	start := time.Now()
	return func() {
		duration := time.Since(start)

		t.mu.Lock()
		defer t.mu.Unlock()

		t.TotalDuration += duration
		t.CallCount++
		if duration > t.MaxDuration {
			t.MaxDuration = duration
		}
	}
}

// GetTotalDuration returns the total accumulated duration for this timer.
func (t *Timer) GetTotalDuration() time.Duration {
	t.mu.RLock()
	defer t.mu.RUnlock()
	return t.TotalDuration
}
