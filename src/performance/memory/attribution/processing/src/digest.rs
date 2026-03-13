// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{ProcessedAttributionData, ZXName};
use anyhow::Result;
use fidl_fuchsia_kernel as fkernel;
use fidl_fuchsia_memory_attribution_plugin as fplugin;
use regex::bytes::Regex;
use serde::de::Error;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::collections::hash_map::Entry::Occupied;
#[cfg(target_os = "fuchsia")]
use {crate::CATEGORY_MEMORY_CAPTURE, fuchsia_trace::duration};

const UNDIGESTED: &str = "Undigested";
const ORPHANED: &str = "Orphaned";
const KERNEL: &str = "Kernel";
const FREE: &str = "Free";
const PAGER_TOTAL: &str = "[Addl]PagerTotal";
const PAGER_NEWEST: &str = "[Addl]PagerNewest";
const PAGER_OLDEST: &str = "[Addl]PagerOldest";
const DISCARDABLE_LOCKED: &str = "[Addl]DiscardableLocked";
const DISCARDABLE_UNLOCKED: &str = "[Addl]DiscardableUnlocked";
const ZRAM_COMPRESSED_BYTES: &str = "[Addl]ZramCompressedBytes";
const POPULATED_ANONYMOUS_BYTES: &str = "[Addl]PopulatedAnonymousBytes";

/// Represents a specification for aggregating memory usage in meaningful groups.
///
/// `name` represents the meaningful name of the group; grouping is done based on process and VMO
/// names.
///
// Note: This needs to mirror `//src/lib/assembly/memory_buckets/src/memory_buckets.rs`, but cannot
// reuse it directly because it is an host-only library.
#[derive(Clone, Debug, Deserialize)]
pub struct BucketDefinition {
    pub name: String,
    #[serde(deserialize_with = "deserialize_regex")]
    pub process: Option<Regex>,
    #[serde(deserialize_with = "deserialize_regex")]
    pub vmo: Option<Regex>,
    #[serde(default, deserialize_with = "deserialize_regex")]
    pub principal: Option<Regex>,
    pub event_code: u64,
}

impl BucketDefinition {
    /// Tests whether a process matches this bucket's definition, based on its name.
    fn process_match(&self, process: &ZXName) -> bool {
        self.process.as_ref().is_none_or(|p| p.is_match(process.as_bstr()))
    }

    /// Tests whether a VMO matches this bucket's definition, based on its name.
    fn vmo_match(&self, vmo: &ZXName) -> bool {
        self.vmo.as_ref().is_none_or(|v| v.is_match(vmo.as_bstr()))
    }

    /// Tests whether any of the specified principal names match this bucket's definition.
    fn principals_match(&self, principals: &Vec<&str>) -> bool {
        self.principal
            .as_ref()
            .is_none_or(|a| principals.iter().any(|name| a.is_match(name.as_bytes())))
    }
}

// Teach serde to deserialize an optional regex.
fn deserialize_regex<'de, D>(d: D) -> Result<Option<Regex>, D::Error>
where
    D: Deserializer<'de>,
{
    // Deserialize as Option<&str>
    Option::<String>::deserialize(d)
        // If the parsing failed, return the error, otherwise transform the value
        .and_then(|os| {
            os
                // If there is a value, try to parse it as a Regex.
                .map(|s| {
                    Regex::new(&s)
                        // If the regex compilation failed, wrap the error in the error type expected
                        // by serde.
                        .map_err(D::Error::custom)
                })
                // If there was a value but it failed to compile, return an error, otherwise return
                // the potentially parsed option.
                .transpose()
        })
}

/// Aggregates bytes in categories with human readable names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Bucket {
    pub name: String,
    pub populated_size: u64,
    pub committed_size: u64,
    pub vmos: Option<Vec<NamedVmo>>,
}

/// Contains a view of the system's memory usage, aggregated in groups called buckets, which are
/// configurable.
#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Digest {
    pub buckets: Vec<Bucket>,
}

/// Non-owning structure to keep track of known undigested VMOs.
struct UndigestedVmo<'a> {
    populated_size: u64,
    committed_size: u64,
    name: &'a ZXName,
    principals: &'a Vec<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
