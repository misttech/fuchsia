// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Error};
use fidl_fuchsia_memory_sampler::ModuleMap;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use prost::Message;
use zx::Vmo;

use crate::crash_reporter::ProfileReport;
use crate::pprof;

/// The default sampling rate in bytes (128 KiB). This matches the default
/// client-side sampling rate configured in the instrumentation library.
const DEFAULT_SAMPLING_RATE_BYTES: u64 = 131072;

/// The gzip compression level applied to profiles.
///
/// Experimental tests suggest that level 1 is a good tradeoff between
/// compression rate and CPU usage.
const PROFILE_COMPRESSION_LEVEL: Compression = Compression::new(1);

pub type StackTrace = Vec<u64>;

/// Represents an allocation for which no deallocation has been
/// reported.
#[derive(Clone, Debug, PartialEq)]
pub struct LiveAllocation {
    pub size: u64,
    pub scale_factor: f64,
    pub stack_trace: Rc<StackTrace>,
}

/// Aggregated counter of allocations for which a deallocation has
/// been recorded.
#[derive(Clone, Default, Debug)]
pub struct DeadAllocationCounter {
    pub total_size: f64,
    pub count: f64,
}

/// Aggregated counter of deallocations.
#[derive(Clone, Default, Debug)]
pub struct DeallocationCounter {
    pub total_size: f64,
    pub count: f64,
}

static NEXT_PROVIDER_ID: AtomicU64 = AtomicU64::new(1);

/// Accumulator for profiling information.
#[derive(Debug)]
pub struct ProfileBuilder {
    provider_id: u64,
    process_name: String,
    module_map: Vec<ModuleMap>,
    /// Mapping from addresses to allocations; we assume there can
    /// only be one live allocation at a given address. Reallocations
    /// are recorded as a sequence of a deallocation and a new
    /// allocation.
    live_allocations: HashMap<u64, LiveAllocation>,
    /// Mapping from a stack trace to a counter of dead
    /// allocations. This representation aggregates allocations from
    /// the same call site to save memory.
    dead_allocations: HashMap<Rc<StackTrace>, DeadAllocationCounter>,
    /// Mapping from a stack trace to a counter of deallocations. This
    /// representation aggregates deallocations from the same call
    /// site to save memory.
    deallocations: HashMap<Rc<StackTrace>, DeallocationCounter>,
    /// Set used to hold references to recorded stack traces. This can
    /// be used to drastically reduce memory usage from allocations
    /// from an already known call site: additional allocations would
    /// store a reference, rather than the entire stack trace that
    /// could be fairly large.
    stack_traces: HashSet<Rc<StackTrace>>,
    is_lossy: bool,
}

impl Default for ProfileBuilder {
    fn default() -> Self {
        Self {
            provider_id: NEXT_PROVIDER_ID.fetch_add(1, Ordering::Relaxed),
            process_name: String::new(),
            module_map: Vec::new(),
            live_allocations: HashMap::new(),
            dead_allocations: HashMap::new(),
            deallocations: HashMap::new(),
            stack_traces: HashSet::new(),
            is_lossy: false,
        }
    }
}

/// Computes the unsampling scale factor for an allocation of a given `size` using
/// the Poisson sampling interval `rate`.
///
/// Under Poisson sampling, the probability of sampling an allocation of size $S$
/// with a mean sampling rate $R$ is $P(S) = 1 - e^{-S / R}$.
///
/// To reconstruct the original unsampled allocation volume, we scale each sampled
/// allocation's size and count by the inverse of its sampling probability:
/// $W = 1 / P(S) = 1 / (1 - e^{-S / R})$.
///
/// A rate of 0 is meaningful: it denotes "never wait before sampling again", i.e.
/// sample every allocation.
fn scale_factor(size: u64, rate: u64) -> f64 {
    if size == 0 || rate == 0 { 1.0 } else { -1.0 / (-(size as f64) / (rate as f64)).exp_m1() }
}

impl ProfileBuilder {
    pub fn process_name(&self) -> &str {
        &self.process_name
    }

