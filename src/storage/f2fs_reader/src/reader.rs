// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::block_cache::BlockCache;
use crate::checkpoint::*;
use crate::crypto;
use crate::dir::{DentryBlock, DirEntry};
use crate::inode::{self, Inode};
use crate::nat::{Nat, NatJournal, RawNatEntry, SummaryBlock};
use crate::superblock::{
    BLOCK_SIZE, BLOCKS_PER_SEGMENT, F2FS_MAGIC, SEGMENT_SIZE, SUPERBLOCK_OFFSET, SuperBlock,
    f2fs_crc32,
};
use anyhow::{Error, anyhow, bail, ensure};
use async_trait::async_trait;
use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use storage_device::Device;
use storage_device::buffer::Buffer;
use zerocopy::FromBytes;

// Used to indicate zero pages (when used as block_addr) and end of list (when used as nid).
pub const NULL_ADDR: u32 = 0;
// Used to indicate a new page that hasn't been allocated yet.
pub const NEW_ADDR: u32 = 0xffffffff;

/// This trait is exposed to allow unit testing of Inode and other structs.
/// It is implemented by F2fsReader.
#[async_trait]
pub(super) trait Reader {
    /// Read a raw block from disk.
    /// `block_addr` is the physical block offset on the device.
    async fn read_raw_block(&self, block_addr: u32) -> Result<Buffer<'_>, Error>;

    /// Reads a logical 'node' block from the disk (i.e. via NAT indirection)
    async fn read_node(&self, nid: u32) -> Result<Buffer<'_>, Error>;

    /// Attempt to retrieve a key given its identifier.
    fn get_key(&self, _identifier: &[u8; 16]) -> Option<&[u8; 64]> {
        None
    }

    /// Returns the filesystem UUID. This is needed for some decryption policies.
    fn fs_uuid(&self) -> &[u8; 16];

    /// Attempt to obtain a decryptor for a given crypto context.
    /// Will return None if the main key is not known.
    fn get_decryptor_for_inode(&self, inode: &Inode) -> Option<crypto::PerFileDecryptor> {
        if let Some(context) = inode.context {
            if let Some(main_key) = self.get_key(&context.main_key_identifier) {
                return Some(crypto::PerFileDecryptor::new(main_key, context, self.fs_uuid()));
            }
        }
        None
    }

    /// Look up a raw NAT entry given a node ID.
    async fn get_nat_entry(&self, nid: u32) -> Result<RawNatEntry, Error>;
}

pub struct F2fsReader {
    device: Arc<dyn Device>,
    superblock: SuperBlock,     // 1kb, points at checkpoints
    checkpoint: CheckpointPack, // pair of a/b segments (alternating versions)
    nat: Option<Nat>,

    // A simple key store.
    keys: HashMap<[u8; 16], [u8; 64]>,
    cache: BlockCache,
}

impl Drop for F2fsReader {
    fn drop(&mut self) {
        // Zero keys in RAM for extra safety.
        self.keys.values_mut().for_each(|v| {
            *v = [0u8; 64];
        });
    }
}

impl F2fsReader {
    pub fn superblock(&self) -> &SuperBlock {
        &self.superblock
    }

    pub fn checkpoint(&self) -> &CheckpointPack {
        &self.checkpoint
    }

    pub async fn open_device(device: Arc<dyn Device>) -> Result<Self, Error> {
        let (superblock, checkpoint) =
            match Self::try_from_superblock(device.as_ref(), SUPERBLOCK_OFFSET).await {
                Ok(x) => x,
                Err(e) => Self::try_from_superblock(device.as_ref(), SUPERBLOCK_OFFSET * 2)
                    .await
                    .map_err(|_| e)?,
            };
        let mut this = Self {
            device,
            superblock,
            checkpoint,
            nat: None,
            keys: HashMap::with_capacity(16),
            cache: BlockCache::new(1024, BLOCK_SIZE),
        };
        let nat_journal = this.read_nat_journal().await?;
        this.nat = Some(Nat::new(
            this.superblock.nat_blkaddr,
            this.checkpoint.nat_bitmap.clone(),
            nat_journal,
        ));
        Ok(this)
    }