/// Owning structure to report known VMOs.
pub struct NamedVmo {
    pub name: ZXName,
    pub populated_size: u64,
    pub committed_size: u64,
    pub principals: Vec<String>,
}

impl Digest {
    /// Given means to query the system for memory usage, and a specification, this function
    /// aggregates the current memory usage into human displayable units we call buckets.
    pub fn compute(
        attribution_data: &ProcessedAttributionData,
        kmem_stats: &fkernel::MemoryStats,
        kmem_stats_compression: &fkernel::MemoryStatsCompression,
        bucket_definitions: &[BucketDefinition],
        detailed_vmos: bool,
    ) -> Result<Digest> {
        #[cfg(target_os = "fuchsia")]
        duration!(CATEGORY_MEMORY_CAPTURE, c"Digest::compute");

        // Maps resources' (VMO, Process, Job. See Resource) ids
        // to their owner, i.e. the principal they have been
        // attributed to.
        let owners: HashMap<u64, Vec<&str>> = {
            let koid_to_principal = attribution_data
                .principals
                .iter()
                .flat_map(|(_, p)| p.resources.iter().map(|r| (*r, p.name())));

            let mut owners: HashMap<u64, Vec<_>> = HashMap::new();
            for (koid, principal) in koid_to_principal {
                let principals = owners.entry(koid).or_default();
                principals.push(principal);
            }
            owners
        };

        let no_principals = vec![];
        let mut populated_reclaimable_bytes = 0;
        let mut undigested_vmos: HashMap<u64, UndigestedVmo<'_>> = attribution_data
            .resources
            .iter()
            .filter_map(|(koid, r)| match &r.resource.resource_type {
                fplugin::ResourceType::Vmo(vmo) => {
                    attribution_data.resource_names.get(r.resource.name_index).and_then(|name| {
                        let populated_size = vmo.scaled_populated_bytes?;
                        let committed_size = vmo.scaled_committed_bytes?;
                        if vmo.flags.map_or(false, |flags| {
                            flags
                                & (zx_types::ZX_INFO_VMO_PAGER_BACKED
                                    | zx_types::ZX_INFO_VMO_DISCARDABLE)
                                != 0
                        }) {
                            populated_reclaimable_bytes += populated_size;
                        }
                        Some((
                            *koid,
                            UndigestedVmo {
                                name,
                                populated_size,
                                committed_size,
                                principals: owners.get(koid).unwrap_or(&no_principals),
                            },
                        ))
                    })
                }
                _ => None,
            })
            .collect();
        let processes: Vec<(&ZXName, &fplugin::Process)> = attribution_data
            .resources
            .values()
            .filter_map(|r| match &r.resource.resource_type {
                fplugin::ResourceType::Process(process) => attribution_data
                    .resource_names
                    .get(r.resource.name_index)
                    .map(|name| (name, process)),
                _ => None,
            })
            .collect();

        let mut buckets: Vec<Bucket> = bucket_definitions
            .iter()
            .map(|bd| {
                let mut bucket = Bucket {
                    name: bd.name.to_owned(),
                    populated_size: 0,
                    committed_size: 0,
                    vmos: None,
                };
                processes.iter().for_each(|(process_name, process)| {
                    if bd.process_match(process_name) {
                        for koid in process.vmos.iter().flatten() {
                            let (populated_size, committed_size) = match undigested_vmos
                                .entry(*koid)
                            {
                                Occupied(e) => {
                                    let UndigestedVmo { name, principals, .. } = e.get();
                                    if bd.vmo_match(&name) && bd.principals_match(principals) {
                                        let (_, vmo) = e.remove_entry();
                                        if detailed_vmos {
                                            bucket.vmos.get_or_insert_default().push(NamedVmo {
                                                name: vmo.name.clone(),
                                                populated_size: vmo.populated_size,
                                                committed_size: vmo.committed_size,
                                                principals: vmo
                                                    .principals
                                                    .into_iter()
                                                    .map(|&name| name.to_owned())
                                                    .collect(),
                                            });
                                        }
                                        (vmo.populated_size, vmo.committed_size)
                                    } else {
                                        (0, 0)
                                    }
                                }
                                _ => (0, 0),
                            };
                            bucket.committed_size += committed_size;
                            bucket.populated_size += populated_size;
                        }
                    };
                });
                bucket
            })
            .collect();

        // This bucket contains the total size of the known VMOs that have not been covered
        // by any other bucket.
        let undigested = {
            let (populated_size, committed_size) = undigested_vmos
                .values()
                .map(|UndigestedVmo { populated_size, committed_size, .. }| {
                    (*populated_size, *committed_size)
                })
                .fold((0, 0), |(total_populated, total_committed), (populated, committed)| {
                    (total_populated + populated, total_committed + committed)
                });

            Bucket {
                name: UNDIGESTED.to_string(),
                populated_size: populated_size,
                committed_size,
                vmos: if detailed_vmos {
                    Some(
                        undigested_vmos
                            .values()
                            .map(|vmo| NamedVmo {
                                name: vmo.name.clone(),
                                populated_size: vmo.populated_size,
                                committed_size: vmo.committed_size,
                                principals: vmo
                                    .principals
                                    .into_iter()
                                    .map(|&name| name.to_owned())
                                    .collect(),
                            })
                            .collect(),
                    )
                } else {
                    None
                },
            }
        };

        let total_vmo_size: u64 = undigested.committed_size
            + buckets.iter().map(|Bucket { committed_size, .. }| committed_size).sum::<u64>();

        // Extend the configured aggregation with a number of additional, occasionally useful meta
        // aggregations.
        buckets.extend([
            undigested,
            // This bucket accounts for VMO bytes that have been allocated by the kernel, but not
            // claimed by any VMO (anymore).
            {
                let size = kmem_stats.vmo_bytes.unwrap_or(0).saturating_sub(total_vmo_size);
                Bucket {
                    name: ORPHANED.to_string(),
                    populated_size: size,
                    committed_size: size,
                    vmos: None,
                }
            },
            // This bucket aggregates overall kernel memory usage.
            {
                let size = (|| {
                    Some(
                        kmem_stats.wired_bytes?
                            + kmem_stats.total_heap_bytes?
                            + kmem_stats.mmu_overhead_bytes?
                            + kmem_stats.ipc_bytes?
                            + kmem_stats.other_bytes?
                            + kmem_stats.slab_bytes?
                            + kmem_stats.cache_bytes?,
                    )
                })()
                .unwrap_or(0);
                Bucket {
                    name: KERNEL.to_string(),
                    populated_size: size,
                    committed_size: size,
                    vmos: None,
                }
            },
            // This bucket contains the amount of free memory in the system.
            {
                let size = kmem_stats.free_bytes.unwrap_or(0);
                Bucket {
                    name: FREE.to_string(),
                    populated_size: size,
                    committed_size: size,
                    vmos: None,
                }
            },
            // Those buckets contain pager related information.
            {
                let size = kmem_stats.vmo_reclaim_total_bytes.unwrap_or(0);
                Bucket {
                    name: PAGER_TOTAL.to_string(),
                    populated_size: size,
                    committed_size: size,
                    vmos: None,
                }
            },
            {
                let size = kmem_stats.vmo_reclaim_newest_bytes.unwrap_or(0);
                Bucket {
                    name: PAGER_NEWEST.to_string(),
                    populated_size: size,
                    committed_size: size,
                    vmos: None,
                }
            },
            {
                let size = kmem_stats.vmo_reclaim_oldest_bytes.unwrap_or(0);
                Bucket {
                    name: PAGER_OLDEST.to_string(),
                    populated_size: size,
                    committed_size: size,
                    vmos: None,
                }
            },
            // Those buckets account for discardable memory.
            {
                let size = kmem_stats.vmo_discardable_locked_bytes.unwrap_or(0);
                Bucket {
                    name: DISCARDABLE_LOCKED.to_string(),
                    populated_size: size,
                    committed_size: size,
                    vmos: None,
                }
            },
            {
                let size = kmem_stats.vmo_discardable_unlocked_bytes.unwrap_or(0);
                Bucket {
                    name: DISCARDABLE_UNLOCKED.to_string(),
                    populated_size: size,
                    committed_size: size,
                    vmos: None,
                }
            },
            // This bucket accounts for compressed memory.
            {
                let size = kmem_stats_compression.compressed_storage_bytes.unwrap_or(0);
                Bucket {
                    name: ZRAM_COMPRESSED_BYTES.to_string(),
                    populated_size: size,
                    committed_size: size,
                    vmos: None,
                }
            },
            // This bucket accounts for all populated anonymous memory (non-reclaimable).
            {
                let size = (|| {
                    Some(
                        kmem_stats.total_bytes?
                            + kmem_stats_compression
                                .uncompressed_storage_bytes?
                                .saturating_sub(kmem_stats.free_bytes?)
                                .saturating_sub(kmem_stats.zram_bytes?)
                                .saturating_sub(populated_reclaimable_bytes),
                    )
                })()
                .unwrap_or(0);

                Bucket {
                    name: POPULATED_ANONYMOUS_BYTES.to_string(),
                    populated_size: size,
                    committed_size: size,
                    vmos: None,
                }
            },
        ]);
        Ok(Digest { buckets })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Attribution, AttributionData, GlobalPrincipalIdentifier, Principal, PrincipalDescription,
        PrincipalType, ProcessedAttributionData, Resource, ResourceReference, attribute_vmos,
    };
    use fidl_fuchsia_memory_attribution_plugin as fplugin;