    /// Remove all stack traces that are no longer referenced by any
    /// recorded allocation.
    ///
    /// Note: this function can be used to reclaim memory after
    /// consuming allocations/deallocations (e.g. by producing a
    /// partial profile).
    fn prune_unreferenced_stack_traces(&mut self) {
        self.stack_traces.retain(|st| Rc::strong_count(st) > 1);
    }
    /// Add the given stack_trace to the cache, if needed, then return
    /// a reference to the cached value.
    fn cache_stack_trace(&mut self, stack_trace: StackTrace) -> Rc<StackTrace> {
        // Note: `Entry` on `HashSet` would save us from cloning the
        // stack trace here.
        if !self.stack_traces.contains(&stack_trace) {
            self.stack_traces.insert(Rc::new(stack_trace.clone()));
        };
        self.stack_traces.get(&stack_trace).unwrap().clone()
    }
    /// Register an allocation.

    /// It is considered live until a deallocation for the same address has been
    /// reported. Note that this assumes that allocations and deallocations at a
    /// given address are reported in order.
    ///
    /// `sampling_rate` is the sampling rate used by the sampler at the time of
    /// sampling this allocation, and is used to weight the allocation by its
    /// likelihood to get sampled.
    pub fn allocate(
        &mut self,
        address: u64,
        stack_trace: StackTrace,
        size: u64,
        sampling_rate: Option<u64>,
    ) {
        let scale = scale_factor(size, sampling_rate.unwrap_or(DEFAULT_SAMPLING_RATE_BYTES));
        self.allocate_with_scale(address, stack_trace, size, scale);
    }

    pub fn allocate_with_scale(
        &mut self,
        address: u64,
        stack_trace: StackTrace,
        size: u64,
        scale_factor: f64,
    ) {
        let stack_trace = self.cache_stack_trace(stack_trace);
        self.live_allocations.insert(address, LiveAllocation { size, scale_factor, stack_trace });
    }
    /// Register a deallocation, if the corresponding allocation has
    /// been registered before.
    pub fn deallocate(&mut self, address: u64, stack_trace: StackTrace) {
        self.live_allocations.remove(&address).map(|allocation| {
            {
                let dead_allocation =
                    self.dead_allocations.entry(allocation.stack_trace).or_default();
                dead_allocation.count += allocation.scale_factor;
                dead_allocation.total_size += (allocation.size as f64) * allocation.scale_factor;
            }
            {
                let stack_trace = self.cache_stack_trace(stack_trace);
                let deallocation = self.deallocations.entry(stack_trace).or_default();
                deallocation.count += allocation.scale_factor;
                deallocation.total_size += (allocation.size as f64) * allocation.scale_factor;
            }
        });
    }
    /// Set the process information necessary to produce a profile.
    pub fn set_process_info(
        &mut self,
        process_name: Option<String>,
        module_map: impl Iterator<Item = ModuleMap>,
    ) {
        if let Some(process_name) = process_name {
            self.process_name = process_name;
        }
        self.module_map.extend(module_map);
    }
    /// Returns the approximate amount of stack traces currently
    /// recorded in this instance.
    ///
    /// Note: Consuming past allocation events (e.g. by producing a
    /// partial profile) can be followed by a call to
    /// `self.prune_unereferenced_stack_traces` to reclaim space; this
    /// function can be used as an heuristic to estimate the amount of
    /// memory that can be reclaimed.
    pub fn get_approximate_reclaimable_stack_traces_count(&self) -> usize {
        self.dead_allocations.len() + self.deallocations.len()
    }
    /// Sets whether the profile session experienced buffer saturation and is lossy.
    pub fn set_lossy(&mut self, is_lossy: bool) {
        self.is_lossy = is_lossy;
    }

    /// Returns whether the profile session experienced buffer saturation and is lossy.
    pub fn is_lossy(&self) -> bool {
        self.is_lossy
    }

    /// Returns the process name to report, annotated when the profile is lossy.
    /// The annotation rides on the name because that is what `pprof` surfaces
    /// in the banner, tab title and profile picker, making the taint hard to
    /// overlook.
    fn annotated_process_name(&self) -> String {
        if !self.is_lossy {
            return self.process_name.clone();
        }
        if self.process_name.is_empty() {
            TAINT_SUFFIX.to_string()
        } else {
            format!("{} {}", self.process_name, TAINT_SUFFIX)
        }
    }