    async fn try_from_superblock(
        device: &dyn Device,
        superblock_offset: u64,
    ) -> Result<(SuperBlock, CheckpointPack), Error> {
        let superblock = SuperBlock::read_from_device(device, superblock_offset).await?;
        let checkpoint_addr = superblock.cp_blkaddr;
        let checkpoint_a_offset = BLOCK_SIZE as u64 * checkpoint_addr as u64;
        let checkpoint_b_offset = checkpoint_a_offset + SEGMENT_SIZE as u64;
        // There are two checkpoint packs in consecutive segments.
        let checkpoint = match (
            CheckpointPack::read_from_device(device, checkpoint_a_offset).await,
            CheckpointPack::read_from_device(device, checkpoint_b_offset).await,
        ) {
            (Ok(a), Ok(b)) => {
                Ok(if a.header.checkpoint_ver > b.header.checkpoint_ver { a } else { b })
            }
            (Ok(a), Err(_b)) => Ok(a),
            (Err(_), Ok(b)) => Ok(b),
            (Err(a), Err(_b)) => Err(a),
        }?;

        // Min metadata segment count is 1 superblock, 1 ssa, (ckpt + sit + nat) * 2
        const MIN_METADATA_SEGMENT_COUNT: u32 = 8;

        // Make sure the metadata fits on the device (according to the superblock)
        let metadata_segment_count = superblock.segment_count_sit
            + superblock.segment_count_nat
            + checkpoint.header.rsvd_segment_count
            + superblock.segment_count_ssa
            + superblock.segment_count_ckpt;
        ensure!(
            metadata_segment_count <= superblock.segment_count
                && metadata_segment_count >= MIN_METADATA_SEGMENT_COUNT,
            "Bad segment counts in checkpoint"
        );
        Ok((superblock, checkpoint))
    }

    /// Returns the block address that the checkpoint starts at.
    pub fn checkpoint_start_addr(&self) -> u32 {
        self.superblock.cp_blkaddr
            + if self.checkpoint.header.checkpoint_ver % 2 == 1 {
                0
            } else {
                BLOCKS_PER_SEGMENT as u32
            }
    }

    fn nat(&self) -> &Nat {
        self.nat.as_ref().unwrap()
    }

    async fn read_nat_journal(&mut self) -> Result<HashMap<u32, RawNatEntry>, Error> {
        if self.checkpoint.header.ckpt_flags & CKPT_FLAG_COMPACT_SUMMARY != 0 {
            // The "compact summary" feature packs NAT/SIT/summary into one block.
            // The NAT journal entries come first.
            let block = self
                .read_raw_block(
                    self.checkpoint_start_addr() + self.checkpoint.header.cp_pack_start_sum,
                )
                .await?;
            let n_nats = u16::read_from_bytes(&block.as_slice()[..2]).unwrap();
            let nat_journal = NatJournal::read_from_bytes(
                &block.as_slice()[2..2 + std::mem::size_of::<NatJournal>()],
            )
            .unwrap();
            ensure!(
                (n_nats as usize) <= nat_journal.entries.len(),
                "n_nats larger than block size"
            );
            Ok(HashMap::from_iter(
                nat_journal.entries[..n_nats as usize].into_iter().map(|e| (e.ino, e.entry)),
            ))
        } else {
            // Read the default summary block location from the "hot data" segment.
            let blk_addr = if self.checkpoint.header.ckpt_flags & CKPT_FLAG_UNMOUNT != 0 {
                self.checkpoint_start_addr() + self.checkpoint.header.cp_pack_total_block_count - 5
            } else {
                self.checkpoint_start_addr() + self.checkpoint.header.cp_pack_total_block_count - 2
            };
            let block = self.read_raw_block(blk_addr).await?;
            let summary = SummaryBlock::read_from_bytes(block.as_slice()).unwrap();
            ensure!(summary.footer.entry_type == 0u8, "sum_type != 0 in summary footer");
            let actual_checksum = f2fs_crc32(F2FS_MAGIC, &block.as_slice()[..BLOCK_SIZE - 4]);
            let expected_checksum = summary.footer.check_sum;
            ensure!(actual_checksum == expected_checksum, "Summary block has invalid checksum");
            let mut out = HashMap::new();
            for i in 0..summary.n_nats as usize {
                out.insert(
                    summary.nat_journal.entries[i].ino,
                    summary.nat_journal.entries[i].entry,
                );
            }
            Ok(out)
        }
    }

