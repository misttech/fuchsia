// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package tefmocheck

import (
	_ "embed"
	"regexp"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
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
    name: "regex_check"
    string: 'error \d+: .*\ndetail'
    is_regex: true
    prefilter_prefix: "detail"
    log_types:
      - swarmingOutputType
    skip_passed_task: true
    attribute_to_test: true
    add_tag: true
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

	testRegex := regexp.MustCompile(`error \d+: .*\ndetail`)

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
			CheckName:            "regex_check",
			String:               `error \d+: .*\ndetail`,
			PrefilterPrefix:      "detail",
			prefilterPrefixBytes: []byte("detail"),
			Type:                 swarmingOutputType,
			SkipPassedTask:       true,
			AttributeToTest:      true,
			AddTag:               true,
			regex:                testRegex,
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

	regexComparer := cmp.Comparer(func(a, b *regexp.Regexp) bool {
		if a == nil || b == nil {
			return a == b
		}
		return a.String() == b.String()
	})

	if diff := cmp.Diff(expected, checks, cmp.AllowUnexported(stringInLogCheck{}, logBlock{}), regexComparer); diff != "" {
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
			name:    "string_in_log regex missing name",
			yaml:    "failure_mode_checks: [{kind: 'string_in_log', string: 'foo.*', is_regex: true, log_types: ['swarmingOutputType']}]",
			wantErr: "'name' field is required",
		},
		{
			name:    "string_in_log prefilter_prefix without regex",
			yaml:    "failure_mode_checks: [{kind: 'string_in_log', string: 'foo', prefilter_prefix: 'bar', log_types: ['swarmingOutputType']}]",
			wantErr: "'prefilter_prefix' field can only be set when 'is_regex' is true",
		},
		{
			name:    "string_in_log prefilter_prefix not in regex",
			yaml:    "failure_mode_checks: [{kind: 'string_in_log', name: 'typo', string: 'foo.*', is_regex: true, prefilter_prefix: 'bar', log_types: ['swarmingOutputType']}]",
			wantErr: "must appear in regex pattern",
		},
		{
			name:    "string_in_log invalid regex",
			yaml:    "failure_mode_checks: [{kind: 'string_in_log', name: 'bad', string: '[unclosed', is_regex: true, log_types: ['swarmingOutputType']}]",
			wantErr: "error parsing regexp",
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

func TestStringInLogCheck_NameOverride(t *testing.T) {
	yamlData := `
failure_mode_checks:
  - kind: "string_in_log"
    name: "custom_string_name"
    string: "some specific error message"
    log_types:
      - syslogType
  - kind: "string_in_log"
    string: "default string message"
    log_types:
      - syslogType
`
	checks, err := LoadChecksFromYAML([]byte(yamlData))
	if err != nil {
		t.Fatalf("LoadChecksFromYAML failed: %v", err)
	}
	if len(checks) != 2 {
		t.Fatalf("got %d checks, want 2", len(checks))
	}
	if got, want := checks[0].Name(), "string_in_log/syslog.txt/custom_string_name"; got != want {
		t.Errorf("checks[0].Name() = %q, want %q", got, want)
	}
	if got, want := checks[1].Name(), "string_in_log/syslog.txt/default_string_message"; got != want {
		t.Errorf("checks[1].Name() = %q, want %q", got, want)
	}
}

func TestFastbootAndHealthCheckOrdering(t *testing.T) {
	tests := []struct {
		name           string
		serialLog      string
		log            string
		wantName       string
		skipPassedTask bool
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
		{
			name:           "ufs_cpp_driver_start_failed_before_ssh_timeout",
			serialLog:      "[00001.368] 03431:03433> [driver_manager.cm] ERROR: Failed to start driver 'driver/ufs-pdev.so' in driver host: ZX_ERR_INTERNAL\n",
			log:            "2026-10-20 14:43:39.297415 botanist: run.go:938: botanist ERROR: botanist failed: timed out trying to connect to ssh\n",
			wantName:       "testing_failure_mode/string_in_log/serial_log.txt/Failed_to_start_driver_'driver/ufs",
			skipPassedTask: true,
		},
		{
			name:           "ufs_cpp_driver_start_failed_on_second_boot_before_failed_to_run_health_check",
			serialLog:      "[00001.512] 03431:03433> [driver, ufs] INFO: Bind Success\n[00009.403] 03431:03433> [driver_manager.cm] ERROR: Failed to start driver 'driver/ufs-pdev.so' in driver host: ZX_ERR_TIMED_OUT\n",
			log:            "2026-10-20 14:52:30.965358 botanist: run.go:938: botanist ERROR: botanist failed: failed to run health check: FFX command failed (ffx --config log.level=debug target wait -t 90): exit status 1\n",
			wantName:       "testing_failure_mode/string_in_log/serial_log.txt/Failed_to_start_driver_'driver/ufs",
			skipPassedTask: true,
		},
		{
			name:           "ufs_rust_driver_start_failed_before_ssh_timeout",
			serialLog:      "[00001.368] 03431:03433> [driver_manager.cm] ERROR: Failed to start driver 'driver/ufs_rust.so' in driver host: ZX_ERR_TIMED_OUT\n",
			log:            "2026-10-20 14:43:39.297415 botanist: run.go:938: botanist ERROR: botanist failed: timed out trying to connect to ssh\n",
			wantName:       "testing_failure_mode/string_in_log/serial_log.txt/Failed_to_start_driver_'driver/ufs",
			skipPassedTask: true,
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			checks, err := LoadChecksFromYAML(embeddedCheckConfigsYAML)
			if err != nil {
				t.Fatalf("LoadChecksFromYAML failed: %v", err)
			}
			var serialLogs [][]byte
			if tc.serialLog != "" {
				serialLogs = [][]byte{[]byte(tc.serialLog)}
			}
			for _, logOutput := range []string{
				tc.log,
				tc.log + "botanist ERROR: FFX command failed\n",
			} {
				to := TestingOutputs{
					SwarmingOutput: []byte(logOutput),
					SerialLogs:     serialLogs,
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
			if tc.skipPassedTask {
				// Omit SwarmingOutput so botanist checks without skip_passed_task do not fire.
				passedTo := TestingOutputs{
					SerialLogs:  serialLogs,
					TestSummary: &runtests.TestSummary{},
					SwarmingSummary: &SwarmingTaskSummary{
						Results: &SwarmingRpcsTaskResult{
							TaskId:  "abc",
							State:   "COMPLETED",
							Failure: false,
						},
					},
				}
				got, err := RunChecks(checks, &passedTo, "")
				if err != nil {
					t.Fatalf("RunChecks on passed task failed: %v", err)
				}
				if len(got) != 0 {
					t.Errorf("RunChecks on passed task returned %d results, want 0: %+v", len(got), got)
				}
			}
		})
	}
}

func TestGoCrashChecks(t *testing.T) {
	checks, err := LoadChecksFromYAML(embeddedCheckConfigsYAML)
	if err != nil {
		t.Fatalf("LoadChecksFromYAML failed: %v", err)
	}

	const (
		panicCheckName = "testing_failure_mode/string_in_log/infra_and_test_std_and_klog.txt/go_panic"
		fatalCheckName = "testing_failure_mode/string_in_log/infra_and_test_std_and_klog.txt/go_fatal_error"
	)

	tests := []struct {
		name              string
		log               string
		perTest           []TestLog
		failed            bool
		wantName          string
		wantFailureReason string
		wantTags          []build.TestTag
	}{
		{
			name:              "runtime nil pointer panic",
			log:               "panic: runtime error: invalid memory address or nil pointer dereference\n[signal SIGSEGV: segmentation violation code=0x1 addr=0x0 pc=0x1234]\n\ngoroutine 1 [running]:\nmain.main()\n",
			failed:            true,
			wantName:          panicCheckName,
			wantFailureReason: "panic: runtime error: invalid memory address or nil pointer dereference",
		},
		{
			name:              "closed channel panic",
			log:               "panic: send on closed channel\n\ngoroutine 18 [running]:\ngo.fuchsia.dev/fuchsia/tools/net/mdns.(*mDNS).Start.func1()\n",
			failed:            true,
			wantName:          panicCheckName,
			wantFailureReason: "panic: send on closed channel",
		},
		{
			name:              "explicit string panic",
			log:               "panic: unexpected invariant violation\n\ngoroutine 4 [running]:\ngo.fuchsia.dev/fuchsia/tools/botanist.foo()\n",
			failed:            true,
			wantName:          panicCheckName,
			wantFailureReason: "panic: unexpected invariant violation",
		},
		{
			name:              "prefixed subprocess panic",
			log:               "2026-10-20 10:00:00.000000 testrunner: panic: sync: negative WaitGroup counter\n2026-10-20 10:00:00.000001 testrunner: \n2026-10-20 10:00:00.000002 testrunner: goroutine 9 [running]:\n",
			failed:            true,
			wantName:          panicCheckName,
			wantFailureReason: "2026-10-20 10:00:00.000000 testrunner: panic: sync: negative WaitGroup counter",
		},
		{
			name:              "interleaved stderr mid-panic and mid-goroutine header",
			log:               "panic: 2026-10-09 15:55:01.929241 runtime error: invalid memory address or nil pointer dereference\n\n[signal SIGSEGV: segmentation violation code=0x1 addr=0x0 pc=0x85353a]\n\ngoroutine 7 [running2026-10-09 15:55:02.210465 botanist: packet from [fe80::1]\n]:\ngo.fuchsia.dev/fuchsia/tools/net/mdns.(*mDNS).Close(0xc00007c2a0)\n",
			failed:            true,
			wantName:          panicCheckName,
			wantFailureReason: "panic: 2026-10-09 15:55:01.929241 runtime error: invalid memory address or nil pointer dereference",
		},
		{
			name:              "concurrent map writes fatal error",
			log:               "fatal error: concurrent map writes\n\ngoroutine 7 [running]:\ngo.fuchsia.dev/fuchsia/tools/botanist.bar()\n",
			failed:            true,
			wantName:          fatalCheckName,
			wantFailureReason: "fatal error: concurrent map writes",
		},
		{
			name:              "all goroutines asleep deadlock fatal error",
			log:               "fatal error: all goroutines are asleep - deadlock!\n\ngoroutine 1 [chan receive]:\nmain.main()\n",
			failed:            true,
			wantName:          fatalCheckName,
			wantFailureReason: "fatal error: all goroutines are asleep - deadlock!",
		},
		{
			name:              "runtime stack system throw fatal error",
			log:               "fatal error: runtime: out of memory\n\nruntime stack:\nruntime.throw({0x1234, 0x15})\n",
			failed:            true,
			wantName:          fatalCheckName,
			wantFailureReason: "fatal error: runtime: out of memory",
		},
		{
			name: "per-test panic attributes tag to running test",
			log:  "panic: slice bounds out of range\n\ngoroutine 3 [running]:\n",
			perTest: []TestLog{
				{
					TestName: "fuchsia-pkg://fuchsia.com/foo#meta/foo.cm",
					Bytes:    []byte("panic: slice bounds out of range\n\ngoroutine 3 [running]:\n"),
					FilePath: "foo/stdio.txt",
					Index:    0,
				},
			},
			failed:            true,
			wantName:          panicCheckName,
			wantFailureReason: "panic: slice bounds out of range",
			wantTags:          []build.TestTag{{Key: "test_name", Value: "fuchsia-pkg://fuchsia.com/foo#meta/foo.cm"}},
		},
		{
			name:     "passed task ignores unit test panic output",
			log:      "panic: expected test panic\n\ngoroutine 5 [running]:\n",
			failed:   false,
			wantName: "",
		},
		{
			name:     "single line mentioning goroutine and panic does not false positive",
			log:      "2026-10-20 10:00:00.000000 test: goroutine observed panic: synthetic message\n",
			failed:   true,
			wantName: "",
		},
		{
			name:     "non-Go panic without goroutine stack header does not false positive",
			log:      "2026-10-20 10:00:00.000000 device: panic: firmware assertion failed\nstack backtrace:\n  0: 0xffff0000\n",
			failed:   true,
			wantName: "",
		},
		{
			name:     "non-fatal error prefix at start of line does not trigger go_fatal_error",
			log:      "non-fatal error: transient glitch\n\ngoroutine 1 [running]:\n",
			failed:   true,
			wantName: "",
		},
		{
			name:     "prefixed non-fatal error does not trigger go_fatal_error",
			log:      "2026-10-20 10:00:00.000000 test: non-fatal error: transient glitch\n\ngoroutine 1 [running]:\n",
			failed:   true,
			wantName: "",
		},
		{
			name:     "prefixed non-panic does not trigger go_panic",
			log:      "2026-10-20 10:00:00.000000 test: non-panic: synthetic message\n\ngoroutine 1 [running]:\n",
			failed:   true,
			wantName: "",
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			to := TestingOutputs{
				SwarmingOutput:        []byte(tc.log),
				SwarmingOutputPerTest: tc.perTest,
				SwarmingSummary: &SwarmingTaskSummary{
					Results: &SwarmingRpcsTaskResult{
						TaskId:  "task-123",
						State:   "COMPLETED",
						Failure: tc.failed,
					},
				},
			}
			got, err := RunChecks(checks, &to, "")
			if err != nil {
				t.Fatalf("RunChecks failed: %v", err)
			}
			if tc.wantName == "" {
				if len(got) != 0 {
					t.Errorf("expected no matches, got %+v", got)
				}
				return
			}
			if len(got) != 1 || got[0].Name != tc.wantName {
				t.Errorf("RunChecks() = %+v, want single check %q", got, tc.wantName)
				return
			}
			if diff := cmp.Diff(tc.wantTags, got[0].Tags); diff != "" {
				t.Errorf("Tags diff (-want +got):\n%s", diff)
			}
			if tc.wantFailureReason != "" {
				if got[0].TestResult.FailureReason == nil || len(got[0].TestResult.FailureReason.Errors) == 0 {
					t.Fatalf("expected FailureReason with error message, got: %+v", got[0].TestResult.FailureReason)
				}
				if gotReason := got[0].TestResult.FailureReason.Errors[0].Message; gotReason != tc.wantFailureReason {
					t.Errorf("FailureReason.Errors[0].Message = %q, want %q", gotReason, tc.wantFailureReason)
				}
			}
		})
	}
}
