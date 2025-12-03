// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, anyhow};
use block_server::async_interface::{Interface, SessionManager};
use block_server::{BlockInfo, BlockServer, DeviceInfo, ReadOptions, WriteOptions};
use fidl::endpoints::{ClientEnd, FromClient, RequestStream, ServerEnd, create_endpoints};
use fs_management::filesystem::BlockConnector;
use fuchsia_sync::Mutex;
use fxfs_crypto::{FscryptSoftwareInoLblk32FileCipher, UnwrappedKey};
use rand::Rng as _;
use std::borrow::Cow;
use std::collections::HashMap;
use std::num::NonZero;
use std::sync::Arc;
use {fidl_fuchsia_hardware_block as fblock, fidl_fuchsia_hardware_block_volume as fvolume};

/// The Observer can silently discard writes, or fail them explicitly (zx::Status::IO is returned).
pub enum WriteAction {
    Write,
    Discard,
    Fail,
}

pub trait Observer: Send + Sync {
    fn read(
        &self,
        _device_block_offset: u64,
        _block_count: u32,
        _vmo: &Arc<zx::Vmo>,
        _vmo_offset: u64,
    ) {
    }

    fn write(
        &self,
        _device_block_offset: u64,
        _block_count: u32,
        _vmo: &Arc<zx::Vmo>,
        _vmo_offset: u64,
        _opts: WriteOptions,
    ) -> WriteAction {
        WriteAction::Write
    }

    // If [`VmoBackedServerOptions::write_tracking`] is enabled, `writes` is set to the batch since
    // last flush or barrier and can be freely modified.
    fn barrier(&self, _writes: Option<&mut Writes>) {}

    // If [`VmoBackedServerOptions::write_tracking`] is enabled, `writes` is set to the batch since
    // last flush or barrier and can be freely modified.
    fn flush(&self, _writes: Option<&mut Writes>) {}

    // If [`VmoBackedServerOptions::write_tracking`] is enabled, `writes` is set to the batch since
    // last flush or barrier and can be freely modified.
    fn close(&self, _writes: Option<&mut Writes>) {}

    fn trim(&self, _device_block_offset: u64, _block_count: u32) {}
}

pub struct FscryptInfo {
    // Maps keyslots to lblk32 software ciphers used to encrypt/decrypt file contents.
    fscrypt_keys: HashMap<u8, FscryptSoftwareInoLblk32FileCipher>,
    next_key_slot: u8,
}

/// A local server backed by a VMO.
pub struct VmoBackedServer {
    server: BlockServer<SessionManager<Data>>,
    fscrypt_info: Arc<Mutex<FscryptInfo>>,
}

/// The initial contents of the VMO.  This also determines the size of the block device.
pub enum InitialContents<'a> {
    /// An empty VMO will be created with capacity for this many *blocks*.
    FromCapacity(u64),
    /// A VMO is created with capacity for this many *blocks* and the buffer's contents copied into
    /// it.
    FromCapacityAndBuffer(u64, &'a [u8]),
    /// A VMO is created which is exactly large enough for the initial contents (rounded up to block
    /// size), and the buffer's contents copied into it.
    FromBuffer(&'a [u8]),
    /// The provided VMO is used.  If its size is not block-aligned, the data will be truncated.
    FromVmo(zx::Vmo),
}

pub struct VmoBackedServerOptions<'a> {
    /// NB: `block_count` is ignored as that comes from `initial_contents`.
    pub info: DeviceInfo,
    pub block_size: u32,
    pub initial_contents: InitialContents<'a>,
    pub observer: Option<Box<dyn Observer>>,
    /// Enables write tracking so [`Observer::flush`] and [`Observer::barrier`] will be provided
    /// with [`Writes`].
    /// Note that this is expensive and should mainly be used for tests.
    pub write_tracking: bool,
}