    pub fn root_ino(&self) -> u32 {
        self.superblock.root_ino
    }

    /// Gives the maximum addressable inode. This can be used to ensure we don't have namespace
    /// collisions when building hybrid images.
    pub fn max_ino(&self) -> u32 {
        (self.checkpoint.nat_bitmap.len() * 8) as u32
    }

    /// Registers a new main key.
    /// This 'unlocks' any files using this key.
    pub fn add_key(&mut self, main_key: &[u8; 64]) -> [u8; 16] {
        let identifier = fscrypt::main_key_to_identifier(main_key);
        println!("Adding key with identifier {}", hex::encode(identifier));
        self.keys.insert(identifier.clone(), main_key.clone());
        identifier
    }

    /// Read an inode for a directory and return entries.
    pub async fn readdir(&self, ino: u32) -> Result<Vec<DirEntry>, Error> {
        let inode = Inode::try_load(self, ino).await?;
        let decryptor = self.get_decryptor_for_inode(&inode);
        let mode = inode.header.mode;
        let advise_flags = inode.header.advise_flags;
        let flags = inode.header.flags;
        ensure!(mode.contains(inode::Mode::Directory), "not a directory");
        if let Some(entries) = inode.get_inline_dir_entries(
            advise_flags.contains(inode::AdviseFlags::Encrypted),
            flags.contains(inode::Flags::Casefold),
            &decryptor,
        )? {
            Ok(entries)
        } else {
            let mut entries = Vec::new();

            // Entries are stored in a series of increasingly larger hash tables.
            // The number of these that exist are based on inode.dir_depth.
            // Thankfully, we don't need to worry about this as the total number of blocks is
            // bound to inode.header.size and we can just skip NULL blocks.
            for mut extent in inode.data_blocks() {
                for _ in 0..extent.length {
                    let dentry_block = DentryBlock::read_from_bytes(
                        self.read_raw_block(extent.physical_block_num).await?.as_slice(),
                    )
                    .unwrap();
                    entries.append(&mut dentry_block.get_entries(
                        ino,
                        advise_flags.contains(inode::AdviseFlags::Encrypted),
                        flags.contains(inode::Flags::Casefold),
                        &decryptor,
                    )?);
                    extent.physical_block_num += 1;
                }
            }
            Ok(entries)
        }
    }

    /// Read an inode and associated blocks from disk.
    pub async fn read_inode(&self, ino: u32) -> Result<Box<Inode>, Error> {
        Inode::try_load(self, ino).await
    }

