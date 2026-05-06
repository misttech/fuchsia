// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "1024"]

use criterion::{Benchmark, Criterion};
use fuchsia_criterion::FuchsiaCriterion;
use std::time::Duration;

const POLICY_BYTES: &[u8] =
    include_bytes!("../../../../lib/selinux/testdata/policies/aosp_sepolicy");

fn load_policy_bench() -> Benchmark {
    Benchmark::new("load_policy", move |b| {
        b.iter(|| {
            let server = selinux::SecurityServer::new_default();
            let _ = server.load_policy(POLICY_BYTES.to_vec());
        })
    })
}

fn security_context_to_sid_bench(
    name_suffix: &'static str,
    context_bytes: &'static [u8],
) -> Benchmark {
    let server = selinux::SecurityServer::new_default();
    let _ = server.load_policy(POLICY_BYTES.to_vec()).unwrap();

    let server_clone = server.clone();
    Benchmark::new(format!("security_context_to_sid_{}", name_suffix), move |b| {
        b.iter(|| {
            let _ = server_clone.security_context_to_sid(context_bytes.into()).unwrap();
        })
    })
}

fn sid_to_security_context_bench(
    name_suffix: &'static str,
    context_bytes: &'static [u8],
) -> Benchmark {
    let server = selinux::SecurityServer::new_default();
    let _ = server.load_policy(POLICY_BYTES.to_vec()).unwrap();
    let sid = server.security_context_to_sid(context_bytes.into()).unwrap();

    let server_clone = server.clone();
    Benchmark::new(format!("sid_to_security_context_{}", name_suffix), move |b| {
        b.iter(|| {
            let _ = server_clone.sid_to_security_context(sid).unwrap();
        })
    })
}

fn compute_access_decision_bench(
    name_suffix: &'static str,
    context_bytes: &'static [u8],
) -> Benchmark {
    let server = selinux::SecurityServer::new_default();
    let _ = server.load_policy(POLICY_BYTES.to_vec()).unwrap();
    let sid = server.security_context_to_sid(context_bytes.into()).unwrap();
    let class_id = server.class_id_by_name("process").unwrap();

    let server_clone = server.clone();
    Benchmark::new(format!("compute_access_decision_{}", name_suffix), move |b| {
        b.iter(|| {
            let _ = server_clone.compute_access_decision_raw(sid, sid, class_id);
        })
    })
}

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

    c.bench("fuchsia.sestarnix", load_policy_bench());
    c.bench("fuchsia.sestarnix", security_context_to_sid_bench("simple", b"u:r:kernel:s0"));
    c.bench(
        "fuchsia.sestarnix",
        security_context_to_sid_bench("c0_c255", b"u:r:kernel:s0:c0.c255"),
    );
    c.bench("fuchsia.sestarnix", sid_to_security_context_bench("simple", b"u:r:kernel:s0"));
    c.bench(
        "fuchsia.sestarnix",
        sid_to_security_context_bench("c0_c255", b"u:r:kernel:s0:c0.c255"),
    );
    c.bench("fuchsia.sestarnix", compute_access_decision_bench("simple", b"u:r:kernel:s0"));
    c.bench(
        "fuchsia.sestarnix",
        compute_access_decision_bench("c0_c255", b"u:r:kernel:s0:c0.c255"),
    );
}
