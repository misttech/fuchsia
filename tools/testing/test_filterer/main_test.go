// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"testing"

	"github.com/google/go-cmp/cmp"
	swarmingpb "go.chromium.org/luci/swarming/proto/api_v2"
	"go.fuchsia.dev/fuchsia/tools/integration/testsharder"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
	"google.golang.org/protobuf/testing/protocmp"
)

func TestCreateSkippedShards(t *testing.T) {
	testCases := []struct {
		name            string
		shards          []testsharder.Shard
		metadata        PresubmitRetryMetadata
		flags           flags
		expectedResults []testsharder.Shard
		expectErr       bool
	}{
		{
			name: "build not skipping building fuchsia on a presubmit retry",
			shards: []testsharder.Shard{
				{Name: "shard1", Skippable: false},
				{Name: "shard2", Skippable: true},
				{Name: "shard3", Skippable: false},
			},
			metadata: PresubmitRetryMetadata{
				ShardToFailedTestsMap: map[string][]string{
					"shard1": {"test1"},
					"shard3": {"test2"},
				},
				ShardsWithTefmocheckFailures: []string{},
			},
			flags: flags{
				hasReusedBuildArtifacts:        false,
				skipPreviouslyPassedTestShards: true,
				skipPreviouslyPassedTests:      true,
			},
			expectedResults: []testsharder.Shard{{Name: "shard2", Skippable: true}},
			expectErr:       false,
		},
		{
			name: "build not skipping test shards on a presubmit retry",
			shards: []testsharder.Shard{
				{Name: "shard1", Skippable: false},
				{Name: "shard2", Skippable: true},
				{Name: "shard3", Skippable: false},
			},
			metadata: PresubmitRetryMetadata{
				ShardToFailedTestsMap: map[string][]string{
					"shard1": {"test1"},
					"shard3": {"test2"},
				},
				ShardsWithTefmocheckFailures: []string{},
			},
			flags: flags{
				hasReusedBuildArtifacts:        true,
				skipPreviouslyPassedTestShards: false,
				skipPreviouslyPassedTests:      true,
			},
			expectedResults: []testsharder.Shard{{Name: "shard2", Skippable: true}},
			expectErr:       false,
		},
		{
			name: "shard to failed tests map is empty",
			shards: []testsharder.Shard{
				{Name: "shard1", Skippable: false},
				{Name: "shard2", Skippable: false},
				{Name: "shard3", Skippable: true},
			},
			metadata: PresubmitRetryMetadata{
				ShardToFailedTestsMap:        map[string][]string{},
				ShardsWithTefmocheckFailures: []string{},
			},
			flags: flags{
				hasReusedBuildArtifacts:        true,
				skipPreviouslyPassedTestShards: true,
				skipPreviouslyPassedTests:      true,
			},
			expectedResults: []testsharder.Shard{{Name: "shard3", Skippable: true}},
			expectErr:       false,
		},
		{
			name: "shards with tefmocheck failures are not skipped",
			shards: []testsharder.Shard{
				{Name: "shard1", Skippable: false},
				{Name: "shard2", Skippable: false},
				{Name: "shard3", Skippable: true},
			},
			metadata: PresubmitRetryMetadata{
				ShardToFailedTestsMap:        map[string][]string{"shard1": {"test1"}},
				ShardsWithTefmocheckFailures: []string{"shard2"},
			},
			flags: flags{
				hasReusedBuildArtifacts:        true,
				skipPreviouslyPassedTestShards: true,
				skipPreviouslyPassedTests:      true,
			},
			expectedResults: []testsharder.Shard{{Name: "shard3", Skippable: true}},
			expectErr:       false,
		},
		{
			name:   "empty shards",
			shards: []testsharder.Shard{},
			metadata: PresubmitRetryMetadata{
				ShardToFailedTestsMap: map[string][]string{
					"shard1": {"test1"},
					"shard3": {"test2"},
				},
				ShardsWithTefmocheckFailures: []string{},
			},
			flags: flags{
				hasReusedBuildArtifacts:        true,
				skipPreviouslyPassedTestShards: true,
				skipPreviouslyPassedTests:      true,
			},
			expectedResults: []testsharder.Shard{},
			expectErr:       false,
		},
		{
			name: "skip shards that are not in the shard to failed test map and skippable shards",
			shards: []testsharder.Shard{
				{Name: "shard1", Skippable: false},
				{Name: "shard2", Skippable: false},
				{Name: "shard3", Skippable: true},
			},
			metadata: PresubmitRetryMetadata{
				ShardToFailedTestsMap: map[string][]string{
					"shard1": {"test1"},
				},
				ShardsWithTefmocheckFailures: []string{},
			},
			flags: flags{
				hasReusedBuildArtifacts:        true,
				skipPreviouslyPassedTestShards: true,
				skipPreviouslyPassedTests:      false,
			},
			expectedResults: []testsharder.Shard{
				{Name: "shard2", Skippable: false},
				{Name: "shard3", Skippable: true},
			},
			expectErr: false,
		},
		{
			name: "only add skipped results for previously passed tests",
			shards: []testsharder.Shard{
				{Name: "shard1", Skippable: false, SummaryIfSkipped: runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{Name: "test1"},
						{Name: "test2"},
					},
				}},
				{Name: "shard2", Skippable: false, SummaryIfSkipped: runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{Name: "test3"},
						{Name: "test4"},
					},
				}},
				{Name: "shard3", Skippable: true, SummaryIfSkipped: runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{Name: "test5"},
						{Name: "test6"},
					},
				}},
			},
			metadata: PresubmitRetryMetadata{
				ShardToFailedTestsMap: map[string][]string{
					"shard1": {"test1"},
				},
				ShardsWithTefmocheckFailures: []string{},
			},
			flags: flags{
				hasReusedBuildArtifacts:        true,
				skipPreviouslyPassedTestShards: true,
				skipPreviouslyPassedTests:      true,
			},
			expectedResults: []testsharder.Shard{
				{Name: "shard1", Skippable: false, SummaryIfSkipped: runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{Name: "test2"},
					},
				}},
				{Name: "shard2", Skippable: false, SummaryIfSkipped: runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{Name: "test3"},
						{Name: "test4"},
					},
				}},
				{Name: "shard3", Skippable: true, SummaryIfSkipped: runtests.TestSummary{
					Tests: []runtests.TestDetails{
						{Name: "test5"},
						{Name: "test6"},
					},
				}},
			},
			expectErr: false,
		},
	}
	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			result := createSkippedShards(tc.shards, tc.metadata, &tc.flags)

			if diff := cmp.Diff(tc.expectedResults, result, protocmp.Transform()); diff != "" {
				t.Errorf("createSkippedShards() mismatch (-want +got):\n%s", diff)
			}
		})
	}
}