impl Default for VmoBackedServerOptions<'_> {
    fn default() -> Self {
        VmoBackedServerOptions {
            info: DeviceInfo::Block(BlockInfo {
                device_flags: fblock::Flag::empty(),
                block_count: 0,
                max_transfer_blocks: None,
            }),
            block_size: 512,
            initial_contents: InitialContents::FromCapacity(0),
            observer: None,
            write_tracking: false,
        }
    }
}

impl VmoBackedServerOptions<'_> {
    pub fn build(self) -> Result<VmoBackedServer, Error> {
        let (data, block_count) = match self.initial_contents {
            InitialContents::FromCapacity(block_count) => {
                (zx::Vmo::create(block_count * self.block_size as u64)?, block_count)
            }
            InitialContents::FromCapacityAndBuffer(block_count, buf) => {
                let needed =
                    buf.len()
                        .checked_next_multiple_of(self.block_size as usize)
                        .ok_or_else(|| anyhow!("Invalid buffer size"))? as u64
                        / self.block_size as u64;
                if needed > block_count {
                    return Err(anyhow!("Not enough capacity: {needed} vs {block_count}"));
                }
                let vmo = zx::Vmo::create(block_count * self.block_size as u64)?;
                vmo.write(buf, 0)?;
                (vmo, block_count)
            }
            InitialContents::FromBuffer(buf) => {
                let block_count =
                    buf.len()
                        .checked_next_multiple_of(self.block_size as usize)
                        .ok_or_else(|| anyhow!("Invalid buffer size"))? as u64
                        / self.block_size as u64;
                let vmo = zx::Vmo::create(block_count * self.block_size as u64)?;
                vmo.write(buf, 0)?;
                (vmo, block_count)
            }
            InitialContents::FromVmo(vmo) => {
                let size = vmo.get_size()?;
                let block_count = size / self.block_size as u64;
                (vmo, block_count)
            }
        };

        let info = match self.info {
            DeviceInfo::Block(mut info) => {
                info.block_count = block_count;
                DeviceInfo::Block(info)
            }
            DeviceInfo::Partition(mut info) => {
                info.block_range = Some(0..block_count);
                DeviceInfo::Partition(info)
            }
        };
        let fscrypt_info =
            Arc::new(Mutex::new(FscryptInfo { fscrypt_keys: HashMap::new(), next_key_slot: 0 }));
        Ok(VmoBackedServer {
            server: BlockServer::new(
                self.block_size,
                Arc::new(Data {
                    info,
                    block_size: self.block_size,
                    data,
                    observer: self.observer,
                    write_tracking: if self.write_tracking {
                        Some(Mutex::new(Writes::new(self.block_size as u64)))
                    } else {
                        None
                    },
                    fscrypt_info: fscrypt_info.clone(),
                }),
            ),
            fscrypt_info,
        })
    }
}

impl VmoBackedServer {
    /// Handles `requests`.  The future will resolve when the stream terminates.
    pub async fn serve(&self, requests: fvolume::VolumeRequestStream) -> Result<(), Error> {
        let res = self.server.handle_requests(requests).await;
        self.server.session_manager().interface().client_closed();
        res
    }

    /// Implements software-fallback for fuchsia_hardware_inlineencryption.ProgramKey. There is no
    /// limit on keyslots with the software fallback. As such, there is no mapping between keyslots
    /// and FIDL connections or key eviction.
    pub fn program_key(&self, xts_key: &[u8; 64]) -> u8 {
        let unwrapped_key = UnwrappedKey::new(xts_key.to_vec());
        let cipher = FscryptSoftwareInoLblk32FileCipher::new(&unwrapped_key);
        let mut fscrypt_info = self.fscrypt_info.lock();
        let slot = fscrypt_info.next_key_slot;
        fscrypt_info.fscrypt_keys.insert(slot, cipher);
        fscrypt_info.next_key_slot += 1;
        slot
    }
}

/// Implements `BlockConnector` to vend connections to a VmoBackedServer.
pub struct VmoBackedServerConnector {
    scope: fuchsia_async::Scope,
    server: Arc<VmoBackedServer>,
}

