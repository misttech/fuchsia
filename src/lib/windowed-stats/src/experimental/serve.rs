// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use derivative::Derivative;
use fuchsia_async as fasync;
use fuchsia_inspect::{Inspector, Node as InspectNode};
use fuchsia_sync::Mutex;
use futures::channel::mpsc;
use futures::{Future, FutureExt as _, StreamExt as _, select};
use log::{error, info, warn};
use std::sync::Arc;

use crate::experimental::clock::{Timed, Timestamp};
use crate::experimental::series::interpolation::InterpolationKind;
use crate::experimental::series::statistic::{FoldError, Metadata, SerialStatistic};
use crate::experimental::series::{TimeMatrix, TimeMatrixFold, TimeMatrixTick};

// TODO(https://fxbug.dev/375489301): It is not possible to inject a mock time matrix into this
//                                    function. Refactor the function so that a unit test can
//                                    assert that interpolation occurs after an interval of time.
/// Creates a client and server for interpolating and recording time matrices to the given [Inspect
/// node][`Node`].
///
/// The client end can be used to instrument and send a [`TimeMatrix`] to the server. The server
/// end must be polled to incorporate and interpolate time matrices.
///
/// [`Node`]: fuchsia_inspect::Node
/// [`TimeMatrix`]: crate::experimental::series::TimeMatrix
pub fn serve_time_matrix_inspection(
    node: InspectNode,
) -> (TimeMatrixClient, impl Future<Output = Result<(), anyhow::Error>>) {
    /// The buffer capacity of the MPSC channel through which time matrices are sent from clients
    /// to the server future.
    const TIME_MATRIX_SENDER_BUFFER_SIZE: usize = 250;

    /// The duration between interpolating data in inspected time matrices.
    const INTERPOLATION_PERIOD: zx::MonotonicDuration = zx::MonotonicDuration::from_minutes(5);

    let (sender, mut receiver) = mpsc::channel::<SharedTimeMatrix>(TIME_MATRIX_SENDER_BUFFER_SIZE);

    let client = TimeMatrixClient::new(sender, node.clone_weak());
    let server = async move {
        let _node = node;
        let mut matrices = vec![];

        let mut tick = fasync::Interval::new(INTERPOLATION_PERIOD);
        loop {
            select! {
                // Incorporate time matrices received from the client.
                matrix = receiver.next() => {
                    match matrix {
                        Some(matrix) => {
                            matrices.push(matrix);
                        }
                        None => {
                            info!("time matrix inspection terminated.");
                        }
                    }
                }
                // Periodically fold buffered samples into and interpolate time matrices.
                _ = tick.next() => {
                    // TODO(https://fxbug.dev/375255877): Log more information, such as the name
                    //                                    associated with the matrix.
                    for matrix in matrices.iter() {
                        // Querying the current timestamp for each matrix like this introduces a
                        // bias: the more recently a matrix has been pushed into `matrices`, the
                        // more recent the timestamp used for tick.
                        //
                        // However, if we take the timestamp before acquiring the lock, we risk
                        // running into Monotonicity error if another task folds a sample to
                        // time series after we acquire the timestamp and before we tick.
                        if let Err(error) = matrix.lock().tick(Timestamp::now()) {
                            warn!("failed to tick time matrix: {:?}", error);
                        }
                    }
                }
            }
        }
    };
    (client, server)
}

pub trait InspectSender {
    /// Sends a [`TimeMatrix`] to the client's inspection server.
    ///
    /// See [`inspect_time_matrix_with_metadata`].
    ///
    /// [`inspect_time_matrix_with_metadata`]: crate::experimental::serve::TimeMatrixClient::inspect_time_matrix_with_metadata
    /// [`TimeMatrix`]: crate::experimental::series::TimeMatrix
    fn inspect_time_matrix<F, P>(
        &self,
        name: impl Into<String>,
        matrix: TimeMatrix<F, P>,
    ) -> InspectedTimeMatrix<F::Sample>
    where
        TimeMatrix<F, P>: 'static + TimeMatrixFold<F::Sample> + Send,
        Metadata<F>: 'static + Send + Sync,
        F: SerialStatistic<P>,
        F::Sample: Send,
        P: InterpolationKind;

    /// Sends a [`TimeMatrix`] to the client's inspection server.
    ///
    /// This function lazily records the given [`TimeMatrix`] to Inspect. The server end
    /// periodically interpolates the matrix and records data as needed. The returned
    /// [handle][`InspectedTimeMatrix`] can be used to fold samples into the matrix.
    ///
    /// [`InspectedTimeMatrix`]: crate::experimental::serve::InspectedTimeMatrix
    /// [`TimeMatrix`]: crate::experimental::series::TimeMatrix
    fn inspect_time_matrix_with_metadata<F, P>(
        &self,
        name: impl Into<String>,
        matrix: TimeMatrix<F, P>,
        metadata: impl Into<Metadata<F>>,
    ) -> InspectedTimeMatrix<F::Sample>
    where
        TimeMatrix<F, P>: 'static + TimeMatrixFold<F::Sample> + Send,
        Metadata<F>: 'static + Send + Sync,
        F: SerialStatistic<P>,
        F::Sample: Send,
        P: InterpolationKind;
}

