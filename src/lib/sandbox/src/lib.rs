// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A library for interacting with the `fuchsia.component.sandbox` FIDL APIs.
//!
//! This library provides type-safe wrappers around the raw FIDL types for
//! capabilities and the capability store, simplifying their use.

use fuchsia_component::client;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;
use {fidl_fuchsia_component_sandbox as fsandbox, fidl_fuchsia_io as fio};

/// A trait for types that can be imported into a [`CapabilityStore`].
///
/// Meant to be implemented by newtypes of [`CapabilityHandle`].
pub trait Importable<'a>: Sized + Into<Capability> + TryFrom<Capability, Error = Error> {
    /// The type of handle that will be returned on import.
    type Handle: CapabilityHandle<'a>;
}

/// An error that can occur when interacting with the sandbox APIs.
#[derive(Error, Clone, Debug)]
pub enum Error {
    /// Failed to connect to `fuchsia.component.sandbox.CapabilityStore`.
    #[error("failed to connect to fuchsia.component.sandbox/CapabilityStore: {0}")]
    FailedConnect(zx::Status),
    /// An operation expected a capability of a certain type, but received a different one.
    #[error("wrong Capability type; got \"{got}\", want \"{want}\"")]
    WrongType { got: &'static str, want: &'static str },
    /// A FIDL transport error occurred.
    #[error("FIDL error {0}")]
    Fidl(#[from] fidl::Error),
    /// An error returned from the `fuchsia.component.sandbox.CapabilityStore` protocol.
    #[error("CapabilityStoreError {0:?}")]
    CapabilityStore(fsandbox::CapabilityStoreError),
}

impl From<fsandbox::CapabilityStoreError> for Error {
    fn from(value: fsandbox::CapabilityStoreError) -> Self {
        Self::CapabilityStore(value)
    }
}

/// A helper struct to generate unused capability IDs.
///
/// This is clonable and thread-safe. There should generally be one of these
/// for each [`CapabilityStore`] connection.
#[derive(Clone, Default, Debug)]
pub struct CapabilityIdGenerator {
    next_id: Arc<AtomicU64>,
}

impl CapabilityIdGenerator {
    /// Creates a new ID generator.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the next free id.
    pub fn next(&self) -> u64 {
        self.range(1)
    }

    /// Get a range of free ids of size `size`.
    /// This returns the first id in the range.
    pub fn range(&self, size: u64) -> u64 {
        self.next_id.fetch_add(size, Ordering::Relaxed)
    }
}

/// A wrapper over [`fsandbox::CapabilityStoreProxy`] providing self-contained types.
#[derive(Clone, Debug)]
pub struct CapabilityStore {
    proxy: fsandbox::CapabilityStoreProxy,
    id_gen: CapabilityIdGenerator,
}

impl<'a> CapabilityStore {
    /// Connects to the `fuchsia.component.sandbox/CapabilityStore` protocol.
    pub fn connect() -> Result<Self, Error> {
        let proxy = client::connect_to_protocol::<fsandbox::CapabilityStoreMarker>().map_err(
            |e| match e.downcast_ref() {
                Some(status) => Error::FailedConnect(*status),
                None => Error::FailedConnect(zx::Status::INTERNAL),
            },
        )?;
        Ok(Self { proxy, id_gen: CapabilityIdGenerator::default() })
    }