    /// Takes an inode for a symlink and the link as a set of bytes, decrypted if possible.
    pub fn read_symlink(&self, inode: &Inode) -> Result<Box<[u8]>, Error> {
        if let Some(inline_data) = inode.inline_data.as_deref() {
            let mut filename = inline_data.to_vec();
            if inode.header.advise_flags.contains(inode::AdviseFlags::Encrypted) {
                // Encrypted symlinks have a 2-byte length prefix.
                ensure!(filename.len() >= 2, "invalid encrypted symlink");
                let symlink_len = u16::read_from_bytes(&filename[..2]).unwrap();
                filename.drain(..2);
                filename.truncate(symlink_len as usize);
                ensure!(symlink_len == filename.len() as u16, "invalid encrypted symlink");
                if let Some(decryptor) = self.get_decryptor_for_inode(inode) {
                    decryptor.decrypt_filename_data(inode.footer.ino, &mut filename);
                } else {
                    // Symlinks don't have a hash code, so we just use 0.
                    let proxy_filename: String =
                        fscrypt::proxy_filename::ProxyFilename::new_with_hash_code(0, &filename)
                            .into();
                    filename = proxy_filename.as_bytes().to_vec();
                }
                // Unfortunately, it seems we still have to remove trailing nulls.
                // fscrypt + f2fs publishes a file size equal to padded symlink length + 2 bytes.
                while let Some(0) = filename.last() {
                    filename.pop();
                }
            }
            Ok(filename.into_boxed_slice())
        } else {
            bail!("Not a valid symlink");
        }
    }

    /// Reads and returns a data block of a file.
    /// On success, this will return Some(Buffer) containing the data or None if the file is sparse.
    pub async fn read_data(
        &self,
        inode: &Inode,
        block_num: u32,
    ) -> Result<Option<Buffer<'_>>, Error> {
        let inline_flags = inode.header.inline_flags;
        ensure!(
            !inline_flags.contains(crate::InlineFlags::Data),
            "Can't use read_data() on inline file."
        );
        let block_addr = inode.data_block_addr(block_num);
        if block_addr == NULL_ADDR || block_addr == NEW_ADDR {
            // Treat as an empty page
            return Ok(None);
        }
        let mut buffer = self.read_raw_block(block_addr).await?;
        if let Some(decryptor) = self.get_decryptor_for_inode(inode) {
            decryptor.decrypt_data(inode.footer.ino, block_num, buffer.as_mut().as_mut_slice());
        }
        Ok(Some(buffer))
    }
}

#[async_trait]
impl Reader for F2fsReader {
    /// `block_addr` is the physical block offset on the device.
    async fn read_raw_block(&self, block_addr: u32) -> Result<Buffer<'_>, Error> {
        if let Some(block) = self.cache.get_buffer(block_addr, self.device.deref()).await {
            return Ok(block);
        }

        const READAHEAD: u64 = 16;
        let end = std::cmp::min(block_addr as u64 + READAHEAD, self.device.block_count());
        let count = end.saturating_sub(block_addr as u64).max(1) as usize;

        let mut buffer = self.device.allocate_buffer(count * BLOCK_SIZE).await;
        self.device
            .read(block_addr as u64 * BLOCK_SIZE as u64, buffer.as_mut())
            .await
            .map_err(|_| anyhow!("device read failed"))?;