impl VmoBackedServerConnector {
    pub fn new(scope: fuchsia_async::Scope, server: Arc<VmoBackedServer>) -> Self {
        Self { scope, server }
    }
}

impl BlockConnector for VmoBackedServerConnector {
    fn connect_channel_to_volume(
        &self,
        server_end: ServerEnd<fvolume::VolumeMarker>,
    ) -> Result<(), Error> {
        let server = self.server.clone();
        let _ = self.scope.spawn(async move {
            let _ = server.serve(server_end.into_stream()).await;
        });
        Ok(())
    }
}

/// Keeps track of a sequence of writes since the last flush or barrier, and allows them to be
/// arbitrarily modified or re-ordered.
pub struct Writes {
    block_size: u64,
    block_offsets: Vec<u64>,
    buffer: Vec<u8>,
}

impl Writes {
    fn new(block_size: u64) -> Self {
        Self { block_size, block_offsets: vec![], buffer: vec![] }
    }

    fn insert(&mut self, block_offset: u64, contents: &[u8]) {
        let block_count = contents.len() as u64 / self.block_size;
        let mut buf_offset = 0;
        for offset in block_offset..block_offset + block_count {
            self.block_offsets.push(offset);
            self.buffer
                .extend_from_slice(&contents[buf_offset..buf_offset + self.block_size as usize]);
            buf_offset += self.block_size as usize;
        }
    }

    // Reads the last written value, falling back to `data` if there are no local updates.
    fn read(
        &self,
        data: &zx::Vmo,
        block_offset: u64,
        contents: &mut [u8],
    ) -> Result<(), zx::Status> {
        let block_count = contents.len() as u64 / self.block_size;
        let max_offset = block_offset + block_count;
        data.read(contents, block_offset * self.block_size)?;
        // Apply any buffered writes that would overwrite the actual contents.  If the same offset
        // shows up multiple times, we want to use the most recent write, so it's important to
        // iterate in order.
        for (idx, offset) in self.block_offsets.iter().enumerate() {
            if *offset >= block_offset && *offset < max_offset {
                let in_offset = idx * self.block_size as usize;
                let out_offset = ((*offset - block_offset) * self.block_size) as usize;
                contents[out_offset..out_offset + self.block_size as usize]
                    .copy_from_slice(&self.buffer[in_offset..in_offset + self.block_size as usize]);
            }
        }
        Ok(())
    }

    // Persists all writes to `data` and empties the cache.
    fn apply(&mut self, data: &zx::Vmo) -> Result<(), zx::Status> {
        let mut buf_offset = 0;
        for offset in self.block_offsets.drain(..) {
            data.write(
                &self.buffer[buf_offset..buf_offset + self.block_size as usize],
                offset * self.block_size,
            )?;
            buf_offset += self.block_size as usize;
        }
        self.buffer.clear();
        Ok(())
    }

    /// Returns an iterator over the batch of writes (in temporal sequence).
    pub fn iter(&self) -> impl Iterator<Item = (&u64, &[u8])> {
        self.block_offsets.iter().zip(self.buffer.windows(self.block_size as usize))
    }

    fn swap_writes(&mut self, i: usize, j: usize) {
        self.block_offsets.swap(i, j);
        let bs = self.block_size as usize;
        let mut buf = vec![0u8; bs];
        buf.copy_from_slice(&self.buffer[i * bs..(i + 1) * bs]);
        self.buffer.copy_within(j * bs..(j + 1) * bs, i * bs);
        self.buffer[j * bs..(j + 1) * bs].copy_from_slice(&buf[..]);
    }

    /// Reorders all writes.
    pub fn shuffle(&mut self) {
        // Implements the Fisher–Yates shuffle.
        let mut rng = rand::rng();
        for i in 0..self.block_offsets.len() {
            let j = rng.random_range(0..=i);
            if i != j {
                self.swap_writes(i, j);
            }
        }
    }

