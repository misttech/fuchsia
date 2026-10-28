#!/usr/bin/env python3
# allow-non-vendored-python
# Copyright 2026 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import hashlib
import json
import os
import socket
import sys
import time

args = sys.argv[1:]
target = None
log_dir = None
socket_path = None
baud = None
simple = False
log_level = None
for i in range(len(args)):
    if args[i] == "--target" and i + 1 < len(args):
        target = args[i + 1]
    elif args[i] == "--log-dir" and i + 1 < len(args):
        log_dir = args[i + 1]
    elif args[i] == "--socket" and i + 1 < len(args):
        socket_path = args[i + 1]
    elif args[i] == "--baud" and i + 1 < len(args):
        baud = int(args[i + 1])
    elif args[i] == "--simple":
        simple = True
    elif args[i] == "--log-level" and i + 1 < len(args):
        log_level = args[i + 1]

if target and log_dir:
    if socket_path:
        meta_path = os.path.splitext(socket_path)[0] + ".json"
        base = os.path.basename(meta_path)
        h = (
            base[len("ffx_uart_") : -len(".json")]
            if base.startswith("ffx_uart_") and base.endswith(".json")
            else hashlib.sha256(target.encode("utf-8")).hexdigest()[:16]
        )
    else:
        try:
            canon = os.path.realpath(target)
        except Exception:
            canon = target
        h = hashlib.sha256(canon.encode("utf-8")).hexdigest()[:16]
        meta_path = os.path.join(log_dir, f"ffx_uart_{h}.json")
    pid = os.fork()
    if pid == 0:
        devnull = os.open(os.devnull, os.O_RDWR)
        os.dup2(devnull, 0)
        os.dup2(devnull, 1)
        os.dup2(devnull, 2)
        os.close(devnull)
        my_pid = os.getpid()
        data = {
            "pid": my_pid,
            "target": target,
            "status": "Connected",
            "id": h,
            "baud": baud,
            "protocol": "ResendSP",
            "log_level": log_level,
        }
        os.makedirs(log_dir, exist_ok=True)
        sock_fd = None
        control_fd = None
        if socket_path:
            if os.path.exists(socket_path):
                os.remove(socket_path)
            sock_fd = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            sock_fd.bind(socket_path)
            sock_fd.listen(5)

            control_path = os.path.splitext(socket_path)[0] + ".control"
            if os.path.exists(control_path):
                os.remove(control_path)
            control_fd = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            control_fd.bind(control_path)
            control_fd.listen(5)
        tmp_meta_path = f"{meta_path}.{my_pid}.tmp"
        with open(tmp_meta_path, "w") as f:
            json.dump(data, f)
            f.flush()
        os.replace(tmp_meta_path, meta_path)
        time.sleep(3600)
        if sock_fd:
            sock_fd.close()
        if control_fd:
            control_fd.close()
    else:
        sys.exit(0)
else:
    sys.exit(0)
