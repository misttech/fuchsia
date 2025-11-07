// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_criterion::FuchsiaCriterion;
use fuchsia_criterion::criterion::Criterion;

pub(crate) mod ip;
mod tcp;
mod udp;

/// A trait abstracting [`criterion::Bencher`] for use in tests.
trait Bencher {
    fn iter<O, F: FnMut() -> O>(&mut self, f: F);
}

trait BenchmarkGroup {
    type Bencher: Bencher;
    fn register(&mut self, name: impl Into<String>, f: impl FnMut(&mut Self::Bencher) + 'static);
}

impl Bencher for criterion::Bencher {
    fn iter<O, F: FnMut() -> O>(&mut self, f: F) {
        self.iter(f);
    }
}

impl BenchmarkGroup for Option<criterion::Benchmark> {
    type Bencher = criterion::Bencher;

    fn register(&mut self, name: impl Into<String>, f: impl FnMut(&mut Self::Bencher) + 'static) {
        *self = Some(match self.take() {
            Some(b) => b.with_function(name, f),
            None => criterion::Benchmark::new(name, f),
        })
    }
}

fn gather_benchmarks<G: BenchmarkGroup>(group: &mut G) {
    tcp::get_benches(group);
    udp::get_benches(group);
}

fn main() {
    let mut c = FuchsiaCriterion::default();
    let internal_c: &mut Criterion = &mut c;
    *internal_c = std::mem::take(internal_c)
        .warm_up_time(std::time::Duration::from_millis(1))
        .measurement_time(std::time::Duration::from_millis(100))
        .sample_size(100);
    let name = "fuchsia.netstack.packet-formats";

    let mut bench: Option<criterion::Benchmark> = None;
    gather_benchmarks(&mut bench);
    let _: &mut Criterion = c.bench(name, bench.expect("no benchmarks registered"));
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(crate) struct TestBencher;

    impl Bencher for TestBencher {
        fn iter<O, F: FnMut() -> O>(&mut self, mut f: F) {
            for _ in 0..2 {
                let _ = f();
            }
        }
    }

    impl BenchmarkGroup for TestBencher {
        type Bencher = Self;

        fn register(
            &mut self,
            name: impl Into<String>,
            mut f: impl FnMut(&mut Self::Bencher) + 'static,
        ) {
            // Add something to stdout so we can tell benchmarks apart, given we
            // can't dynamically generate test cases.
            println!("=== running {}", name.into());
            f(self);
        }
    }

    /// Runs all the benchmarks with a TestBencher with few iterations as a
    /// smoke test that all benchmarks are correct.
    #[test]
    fn smoke_test_benches() {
        gather_benchmarks(&mut TestBencher);
    }
}
