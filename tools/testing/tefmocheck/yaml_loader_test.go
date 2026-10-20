// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package tefmocheck

import (
	_ "embed"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
)

//go:embed check-configs.yaml
var embeddedCheckConfigsYAML []byte

func TestLoadChecksFromYAML(t *testing.T) {
	yamlData := `
failure_mode_checks:
  - kind: "string_in_log"
    string: "Exceeded safe temperature range"
    log_types:
      - syslogType
  - kind: "string_in_log"
    string: "botanist ERROR: SerialReadErrorMsg"
    log_types:
      - swarmingOutputType
  - kind: "string_in_log"
    string: "cas: failed to call UploadIfMissing"
    infra_failure: true
    log_types:
      - swarmingOutputType
    only_on_states:
      - BOT_DIED
    skip_passed_test: true
    ignore_flakes: true
    attribute_to_test: true
    add_tag: true
  - kind: "string_in_log"
    string: "DEVICE SUSPEND TIMED OUT"
    log_types:
      - serialLogType
      - syslogType
  - kind: "string_in_log"
    string: "critical to root job killed with"
    log_types:
      - swarmingOutputType
      - serialLogType
      - syslogType
    except_blocks:
        # LINT.IfChange(mobly_test_start)
      - start_string: "======== Mobly config content ========"
        # LINT.ThenChange(//src/testing/end_to_end/mobly_driver/mobly_driver/api/api_infra.py:mobly_test_start)
        # LINT.IfChange(mobly_test_end)
        end_string: "[=====MOBLY RESULTS=====]"
        # LINT.ThenChange(//src/testing/end_to_end/mobly_driver/mobly_driver/api/api_infra.py:mobly_test_end)
      - start_string: "=== RUN   TestKillCriticalProcess"
        end_string: "--- PASS: TestKillCriticalProcess"
      - start_string: "=== RUN   TestOOMHard"
        end_string: "--- PASS: TestOOMHard"
`

	checks, err := LoadChecksFromYAML([]byte(yamlData))
	if err != nil {
		t.Fatalf("LoadChecksFromYAML failed: %v", err)
	}

	exceptBlocks := []*logBlock{
		{
			startString: "======== Mobly config content ========",
			endString:   "[=====MOBLY RESULTS=====]",
		},
		{
			startString: "=== RUN   TestKillCriticalProcess",
			endString:   "--- PASS: TestKillCriticalProcess",
		},
		{
			startString: "=== RUN   TestOOMHard",
			endString:   "--- PASS: TestOOMHard",
		},
	}

	expected := []FailureModeCheck{
		&stringInLogCheck{
			String: "Exceeded safe temperature range",
			Type:   syslogType,
		},
		&stringInLogCheck{
			String: "botanist ERROR: SerialReadErrorMsg",
			Type:   swarmingOutputType,
		},
		&stringInLogCheck{
			String:          "cas: failed to call UploadIfMissing",
			Type:            swarmingOutputType,
			InfraFailure:    true,
			OnlyOnStates:    []string{"BOT_DIED"},
			SkipPassedTest:  true,
			IgnoreFlakes:    true,
			AttributeToTest: true,
			AddTag:          true,
		},
		&stringInLogCheck{
			String: "DEVICE SUSPEND TIMED OUT",
			Type:   serialLogType,
		},
		&stringInLogCheck{
			String: "DEVICE SUSPEND TIMED OUT",
			Type:   syslogType,
		},
		&stringInLogCheck{
			String:       "critical to root job killed with",
			Type:         swarmingOutputType,
			ExceptBlocks: exceptBlocks,
		},
		&stringInLogCheck{
			String:       "critical to root job killed with",
			Type:         serialLogType,
			ExceptBlocks: exceptBlocks,
		},
		&stringInLogCheck{
			String:       "critical to root job killed with",
			Type:         syslogType,
			ExceptBlocks: exceptBlocks,
		},
	}

	if diff := cmp.Diff(expected, checks, cmp.AllowUnexported(stringInLogCheck{}, logBlock{})); diff != "" {
		t.Errorf("LoadChecksFromYAML() returned diff (-want +got):\n%s", diff)
	}
}

func TestLoadChecksFromYAML_Errors(t *testing.T) {
	tests := []struct {
		name    string
		yaml    string
		wantErr string
	}{
		{
			name:    "string_in_log missing string",
			yaml:    "failure_mode_checks: [{kind: 'string_in_log', log_types: ['serialLogType']}]",
			wantErr: "field is required",
		},
		{
			name:    "string_in_log missing type",
			yaml:    "failure_mode_checks: [{kind: 'string_in_log', string: 'error'}]",
			wantErr: "field is required",
		},
		{
			name:    "unknown kind",
			yaml:    "failure_mode_checks: [{kind: 'invalid_kind'}]",
			wantErr: "unknown check kind",
		},
		{
			name:    "unknown log type",
			yaml:    "failure_mode_checks: [{kind: 'string_in_log', string: 'error', log_types: ['invalidLogType']}]",
			wantErr: "invalid log type",
		},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			_, err := LoadChecksFromYAML([]byte(tt.yaml))
			if err == nil {
				t.Fatal("expected error, got nil")
			}
			if !strings.Contains(err.Error(), tt.wantErr) {
				t.Errorf("error %q does not contain %q", err.Error(), tt.wantErr)
			}
		})
	}
}