    fn get_attribution_data() -> ProcessedAttributionData {
        attribute_vmos(AttributionData {
            principals_vec: vec![
                Principal {
                    identifier: GlobalPrincipalIdentifier::new_for_test(1),
                    description: Some(PrincipalDescription::Component("principal".to_owned())),
                    principal_type: PrincipalType::Runnable,
                    parent: Some(GlobalPrincipalIdentifier::new_for_test(2)),
                },
                Principal {
                    identifier: GlobalPrincipalIdentifier::new_for_test(2),
                    description: Some(PrincipalDescription::Component("parent".to_owned())),
                    principal_type: PrincipalType::Runnable,
                    parent: None,
                },
            ],
            resources_vec: vec![
                Resource {
                    koid: 10,
                    name_index: 0,
                    resource_type: fplugin::ResourceType::Vmo(fplugin::Vmo {
                        parent: None,
                        private_committed_bytes: Some(1024),
                        private_populated_bytes: Some(2048),
                        scaled_committed_bytes: Some(512),
                        scaled_populated_bytes: Some(2048),
                        total_committed_bytes: Some(1024),
                        total_populated_bytes: Some(2048),
                        ..Default::default()
                    }),
                },
                Resource {
                    koid: 20,
                    name_index: 1,
                    resource_type: fplugin::ResourceType::Vmo(fplugin::Vmo {
                        parent: None,
                        private_committed_bytes: Some(1024),
                        private_populated_bytes: Some(2048),
                        scaled_committed_bytes: Some(512),
                        scaled_populated_bytes: Some(2048),
                        total_committed_bytes: Some(1024),
                        total_populated_bytes: Some(2048),
                        ..Default::default()
                    }),
                },
                Resource {
                    koid: 30,
                    name_index: 1,
                    resource_type: fplugin::ResourceType::Process(fplugin::Process {
                        vmos: Some(vec![10, 20]),
                        ..Default::default()
                    }),
                },
            ],
            resource_names: vec![
                ZXName::try_from_bytes(b"resource").unwrap(),
                ZXName::try_from_bytes(b"matched").unwrap(),
            ],
            attributions: vec![Attribution {
                source: GlobalPrincipalIdentifier::new_for_test(1),
                subject: GlobalPrincipalIdentifier::new_for_test(1),
                resources: vec![ResourceReference::KernelObject(20)],
            }],
        })
    }

