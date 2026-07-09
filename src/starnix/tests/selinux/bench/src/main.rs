// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "1024"]

use criterion::{Benchmark, Criterion};
use fuchsia_criterion::FuchsiaCriterion;
use security::PermissionFlags;
use selinux::policy::{AccessVector, KernelAccessDecision};
use selinux::{AccessQueryArgs, ConcurrentAccessCache, KernelClass, SecurityId};
use starnix_core::fs::tmpfs::TmpFs;
use starnix_core::security;
use starnix_core::task::container_namespace::ContainerNamespace;
use starnix_core::task::{CurrentTask, Kernel, KernelFeatures, SchedulerManager, SystemLimits};
use starnix_core::testing::{AutoReleasableTask, PanickingFile};
use starnix_core::vfs::FsContext;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

const POLICY_BYTES: &[u8] =
    include_bytes!("../../../../lib/selinux/testdata/policies/aosp_sepolicy");

fn create_test_kernel(security_server: Option<Arc<selinux::SecurityServer>>) -> Arc<Kernel> {
    Kernel::new(
        b"".into(),
        KernelFeatures::default(),
        SystemLimits::default(),
        ContainerNamespace::new(),
        SchedulerManager::empty_for_tests(),
        /* crash_reporter_proxy=*/ None,
        fuchsia_inspect::Node::default(),
        security::testing::kernel_state(security_server),
        /* time_adjustment_proxy=*/ None,
        /* device_tree=*/ None,
    )
    .expect("failed to create kernel")
}

fn create_kernel_and_task_with_selinux()
-> (Arc<Kernel>, AutoReleasableTask, Arc<selinux::SecurityServer>) {
    let security_server = selinux::SecurityServer::new_default();
    security_server.load_policy(POLICY_BYTES.to_vec()).unwrap();

    let kernel = create_test_kernel(Some(security_server.clone()));

    let file_system = TmpFs::new_fs(&kernel);
    let fs = FsContext::new(starnix_core::vfs::Namespace::new(file_system));

    let task = starnix_core::execution::create_system_task(&kernel, fs)
        .expect("failed to create system task");

    // Set the system task for testing so that release()'s time_stats() assertion succeeds!
    let shared_task = CurrentTask::new(
        task.task.clone(),
        starnix_core::task::ThreadState::<starnix_registers::HeapRegs>::default().into(),
    );
    kernel.kthreads.init(shared_task).expect("failed to initialize kthreads");

    let mut creds = (*task.real_creds()).clone();
    creds.security_state =
        starnix_core::security::task_for_context(&task, b"u:r:kernel:s0".into()).unwrap();
    task.set_creds(creds);

    (kernel, task.into(), security_server)
}

fn create_file_bench(
    name: &'static str,
    hook_closure: impl Fn(&CurrentTask, &starnix_core::vfs::FileObject) + Send + Sync + 'static,
) -> Benchmark {
    let executor = fuchsia_async::LocalExecutor::default();
    let (kernel, task, _security_server) = create_kernel_and_task_with_selinux();
    let task = Box::leak(Box::new(task));
    let file = Box::leak(Box::new(PanickingFile::new_file(task)));

    Benchmark::new(name, move |bench| {
        let _kernel = &kernel;
        let _executor = &executor;
        bench.iter(|| {
            hook_closure(&**task, &**file);
        })
    })
}

fn load_policy_bench() -> Benchmark {
    Benchmark::new("load_policy", move |b| {
        b.iter(|| {
            let server = selinux::SecurityServer::new_default();
            let _ = criterion::black_box(server.load_policy(POLICY_BYTES.to_vec()));
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
            let _ = criterion::black_box(
                server_clone.security_context_to_sid(context_bytes.into()).unwrap(),
            );
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
            let _ = criterion::black_box(server_clone.sid_to_security_context(sid).unwrap());
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
            let _ =
                criterion::black_box(server_clone.compute_access_decision_raw(sid, sid, class_id));
        })
    })
}

fn concurrent_access_cache_get_bench() -> Benchmark {
    let cache = ConcurrentAccessCache::new(selinux::DEFAULT_SHARED_SIZE.access_cache_capacity);
    let value = KernelAccessDecision {
        allow: AccessVector::ALL,
        audit: AccessVector::NONE,
        flags: 0,
        todo_bug: None,
    };

    let keys: Vec<_> = (1..=1000)
        .map(|i| AccessQueryArgs {
            source_sid: SecurityId(NonZeroU32::new(i).unwrap()),
            target_sid: SecurityId(NonZeroU32::new(i + 1).unwrap()),
            target_class: KernelClass::Process,
        })
        .collect();

    for key in &keys {
        let _ = cache.get_or_try_insert::<()>(key, || Ok(value));
    }

    Benchmark::new("concurrent_access_cache_get", move |b| {
        b.iter(|| {
            for key in &keys {
                let _ = criterion::black_box(cache.get_or_try_insert::<()>(key, || Ok(value)));
            }
        })
    })
}

fn file_permission_bench() -> Benchmark {
    create_file_bench("file_permission", |task, file| {
        let _ = criterion::black_box(
            security::file_permission(task, file, PermissionFlags::READ).unwrap(),
        );
    })
}

fn fs_node_permission_bench() -> Benchmark {
    create_file_bench("fs_node_permission", |task, file| {
        let _ = criterion::black_box(
            security::fs_node_permission(
                task,
                file.node(),
                PermissionFlags::READ,
                security::Auditable::None,
            )
            .unwrap(),
        );
    })
}

fn check_file_ioctl_access_bench() -> Benchmark {
    create_file_bench("check_file_ioctl_access", |task, file| {
        let _ = criterion::black_box(
            security::check_file_ioctl_access(task, file, starnix_uapi::TCGETS).unwrap(),
        );
    })
}

fn binder_transaction_bench() -> Benchmark {
    let executor = fuchsia_async::LocalExecutor::default();
    let (kernel, task, _security_server) = create_kernel_and_task_with_selinux();
    let task = Box::leak(Box::new(task));
    let connection_state = Box::leak(Box::new(security::binder_connection_alloc(task)));

    Benchmark::new("binder_transaction", move |b| {
        let _kernel = &kernel;
        let _executor = &executor;
        b.iter(|| {
            let _ = criterion::black_box(
                security::binder_transaction(task, task, connection_state).unwrap(),
            );
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
        .warm_up_time(Duration::from_millis(100))
        .measurement_time(Duration::from_secs(1))
        .sample_size(50);

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
    c.bench("fuchsia.sestarnix", concurrent_access_cache_get_bench());
    c.bench("fuchsia.sestarnix", file_permission_bench());
    c.bench("fuchsia.sestarnix", fs_node_permission_bench());
    c.bench("fuchsia.sestarnix", check_file_ioctl_access_bench());
    c.bench("fuchsia.sestarnix", binder_transaction_bench());
}
