// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_memory_attribution as fattribution;
use futures::TryStreamExt;
use refaults_vmo::PageFaultCounter;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use zx::Status;

pub trait PageFaultProvider: Clone + Send + Sync + 'static {
    /// Returns the current number of page refaults of the system, since its start, i.e. pages
    /// resupplied after eviction.
    fn get_refault_count(&self) -> u64;

    /// Returns the total number of major page faults of the system, since its start, i.e. pages
    /// supplied by user pagers including readahead.
    fn get_fault_count(&self) -> u64;
}

#[derive(Default, Clone)]
pub struct PageFaultProviderImpl {
    inner: Arc<Mutex<Inner>>,
}

impl PageFaultProviderImpl {
    pub async fn listen_to_page_faults(
        &self,
        mut stream: fattribution::PagerStatsSinkRequestStream,
    ) -> Result<(), anyhow::Error> {
        loop {
            match stream.try_next().await {
                Ok(Some(request)) => match request {
                    fattribution::PagerStatsSinkRequest::SendPagerStats {
                        pager_stats_vmo, ..
                    } => {
                        self.inner.lock().unwrap().set_new_counter(pager_stats_vmo)?;
                    }
                    fattribution::PagerStatsSinkRequest::_UnknownMethod { .. } => unimplemented!(),
                },
                Ok(None) => {
                    return Ok(());
                }
                Err(e) => {
                    return Err(e.into());
                }
            }
        }
    }
}

impl PageFaultProvider for PageFaultProviderImpl {
    fn get_refault_count(&self) -> u64 {
        self.inner.lock().unwrap().get_refault_count()
    }

    fn get_fault_count(&self) -> u64 {
        self.inner.lock().unwrap().get_fault_count()
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct CounterIndex(usize);

#[derive(Default)]
struct Inner {
    counters: HashMap<CounterIndex, PageFaultCounter>,
}

impl Inner {
    fn set_new_counter(&mut self, pager_stats_vmo: zx::Vmo) -> Result<(), Status> {
        let index = CounterIndex(self.counters.len());
        self.counters.insert(index, PageFaultCounter::from_vmo_readonly(pager_stats_vmo)?);
        Ok(())
    }

    fn get_refault_count(&self) -> u64 {
        self.counters.values().map(|c| c.read_refaults(Ordering::Relaxed)).sum::<u64>()
    }

    fn get_fault_count(&self) -> u64 {
        self.counters.values().map(|c| c.read_faults(Ordering::Relaxed)).sum::<u64>()
    }
}