    /// Imports a capability into the store.
    pub async fn import<T>(&'a self, value: T) -> Result<T::Handle, Error>
    where
        T: Importable<'a>,
    {
        let id = self.id_gen.next();
        self.proxy.import(id, value.into().0).await??;
        Ok(T::Handle::from_store(self, id))
    }

    /// Creates a new, empty dictionary in the capability store.
    pub async fn create_dictionary(&'a self) -> Result<Dictionary<'a>, Error> {
        let id = self.id_gen.next();
        self.proxy.dictionary_create(id).await??;
        Ok(Dictionary { store: self, id })
    }

    /// Creates a new `Connector` capability from a `Receiver` client end.
    pub async fn create_connector(
        &'a self,
        client_end: fidl::endpoints::ClientEnd<fsandbox::ReceiverMarker>,
    ) -> Result<Connector<'a>, Error> {
        let id = self.id_gen.next();
        self.proxy.connector_create(id, client_end).await??;
        Ok(Connector { store: self, id })
    }
}

/// A handle to a [`fsandbox::Capability`] stored in a repository held by the
/// component framework.
///
/// This handle represents a capability of an unconfirmed type. Specify type
/// parameters on `export()` methods to derive the type.
#[allow(async_fn_in_trait)]
pub trait CapabilityHandle<'a>: Sized {
    fn from_store(store: &'a CapabilityStore, id: fsandbox::CapabilityId) -> Self;
    fn store(&self) -> &'a CapabilityStore;
    fn id(&self) -> fsandbox::CapabilityId;

    /// Duplicates the capability referenced by this handle, returning a new
    /// handle to the duplicated capability.
    async fn duplicate(&'a self) -> Result<Self, Error> {
        let dup_id = self.store().id_gen.next();
        self.store().proxy.duplicate(self.id(), dup_id).await??;
        Ok(Self::from_store(self.store(), dup_id))
    }

    /// Drop the referenced Capability held by the component framework runtime.
    async fn drop(self) -> Result<(), Error> {
        Ok(self.store().proxy.drop(self.id()).await??)
    }

    /// Extract the value of the referenced Capability and remove it from the
    /// component framework runtime.
    async fn export<T>(self) -> Result<T, Error>
    where
        T: Importable<'a, Handle = Self>,
    {
        let cap = self.store().proxy.export(self.id()).await??;
        T::try_from(cap.into())
    }
}

/// A type-safe wrapper for a [`fsandbox::Capability`].
///
/// This enum is used to represent different kinds of capabilities that can be
/// stored in the [`CapabilityStore`]. See the [`From`] implementations for how to
/// create a [`Capability`] from a specific type like [`Dictionary`] or [`Data`].
#[derive(Debug)]
pub struct Capability(fsandbox::Capability);

impl From<Capability> for fsandbox::Capability {
    fn from(value: Capability) -> Self {
        value.0
    }
}

impl From<fsandbox::Capability> for Capability {
    fn from(value: fsandbox::Capability) -> Self {
        Self(value)
    }
}

/// Returns the type name of a capability as a static string.
fn capability_type_name(cap: fsandbox::Capability) -> &'static str {
    match cap {
        fsandbox::Capability::Unit(_) => "Unit",
        fsandbox::Capability::Handle(_) => "Handle",
        fsandbox::Capability::Dictionary(_) => "Dictionary",
        fsandbox::Capability::Connector(_) => "Connector",
        fsandbox::Capability::Directory(_) => "Directory",
        fsandbox::Capability::DirEntry(_) => "DirEntry",
        fsandbox::Capability::ConnectorRouter(_) => "ConnectorRouter",
        fsandbox::Capability::DictionaryRouter(_) => "DictionaryRouter",
        fsandbox::Capability::DirEntryRouter(_) => "DirEntryRouter",
        fsandbox::Capability::DataRouter(_) => "DataRouter",
        fsandbox::Capability::DirConnectorRouter(_) => "DirConnectorRouter",
        fsandbox::Capability::Data(data) => match data {
            fsandbox::Data::Bytes(_) => "Data::Bytes",
            fsandbox::Data::String(_) => "Data::String",
            fsandbox::Data::Int64(_) => "Data::Int64",
            fsandbox::Data::Uint64(_) => "Data::Uint64",
            _ => "Data::Unknown",
        },
        _ => "Unknown",
    }
}

/// Implement TryFrom on simple [`fsandbox::Capability`] variants.
macro_rules! impl_try_from_capability {
    ($t:ty, $variant:path, $variant_type_name:expr) => {
        impl TryFrom<Capability> for $t {
            type Error = Error;
            fn try_from(value: Capability) -> Result<Self, Self::Error> {
                match value.0 {
                    $variant(inner) => Ok(inner),
                    capability => Err(Error::WrongType {
                        got: capability_type_name(capability),
                        want: $variant_type_name,
                    }),
                }
            }
        }
    };
}