    fn get_kernel_stats() -> (fkernel::MemoryStats, fkernel::MemoryStatsCompression) {
        (
            fkernel::MemoryStats {
                total_bytes: Some(1),
                free_bytes: Some(2),
                wired_bytes: Some(3),
                total_heap_bytes: Some(4),
                free_heap_bytes: Some(5),
                vmo_bytes: Some(10000),
                mmu_overhead_bytes: Some(7),
                ipc_bytes: Some(8),
                other_bytes: Some(9),
                free_loaned_bytes: Some(10),
                cache_bytes: Some(11),
                slab_bytes: Some(12),
                zram_bytes: Some(13),
                vmo_reclaim_total_bytes: Some(14),
                vmo_reclaim_newest_bytes: Some(15),
                vmo_reclaim_oldest_bytes: Some(16),
                vmo_reclaim_disabled_bytes: Some(17),
                vmo_discardable_locked_bytes: Some(18),
                vmo_discardable_unlocked_bytes: Some(19),
                ..Default::default()
            },
            fkernel::MemoryStatsCompression {
                uncompressed_storage_bytes: Some(20),
                compressed_storage_bytes: Some(21),
                compressed_fragmentation_bytes: Some(22),
                compression_time: Some(23),
                decompression_time: Some(24),
                total_page_compression_attempts: Some(25),
                failed_page_compression_attempts: Some(26),
                total_page_decompressions: Some(27),
                compressed_page_evictions: Some(28),
                eager_page_compressions: Some(29),
                memory_pressure_page_compressions: Some(30),
                critical_memory_page_compressions: Some(31),
                pages_decompressed_unit_ns: Some(32),
                pages_decompressed_within_log_time: Some([40, 41, 42, 43, 44, 45, 46, 47]),
                ..Default::default()
            },
        )
    }

