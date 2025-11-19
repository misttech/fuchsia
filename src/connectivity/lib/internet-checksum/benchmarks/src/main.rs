// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_criterion::FuchsiaCriterion;
use fuchsia_criterion::criterion::{self as criterion, Criterion};
#[allow(unused)]
use internet_checksum::{Checksum, checksum, update};

fn main() {
    let mut c = FuchsiaCriterion::default();
    let internal_c: &mut Criterion = &mut c;
    *internal_c = std::mem::take(internal_c)
        .warm_up_time(std::time::Duration::from_millis(1))
        .measurement_time(std::time::Duration::from_millis(100))
        .sample_size(100);
    let name = "fuchsia.netstack.internet-checksum";

    let bench = criterion::Benchmark::new("checksum/20", bench_checksum::<20>)
        .with_function("checksum/31", bench_checksum::<31>)
        .with_function("checksum/32", bench_checksum::<32>)
        .with_function("checksum/64", bench_checksum::<64>)
        .with_function("checksum/128", bench_checksum::<128>)
        .with_function("checksum/256", bench_checksum::<256>)
        .with_function("checksum/1023", bench_checksum::<1023>)
        .with_function("checksum/1024", bench_checksum::<1024>)
        .with_function("update/2", bench_update::<2>)
        .with_function("update/4", bench_update::<4>)
        .with_function("update/8", bench_update::<8>);

    let _: &mut Criterion = c.bench(name, bench);
}

fn bench_checksum<const N: usize>(bencher: &mut criterion::Bencher) {
    bencher.iter(|| {
        let buf = criterion::black_box([0xFF; N]);
        let mut c = Checksum::new();
        c.add_bytes(&buf);
        let _ = criterion::black_box(c.checksum());
    });
}

fn bench_update<const N: usize>(bencher: &mut criterion::Bencher) {
    bencher.iter(|| {
        let old = criterion::black_box([0xDE; N]);
        let new = criterion::black_box([0xAD; N]);
        let _ = criterion::black_box(update([0xBE, 0xEF], &old[..], &new[..]));
    });
}
