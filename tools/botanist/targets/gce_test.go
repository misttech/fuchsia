// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package targets

import (
	"bufio"
	"context"
	"crypto/x509"
	"encoding/pem"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"golang.org/x/crypto/ssh"
)

func TestGeneratePrivateKey(t *testing.T) {
	path, err := generatePrivateKey()
	if err != nil {
		t.Fatalf("generatePrivateKey() failed: got %s, want <nil> error", err)
	}
	defer os.Remove(path)

	// Load the pkey and ensure that it's in the right format.
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("ReadFile(%s) failed: got %s, want <nil> error", path, err)
	}
	block, _ := pem.Decode(b)
	pkey, err := x509.ParsePKCS1PrivateKey(block.Bytes)
	if err != nil {
		t.Fatalf("x509.ParsePKCS1PrivateKey(bytes) failed: got %s, want <nil> error", err)
	}
	if err := pkey.Validate(); err != nil {
		t.Errorf("pkey.Validate() failed: got %s, want <nil> error", err)
	}
}

func TestGeneratePublicKey(t *testing.T) {
	pkeyPath, err := generatePrivateKey()
	if err != nil {
		t.Fatalf("generatePrivateKey() failed: got %s, want <nil> error", err)
	}
	defer os.Remove(pkeyPath)
	path, err := generatePublicKey(pkeyPath)
	if err != nil {
		t.Fatalf("generatePublicKey(%s) failed: got %s, want <nil> error", pkeyPath, err)
	}
	defer os.Remove(path)

	// Load the public key and ensure that it's in the right format.
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("ReadFile(%s) failed: got %s, want <nil> error", path, err)
	}
	key, _, _, _, err := ssh.ParseAuthorizedKey(b)
	if err != nil {
		t.Fatalf("ssh.ParseAuthorizedKey(pubkeyBytes) failed: got %s, want <nil> error", err)
	}
	if key.Type() != "ssh-rsa" {
		t.Errorf("key %s has wrong type: got %s, want ssh-rsa", key, key.Type())
	}
}

func TestGetSerialEndpoint(t *testing.T) {
	tests := []struct {
		zone string
		want string
	}{
		{
			zone: "us-east1-b",
			want: "us-east1-ssh-serialport.googleapis.com:9600",
		},
		{
			zone: "us-central1-a",
			want: "us-central1-ssh-serialport.googleapis.com:9600",
		},
	}
	for _, tc := range tests {
		if got := getSerialEndpoint(tc.zone); got != tc.want {
			t.Errorf("getSerialEndpoint(%q) = %q, want %q", tc.zone, got, tc.want)
		}
	}
}

// fakePipeSerial wraps an in-memory bidirectional pipe to simulate a GCE serial console.
type fakePipeSerial struct {
	r  io.ReadCloser
	w  io.WriteCloser
	in io.Closer
}

func (f *fakePipeSerial) Read(p []byte) (int, error) {
	return f.r.Read(p)
}

func (f *fakePipeSerial) Write(p []byte) (int, error) {
	return f.w.Write(p)
}

func (f *fakePipeSerial) Close() error {
	_ = f.w.Close()
	_ = f.in.Close()
	return f.r.Close()
}

var provisionCmdRe = regexp.MustCompile(`echo "([^"]+)" > /data/ssh/authorized_keys.*echo "(SSH_KEY_PROVISIONED_\d+) \$line"`)