    fn sort_buckets_for_assert(digest: &mut Digest) {
        for bucket in digest.buckets.iter_mut() {
            for vmos in bucket.vmos.iter_mut() {
                vmos.sort_by(|vmo1, vmo2| vmo1.name.cmp(&vmo2.name));
            }
        }
    }

    #[test]
    fn test_digest_no_definitions() {
        let (kernel_stats, kernel_stats_compression) = get_kernel_stats();
        let digest = {
            let mut digest = Digest::compute(
                &get_attribution_data(),
                &kernel_stats,
                &kernel_stats_compression,
                &vec![],
                true,
            )
            .unwrap();
            sort_buckets_for_assert(&mut digest);
            digest
        };
        let expected_buckets = vec![
            // The two VMOs are unmatched, 512 + 512
            Bucket {
                name: UNDIGESTED.to_string(),
                populated_size: 4096,
                committed_size: 1024,
                vmos: Some(vec![
                    NamedVmo {
                        name: ZXName::from_string_lossy("matched"),
                        populated_size: 2048,
                        committed_size: 512,
                        principals: vec!["principal".to_string()],
                    },
                    NamedVmo {
                        name: ZXName::from_string_lossy("resource"),
                        populated_size: 2048,
                        committed_size: 512,
                        principals: vec![],
                    },
                ]),
            },
            // No matched VMOs, one UNDIGESTED VMO => 10000 - 1024 = 8976
            Bucket {
                name: ORPHANED.to_string(),
                populated_size: 8976,
                committed_size: 8976,
                vmos: None,
            },
            // wired + heap + mmu + ipc + other + slab + cache => 3 + 4 + 7 + 8 + 9 + 12 + 11 = 54
            Bucket { name: KERNEL.to_string(), populated_size: 54, committed_size: 54, vmos: None },
            Bucket { name: FREE.to_string(), populated_size: 2, committed_size: 2, vmos: None },
            Bucket {
                name: PAGER_TOTAL.to_string(),
                populated_size: 14,
                committed_size: 14,
                vmos: None,
            },
            Bucket {
                name: PAGER_NEWEST.to_string(),
                populated_size: 15,
                committed_size: 15,
                vmos: None,
            },
            Bucket {
                name: PAGER_OLDEST.to_string(),
                populated_size: 16,
                committed_size: 16,
                vmos: None,
            },
            Bucket {
                name: DISCARDABLE_LOCKED.to_string(),
                populated_size: 18,
                committed_size: 18,
                vmos: None,
            },
            Bucket {
                name: DISCARDABLE_UNLOCKED.to_string(),
                populated_size: 19,
                committed_size: 19,
                vmos: None,
            },
            Bucket {
                name: ZRAM_COMPRESSED_BYTES.to_string(),
                populated_size: 21,
                committed_size: 21,
                vmos: None,
            },
            Bucket {
                name: POPULATED_ANONYMOUS_BYTES.to_string(),
                populated_size: 6,
                committed_size: 6,
                vmos: None,
            },
        ];

        assert_eq!(digest.buckets, expected_buckets);
    }

