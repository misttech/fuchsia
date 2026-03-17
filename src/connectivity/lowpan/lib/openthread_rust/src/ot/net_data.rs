// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::prelude_internal::*;

/// The maximum length of Thread network data, in bytes.
pub const MAX_NET_DATA_LEN: usize = 255;

/// Iterator type for on-mesh prefixes in network data.
#[allow(missing_debug_implementations)]
pub struct OnMeshPrefixIterator<'a, T: ?Sized> {
    ot_instance: &'a T,
    ot_iter: otNetworkDataIterator,
}

impl<T: ?Sized + NetData> Iterator for OnMeshPrefixIterator<'_, T> {
    type Item = BorderRouterConfig;
    fn next(&mut self) -> Option<Self::Item> {
        self.ot_instance.iter_next_on_mesh_prefix(&mut self.ot_iter)
    }
}

/// Methods from the [OpenThread "NetData" Module][1].
///
/// [1]: https://openthread.io/reference/group/api-thread-general
pub trait NetData {
    /// Functional equivalent of [`otsys::otNetDataGet`](crate::otsys::otNetDataGet).
    fn net_data_get<'a>(&self, stable: bool, data: &'a mut [u8]) -> Result<&'a [u8]>;

    /// Same as [`net_data_get`], but returns the net data as a vector.
    fn net_data_as_vec(&self, stable: bool) -> Result<Vec<u8>> {
        let mut ret = vec![0; MAX_NET_DATA_LEN];

        let len = self.net_data_get(stable, ret.as_mut_slice())?.len();

        ret.truncate(len);

        Ok(ret)
    }

    /// Functional equivalent of [`otsys::otNetDataGetVersion`](crate::otsys::otNetDataGetVersion).
    fn net_data_get_version(&self) -> u8;

    /// Functional equivalent of
    /// [`otsys::otNetDataGetStableVersion`](crate::otsys::otNetDataGetStableVersion).
    fn net_data_get_stable_version(&self) -> u8;

    /// Functional equivalent of [`otsys::otNetDataGetNextOnMeshPrefix`](crate::otsys::otNetDataGetNextOnMeshPrefix).
    fn iter_next_on_mesh_prefix(
        &self,
        ot_iter: &mut otNetworkDataIterator,
    ) -> Option<BorderRouterConfig>;

    /// Returns an iterator for iterating over on-mesh prefixes.
    fn iter_on_mesh_prefixes(&self) -> OnMeshPrefixIterator<'_, Self> {
        OnMeshPrefixIterator { ot_instance: self, ot_iter: OT_NETWORK_DATA_ITERATOR_INIT }
    }
}

impl<T: NetData + Boxable> NetData for ot::Box<T> {
    fn net_data_get<'a>(&self, stable: bool, data: &'a mut [u8]) -> Result<&'a [u8]> {
        self.as_ref().net_data_get(stable, data)
    }

    fn net_data_get_version(&self) -> u8 {
        self.as_ref().net_data_get_version()
    }

    fn net_data_get_stable_version(&self) -> u8 {
        self.as_ref().net_data_get_version()
    }

    fn iter_next_on_mesh_prefix(
        &self,
        ot_iter: &mut otNetworkDataIterator,
    ) -> Option<BorderRouterConfig> {
        self.as_ref().iter_next_on_mesh_prefix(ot_iter)
    }
}

impl NetData for Instance {
    fn net_data_get<'a>(&self, stable: bool, data: &'a mut [u8]) -> Result<&'a [u8]> {
        let mut len: u8 = data.len().min(MAX_NET_DATA_LEN).try_into().unwrap();

        Error::from(unsafe {
            otNetDataGet(self.as_ot_ptr(), stable, data.as_mut_ptr(), (&mut len) as *mut u8)
        })
        .into_result()?;

        Ok(&data[..(len as usize)])
    }

    fn net_data_get_version(&self) -> u8 {
        unsafe { otNetDataGetVersion(self.as_ot_ptr()) }
    }

    fn net_data_get_stable_version(&self) -> u8 {
        unsafe { otNetDataGetStableVersion(self.as_ot_ptr()) }
    }

    fn iter_next_on_mesh_prefix(
        &self,
        ot_iter: &mut otNetworkDataIterator,
    ) -> Option<BorderRouterConfig> {
        unsafe {
            let mut ret = BorderRouterConfig::default();
            match Error::from(otNetDataGetNextOnMeshPrefix(
                self.as_ot_ptr(),
                ot_iter as *mut otNetworkDataIterator,
                ret.as_ot_mut_ptr(),
            )) {
                Error::NotFound => None,
                Error::None => Some(ret),
                err => panic!("Unexpected error from otNetDataGetNextOnMeshPrefix: {err:?}"),
            }
        }
    }
}