type SharedTimeMatrix = Arc<Mutex<dyn TimeMatrixTick>>;

pub struct TimeMatrixClient {
    // TODO(https://fxbug.dev/432324973): Synchronizing the sender end of a channel like this is an
    //                                    anti-pattern. Consider removing the mutex. See the linked
    //                                    bug for discussion of the ramifications.
    sender: Arc<Mutex<mpsc::Sender<SharedTimeMatrix>>>,
    node: InspectNode,
}

impl TimeMatrixClient {
    fn new(sender: mpsc::Sender<SharedTimeMatrix>, node: InspectNode) -> Self {
        Self { sender: Arc::new(Mutex::new(sender)), node }
    }

    fn inspect_and_record_with<F, P, R>(
        &self,
        name: impl Into<String>,
        matrix: TimeMatrix<F, P>,
        record: R,
    ) -> InspectedTimeMatrix<F::Sample>
    where
        TimeMatrix<F, P>: 'static + TimeMatrixFold<F::Sample> + Send,
        Metadata<F>: 'static + Send + Sync,
        F: SerialStatistic<P>,
        F::Sample: Send,
        P: InterpolationKind,
        R: 'static + Clone + Fn(&InspectNode) + Send + Sync,
    {
        let name = name.into();
        let matrix = Arc::new(Mutex::new(matrix));
        self::record_lazy_time_matrix_with(&self.node, &name, matrix.clone(), record);
        if let Err(error) = self.sender.lock().try_send(matrix.clone()) {
            error!("failed to send time matrix \"{}\" to inspection server: {:?}", name, error);
        }
        InspectedTimeMatrix::new(name, matrix)
    }
}

impl Clone for TimeMatrixClient {
    fn clone(&self) -> Self {
        TimeMatrixClient { sender: self.sender.clone(), node: self.node.clone_weak() }
    }
}

impl InspectSender for TimeMatrixClient {
    fn inspect_time_matrix<F, P>(
        &self,
        name: impl Into<String>,
        matrix: TimeMatrix<F, P>,
    ) -> InspectedTimeMatrix<F::Sample>
    where
        TimeMatrix<F, P>: 'static + TimeMatrixFold<F::Sample> + Send,
        Metadata<F>: 'static + Send + Sync,
        F: SerialStatistic<P>,
        F::Sample: Send,
        P: InterpolationKind,
    {
        self.inspect_and_record_with(name, matrix, |_node| {})
    }

    fn inspect_time_matrix_with_metadata<F, P>(
        &self,
        name: impl Into<String>,
        matrix: TimeMatrix<F, P>,
        metadata: impl Into<Metadata<F>>,
    ) -> InspectedTimeMatrix<F::Sample>
    where
        TimeMatrix<F, P>: 'static + TimeMatrixFold<F::Sample> + Send,
        Metadata<F>: 'static + Send + Sync,
        F: SerialStatistic<P>,
        F::Sample: Send,
        P: InterpolationKind,
    {
        let metadata = Arc::new(metadata.into());
        self.inspect_and_record_with(name, matrix, move |node| {
            use crate::experimental::series::metadata::Metadata;

            node.record_child("metadata", |node| {
                metadata.record(node);
            })
        })
    }
}

#[derive(Derivative)]
#[derivative(Debug, Clone)]
pub struct InspectedTimeMatrix<T> {
    name: String,
    #[derivative(Debug = "ignore")]
    matrix: Arc<Mutex<dyn TimeMatrixFold<T> + Send>>,
}

impl<T> InspectedTimeMatrix<T> {
    pub(crate) fn new(
        name: impl Into<String>,
        matrix: Arc<Mutex<dyn TimeMatrixFold<T> + Send>>,
    ) -> Self {
        Self { name: name.into(), matrix }
    }

    pub fn fold(&self, sample: Timed<T>) -> Result<(), FoldError> {
        self.matrix.lock().fold(sample)
    }

    pub fn fold_or_log_error(&self, sample: Timed<T>) {
        if let Err(error) = self.matrix.lock().fold(sample) {
            warn!("failed to fold sample into time matrix \"{}\": {:?}", self.name, error);
        }
    }
}