    #[test]
    fn test_digest_with_matching_vmo() -> Result<(), anyhow::Error> {
        let (kernel_stats, kernel_stats_compression) = get_kernel_stats();
        let digest = {
            let mut digest = Digest::compute(
                &get_attribution_data(),
                &kernel_stats,
                &kernel_stats_compression,
                &vec![BucketDefinition {
                    name: "matched".to_string(),
                    process: None,
                    vmo: Some(Regex::new("matched")?),
                    principal: None,
                    event_code: Default::default(),
                }],
                true,
            )
            .unwrap();
            sort_buckets_for_assert(&mut digest);
            digest
        };
        let expected_buckets = vec![
            // One VMO is matched, the other is not
            Bucket {
                name: "matched".to_string(),
                populated_size: 2048,
                committed_size: 512,
                vmos: Some(vec![NamedVmo {
                    name: ZXName::from_string_lossy("matched"),
                    populated_size: 2048,
                    committed_size: 512,
                    principals: vec!["principal".to_owned()],
                }]),
            },
            // One unmatched VMO
            Bucket {
                name: UNDIGESTED.to_string(),
                populated_size: 2048,
                committed_size: 512,
                vmos: Some(vec![NamedVmo {
                    name: ZXName::from_string_lossy("resource"),
                    populated_size: 2048,
                    committed_size: 512,
                    principals: vec![],
                }]),
            },
            // One matched VMO, one unmatched VMO //=> 10000 - 512 - 512 = 8976
            Bucket {
                name: ORPHANED.to_string(),
                populated_size: 8976,
                committed_size: 8976,
                vmos: None,
            },
            // wired + heap + mmu + ipc + other + slab + cache => 3 + 4 + 7 + 8 + 9 + 12 + 11 = 54
            Bucket { name: KERNEL.to_string(), populated_size: 54, committed_size: 54, vmos: None },
            Bucket { name: FREE.to_string(), populated_size: 2, committed_size: 2, vmos: None },
            Bucket {
                name: PAGER_TOTAL.to_string(),
                populated_size: 14,
                committed_size: 14,
                vmos: None,
            },
            Bucket {
                name: PAGER_NEWEST.to_string(),
                populated_size: 15,
                committed_size: 15,
                vmos: None,
            },
            Bucket {
                name: PAGER_OLDEST.to_string(),
                populated_size: 16,
                committed_size: 16,
                vmos: None,
            },
            Bucket {
                name: DISCARDABLE_LOCKED.to_string(),
                populated_size: 18,
                committed_size: 18,
                vmos: None,
            },
            Bucket {
                name: DISCARDABLE_UNLOCKED.to_string(),
                populated_size: 19,
                committed_size: 19,
                vmos: None,
            },
            Bucket {
                name: ZRAM_COMPRESSED_BYTES.to_string(),
                populated_size: 21,
                committed_size: 21,
                vmos: None,
            },
            Bucket {
                name: POPULATED_ANONYMOUS_BYTES.to_string(),
                populated_size: 6,
                committed_size: 6,
                vmos: None,
            },
        ];

        assert_eq!(digest.buckets, expected_buckets);
        Ok(())
    }