    /// Finalize the profile. Consumes this builder.
    pub fn build(self) -> Result<ProfileReport, Error> {
        let process_name = self.annotated_process_name();
        let profile = pprof::build_profile(
            self.module_map.iter(),
            self.live_allocations.values(),
            self.dead_allocations,
            self.deallocations,
            &self.stack_traces,
            self.is_lossy,
        );

        let (vmo, size) = profile_to_vmo(profile)?;
        Ok(ProfileReport::Final { provider_id: self.provider_id, process_name, profile: vmo, size })
    }
    /// Produce a partial profile from a process that is still
    /// live. Drop `dead_allocations` from `self`, and prune the
    /// cache.
    ///
    /// Note: this lets one produce regular running profiles from a
    /// long-lived process, while clearing from memory the state that
    /// will no longer be useful.
    pub fn build_partial_profile(&mut self, iteration: usize) -> Result<ProfileReport, Error> {
        let process_name = self.annotated_process_name();
        let profile = {
            pprof::build_profile(
                self.module_map.iter(),
                self.live_allocations.values(),
                std::mem::replace(&mut self.dead_allocations, HashMap::new()),
                std::mem::replace(&mut self.deallocations, HashMap::new()),
                &self.stack_traces,
                self.is_lossy,
            )
        };
        self.prune_unreferenced_stack_traces();

        let (vmo, size) = profile_to_vmo(profile)?;
        Ok(ProfileReport::Partial {
            provider_id: self.provider_id,
            process_name,
            profile: vmo,
            size,
            iteration,
        })
    }
}

const TAINT_SUFFIX: &str = "[TAINTED: BUFFER OVERFLOW]";

impl ProfileReport {
    /// Merge this `Partial` report with a newer (`Partial` or `Final`) report
    /// from the same provider. If the newer report is `Final`, the result
    /// collapses into a `Final` report.
    ///
    /// On error, returns an intact fallback report (`self` if `newer` failed
    /// to decode, or `newer` otherwise) along with the error so the caller can
    /// recover without losing valid data.
    pub fn merge(self, newer: Self) -> Result<Self, (Self, Error)> {
        let (old_vmo, old_size) = match &self {
            ProfileReport::Partial { profile, size, .. } => (profile, *size),
            ProfileReport::Final { .. } => {
                return Err((
                    newer,
                    anyhow::format_err!("Cannot merge a new profile into an already final report"),
                ));
            }
        };
        let older_proto = match vmo_to_profile(old_vmo, old_size) {
            Ok(proto) => proto,
            Err(e) => return Err((newer, e)),
        };

        let (provider_id, new_name, new_vmo, new_size, new_iteration) = match &newer {
            ProfileReport::Partial { provider_id, process_name, profile, size, iteration } => {
                (*provider_id, process_name, profile, *size, Some(*iteration))
            }
            ProfileReport::Final { provider_id, process_name, profile, size } => {
                (*provider_id, process_name, profile, *size, None)
            }
        };
        let newer_proto = match vmo_to_profile(new_vmo, new_size) {
            Ok(proto) => proto,
            Err(e) => return Err((self, e)),
        };
        let ProfileReport::Partial { process_name: old_name, profile: old_vmo, .. } = self else {
            unreachable!();
        };
        drop(old_vmo);

        let merged_proto = pprof::merge_same_process_profiles(older_proto, newer_proto);
        let (profile, size) = match profile_to_vmo(merged_proto) {
            Ok(res) => res,
            Err(e) => return Err((newer, e)),
        };

        let process_name = if old_name.ends_with(TAINT_SUFFIX) && !new_name.ends_with(TAINT_SUFFIX)
        {
            old_name
        } else {
            new_name.clone()
        };

        match new_iteration {
            Some(iteration) => {
                Ok(ProfileReport::Partial { provider_id, process_name, profile, size, iteration })
            }
            None => Ok(ProfileReport::Final { provider_id, process_name, profile, size }),
        }
    }
}