impl_try_from_capability!(fsandbox::Unit, fsandbox::Capability::Unit, "Unit");
impl_try_from_capability!(fidl::Handle, fsandbox::Capability::Handle, "Handle");
impl_try_from_capability!(fsandbox::DictionaryRef, fsandbox::Capability::Dictionary, "Dictionary");
impl_try_from_capability!(fsandbox::Connector, fsandbox::Capability::Connector, "Connector");
impl_try_from_capability!(
    fidl::endpoints::ClientEnd<fio::DirectoryMarker>,
    fsandbox::Capability::Directory,
    "Directory"
);
impl_try_from_capability!(fsandbox::DirEntry, fsandbox::Capability::DirEntry, "DirEntry");
impl_try_from_capability!(
    fidl::endpoints::ClientEnd<fsandbox::ConnectorRouterMarker>,
    fsandbox::Capability::ConnectorRouter,
    "ConnectorRouter"
);
impl_try_from_capability!(
    fidl::endpoints::ClientEnd<fsandbox::DictionaryRouterMarker>,
    fsandbox::Capability::DictionaryRouter,
    "DictionaryRouter"
);
impl_try_from_capability!(
    fidl::endpoints::ClientEnd<fsandbox::DirEntryRouterMarker>,
    fsandbox::Capability::DirEntryRouter,
    "DirEntryRouter"
);
impl_try_from_capability!(
    fidl::endpoints::ClientEnd<fsandbox::DataRouterMarker>,
    fsandbox::Capability::DataRouter,
    "DataRouter"
);
impl_try_from_capability!(
    fidl::endpoints::ClientEnd<fsandbox::DirConnectorRouterMarker>,
    fsandbox::Capability::DirConnectorRouter,
    "DirConnectorRouter"
);

/// Implement TryFrom on [`fsandbox::Capability::Data`] variants.
macro_rules! impl_data_capability {
    ($t:ty, $variant:path, $variant_type_name:expr) => {
        impl<'a> Importable<'a> for $t {
            type Handle = Data<'a>;
        }

        impl From<$t> for Capability {
            fn from(value: $t) -> Self {
                Self(fsandbox::Capability::Data($variant(value)))
            }
        }

        impl TryFrom<Capability> for $t {
            type Error = Error;
            fn try_from(value: Capability) -> Result<Self, Self::Error> {
                match value.0 {
                    fsandbox::Capability::Data($variant(inner)) => Ok(inner),
                    capability => Err(Error::WrongType {
                        got: capability_type_name(capability),
                        want: $variant_type_name,
                    }),
                }
            }
        }
    };
}

impl_data_capability!(Vec<u8>, fsandbox::Data::Bytes, "Data::Bytes");
impl_data_capability!(String, fsandbox::Data::String, "Data::String");
impl_data_capability!(i64, fsandbox::Data::Int64, "Data:Int64");
impl_data_capability!(u64, fsandbox::Data::Uint64, "Data:Uint64");

impl<'a> Importable<'a> for fsandbox::DictionaryRef {
    type Handle = Dictionary<'a>;
}

impl From<fsandbox::DictionaryRef> for Capability {
    fn from(value: fsandbox::DictionaryRef) -> Self {
        Capability(fsandbox::Capability::Dictionary(value))
    }
}

/// A handle to a dictionary capability in the store.
///
/// Component framework dictionaries are mutable collections of other
/// capabilities, keyed by strings.
#[derive(Debug)]
pub struct Dictionary<'a> {
    store: &'a CapabilityStore,
    id: fsandbox::CapabilityId,
}

impl<'a> CapabilityHandle<'a> for Dictionary<'a> {
    fn from_store(store: &'a CapabilityStore, id: fsandbox::CapabilityId) -> Self {
        Self { store, id }
    }

    fn store(&self) -> &'a CapabilityStore {
        self.store
    }

    fn id(&self) -> fsandbox::CapabilityId {
        self.id
    }
}

impl<'a> Dictionary<'a> {
    /// Inserts a capability into the dictionary with the given key.
    pub async fn insert<V>(&self, key: impl Into<String>, value: V) -> Result<(), Error>
    where
        V: CapabilityHandle<'a>,
    {
        self.store
            .proxy
            .dictionary_insert(
                self.id,
                &fsandbox::DictionaryItem { key: key.into(), value: value.id() },
            )
            .await??;
        Ok(())
    }

    /// Retrieves a capability from the dictionary by its key.
    pub async fn get<T>(&self, key: impl AsRef<str>) -> Result<T, Error>
    where
        T: CapabilityHandle<'a>,
    {
        let id = self.store.id_gen.next();
        self.store.proxy.dictionary_get(self.id, key.as_ref(), id).await??;
        Ok(T::from_store(self.store, id))
    }
}

/// A handle to a connector capability in the store.
///
/// A [`Connector`] can be used to receive a channel from a client component.
#[derive(Debug)]
pub struct Connector<'a> {
    store: &'a CapabilityStore,
    id: fsandbox::CapabilityId,
}

impl<'a> CapabilityHandle<'a> for Connector<'a> {
    fn from_store(store: &'a CapabilityStore, id: fsandbox::CapabilityId) -> Self {
        Self { store, id }
    }

    fn store(&self) -> &'a CapabilityStore {
        self.store
    }

    fn id(&self) -> fsandbox::CapabilityId {
        self.id
    }
}

