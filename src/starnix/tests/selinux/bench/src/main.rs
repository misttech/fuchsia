// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "1024"]

use criterion::{Benchmark, Criterion};
use fuchsia_criterion::FuchsiaCriterion;
use std::time::Duration;

fn main() {
    // List of benchmark programs is passed as the argument list from the
    // component manifest. The arguments passed by the test executor are
    // separated from the arguments in the manifest file by adding "--" at
    // the end of the argument list in the manifest file.
    let mut args: Vec<_> = std::env::args().collect();
    let Some(separator_pos) = args.iter().position(|s| s == "--") else {
        eprintln!("{:?}\n-- not found in the argument list", args);
        std::process::exit(1);
    };

    // Replace separator with the program name.
    args[separator_pos] = args[0].clone();

    let benchmark_args: Vec<_> = args[separator_pos..].iter().map(|s| &**s).collect();

    let mut fc = FuchsiaCriterion::fuchsia_bench_with_args(&benchmark_args);
    let c: &mut Criterion = &mut fc;

    *c = std::mem::take(c)
        .warm_up_time(Duration::from_millis(1))
        .measurement_time(Duration::from_secs(3))
        .sample_size(10);

    // load_policy
    const POLICY_BYTES: &[u8] =
        include_bytes!("../../../../lib/selinux/testdata/policies/emulator");
    let bench_load = Benchmark::new("load_policy", move |b| {
        b.iter(|| {
            let server = selinux::SecurityServer::new_default();
            let _ = server.load_policy(POLICY_BYTES.to_vec());
        })
    });
    c.bench("fuchsia.sestarnix", bench_load);
}