/// Serialize a profile to a VMO, compressed with gzip. On success,
/// returns a tuple of a `Vmo` and the size of its content.
///
/// Note: profiles are compressed as early as possible, because they may
/// sit in the crash reporting queue for a while before being filed; on
/// representative profiles, this divides the size of a queued profile by
/// more than four.
///
/// Note: consumers of pprof profiles detect gzip compression from the
/// header of the profile, so compressing here is transparent to them.
/// The format is reflected in `crash_reporter::PROFILE_ATTACHMENT_EXTENSION`,
/// which has to be updated alongside it.
fn profile_to_vmo(profile: pprof::pproto::Profile) -> Result<(Vmo, u64), Error> {
    let compressed_profile = {
        // Note: each representation of the profile is released as soon
        // as the next one is built, so that the profile, its
        // uncompressed serialization and the VMO never coexist. The
        // capacity hint is slightly above the ~1/4 compression ratio
        // observed on representative profiles to avoid reallocating on
        // profiles above the median compressed size.
        let proto_profile = profile.encode_to_vec();
        drop(profile);
        let mut encoder =
            GzEncoder::new(Vec::with_capacity(proto_profile.len() / 3), PROFILE_COMPRESSION_LEVEL);
        encoder.write_all(&proto_profile).context("Failed to compress profile")?;
        encoder.finish().context("Failed to finalize compressed profile")?
    };

    let size = compressed_profile.len() as u64;
    let vmo = Vmo::create(size)?;
    vmo.write(&compressed_profile[..], 0)?;
    Ok((vmo, size))
}

/// Deserialize a gzip-compressed profile from a VMO.
pub(crate) fn vmo_to_profile(vmo: &Vmo, size: u64) -> Result<pprof::pproto::Profile, Error> {
    let decompressed_profile = {
        let compressed_profile = vmo.read_to_vec(0, size)?;
        let mut decoder = GzDecoder::new(&compressed_profile[..]);
        let mut decompressed = Vec::with_capacity((size as usize).saturating_mul(5));
        decoder.read_to_end(&mut decompressed).context("Failed to decompress profile from VMO")?;
        decompressed
    };
    pprof::pproto::Profile::decode(&decompressed_profile[..])
        .context("Failed to decode pprof protobuf from VMO")
}

#[cfg(test)]
mod test {
    use crate::crash_reporter::ProfileReport;
    use crate::pprof::pproto::Profile;
    use crate::profile_builder::{
        DEFAULT_SAMPLING_RATE_BYTES, DeadAllocationCounter, DeallocationCounter, ModuleMap,
        ProfileBuilder,
    };
    use fidl_fuchsia_memory_sampler::ExecutableSegment;
    use flate2::read::GzDecoder;
    use prost::Message;
    use std::io::Read;

    #[fuchsia::test]
    fn test_allocate() {
        let mut builder = ProfileBuilder::default();
        let address = 0x1000;
        let stack_trace = vec![];
        let size = 10;

        builder.allocate(address, stack_trace.clone(), size, None);

        let allocation =
            builder.live_allocations.get(&address).expect("Could not retrieve live allocation.");

        let expected_scale_factor = super::scale_factor(size, DEFAULT_SAMPLING_RATE_BYTES);
        assert_eq!(size, allocation.size);
        assert!((allocation.scale_factor - expected_scale_factor).abs() < 1e-5);
        assert_eq!(stack_trace, *(allocation.stack_trace));
        assert_eq!(1.0, super::scale_factor(size, 0));
        assert_eq!(1.0, super::scale_factor(0, DEFAULT_SAMPLING_RATE_BYTES));
    }

    #[fuchsia::test]
    fn test_deallocate_mismatch() {
        let mut builder = ProfileBuilder::default();
        let address = 0x1000;
        let stack_trace = vec![];

        builder.deallocate(address, stack_trace);

        let deallocations = builder.deallocations;

        assert!(deallocations.is_empty());
    }