/// Records a lazy child node in the given node that records buffers and metadata for the given
/// time matrix.
///
/// The function `f` is passed a node to record arbitrary data after the data semantic and buffers
/// of the time matrix have been recorded using that same node.
fn record_lazy_time_matrix_with<F>(
    node: &InspectNode,
    name: impl Into<String>,
    matrix: Arc<Mutex<dyn TimeMatrixTick + Send>>,
    f: F,
) where
    F: 'static + Clone + Fn(&InspectNode) + Send + Sync,
{
    let name = name.into();
    node.record_lazy_child(name, move || {
        let matrix = matrix.clone();
        let f = f.clone();
        async move {
            let inspector = Inspector::default();
            {
                match matrix.lock().tick_and_get_buffers(Timestamp::now()) {
                    Ok(buffer) => {
                        inspector.root().atomic_update(|node| {
                            node.record_string("type", buffer.data_semantic);
                            node.record_bytes("data", buffer.data);
                            f(node);
                        });
                    }
                    Err(error) => {
                        inspector.root().record_string("type", format!("error: {:?}", error));
                    }
                }
            }
            Ok(inspector)
        }
        .boxed()
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::{AnyBytesProperty, assert_data_tree};
    use fuchsia_async as fasync;
    use futures::task::Poll;
    use std::mem;
    use std::pin::pin;

    use crate::experimental::series::SamplingProfile;
    use crate::experimental::series::interpolation::{ConstantSample, LastSample};
    use crate::experimental::series::metadata::BitSetMap;
    use crate::experimental::series::statistic::{Max, Union};

    #[fuchsia::test]
    fn inspected_time_matrix_folded_sample_appears_in_inspect() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(3_000_000_000));

        let inspector = Inspector::default();
        let (client, _server) =
            serve_time_matrix_inspection(inspector.root().create_child("serve_test_node"));
        let time_matrix = TimeMatrix::<Max<u64>, ConstantSample>::new(
            SamplingProfile::highly_granular(),
            ConstantSample::default(),
        );
        let inspected_matrix = client.inspect_time_matrix("time_series_1", time_matrix);

        inspected_matrix.fold(Timed::now(15)).unwrap();
        exec.set_fake_time(fasync::MonotonicInstant::from_nanos(10_000_000_000));

        assert_data_tree!(@executor exec, inspector, root: contains {
            serve_test_node: {
                time_series_1: {
                    "type": "gauge",
                    "data": vec![
                        1u8, // version number
                        3, 0, 0, 0, // created timestamp
                        10, 0, 0, 0, // last timestamp
                        1, 0, // type: simple8b RLE; subtype: unsigned
                        16, 0, // series 1: length in bytes
                        10, 0, // series 1 granularity: 10s
                        1, 0, // number of selector elements and value blocks
                        0, 0,    // head selector index
                        1,    // number of values in last block
                        0x0f, // RLE selector
                        15, 0, 0, 0, 0, 0, 1, 0, // value 15 appears 1 time
                        7, 0, // series 2: length in bytes
                        60, 0, // series 2 granularity: 60s
                        0, 0, // number of selector elements and value blocks
                        0, 0, // head selector index
                        0, // number of values in last block
                    ]
                }
            }
        });
    }

    #[fuchsia::test]
    async fn serve_time_matrix_inspection_then_inspect_data_tree_contains_buffers() {
        let inspector = Inspector::default();
        let (client, _server) =
            serve_time_matrix_inspection(inspector.root().create_child("serve_test_node"));
        let _matrix = client
            .inspect_time_matrix("connectivity", TimeMatrix::<Union<u64>, LastSample>::default());

        assert_data_tree!(inspector, root: contains {
            serve_test_node: {
                connectivity: {
                    "type": "bitset",
                    "data": AnyBytesProperty,
                }
            }
        });
    }

    #[fuchsia::test]
    async fn serve_time_matrix_inspection_with_metadata_then_inspect_data_tree_contains_metadata() {
        let inspector = Inspector::default();
        let (client, _server) =
            self::serve_time_matrix_inspection(inspector.root().create_child("serve_test_node"));
        let _matrix = client.inspect_time_matrix_with_metadata(
            "engine",
            TimeMatrix::<Union<u64>, LastSample>::default(),
            BitSetMap::from_ordered(["check", "oil", "battery", "coolant"]),
        );

        assert_data_tree!(inspector, root: contains {
            serve_test_node: {
                engine: {
                    "type": "bitset",
                    "data": AnyBytesProperty,
                    metadata: {
                        index: {
                            "0": "check",
                            "1": "oil",
                            "2": "battery",
                            "3": "coolant",
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn drop_time_matrix_client_then_server_continues_execution() {
        let mut executor = fasync::TestExecutor::new_with_fake_time();

        let inspector = Inspector::default();
        let (client, server) =
            serve_time_matrix_inspection(inspector.root().create_child("serve_test_node"));
        let mut server = pin!(server);

        mem::drop(client);

        // The server future should continue execution even if its associated client is dropped.
        let Poll::Pending = executor.run_until_stalled(&mut server) else {
            panic!("time matrix inspection server terminated unexpectedly");
        };
    }
}