func TestCreateFilteredTaskRequests(t *testing.T) {
	testCases := []struct {
		name            string
		taskRequests    []swarmingpb.NewTaskRequest
		metadata        PresubmitRetryMetadata
		flags           flags
		expectedResults []swarmingpb.NewTaskRequest
		expectErr       bool
	}{
		{
			name: "build not skipping building fuchsia on a presubmit retry",
			taskRequests: []swarmingpb.NewTaskRequest{
				{Name: "shard1|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard2|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard3|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
			},
			metadata: PresubmitRetryMetadata{
				ShardToFailedTestsMap: map[string][]string{
					"shard1": {"test1"},
					"shard3": {"test2"},
				},
				ShardsWithTefmocheckFailures: []string{},
			},
			flags: flags{
				hasReusedBuildArtifacts:        false,
				skipPreviouslyPassedTestShards: true,
				skipPreviouslyPassedTests:      true,
			},
			expectedResults: []swarmingpb.NewTaskRequest{
				{Name: "shard1|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard2|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard3|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
			},
			expectErr: false,
		},
		{
			name: "build not skipping test shards on a presubmit retry",
			taskRequests: []swarmingpb.NewTaskRequest{
				{Name: "shard1|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard2|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard3|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
			},
			metadata: PresubmitRetryMetadata{
				ShardToFailedTestsMap: map[string][]string{
					"shard1": {"test1"},
					"shard3": {"test2"},
				},
				ShardsWithTefmocheckFailures: []string{},
			},
			flags: flags{
				hasReusedBuildArtifacts:        true,
				skipPreviouslyPassedTestShards: false,
				skipPreviouslyPassedTests:      true,
			},
			expectedResults: []swarmingpb.NewTaskRequest{
				{Name: "shard1|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard2|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard3|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
			},
			expectErr: false,
		},
		{
			name: "shard to failed tests map is empty",
			taskRequests: []swarmingpb.NewTaskRequest{
				{Name: "shard1|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard2|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard3|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
			},
			metadata: PresubmitRetryMetadata{
				ShardToFailedTestsMap:        map[string][]string{},
				ShardsWithTefmocheckFailures: []string{},
			},
			flags: flags{
				hasReusedBuildArtifacts:        true,
				skipPreviouslyPassedTestShards: true,
				skipPreviouslyPassedTests:      true,
			},
			expectedResults: []swarmingpb.NewTaskRequest{
				{Name: "shard1|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard2|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard3|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
			},
			expectErr: false,
		},
		{
			name: "tasks with tefmocheck failures are not skipped",
			taskRequests: []swarmingpb.NewTaskRequest{
				{Name: "shard1|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard2|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard3|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard4|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
			},
			metadata: PresubmitRetryMetadata{
				ShardToFailedTestsMap:        map[string][]string{"shard1": {"test1"}, "shard3": {"test2"}},
				ShardsWithTefmocheckFailures: []string{"shard2", "shard3"},
			},
			flags: flags{
				hasReusedBuildArtifacts:        true,
				skipPreviouslyPassedTestShards: true,
				skipPreviouslyPassedTests:      true,
			},
			expectedResults: []swarmingpb.NewTaskRequest{
				{Name: "shard1|builder", TaskSlices: []*swarmingpb.TaskSlice{
					{Properties: &swarmingpb.TaskProperties{
						Env: []*swarmingpb.StringPair{
							{Key: "TEST_ALLOWLIST_LENGTH", Value: "1"},
							{Key: "TEST_ALLOWLIST_INDEX_0", Value: "test1"},
						},
					}},
				}},
				{Name: "shard2|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard3|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
			},
			expectErr: false,
		},
		{
			name:         "empty task requests",
			taskRequests: []swarmingpb.NewTaskRequest{},
			metadata: PresubmitRetryMetadata{
				ShardToFailedTestsMap: map[string][]string{
					"shard1": {"test1"},
					"shard3": {"test2"},
				},
				ShardsWithTefmocheckFailures: []string{},
			},
			flags: flags{
				hasReusedBuildArtifacts:        true,
				skipPreviouslyPassedTestShards: true,
				skipPreviouslyPassedTests:      true,
			},
			expectedResults: []swarmingpb.NewTaskRequest{},
			expectErr:       false,
		},
		{
			name: "skip task requests that are not in the shard to failed test map and skippable shards",
			taskRequests: []swarmingpb.NewTaskRequest{
				{Name: "shard1|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard2|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard3|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
			},
			metadata: PresubmitRetryMetadata{
				ShardToFailedTestsMap:        map[string][]string{"shard1": {"test1"}},
				ShardsWithTefmocheckFailures: []string{},
			},
			flags: flags{
				hasReusedBuildArtifacts:        true,
				skipPreviouslyPassedTestShards: true,
				skipPreviouslyPassedTests:      false,
			},
			expectedResults: []swarmingpb.NewTaskRequest{
				{
					Name: "shard1|builder", TaskSlices: []*swarmingpb.TaskSlice{{
						Properties: &swarmingpb.TaskProperties{},
					}},
				},
			},
			expectErr: false,
		},
		{
			name: "only run previously failed tests",
			taskRequests: []swarmingpb.NewTaskRequest{
				{Name: "shard1|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard2|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
				{Name: "shard3|builder", TaskSlices: []*swarmingpb.TaskSlice{{
					Properties: &swarmingpb.TaskProperties{},
				}}},
			},
			metadata: PresubmitRetryMetadata{
				ShardToFailedTestsMap:        map[string][]string{"shard1": {"test1"}},
				ShardsWithTefmocheckFailures: []string{},
			},
			flags: flags{
				hasReusedBuildArtifacts:        true,
				skipPreviouslyPassedTestShards: true,
				skipPreviouslyPassedTests:      true,
			},
			expectedResults: []swarmingpb.NewTaskRequest{
				{Name: "shard1|builder", TaskSlices: []*swarmingpb.TaskSlice{
					{Properties: &swarmingpb.TaskProperties{
						Env: []*swarmingpb.StringPair{
							{Key: "TEST_ALLOWLIST_LENGTH", Value: "1"},
							{Key: "TEST_ALLOWLIST_INDEX_0", Value: "test1"},
						},
					}},
				}},
			},
			expectErr: false,
		},
	}
	for _, tc := range testCases {
		t.Run(tc.name, func(t *testing.T) {
			result := createFilteredTaskRequests(tc.taskRequests, tc.metadata, &tc.flags)

			if diff := cmp.Diff(tc.expectedResults, result, protocmp.Transform()); diff != "" {
				t.Errorf("createFilteredTaskRequests() mismatch (-want +got):\n%s", diff)
			}
		})
	}
}