    #[test]
    fn test_digest_with_matching_process() -> Result<(), anyhow::Error> {
        let (kernel_stats, kernel_stats_compression) = get_kernel_stats();
        let digest = {
            let mut digest = Digest::compute(
                &get_attribution_data(),
                &kernel_stats,
                &kernel_stats_compression,
                &vec![BucketDefinition {
                    name: "matched".to_string(),
                    process: Some(Regex::new("matched")?),
                    vmo: None,
                    principal: None,
                    event_code: Default::default(),
                }],
                true,
            )
            .unwrap();
            sort_buckets_for_assert(&mut digest);
            digest
        };
        let expected_buckets = vec![
            // Both VMOs are matched => 512 + 512 = 1024
            Bucket {
                name: "matched".to_string(),
                populated_size: 4096,
                committed_size: 1024,
                vmos: Some(vec![
                    NamedVmo {
                        name: ZXName::from_string_lossy("matched"),
                        populated_size: 2048,
                        committed_size: 512,
                        principals: vec!["principal".to_owned()],
                    },
                    NamedVmo {
                        name: ZXName::from_string_lossy("resource"),
                        populated_size: 2048,
                        committed_size: 512,
                        principals: vec![],
                    },
                ]),
            },
            // No unmatched VMO
            Bucket {
                name: UNDIGESTED.to_string(),
                populated_size: 0,
                committed_size: 0,
                vmos: Some(vec![]),
            },
            // Two matched VMO => 10000 - 512 - 512 = 8976
            Bucket {
                name: ORPHANED.to_string(),
                populated_size: 8976,
                committed_size: 8976,
                vmos: None,
            },
            // wired + heap + mmu + ipc + other + slab + cache => 3 + 4 + 7 + 8 + 9 + 12 + 11 = 54
            Bucket { name: KERNEL.to_string(), populated_size: 54, committed_size: 54, vmos: None },
            Bucket { name: FREE.to_string(), populated_size: 2, committed_size: 2, vmos: None },
            Bucket {
                name: PAGER_TOTAL.to_string(),
                populated_size: 14,
                committed_size: 14,
                vmos: None,
            },
            Bucket {
                name: PAGER_NEWEST.to_string(),
                populated_size: 15,
                committed_size: 15,
                vmos: None,
            },
            Bucket {
                name: PAGER_OLDEST.to_string(),
                populated_size: 16,
                committed_size: 16,
                vmos: None,
            },
            Bucket {
                name: DISCARDABLE_LOCKED.to_string(),
                populated_size: 18,
                committed_size: 18,
                vmos: None,
            },
            Bucket {
                name: DISCARDABLE_UNLOCKED.to_string(),
                populated_size: 19,
                committed_size: 19,
                vmos: None,
            },
            Bucket {
                name: ZRAM_COMPRESSED_BYTES.to_string(),
                populated_size: 21,
                committed_size: 21,
                vmos: None,
            },
            Bucket {
                name: POPULATED_ANONYMOUS_BYTES.to_string(),
                populated_size: 6,
                committed_size: 6,
                vmos: None,
            },
        ];

        assert_eq!(digest.buckets, expected_buckets);
        Ok(())
    }

    #[test]
    fn test_digest_with_matching_principal() -> Result<(), anyhow::Error> {
        let (kernel_stats, kernel_stats_compression) = get_kernel_stats();
        let digest = {
            let mut digest = Digest::compute(
                &get_attribution_data(),
                &kernel_stats,
                &kernel_stats_compression,
                &vec![BucketDefinition {
                    name: "matched".to_string(),
                    process: None,
                    vmo: None,
                    principal: Some(Regex::new("principal")?),
                    event_code: Default::default(),
                }],
                true,
            )
            .unwrap();
            sort_buckets_for_assert(&mut digest);
            digest
        };
        let expected_buckets = vec![
            // One VMO is matched, the other is not
            Bucket {
                name: "matched".to_string(),
                populated_size: 2048,
                committed_size: 512,
                vmos: Some(vec![NamedVmo {
                    name: ZXName::from_string_lossy("matched"),
                    populated_size: 2048,
                    committed_size: 512,
                    principals: vec!["principal".to_owned()],
                }]),
            },
            // One unmatched VMO
            Bucket {
                name: UNDIGESTED.to_string(),
                populated_size: 2048,
                committed_size: 512,
                vmos: Some(vec![NamedVmo {
                    name: ZXName::from_string_lossy("resource"),
                    populated_size: 2048,
                    committed_size: 512,
                    principals: vec![],
                }]),
            },
            // One matched VMO, one unmatched VMO //=> 10000 - 512 - 512 = 8976
            Bucket {
                name: ORPHANED.to_string(),
                populated_size: 8976,
                committed_size: 8976,
                vmos: None,
            },
            // wired + heap + mmu + ipc + other + slab + cache => 3 + 4 + 7 + 8 + 9 + 12 + 11 = 54
            Bucket { name: KERNEL.to_string(), populated_size: 54, committed_size: 54, vmos: None },
            Bucket { name: FREE.to_string(), populated_size: 2, committed_size: 2, vmos: None },
            Bucket {
                name: PAGER_TOTAL.to_string(),
                populated_size: 14,
                committed_size: 14,
                vmos: None,
            },
            Bucket {
                name: PAGER_NEWEST.to_string(),
                populated_size: 15,
                committed_size: 15,
                vmos: None,
            },
            Bucket {
                name: PAGER_OLDEST.to_string(),
                populated_size: 16,
                committed_size: 16,
                vmos: None,
            },
            Bucket {
                name: DISCARDABLE_LOCKED.to_string(),
                populated_size: 18,
                committed_size: 18,
                vmos: None,
            },
            Bucket {
                name: DISCARDABLE_UNLOCKED.to_string(),
                populated_size: 19,
                committed_size: 19,
                vmos: None,
            },
            Bucket {
                name: ZRAM_COMPRESSED_BYTES.to_string(),
                populated_size: 21,
                committed_size: 21,
                vmos: None,
            },
            Bucket {
                name: POPULATED_ANONYMOUS_BYTES.to_string(),
                populated_size: 6,
                committed_size: 6,
                vmos: None,
            },
        ];

        assert_eq!(digest.buckets, expected_buckets);
        Ok(())
    }