        for i in 0..count {
            let slice = &buffer.as_slice()[i * BLOCK_SIZE..(i + 1) * BLOCK_SIZE];
            self.cache.insert(block_addr + i as u32, slice.to_vec());
        }
        Ok(self.cache.get_buffer(block_addr, self.device.deref()).await.unwrap())
    }

    async fn read_node(&self, nid: u32) -> Result<Buffer<'_>, Error> {
        let nat_entry = self.get_nat_entry(nid).await?;
        self.read_raw_block(nat_entry.block_addr).await
    }

    fn get_key(&self, identifier: &[u8; 16]) -> Option<&[u8; 64]> {
        self.keys.get(identifier)
    }

    fn fs_uuid(&self) -> &[u8; 16] {
        &self.superblock.uuid
    }

    async fn get_nat_entry(&self, nid: u32) -> Result<RawNatEntry, Error> {
        if let Some(entry) = self.nat().nat_journal.get(&nid) {
            return Ok(*entry);
        }
        let nat_block_addr = self.nat().get_nat_block_for_entry(nid)?;
        let offset = self.nat().get_nat_block_offset_for_entry(nid);
        let block = self.read_raw_block(nat_block_addr).await?;
        Ok(RawNatEntry::read_from_bytes(
            &block.as_slice()[offset..offset + std::mem::size_of::<RawNatEntry>()],
        )
        .unwrap())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::dir::FileType;
    use crate::xattr;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;

    use storage_device::fake_device::FakeDevice;

    fn open_test_image(path: &str) -> FakeDevice {
        let path = std::path::PathBuf::from(path);
        println!("path is {path:?}");
        FakeDevice::from_image(
            zstd::Decoder::new(std::fs::File::open(&path).expect("open image"))
                .expect("decompress image"),
            BLOCK_SIZE as u32,
        )
        .expect("open image")
    }

    #[fuchsia::test]
    async fn test_open_fs() {
        let device = open_test_image("/pkg/testdata/f2fs.img.zst");

        let f2fs = F2fsReader::open_device(Arc::new(device)).await.expect("open ok");
        // Root inode is a known constant.
        assert_eq!(f2fs.root_ino(), 3);
        let superblock = &f2fs.superblock;
        let major_ver = superblock.major_ver;
        let minor_ver = superblock.minor_ver;
        assert_eq!(major_ver, 1);
        assert_eq!(minor_ver, 16);
        assert_eq!(superblock.get_total_size(), 256 << 20);
        assert_eq!(superblock.get_volume_name().expect("get volume name"), "testimage");
    }

    // Helper method to walk paths.
    async fn resolve_inode_path(f2fs: &F2fsReader, path: &str) -> Result<u32, Error> {
        let path = PathBuf::from(path.strip_prefix("/").unwrap());
        let mut ino = f2fs.root_ino();
        for filename in &path {
            let entries = f2fs.readdir(ino).await?;
            if let Some(entry) = entries.iter().filter(|e| *e.filename == *filename).next() {
                ino = entry.ino;
            } else {
                bail!("Not found.");
            }
        }
        Ok(ino)
    }

    #[fuchsia::test]
    async fn test_basic_dirs() {
        let device = open_test_image("/pkg/testdata/f2fs.img.zst");

        let f2fs = F2fsReader::open_device(Arc::new(device)).await.expect("open ok");
        let root_ino = f2fs.root_ino();
        let root_entries = f2fs.readdir(root_ino).await.expect("readdir");
        assert_eq!(root_entries.len(), 7);
        assert_eq!(root_entries[0].filename, "a");
        assert_eq!(root_entries[0].file_type, FileType::Directory);
        assert_eq!(root_entries[1].filename, "large_dir");
        assert_eq!(root_entries[2].filename, "large_dir2");
        assert_eq!(root_entries[3].filename, "sparse.dat");
        assert_eq!(root_entries[4].filename, "verity");
        assert_eq!(root_entries[5].filename, "fscrypt");
        assert_eq!(root_entries[6].filename, "large_zero");

        let inlined_file_ino =
            resolve_inode_path(&f2fs, "/a/b/c/inlined").await.expect("resolve inlined");
        let inode = Inode::try_load(&f2fs, inlined_file_ino).await.expect("load inode");
        let block_size = inode.header.block_size;
        let size = inode.header.size;
        assert_eq!(block_size, 1);
        assert_eq!(size, 12);
        assert_eq!(inode.inline_data.unwrap().as_ref(), "inline_data\n".as_bytes());

        const REG_FILE_SIZE: u64 = 8 * BLOCK_SIZE as u64 + 8;
        const REG_FILE_BLOCKS: u64 = 9 + 1;
        let regular_file_ino =
            resolve_inode_path(&f2fs, "/a/b/c/regular").await.expect("resolve regular");
        let inode = Inode::try_load(&f2fs, regular_file_ino).await.expect("load inode");
        let block_size = inode.header.block_size;
        let size = inode.header.size;
        assert_eq!(block_size, REG_FILE_BLOCKS);
        assert_eq!(size, REG_FILE_SIZE);
        assert!(inode.inline_data.is_none());
        for i in 0..8 {
            assert_eq!(
                f2fs.read_data(&inode, i).await.expect("read data").unwrap().as_slice(),
                &[0u8; BLOCK_SIZE]
            );
        }
        assert_eq!(
            &f2fs.read_data(&inode, 8).await.expect("read data").unwrap().as_slice()[..9],
            b"01234567\0"
        );

        let symlink_ino =
            resolve_inode_path(&f2fs, "/a/b/c/symlink").await.expect("resolve symlink");
        let inode = Inode::try_load(&f2fs, symlink_ino).await.expect("load inode");
        assert_eq!(f2fs.read_symlink(&inode).expect("read_symlink").as_ref(), b"regular");

        let hardlink_ino =
            resolve_inode_path(&f2fs, "/a/b/c/hardlink").await.expect("resolve hardlink");
        let inode = Inode::try_load(&f2fs, hardlink_ino).await.expect("load inode");
        let block_size = inode.header.block_size;
        let size = inode.header.size;
        assert_eq!(block_size, REG_FILE_BLOCKS);
        assert_eq!(size, REG_FILE_SIZE);

        let chowned_ino =
            resolve_inode_path(&f2fs, "/a/b/c/chowned").await.expect("resolve chowned");
        let inode = Inode::try_load(&f2fs, chowned_ino).await.expect("load inode");
        let uid = inode.header.uid;
        let gid = inode.header.gid;
        assert_eq!(uid, 999);
        assert_eq!(gid, 999);

        let large_dir = resolve_inode_path(&f2fs, "/large_dir").await.expect("resolve large_dir");
        assert_eq!(f2fs.readdir(large_dir).await.expect("readdir").len(), 2001);

        let large_dir2 = resolve_inode_path(&f2fs, "/large_dir2").await.expect("resolve large_dir");
        assert_eq!(f2fs.readdir(large_dir2).await.expect("readdir").len(), 1);

        let sparse_dat =
            resolve_inode_path(&f2fs, "/sparse.dat").await.expect("resolve sparse.dat");
        let inode = Inode::try_load(&f2fs, sparse_dat).await.expect("load inode");
        let data_blocks: Vec<_> = inode.data_blocks().into_iter().collect();
        assert_eq!(data_blocks.len(), 6);
        assert_eq!(data_blocks[0].logical_block_num, 0);
        assert_eq!(data_blocks[0].length, 1);
        // Raw read of block.
        let block = f2fs.read_raw_block(data_blocks[0].physical_block_num).await.expect("read sparse");
        assert_eq!(&block.as_slice()[..3], b"foo");
        // The following chain of blocks are designed to land in each of the self.nids[] ranges.
        assert_eq!(data_blocks[1].logical_block_num, 923);
        assert_eq!(data_blocks[1].length, 1);
        assert_eq!(data_blocks[2].logical_block_num, 1941);
        assert_eq!(data_blocks[2].length, 1);
        assert_eq!(data_blocks[3].logical_block_num, 2959);
        assert_eq!(data_blocks[3].length, 1);
        assert_eq!(data_blocks[4].logical_block_num, 1039283);
        assert_eq!(data_blocks[4].length, 1);
        assert_eq!(data_blocks[5].logical_block_num, 104671683);
        assert_eq!(data_blocks[5].length, 2);
        let block = f2fs.read_raw_block(data_blocks[5].physical_block_num).await.expect("read sparse");
        assert_eq!(block.as_slice(), &[0; BLOCK_SIZE]);
        // Exercise helper method to read block.
        assert_eq!(
            &f2fs.read_data(&inode, 104671684).await.expect("read data block").unwrap().as_slice()
                [..3],
            b"bar"
        );
        // Exercise helper method on zero page. Expect to get back 'None'.
        assert!(f2fs.read_data(&inode, 104671684 - 10).await.expect("read data block").is_none());
    }

    #[fuchsia::test]
    async fn test_xattr() {
        let device = open_test_image("/pkg/testdata/f2fs.img.zst");

        let f2fs = F2fsReader::open_device(Arc::new(device)).await.expect("open ok");
        let sparse_dat =
            resolve_inode_path(&f2fs, "/sparse.dat").await.expect("resolve sparse.dat");
        let inode = Inode::try_load(&f2fs, sparse_dat).await.expect("load inode");
        assert_eq!(
            inode.xattr,
            vec![
                xattr::XattrEntry {
                    index: xattr::Index::User,
                    name: Box::new(b"a".to_owned()),
                    value: Box::new(b"value".to_owned())
                },
                xattr::XattrEntry {
                    index: xattr::Index::User,
                    name: Box::new(b"c".to_owned()),
                    value: Box::new(b"value".to_owned())
                },
                xattr::XattrEntry {
                    index: xattr::Index::User,
                    name: Box::new(b"padding_test_1".to_owned()),
                    value: Box::new(b"v".to_owned())
                },
                xattr::XattrEntry {
                    index: xattr::Index::User,
                    name: Box::new(b"padding_test_2".to_owned()),
                    value: Box::new(b"va".to_owned())
                },
                xattr::XattrEntry {
                    index: xattr::Index::User,
                    name: Box::new(b"padding_test_3".to_owned()),
                    value: Box::new(b"val".to_owned())
                },
                xattr::XattrEntry {
                    index: xattr::Index::User,
                    name: Box::new(b"padding_test_4".to_owned()),
                    value: Box::new(b"valu".to_owned())
                },
                xattr::XattrEntry {
                    index: xattr::Index::User,
                    name: Box::new(b"padding_test_5".to_owned()),
                    value: Box::new(b"value".to_owned())
                },
            ]
        );
    }

    #[fuchsia::test]
    async fn test_fsverity() {
        let device = open_test_image("/pkg/testdata/f2fs.img.zst");
        let mut f2fs = F2fsReader::open_device(Arc::new(device)).await.expect("open ok");
        f2fs.add_key(&[0u8; 64]);
        let verity_files = vec![
            "/verity/inlined",
            "/verity/regular",
            "/verity/merkle_layers.dat",
            "/fscrypt/a/b/regular",
        ];
        for file_path in verity_files {
            let file = resolve_inode_path(&f2fs, file_path).await.expect("resolve file");
            let inode = Inode::try_load(&f2fs, file).await.expect("load inode");
            assert!(inode.header.advise_flags.contains(inode::AdviseFlags::Verity));
        }
        // Verify other files aren't marked for verity.
        let file = resolve_inode_path(&f2fs, "/a/b/c/regular").await.expect("resolve file");
        let inode = Inode::try_load(&f2fs, file).await.expect("load inode");
        assert!(!inode.header.advise_flags.contains(inode::AdviseFlags::Verity));
        // TODO(https://fxbug.dev/399727919): Handle the verity descriptor and merkle tree parsing.
    }

    #[fuchsia::test]
    async fn test_fbe() {
        // Note: The synthetic filenames below are based on the nonce generated at file/directory
        // creation time. This will differ each time a new image is generated.
        // They can be extracted with a simple 'ls -l' by mounting the generated image. i.e.
        //   $ zstd -d testdata/f2fs.img.st
        //   $ sudo mount testdata/f2fs.img /mnt
        //   $ ls /mnt/fscrypt -lR

        // /fscrypt/<a>/<b>/<symlink>
        let str_a = "2ll82QAAAADywluz1Ule7OVNBxUfa5Mw";
        let str_b = "sttckQAAAADLBOCVVgjrZ-CXNkj5E6Cr";
        let str_symlink = "zHAtQgAAAACRNPQYvCKuQo5F8rQUORg3";
        let bytes_symlink_content = b"AAAAAAAAAADUsYZ_qNiiouF7e40xm65S";

        let mut expected : HashSet<_> = [ // files in fscrypt/ dir.
            "2ll82QAAAADywluz1Ule7OVNBxUfa5Mw",
            "65OSUQAAAADqOiZJcQ1El2dpVdYMy84l",
            "7vcnbgAAAAAOWdQfi4wK46uRGQBD0YSy",
            "9Gsv9QAAAADjTeJ_9WdCxZMVTiSWhsWR",
            "FAqGXAAAAAD1jOLXaZN-o8X9PoS67GI7",
            "Rq5qZAAAAAA3y2lvAqesYDnVJWMklWnj",
            "S93sdgAAAABo-YmXNPKtv4wxQCcUslTu",
            "VP8QBwAAAAATw6Ozex0N2gMYrnDsB2aH",
            "xUNjwgAAAADB0pEx5ovwx-AS02L0d1j7VMBRXzM4YnBri2pbasOqbFLhtegXr9kDGNcYd_hyk2mOkQIqu8hk7eARlFl-bq1yLhikhIT9HVC3FMrI7vQ-ewncEjXLDP3KK6RtH3r34S89AlzJZ4DVfXrr_Q5N5mANBbGTzeO70aJHL0Ms-MgkKwjHcbIxXLwcjE2B-mssLAvXam58pSD-aazxS_J2hrxOHGoUYiVJ-rXHozmKxBdWAO6OUW65",
        ].into_iter().collect();

        let device = open_test_image("/pkg/testdata/f2fs.img.zst");

        let mut f2fs = F2fsReader::open_device(Arc::new(device)).await.expect("open ok");

        // First without the key...
        // (The filenames below have been extracted from the generated image by
        // mounting it and manually inspecting.)
        resolve_inode_path(&f2fs, "/fscrypt/a/b/regular")
            .await
            .expect_err("resolve fscrypt regular");
        let fscrypt_dir_ino =
            resolve_inode_path(&f2fs, "/fscrypt").await.expect("resolve encrypted dir");
        let entries = f2fs.readdir(fscrypt_dir_ino).await.expect("readdir");
        println!("entries {entries:?}");

        for entry in entries {
            assert!(expected.remove(entry.filename.as_str()), "unexpected entry {entry:?}");
        }
        assert!(expected.is_empty());

        resolve_inode_path(&f2fs, &format!("/fscrypt/{str_a}"))
            .await
            .expect("resolve encrypted dir");
        let enc_symlink_ino =
            resolve_inode_path(&f2fs, &format!("/fscrypt/{str_a}/{str_b}/{str_symlink}"))
                .await
                .expect("resolve encrypted symlink");
        let symlink_inode =
            Inode::try_load(&f2fs, enc_symlink_ino).await.expect("load symlink inode");
        assert_eq!(
            &*f2fs.read_symlink(&symlink_inode).expect("read_symlink"),
            bytes_symlink_content
        );

        // ...now try with the key
        f2fs.add_key(&[0u8; 64]);
        resolve_inode_path(&f2fs, "/fscrypt/a/b/regular").await.expect("resolve fscrypt regular");
        let inlined_ino = resolve_inode_path(&f2fs, "/fscrypt/a/b/inlined")
            .await
            .expect("resolve fscrypt inlined");
        let short_file = Inode::try_load(&f2fs, inlined_ino).await.expect("load symlink inode");
        assert!(
            !short_file.header.inline_flags.contains(inode::InlineFlags::Data),
            "encrypted files shouldn't be inlined"
        );
        let short_data =
            f2fs.read_data(&short_file, 0).await.expect("read_data").expect("non-empty page");
        assert_eq!(
            &short_data.as_slice()[..short_file.header.size as usize],
            b"test45678abcdef_12345678"
        );

        let symlink_ino = resolve_inode_path(&f2fs, "/fscrypt/a/b/symlink")
            .await
            .expect("resolve fscrypt symlink");
        assert_eq!(symlink_ino, enc_symlink_ino);

        let symlink_inode = Inode::try_load(&f2fs, symlink_ino).await.expect("load symlink inode");
        let symlink = f2fs.read_symlink(&symlink_inode).expect("read_symlink");
        assert_eq!(*symlink, *b"inlined");
    }
}