func TestLoadChecksFromYAMLEmpty(t *testing.T) {
	yamlData := "failure_mode_checks:\n"
	checks, err := LoadChecksFromYAML([]byte(yamlData))
	if err != nil {
		t.Fatalf("LoadChecksFromYAML failed on empty config: %v", err)
	}

	if len(checks) != 0 {
		t.Errorf("Expected 0 checks, got %d", len(checks))
	}
}

func TestFastbootAndHealthCheckOrdering(t *testing.T) {
	tests := []struct {
		name     string
		log      string
		wantName string
	}{
		{
			name:     "target_specification_was_not_found_before_start_target_error",
			log:      "2026-10-16 14:34:25.309750 botanist: run.go:919: botanist ERROR: botanist failed: start target error: FFX command failed (ffx -v --machine json target flash --product-bundle obj/build/images/fuchsia/product_bundle): Target error: Target specification \"<target>\" was not found. Use `ffx target list` to list known targets, and use a different target query.: exit status 1\n",
			wantName: "testing_failure_mode/string_in_log/infra_and_test_std_and_klog.txt/Target_specification_\"<target>\"_was_not_found",
		},
		{
			name:     "timed_out_writing_to_fastboot_interface_before_start_target_error",
			log:      "2026-10-16 14:30:17.000000 botanist: run.go:919: botanist ERROR: botanist failed: start target error: FFX command failed (ffx -v --machine json target flash --product-bundle obj/build/images/fuchsia/product_bundle): Fastboot interface error: Fastboot send error: timed out writing to fastboot interface: exit status 1\n",
			wantName: "testing_failure_mode/string_in_log/infra_and_test_std_and_klog.txt/timed_out_writing_to_fastboot_interface",
		},
		{
			name:     "could_not_verify_upload_timed_out_before_start_target_error",
			log:      "2026-10-16 15:13:02.141111 botanist: run.go:919: botanist ERROR: botanist failed: start target error: FFX command failed (ffx target bootloader --product-bundle obj/build/images/fuchsia/product_bundle boot): Fastboot interface error: Fastboot error: Upload error: Could not verify upload: Timed out: exit status 1\n",
			wantName: "testing_failure_mode/string_in_log/infra_and_test_std_and_klog.txt/Could_not_verify_upload:_Timed_out",
		},
		{
			name:     "failed_to_run_health_check_before_ffx_and_botanist_failed",
			log:      "2026-10-16 21:52:30.965358 botanist: run.go:919: botanist ERROR: botanist failed: failed to run health check: FFX command failed (ffx --config log.level=debug target wait -t 90): exit status 1\n",
			wantName: "testing_failure_mode/string_in_log/infra_and_test_std_and_klog.txt/botanist_ERROR:_failed_to_run_health_check",
		},
		{
			name:     "failed_to_reconnect_to_target_before_botanist_failed",
			log:      "2026-10-16 21:55:00.000000 botanist: run.go:919: botanist ERROR: botanist failed: failed to reconnect to target: failed to resolve IP: context deadline exceeded\n",
			wantName: "testing_failure_mode/string_in_log/infra_and_test_std_and_klog.txt/botanist_ERROR:_failed_to_reconnect_to_target",
		},
		{
			name:     "could_not_resolve_target_ip_address_before_botanist_failed",
			log:      "2026-10-17 17:19:50.123456 botanist: run.go:938: botanist ERROR: botanist failed: could not resolve target IP address for fuchsia-5254-0063-5e7a: context deadline exceeded after 14m0.002325799s\n",
			wantName: "testing_failure_mode/string_in_log/infra_and_test_std_and_klog.txt/botanist_ERROR:_could_not_resolve_target_IP_address",
		},
		{
			name:     "cannot_connect_to_address_before_botanist_failed",
			log:      "2026-10-17 07:06:18.654321 botanist: run.go:938: botanist ERROR: botanist failed: cannot connect to address \"192.168.42.18:22\": ssh: handshake failed: ssh: unable to authenticate, attempted methods [none publickey], no supported methods remain\n",
			wantName: "testing_failure_mode/string_in_log/infra_and_test_std_and_klog.txt/botanist_ERROR:_cannot_connect_to_address",
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			checks, err := LoadChecksFromYAML(embeddedCheckConfigsYAML)
			if err != nil {
				t.Fatalf("LoadChecksFromYAML failed: %v", err)
			}
			for _, logOutput := range []string{
				tc.log,
				tc.log + "botanist ERROR: FFX command failed\n",
			} {
				to := TestingOutputs{
					SwarmingOutput: []byte(logOutput),
					SwarmingSummary: &SwarmingTaskSummary{
						Results: &SwarmingRpcsTaskResult{
							TaskId:  "abc",
							State:   "COMPLETED",
							Failure: true,
						},
					},
				}
				got, err := RunChecks(checks, &to, "")
				if err != nil {
					t.Fatalf("RunChecks failed: %v", err)
				}
				if len(got) != 1 {
					t.Fatalf("RunChecks returned %d results, want 1: %+v", len(got), got)
				}
				if got[0].Name != tc.wantName {
					t.Errorf("got check %q, want %q", got[0].Name, tc.wantName)
				}
			}
		})
	}
}
