// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use derivative::Derivative;
use fuchsia_sync::Mutex;
use std::any::{self, Any};
use std::collections::HashMap;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::sync::Arc;

use crate::experimental::clock::{Timed, Timestamp};
use crate::experimental::series::statistic::{FoldError, Metadata, Sample, Statistic};
use crate::experimental::series::{Gauge, SerializedBuffer, TimeMatrixFold, TimeMatrixTick};
use crate::experimental::serve::{
    BufferedSampler, InspectSender, InspectedTimeMatrix, ServedTimeMatrix,
};

pub type DynamicSample = Box<dyn Any + Send>;

#[derive(Derivative)]
#[derivative(Debug, PartialEq)]
pub enum TimeMatrixCall<T> {
    Fold(Timed<T>),
    Tick(Timestamp),
}

impl<T> TimeMatrixCall<T> {
    fn map<U, F>(self, f: F) -> TimeMatrixCall<U>
    where
        F: FnOnce(T) -> U,
    {
        match self {
            TimeMatrixCall::Fold(timed) => TimeMatrixCall::Fold(timed.map(f)),
            TimeMatrixCall::Tick(timestamp) => TimeMatrixCall::Tick(timestamp),
        }
    }
}

impl<T, E> TimeMatrixCall<Result<T, E>> {
    fn transpose(self) -> Result<TimeMatrixCall<T>, E> {
        match self {
            TimeMatrixCall::Fold(result) => match result.transpose() {
                Ok(sample) => Ok(TimeMatrixCall::Fold(sample)),
                Err(error) => Err(error),
            },
            TimeMatrixCall::Tick(timestamp) => Ok(TimeMatrixCall::Tick(timestamp)),
        }
    }
}

#[derive(Debug)]
pub struct TimeMatrixCallLog {
    calls: HashMap<String, Vec<TimeMatrixCall<DynamicSample>>>,
}

impl TimeMatrixCallLog {
    pub fn drain<T: Any + Send + Clone>(&mut self, name: &str) -> Vec<TimeMatrixCall<T>> {
        self.calls
            .remove(name)
            .unwrap_or_default()
            .into_iter()
            .map(|call| call.map(|sample| sample.downcast::<T>().map(|sample| *sample)))
            .map(TimeMatrixCall::transpose)
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|_| {
                panic!(
                    "in time matrix \"{}\": failed to downcast dynamic sample of type `{}`",
                    name,
                    any::type_name::<T>()
                )
            })
    }

    pub fn as_hash_map(&self) -> &HashMap<String, Vec<TimeMatrixCall<DynamicSample>>> {
        &self.calls
    }

    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

#[derive(Clone)]
pub struct MockTimeMatrixClient {
    matrices: Arc<Mutex<Vec<Box<dyn ServedTimeMatrix>>>>,
    calls: Arc<Mutex<Vec<(String, TimeMatrixCall<DynamicSample>)>>>,
}

impl MockTimeMatrixClient {
    pub fn new() -> Self {
        Self { matrices: Arc::new(Mutex::new(vec![])), calls: Arc::new(Mutex::new(vec![])) }
    }
}

impl MockTimeMatrixClient {
    pub fn fold_buffered_samples(&self) -> TimeMatrixCallLog {
        for matrix in self.matrices.lock().iter_mut() {
            matrix.fold_buffered_samples().unwrap();
        }
        let mut calls = HashMap::<_, Vec<_>>::new();
        for (name, call) in self.calls.lock().drain(..) {
            calls.entry(name).or_default().push(call);
        }
        TimeMatrixCallLog { calls }
    }
}

impl InspectSender for MockTimeMatrixClient {
    fn inspect_time_matrix<M>(
        &self,
        name: impl Into<String>,
        _matrix: M,
    ) -> InspectedTimeMatrix<Sample<M::Statistic>>
    where
        M: 'static + Send + TimeMatrixFold,
        Sample<M::Statistic>: Send,
    {
        let name = name.into();
        let (sender, matrix) = BufferedSampler::from_time_matrix(MockTimeMatrix::new(
            name.clone(),
            self.calls.clone(),
        ));
        self.matrices.lock().push(Box::new(matrix));
        InspectedTimeMatrix::new(name, sender)
    }

    fn inspect_time_matrix_with_metadata<M>(
        &self,
        name: impl Into<String>,
        matrix: M,
        _metadata: impl Into<Metadata<M::Statistic>>,
    ) -> InspectedTimeMatrix<Sample<M::Statistic>>
    where
        M: 'static + Send + TimeMatrixFold,
        Metadata<M::Statistic>: 'static + Send + Sync,
        Sample<M::Statistic>: Send,
    {
        self.inspect_time_matrix(name, matrix)
    }
}

struct MockStatistic<T>(PhantomData<fn() -> T>);

impl<T> Clone for MockStatistic<T> {
    fn clone(&self) -> Self {
        MockStatistic::default()
    }
}

impl<T> Default for MockStatistic<T> {
    fn default() -> Self {
        MockStatistic(PhantomData)
    }
}

impl<T> Statistic for MockStatistic<T>
where
    T: Clone,
{
    type Semantic = Gauge;
    type Sample = T;
    type Aggregation = ();

    fn fold(&mut self, _sample: Self::Sample) -> Result<(), FoldError> {
        Ok(())
    }

    fn fill(&mut self, _sample: Self::Sample, _n: NonZeroUsize) -> Result<(), FoldError> {
        Ok(())
    }

    fn reset(&mut self) {}

    fn aggregation(&self) -> Option<Self::Aggregation> {
        Some(())
    }
}

struct MockTimeMatrix<T> {
    name: String,
    calls: Arc<Mutex<Vec<(String, TimeMatrixCall<DynamicSample>)>>>,
    phantom: PhantomData<fn() -> T>,
}

impl<T> MockTimeMatrix<T> {
    pub fn new(
        name: impl Into<String>,
        calls: Arc<Mutex<Vec<(String, TimeMatrixCall<DynamicSample>)>>>,
    ) -> Self {
        MockTimeMatrix { name: name.into(), calls, phantom: PhantomData }
    }
}

impl<T> TimeMatrixFold for MockTimeMatrix<T>
where
    T: 'static + Clone + Send,
{
    type Statistic = MockStatistic<T>;

    fn fold(&mut self, sample: Timed<T>) -> Result<(), FoldError> {
        let sample = sample.map(|v| Box::new(v) as DynamicSample);
        self.calls.lock().push((self.name.clone(), TimeMatrixCall::Fold(sample)));
        Ok(())
    }
}

impl<T> TimeMatrixTick for MockTimeMatrix<T> {
    fn tick(&mut self, timestamp: Timestamp) -> Result<(), FoldError> {
        self.calls.lock().push((self.name.clone(), TimeMatrixCall::Tick(timestamp)));
        Ok(())
    }

    fn tick_and_get_buffers(
        &mut self,
        timestamp: Timestamp,
    ) -> Result<SerializedBuffer, FoldError> {
        self.tick(timestamp)?;
        Ok(SerializedBuffer { data_semantic: "mock".to_string(), data: vec![] })
    }
}
