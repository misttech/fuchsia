// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use diagnostics_hierarchy::{Property, SelectResult};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error<'a> {
    #[error("Inspect histogram's bucket count changed between samples, rejected by Cobalt")]
    NumberOfHistogramBucketsChanged,

    #[error("The count in at least one histogram bucket decreased between samples")]
    HistogramBucketCountDecreased,

    #[error("Inspect histogram contained an index too large for a u32, required by Cobalt")]
    IndexTooLargeForU32,

    #[error("Inspect histogram contained an invalid index")]
    HistogramIndexOutOfBounds,

    #[error("Inspect histogram has negative count, unsupported by Cobalt")]
    HistogramHasNegativeCount,

    #[error("Sampler expected an Inspect histogram type, but got something else")]
    UnexpectedPropertyTypeForHistogram,

    #[error("Double histograms are unsupported. See https://fxbug.dev/380981679")]
    DoubleHistogramsUnsupported,

    #[error("Property type changed illegally in-between samples")]
    PropertyTypeChangedBetweenSamples,

    #[error("Failed to log metrics due to fidl error")]
    FailedToLogMetrics(#[from] fidl::Error),

    #[error("Metric logger server returned an error: {0:?}")]
    MetricLoggerServer(fidl_fuchsia_metrics::Error),

    #[error("Invalid SelectResult, skipping. SelectResult: {0:?}")]
    InvalidSelectResult(SelectResult<'a, String>),

    #[error("Invalid property type; expected Int or Uint, found {0:?}")]
    InvalidPropertyExpectedIntOrUint(Property),

    #[error("Invalid property type; expected String, found {0:?}")]
    InvalidPropertyExpectedString(Property),
}

impl From<fidl_fuchsia_metrics::Error> for Error<'_> {
    fn from(err: fidl_fuchsia_metrics::Error) -> Self {
        Self::MetricLoggerServer(err)
    }
}