    /// Discards a random number of writes from the tail, simulating a power-cut.
    pub fn discard_some(&mut self) {
        let mut rng = rand::rng();
        let idx = rng.random_range(0..=self.block_offsets.len());
        for i in idx..self.block_offsets.len() {
            self.buffer[i * self.block_size as usize..(i + 1) * self.block_size as usize]
                .fill(0xab);
        }
    }
}

/// Extension trait for test-only functionality.  `unwrap` is used liberally in these functions, to
/// simplify their usage in tests.
pub trait VmoBackedServerTestingExt {
    fn new(block_count: u64, block_size: u32, initial_content: &[u8]) -> Self;
    fn from_vmo(block_size: u32, vmo: zx::Vmo) -> Self;
    fn connect_server(self: &Arc<Self>, server: ServerEnd<fvolume::VolumeMarker>);
    fn connect<R: BlockClient>(self: &Arc<Self>) -> R;
}

pub trait BlockClient: FromClient {}

impl BlockClient for fblock::BlockProxy {}
impl BlockClient for fvolume::VolumeProxy {}
impl BlockClient for fblock::BlockSynchronousProxy {}
impl BlockClient for fvolume::VolumeSynchronousProxy {}
impl BlockClient for ClientEnd<fblock::BlockMarker> {}
impl BlockClient for ClientEnd<fvolume::VolumeMarker> {}

impl VmoBackedServerTestingExt for VmoBackedServer {
    fn new(block_count: u64, block_size: u32, initial_content: &[u8]) -> Self {
        VmoBackedServerOptions {
            block_size,
            initial_contents: InitialContents::FromCapacityAndBuffer(block_count, initial_content),
            ..Default::default()
        }
        .build()
        .unwrap()
    }
    fn from_vmo(block_size: u32, vmo: zx::Vmo) -> Self {
        VmoBackedServerOptions {
            block_size,
            initial_contents: InitialContents::FromVmo(vmo),
            ..Default::default()
        }
        .build()
        .unwrap()
    }

    fn connect<R: BlockClient>(self: &Arc<Self>) -> R {
        let (client, server) = create_endpoints::<R::Protocol>();
        let this = self.clone();
        fuchsia_async::Task::spawn(async move {
            let _ = this.serve(server.into_stream().cast_stream()).await;
        })
        .detach();
        R::from_client(client)
    }

    fn connect_server(self: &Arc<Self>, server: ServerEnd<fvolume::VolumeMarker>) {
        let this = self.clone();
        fuchsia_async::Task::spawn(async move {
            let _ = this.serve(server.into_stream()).await;
        })
        .detach();
    }
}

struct Data {
    info: DeviceInfo,
    block_size: u32,
    data: zx::Vmo,
    observer: Option<Box<dyn Observer>>,
    write_tracking: Option<Mutex<Writes>>,
    fscrypt_info: Arc<Mutex<FscryptInfo>>,
}

impl Data {
    fn client_closed(&self) {
        if let Some(observer) = self.observer.as_ref() {
            let mut write_tracking = self.write_tracking.as_ref().map(|w| w.lock());
            match write_tracking.as_mut() {
                Some(w) => observer.close(Some(&mut *w)),
                None => observer.close(None),
            }
        }
    }
}