    #[test]
    fn test_digest_with_matching_principal_process_and_vmo() -> Result<(), anyhow::Error> {
        let (kernel_stats, kernel_stats_compression) = get_kernel_stats();
        let digest = {
            let mut digest = Digest::compute(
                &get_attribution_data(),
                &kernel_stats,
                &kernel_stats_compression,
                &vec![BucketDefinition {
                    name: "matched".to_string(),
                    process: Some(Regex::new("matched")?),
                    vmo: Some(Regex::new("matched")?),
                    principal: Some(Regex::new("principal")?),
                    event_code: Default::default(),
                }],
                true,
            )
            .unwrap();
            sort_buckets_for_assert(&mut digest);
            digest
        };
        let expected_buckets = vec![
            // One VMO is matched, the other is not
            Bucket {
                name: "matched".to_string(),
                populated_size: 2048,
                committed_size: 512,
                vmos: Some(vec![NamedVmo {
                    name: ZXName::from_string_lossy("matched"),
                    populated_size: 2048,
                    committed_size: 512,
                    principals: vec!["principal".to_owned()],
                }]),
            },
            // One unmatched VMO
            Bucket {
                name: UNDIGESTED.to_string(),
                populated_size: 2048,
                committed_size: 512,
                vmos: Some(vec![NamedVmo {
                    name: ZXName::from_string_lossy("resource"),
                    populated_size: 2048,
                    committed_size: 512,
                    principals: vec![],
                }]),
            },
            // One matched VMO, one unmatched VMO => 10000 - 512 - 512 = 8976
            Bucket {
                name: ORPHANED.to_string(),
                populated_size: 8976,
                committed_size: 8976,
                vmos: None,
            },
            // wired + heap + mmu + ipc + other + slab + cache => 3 + 4 + 7 + 8 + 9 + 12 + 11 = 54
            Bucket { name: KERNEL.to_string(), populated_size: 54, committed_size: 54, vmos: None },
            Bucket { name: FREE.to_string(), populated_size: 2, committed_size: 2, vmos: None },
            Bucket {
                name: PAGER_TOTAL.to_string(),
                populated_size: 14,
                committed_size: 14,
                vmos: None,
            },
            Bucket {
                name: PAGER_NEWEST.to_string(),
                populated_size: 15,
                committed_size: 15,
                vmos: None,
            },
            Bucket {
                name: PAGER_OLDEST.to_string(),
                populated_size: 16,
                committed_size: 16,
                vmos: None,
            },
            Bucket {
                name: DISCARDABLE_LOCKED.to_string(),
                populated_size: 18,
                committed_size: 18,
                vmos: None,
            },
            Bucket {
                name: DISCARDABLE_UNLOCKED.to_string(),
                populated_size: 19,
                committed_size: 19,
                vmos: None,
            },
            Bucket {
                name: ZRAM_COMPRESSED_BYTES.to_string(),
                populated_size: 21,
                committed_size: 21,
                vmos: None,
            },
            Bucket {
                name: POPULATED_ANONYMOUS_BYTES.to_string(),
                populated_size: 6,
                committed_size: 6,
                vmos: None,
            },
        ];

        assert_eq!(digest.buckets, expected_buckets);
        Ok(())
    }
}