    #[fuchsia::test]
    fn test_deallocate_match() {
        let mut builder = ProfileBuilder::default();
        let address = 0x1000;
        let allocation_stack_trace = vec![1, 2];
        let deallocation_stack_trace = vec![3, 4];
        let size = 10;

        builder.allocate(address, allocation_stack_trace.clone(), size, None);
        builder.deallocate(address, deallocation_stack_trace.clone());

        assert!(builder.live_allocations.is_empty());
        let expected_count = super::scale_factor(size, DEFAULT_SAMPLING_RATE_BYTES);
        let expected_size = expected_count * (size as f64);
        {
            let allocations = builder.dead_allocations;
            assert_eq!(1, allocations.values().len());
            {
                let (stack_trace, DeadAllocationCounter { count, total_size }) =
                    allocations.into_iter().next().unwrap();
                assert!((total_size - expected_size).abs() < 1e-5);
                assert!((count - expected_count).abs() < 1e-5);
                assert_eq!(allocation_stack_trace, *(stack_trace));
            }
        }

        {
            let deallocations = builder.deallocations;
            assert_eq!(1, deallocations.values().len());
            {
                let (stack_trace, DeallocationCounter { count, total_size }) =
                    deallocations.into_iter().next().unwrap();
                assert!((total_size - expected_size).abs() < 1e-5);
                assert!((count - expected_count).abs() < 1e-5);
                assert_eq!(deallocation_stack_trace, *(stack_trace));
            }
        }
    }

    #[fuchsia::test]
    fn test_build_partial_profile_prunes_stack_traces() {
        let mut builder = ProfileBuilder::default();
        let address = 0x1000;
        let allocation_stack_trace = vec![1, 2];
        let deallocation_stack_trace = vec![3, 4];
        let size = 10;
        let test_index = 42;

        builder.allocate(address, allocation_stack_trace.clone(), size, None);
        builder.deallocate(address, deallocation_stack_trace.clone());

        assert_ne!(0, builder.get_approximate_reclaimable_stack_traces_count());
        let _ = builder.build_partial_profile(test_index).unwrap();
        assert_eq!(0, builder.get_approximate_reclaimable_stack_traces_count());
    }

    #[fuchsia::test]
    fn test_set_process_info() {
        let mut builder = ProfileBuilder::default();
        let process_name = "test_process".to_string();
        let module_map = vec![ModuleMap {
            build_id: Some(vec![1, 2, 3, 4]),
            executable_segments: Some(vec![ExecutableSegment {
                start_address: Some(0),
                size: Some(10),
                relative_address: Some(100),
                ..Default::default()
            }]),
            ..Default::default()
        }];

        builder.set_process_info(Some(process_name.clone()), module_map.clone().into_iter());

        assert_eq!(process_name, builder.process_name);
        assert_eq!(module_map, builder.module_map);
    }

    #[fuchsia::test]
    fn test_lossy_profile_name() {
        let mut builder = ProfileBuilder::default();
        builder.set_process_info(Some("my_proc".to_string()), [].into_iter());
        builder.set_lossy(true);
        let report = builder.build().unwrap();
        assert_eq!(report.process_name(), "my_proc [TAINTED: BUFFER OVERFLOW]");
    }

    #[fuchsia::test]
    fn test_profile_is_compressed() {
        const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
        let mut builder = ProfileBuilder::default();
        builder.allocate(0x1000, vec![1, 2], 10, None);

        let ProfileReport::Partial { profile, size, .. } =
            builder.build_partial_profile(0).unwrap()
        else {
            panic!("Expected a partial profile.");
        };
        let compressed_profile = profile.read_to_vec(0, size).unwrap();

        // Consumers of profiles rely on the `gzip` header to detect
        // that they are compressed.
        assert_eq!(
            GZIP_MAGIC,
            compressed_profile[..2],
            "Profile should start with the gzip magic bytes."
        );

        let mut proto_profile = Vec::new();
        GzDecoder::new(&compressed_profile[..])
            .read_to_end(&mut proto_profile)
            .expect("Profile should be gzip-compressed.");
        let profile =
            Profile::decode(&proto_profile[..]).expect("Profile should decode as a pprof profile.");
        assert_eq!(1, profile.sample.len());
    }
}