impl Interface for Data {
    fn get_info(&self) -> Cow<'_, DeviceInfo> {
        Cow::Borrowed(&self.info)
    }

    async fn read(
        &self,
        device_block_offset: u64,
        block_count: u32,
        vmo: &Arc<zx::Vmo>,
        vmo_offset: u64,
        opts: ReadOptions,
        _trace_flow_id: Option<NonZero<u64>>,
    ) -> Result<(), zx::Status> {
        if let Some(observer) = self.observer.as_ref() {
            observer.read(device_block_offset, block_count, vmo, vmo_offset);
        }
        if let Some(max) = self.info.max_transfer_blocks().as_ref() {
            // Requests should be split up by the core library
            assert!(block_count <= max.get());
        }
        if device_block_offset + block_count as u64 > self.info.block_count().unwrap() {
            Err(zx::Status::OUT_OF_RANGE)
        } else {
            let mut data = if let Some(tracking) = self.write_tracking.as_ref() {
                let mut data = vec![0u8; block_count as usize * self.block_size as usize];
                tracking.lock().read(&self.data, device_block_offset, &mut data[..])?;
                data
            } else {
                self.data.read_to_vec(
                    device_block_offset * self.block_size as u64,
                    block_count as u64 * self.block_size as u64,
                )?
            };

            if opts.inline_crypto_options.slot != 0xff {
                let fscrypt_info = self.fscrypt_info.lock();
                if let Some(cipher) =
                    fscrypt_info.fscrypt_keys.get(&opts.inline_crypto_options.slot)
                {
                    cipher
                        .decrypt(&mut data, opts.inline_crypto_options.dun as u128)
                        .map_err(|_| zx::Status::IO)?;
                }
            }
            vmo.write(&data[..], vmo_offset)
        }
    }

    async fn write(
        &self,
        device_block_offset: u64,
        block_count: u32,
        vmo: &Arc<zx::Vmo>,
        vmo_offset: u64,
        opts: WriteOptions,
        _trace_flow_id: Option<NonZero<u64>>,
    ) -> Result<(), zx::Status> {
        if let Some(observer) = self.observer.as_ref() {
            match observer.write(device_block_offset, block_count, vmo, vmo_offset, opts) {
                WriteAction::Write => {}
                WriteAction::Discard => return Ok(()),
                WriteAction::Fail => return Err(zx::Status::IO),
            }
        }
        if let Some(max) = self.info.max_transfer_blocks().as_ref() {
            // Requests should be split up by the core library
            assert!(block_count <= max.get());
        }
        if device_block_offset + block_count as u64 > self.info.block_count().unwrap() {
            Err(zx::Status::OUT_OF_RANGE)
        } else {
            let mut data =
                vmo.read_to_vec(vmo_offset, block_count as u64 * self.block_size as u64)?;
            if let Some(tracking) = self.write_tracking.as_ref() {
                tracking.lock().insert(device_block_offset, &data[..]);
            }
            if opts.inline_crypto_options.slot != 0xff {
                let fscrypt_info = self.fscrypt_info.lock();
                if let Some(cipher) =
                    fscrypt_info.fscrypt_keys.get(&opts.inline_crypto_options.slot)
                {
                    cipher
                        .encrypt(&mut data, opts.inline_crypto_options.dun as u128)
                        .map_err(|_| zx::Status::IO)?;
                }
            }
            self.data.write(&data[..], device_block_offset * self.block_size as u64)
        }
    }

    fn barrier(&self) -> Result<(), zx::Status> {
        let mut write_tracking = self.write_tracking.as_ref().map(|w| w.lock());
        if let Some(observer) = self.observer.as_ref() {
            match write_tracking.as_mut() {
                Some(w) => observer.barrier(Some(&mut *w)),
                None => observer.barrier(None),
            }
        }
        if let Some(w) = write_tracking.as_mut() { w.apply(&self.data) } else { Ok(()) }
    }

    async fn flush(&self, _trace_flow_id: Option<NonZero<u64>>) -> Result<(), zx::Status> {
        let mut write_tracking = self.write_tracking.as_ref().map(|w| w.lock());
        if let Some(observer) = self.observer.as_ref() {
            match write_tracking.as_mut() {
                Some(w) => observer.flush(Some(&mut *w)),
                None => observer.flush(None),
            }
        }
        if let Some(w) = write_tracking.as_mut() { w.apply(&self.data) } else { Ok(()) }
    }

    async fn trim(
        &self,
        device_block_offset: u64,
        block_count: u32,
        _trace_flow_id: Option<NonZero<u64>>,
    ) -> Result<(), zx::Status> {
        if let Some(observer) = self.observer.as_ref() {
            observer.trim(device_block_offset, block_count);
        }
        if device_block_offset + block_count as u64 > self.info.block_count().unwrap() {
            Err(zx::Status::OUT_OF_RANGE)
        } else {
            Ok(())
        }
    }
}