impl<'a> Connector<'a> {}

/// A handle to a data capability in the store.
///
/// The underlying [`Data`] in the component framework runtime holds fundamental
/// types like strings, integers, and byte vectors.
#[derive(Debug)]
pub struct Data<'a> {
    store: &'a CapabilityStore,
    id: fsandbox::CapabilityId,
}

impl<'a> CapabilityHandle<'a> for Data<'a> {
    fn from_store(store: &'a CapabilityStore, id: fsandbox::CapabilityId) -> Self {
        Self { store, id }
    }

    fn store(&self) -> &'a CapabilityStore {
        self.store
    }

    fn id(&self) -> fsandbox::CapabilityId {
        self.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fidl::endpoints::create_proxy_and_stream;
    use fuchsia_async as fasync;
    use futures::FutureExt;
    use futures::prelude::*;
    use futures::task::Poll;
    use std::pin::Pin;
    use zx::AsHandleRef as _;

    fn setup_store() -> (CapabilityStore, fsandbox::CapabilityStoreRequestStream) {
        let (proxy, stream) = create_proxy_and_stream::<fsandbox::CapabilityStoreMarker>();
        let store = CapabilityStore { proxy, id_gen: CapabilityIdGenerator::default() };
        (store, stream)
    }

    async fn wait_for_request<F, T>(f: &mut Pin<Box<F>>)
    where
        F: Future<Output = T> + ?Sized,
        T: std::fmt::Debug,
    {
        assert_matches!(fasync::TestExecutor::poll_until_stalled(f.as_mut()).await, Poll::Pending);
    }

    #[test]
    fn capability_id_generator() {
        let id_gen = CapabilityIdGenerator::new();
        assert_eq!(id_gen.next(), 0);
        assert_eq!(id_gen.next(), 1);
        assert_eq!(id_gen.range(5), 2);
        assert_eq!(id_gen.next(), 7);
    }

    #[test]
    fn data_capability_conversions() {
        // String
        let cap: Capability = "hello".to_string().into();
        let str_again: String = cap.try_into().unwrap();
        assert_eq!(str_again, "hello");

        // u64
        let cap: Capability = 123u64.into();
        let num_again: u64 = cap.try_into().unwrap();
        assert_eq!(num_again, 123);

        // Wrong type
        let cap: Capability = "hello".to_string().into();
        let res: Result<u64, _> = cap.try_into();
        assert_matches!(res, Err(Error::WrongType { got: "Data::String", want: _ }));
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn capability_store_import() {
        let (store, mut stream) = setup_store();

        let mut import_fut = store.import("hello".to_string()).boxed_local();
        wait_for_request(&mut import_fut).await;

        let (capability, responder) = assert_matches!(
            stream.next().await.unwrap().unwrap(),
            fsandbox::CapabilityStoreRequest::Import { id, capability, responder } if id == 0 => (capability, responder)
        );

        assert_matches!(capability, fsandbox::Capability::Data(fsandbox::Data::String(s)) if s == "hello");
        responder.send(Ok(())).unwrap();
        let handle = import_fut.await.unwrap();
        assert_eq!(handle.id, 0);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn capability_handle_duplicate() {
        let (store, mut stream) = setup_store();

        // Simulate an existing capability with id 100
        let handle = Data { id: 100, store: &store };

        let mut duplicate_fut = handle.duplicate().boxed_local();
        wait_for_request(&mut duplicate_fut).await;

        let responder = assert_matches!(
            stream.next().await.unwrap().unwrap(),
            fsandbox::CapabilityStoreRequest::Duplicate { id, dest_id, responder } if id == 100 && dest_id == 0 => responder
        );

        responder.send(Ok(())).unwrap();
        let new_handle: Data<'_> = duplicate_fut.await.unwrap();
        assert_eq!(new_handle.id, 0);
        assert_eq!(store.id_gen.next(), 1); // next id is 1
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn capability_handle_drop() {
        let (store, mut stream) = setup_store();
        let handle = Data { id: 100, store: &store };

        let mut drop_fut = handle.drop().boxed_local();
        wait_for_request(&mut drop_fut).await;

        let responder = assert_matches!(
            stream.next().await.unwrap().unwrap(),
            fsandbox::CapabilityStoreRequest::Drop { id, responder } if id == 100 => responder
        );

        responder.send(Ok(())).unwrap();
        drop_fut.await.unwrap();
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn capability_handle_export() {
        let (store, mut stream) = setup_store();
        let handle = Data { id: 100, store: &store };

        let mut export_fut = handle.export::<String>().boxed_local();
        wait_for_request(&mut export_fut).await;

        let responder = assert_matches!(
            stream.next().await.unwrap().unwrap(),
            fsandbox::CapabilityStoreRequest::Export { id, responder } if id == 100 => responder
        );

        let cap = fsandbox::Capability::Data(fsandbox::Data::String("exported".to_string()));
        responder.send(Ok(cap)).unwrap();

        let result = export_fut.await.unwrap();
        assert_eq!(result, "exported");
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn dictionary_create() {
        let (store, mut stream) = setup_store();

        let mut create_fut = store.create_dictionary().boxed_local();
        wait_for_request(&mut create_fut).await;

        let responder = assert_matches!(
            stream.next().await.unwrap().unwrap(),
            fsandbox::CapabilityStoreRequest::DictionaryCreate { id, responder } if id == 0 => responder
        );

        responder.send(Ok(())).unwrap();
        let dict = create_fut.await.unwrap();
        assert_eq!(dict.id, 0);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn dictionary_insert() {
        let (store, mut stream) = setup_store();
        let dict = Dictionary { id: 10, store: &store };
        let data = Data { id: 20, store: &store };

        let mut insert_fut = dict.insert("key", data).boxed_local();
        wait_for_request(&mut insert_fut).await;

        let responder = assert_matches!(
            stream.next().await.unwrap().unwrap(),
            fsandbox::CapabilityStoreRequest::DictionaryInsert { id, item, responder } if id == 10 && item.key == "key" && item.value == 20 => responder
        );

        responder.send(Ok(())).unwrap();
        insert_fut.await.unwrap();
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn dictionary_get() {
        let (store, mut stream) = setup_store();
        let dict = Dictionary { id: 10, store: &store };

        let mut get_fut = dict.get::<Data<'_>>("key").boxed_local();
        wait_for_request(&mut get_fut).await;

        let responder = assert_matches!(
            stream.next().await.unwrap().unwrap(),
            fsandbox::CapabilityStoreRequest::DictionaryGet { id, key, dest_id, responder } if id == 10 && key == "key" && dest_id == 0 => responder
        );

        responder.send(Ok(())).unwrap();
        let handle = get_fut.await.unwrap();
        assert_eq!(handle.id, 0);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn dictionary_export() {
        let (store, mut stream) = setup_store();
        let dict = Dictionary { id: 10, store: &store };

        let mut export_fut = dict.export().boxed_local();
        wait_for_request(&mut export_fut).await;

        let responder = assert_matches!(
            stream.next().await.unwrap().unwrap(),
            fsandbox::CapabilityStoreRequest::Export { id, responder } if id == 10 => responder
        );

        let (client, _server) = zx::EventPair::create();
        let cap = fsandbox::Capability::Dictionary(fsandbox::DictionaryRef { token: client });
        responder.send(Ok(cap)).unwrap();

        let result: fsandbox::DictionaryRef = export_fut.await.unwrap();
        assert_eq!(result.token.as_handle_ref().is_invalid(), false);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn connector_create() {
        let (store, mut stream) = setup_store();
        let (client_end, _server_end) =
            fidl::endpoints::create_endpoints::<fsandbox::ReceiverMarker>();

        let mut create_fut = store.create_connector(client_end).boxed_local();
        wait_for_request(&mut create_fut).await;

        let responder = assert_matches!(
            stream.next().await.unwrap().unwrap(),
            fsandbox::CapabilityStoreRequest::ConnectorCreate { id, receiver, responder } if id == 0 && receiver.as_handle_ref().is_invalid() == false => responder
        );

        responder.send(Ok(())).unwrap();
        let connector = create_fut.await.unwrap();
        assert_eq!(connector.id, 0);
    }

    #[fuchsia::test(allow_stalls = false)]
    async fn data_export() {
        let (store, mut stream) = setup_store();
        let data = Data { id: 10, store: &store };

        let mut export_fut = data.export::<String>().boxed_local();
        wait_for_request(&mut export_fut).await;

        let responder = assert_matches!(
            stream.next().await.unwrap().unwrap(),
            fsandbox::CapabilityStoreRequest::Export { id, responder } if id == 10 => responder
        );

        let cap = fsandbox::Capability::Data(fsandbox::Data::String("exported data".to_string()));
        responder.send(Ok(cap)).unwrap();

        let result = export_fut.await.unwrap();
        assert_eq!(result, "exported data");
    }
}