func TestGCEProvisionSSHKeyAndSerialLog(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	pkeyPath, err := generatePrivateKey()
	if err != nil {
		t.Fatalf("generatePrivateKey() failed: %s", err)
	}
	defer os.Remove(pkeyPath)
	pubkeyPath, err := generatePublicKey(pkeyPath)
	if err != nil {
		t.Fatalf("generatePublicKey() failed: %s", err)
	}
	defer os.Remove(pubkeyPath)

	// Target -> host pipe (serial output from VM) and host -> target pipe (serial input to VM).
	targetOutR, targetOutW := io.Pipe()
	hostInR, hostInW := io.Pipe()
	fakeSerial := &fakePipeSerial{
		r:  targetOutR,
		w:  hostInW,
		in: targetOutW,
	}

	var corruptFirstAttempt atomic.Bool
	corruptFirstAttempt.Store(true)

	// Simulate early boot serial logs (replay-from=0) and shell responses.
	go func() {
		defer targetOutW.Close()
		_, _ = io.WriteString(targetOutW, "[00000.000] early boot log from replay-from=0\n$ ")
		scanner := bufio.NewScanner(hostInR)
		for scanner.Scan() {
			line := scanner.Text()
			if strings.TrimSpace(line) == "" {
				_, _ = io.WriteString(targetOutW, "$ ")
				continue
			}
			// Echo the typed command line just like UART echo mode.
			_, _ = fmt.Fprintf(targetOutW, "%s\n", line)
			if m := provisionCmdRe.FindStringSubmatch(line); m != nil {
				writtenKey := m[1]
				marker := m[2]
				if corruptFirstAttempt.CompareAndSwap(true, false) {
					// Simulate a dropped character on the first attempt.
					writtenKey = writtenKey[:len(writtenKey)-3]
				}
				_, _ = fmt.Fprintf(targetOutW, "%s %s\n$ ", marker, writtenKey)
			}
		}
	}()

	cfg := GCEConfig{
		InstanceName: "fuchsia-gce-test-node",
		Zone:         "us-east1-b",
		CloudProject: "fuchsia-infra",
	}
	opts := Options{
		ExpectsSSH:    true,
		SSHKey:        pkeyPath,
		AuthorizedKey: pubkeyPath,
	}
	base, err := newGenericFuchsia(ctx, cfg.InstanceName, "", []string{opts.SSHKey}, fakeSerial)
	if err != nil {
		t.Fatalf("newGenericFuchsia() failed: %s", err)
	}
	g := &GCE{
		genericFuchsiaTarget: base,
		config:               cfg,
		currentUser:          "swarming",
		loggerCtx:            ctx,
		opts:                 opts,
		serial:               fakeSerial,
		serialBootTimeout:    5 * time.Second,
		serialReadTimeout:    200 * time.Millisecond,
		serialRetryDelay:     50 * time.Millisecond,
	}

	if got := g.Nodename(); got != cfg.InstanceName {
		t.Errorf("g.Nodename() = %q, want %q", got, cfg.InstanceName)
	}

	if err := g.StartSerialServer(); err != nil {
		t.Fatalf("g.StartSerialServer() failed: %s", err)
	}

	serialLogPath := filepath.Join(t.TempDir(), "serial_log.txt")
	captureDone := make(chan error, 1)
	go func() {
		captureDone <- g.CaptureSerialLog(serialLogPath)
	}()

	// Verify early boot serial logs are captured before Start() runs.
	earlyLogDeadline := time.Now().Add(5 * time.Second)
	for {
		if b, err := os.ReadFile(serialLogPath); err == nil && strings.Contains(string(b), "early boot log from replay-from=0") {
			break
		}
		if time.Now().After(earlyLogDeadline) {
			t.Fatal("timed out waiting for CaptureSerialLog to record early boot log before Start()")
		}
		time.Sleep(10 * time.Millisecond)
	}

	if err := g.Start(ctx, nil, "", false); err != nil {
		t.Fatalf("g.Start() failed: %s", err)
	}

	if err := g.Stop(); err != nil {
		t.Fatalf("g.Stop() failed: %s", err)
	}
	if err := <-captureDone; err != nil {
		t.Fatalf("g.CaptureSerialLog() failed: %s", err)
	}

	logBytes, err := os.ReadFile(serialLogPath)
	if err != nil {
		t.Fatalf("ReadFile(%s) failed: %s", serialLogPath, err)
	}
	logStr := string(logBytes)
	if !strings.Contains(logStr, "early boot log from replay-from=0") {
		t.Errorf("serial_log.txt missing early boot log; got:\n%s", logStr)
	}
	if !strings.Contains(logStr, "SSH_KEY_PROVISIONED_2") {
		t.Errorf("serial_log.txt missing retry provisioning marker SSH_KEY_PROVISIONED_2; got:\n%s", logStr)
	}
}
