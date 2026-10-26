// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A simple EROFS serializer for testing and benchmarks. Emits compact inodes using `FlatInline`
//! when a file's tail fits within the metadata block and `FlatPlain` otherwise.

use erofs::{FileType, format};
use zerocopy::IntoBytes;
use zerocopy::byteorder::little_endian::{U16 as LEU16, U32 as LEU32, U64 as LEU64};

const BLOCK_SIZE: usize = 4096;
const INODE_SIZE: usize = format::INODE_SLOT_SIZE as usize;
const MAX_INLINE_TAIL_LEN: usize = BLOCK_SIZE - INODE_SIZE;

/// A node in the tree to be serialized.
#[derive(Debug, Clone)]
pub enum SerializerNode {
    /// A directory with node children.
    Directory { name: String, entries: Vec<SerializerNode> },
    /// A file with byte contents.
    File { name: String, data: Vec<u8> },
    /// A symlink pointing to a target.
    Symlink { name: String, target: Vec<u8> },
}

impl SerializerNode {
    pub fn name(&self) -> &str {
        match self {
            Self::Directory { name, .. } => name,
            Self::File { name, .. } => name,
            Self::Symlink { name, .. } => name,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeType {
    Dir,
    File,
    Symlink,
}

struct FlatNode {
    nid: u64,
    inode_offset: usize,
    is_inline: bool,
    node_type: NodeType,
    contents: FlatNodeContents,
    data_block: u32,
    size: u64,
}

enum FlatNodeContents {
    Directory {
        // (name, node_index, node_type)
        entries: Vec<(String, usize, NodeType)>,
        blocks: Vec<[u8; BLOCK_SIZE]>,
    },
    File {
        data: Vec<u8>,
    },
    Symlink {
        target: Vec<u8>,
    },
}

fn add_node(node: &SerializerNode, nodes: &mut Vec<FlatNode>, parent_idx: usize) -> usize {
    let idx = nodes.len();
    match node {
        SerializerNode::Directory { name: _, entries } => {
            nodes.push(FlatNode {
                nid: 0,
                inode_offset: 0,
                is_inline: false,
                node_type: NodeType::Dir,
                contents: FlatNodeContents::Directory { entries: Vec::new(), blocks: Vec::new() },
                data_block: 0,
                size: 0,
            });

            let mut child_entries = Vec::new();
            child_entries.push((".".to_string(), idx, NodeType::Dir));
            child_entries.push(("..".to_string(), parent_idx, NodeType::Dir));

            for child in entries {
                let child_name = child.name().to_string();
                let child_idx = add_node(child, nodes, idx);
                let child_type = nodes[child_idx].node_type;
                child_entries.push((child_name, child_idx, child_type));
            }

            child_entries.sort_by(|a, b| a.0.cmp(&b.0));
            nodes[idx].contents =
                FlatNodeContents::Directory { entries: child_entries, blocks: Vec::new() };
        }
        SerializerNode::File { name: _, data } => {
            nodes.push(FlatNode {
                nid: 0,
                inode_offset: 0,
                is_inline: false,
                node_type: NodeType::File,
                contents: FlatNodeContents::File { data: data.clone() },
                data_block: 0,
                size: 0,
            });
        }
        SerializerNode::Symlink { name: _, target } => {
            nodes.push(FlatNode {
                nid: 0,
                inode_offset: 0,
                is_inline: false,
                node_type: NodeType::Symlink,
                contents: FlatNodeContents::Symlink { target: target.clone() },
                data_block: 0,
                size: 0,
            });
        }
    }
    idx
}

fn pack_directory_blocks(
    entries: &[(String, usize, NodeType)],
    nodes: &[FlatNode],
) -> Vec<[u8; BLOCK_SIZE]> {
    let mut blocks = Vec::new();
    let mut start = 0;
    while start < entries.len() {
        let mut end = start;
        let mut names_len = 0usize;
        while end < entries.len() {
            let next_count = end - start + 1;
            let next_names_len = names_len + entries[end].0.len();
            if next_count * format::DIRENT_SIZE + next_names_len > BLOCK_SIZE {
                break;
            }
            names_len = next_names_len;
            end += 1;
        }
        assert!(end > start, "Single directory entry exceeds 4096-byte block size");

        let slice = &entries[start..end];
        let mut dir_block = [0u8; BLOCK_SIZE];
        let mut current_nameoff = (slice.len() * format::DIRENT_SIZE) as u16;
        let mut dirents = Vec::with_capacity(slice.len());
        let mut name_bytes = Vec::with_capacity(names_len);

        for (name, target_idx, node_type) in slice {
            let file_type = match node_type {
                NodeType::Dir => FileType::Dir as u8,
                NodeType::File => FileType::RegFile as u8,
                NodeType::Symlink => FileType::Symlink as u8,
            };
            dirents.push(format::Dirent {
                nid: LEU64::new(nodes[*target_idx].nid),
                nameoff: LEU16::new(current_nameoff),
                file_type,
                reserved: 0,
            });
            name_bytes.extend_from_slice(name.as_bytes());
            current_nameoff += name.len() as u16;
        }

        let dirents_bytes = dirents.as_slice().as_bytes();
        dir_block[..dirents_bytes.len()].copy_from_slice(dirents_bytes);
        let names_start = dirents_bytes.len();
        dir_block[names_start..names_start + name_bytes.len()].copy_from_slice(&name_bytes);
        blocks.push(dir_block);
        start = end;
    }
    blocks
}

/// Serialize a directory tree into an EROFS image using compact inodes (`FlatInline` for files
/// with tails that fit within a metadata block, and `FlatPlain` otherwise).
pub fn serialize(root_entries: &[SerializerNode]) -> Vec<u8> {
    let mut nodes = Vec::<FlatNode>::new();

    // Create root directory at index 0 (which will receive NID 0 at offset 4096).
    nodes.push(FlatNode {
        nid: 0,
        inode_offset: 0,
        is_inline: false,
        node_type: NodeType::Dir,
        contents: FlatNodeContents::Directory { entries: Vec::new(), blocks: Vec::new() },
        data_block: 0,
        size: 0,
    });

    let mut root_child_entries = Vec::new();
    root_child_entries.push((".".to_string(), 0, NodeType::Dir));
    root_child_entries.push(("..".to_string(), 0, NodeType::Dir));

    for child in root_entries {
        let child_name = child.name().to_string();
        let child_idx = add_node(child, &mut nodes, 0);
        let child_type = nodes[child_idx].node_type;
        root_child_entries.push((child_name, child_idx, child_type));
    }

    root_child_entries.sort_by(|a, b| a.0.cmp(&b.0));
    nodes[0].contents =
        FlatNodeContents::Directory { entries: root_child_entries, blocks: Vec::new() };

    // 1. Assign metadata offsets and NIDs in the metadata zone (starting at block 1 = byte 4096).
    // Files with a non-zero tail <= 4064 bytes use FlatInline so their tail sits immediately after
    // their 32-byte InodeCompact within the same 4096-byte metadata block.
    let meta_base = BLOCK_SIZE;
    let mut current_meta_offset = meta_base;
    for node in &mut nodes {
        let inline_tail_len = match &node.contents {
            FlatNodeContents::File { data } => {
                let tail_len = data.len() % BLOCK_SIZE;
                if tail_len > 0 && tail_len <= MAX_INLINE_TAIL_LEN {
                    node.is_inline = true;
                    tail_len
                } else {
                    0
                }
            }
            _ => 0,
        };
        let required_meta_bytes = INODE_SIZE + inline_tail_len;
        if (current_meta_offset % BLOCK_SIZE) + required_meta_bytes > BLOCK_SIZE {
            current_meta_offset = current_meta_offset.next_multiple_of(BLOCK_SIZE);
        }
        node.inode_offset = current_meta_offset;
        node.nid = ((current_meta_offset - meta_base) / INODE_SIZE) as u64;
        current_meta_offset =
            (current_meta_offset + required_meta_bytes).next_multiple_of(INODE_SIZE);
    }

    // 2. Build directory blocks first now that all NIDs are assigned, then allocate file and
    // symlink data blocks so directory lookups do not sit adjacent to file data blocks.
    let data_start_block = current_meta_offset.div_ceil(BLOCK_SIZE) as u32;
    let mut next_free_block = std::cmp::max(2, data_start_block);

    for i in 0..nodes.len() {
        if let FlatNodeContents::Directory { entries, .. } = &nodes[i].contents {
            let blocks = pack_directory_blocks(entries, &nodes);
            nodes[i].data_block = next_free_block;
            nodes[i].size = (blocks.len() * BLOCK_SIZE) as u64;
            next_free_block += blocks.len() as u32;
            if let FlatNodeContents::Directory { blocks: dst_blocks, .. } = &mut nodes[i].contents {
                *dst_blocks = blocks;
            }
        }
    }

    for node in &mut nodes {
        match &node.contents {
            FlatNodeContents::Directory { .. } => {}
            FlatNodeContents::File { data } => {
                let len = data.len() as u64;
                node.size = len;
                let data_blocks = if node.is_inline {
                    (len / BLOCK_SIZE as u64) as u32
                } else {
                    len.div_ceil(BLOCK_SIZE as u64) as u32
                };
                if data_blocks > 0 {
                    node.data_block = next_free_block;
                    next_free_block += data_blocks;
                } else {
                    node.data_block = 0;
                }
            }
            FlatNodeContents::Symlink { target } => {
                let len = target.len() as u64;
                node.size = len;
                if len > 0 {
                    node.data_block = next_free_block;
                    let blocks_needed = len.div_ceil(BLOCK_SIZE as u64);
                    next_free_block += blocks_needed as u32;
                } else {
                    node.data_block = 0;
                }
            }
        }
    }

    let total_blocks = next_free_block;
    let mut image = vec![0u8; total_blocks as usize * BLOCK_SIZE];

    // 3. Write Superblock
    let sb = format::SuperBlock {
        magic: LEU32::new(format::EROFS_MAGIC),
        checksum: LEU32::new(0),
        feature_compat: LEU32::new(0),
        block_size_bits: 12, // 4096
        sb_ext_slots: 0,
        root_nid: LEU16::new(0),
        inode_count: LEU64::new(nodes.len() as u64),
        epoch: LEU64::new(1700000000),
        fixed_nsec: LEU32::new(0),
        blocks: LEU32::new(total_blocks),
        meta_block_addr: LEU32::new(1),
        xattr_block_addr: LEU32::new(0),
        uuid: [0; 16],
        volume_name: [0; 16],
        feature_incompat: LEU32::new(0),
        available_compr_algs: LEU16::new(0),
        extra_devices: LEU32::new(0),
        dirblkbits: 0,
        reserved: [0; 37],
    };
    let sb_offset = format::SUPERBLOCK_OFFSET as usize;
    image[sb_offset..sb_offset + std::mem::size_of::<format::SuperBlock>()]
        .copy_from_slice(sb.as_bytes());

    // 4. Write Inodes (and inline tails) + Data Blocks
    for i in 0..nodes.len() {
        let mode = match nodes[i].node_type {
            NodeType::Dir => 0o040000 | 0o755,     // S_IFDIR | rwxr-xr-x
            NodeType::File => 0o100000 | 0o644,    // S_IFREG | rw-r--r--
            NodeType::Symlink => 0o120000 | 0o777, // S_IFLNK | rwxrwxrwx
        };

        let link_count = if nodes[i].node_type == NodeType::Dir { 2 } else { 1 };
        let i_u = nodes[i].data_block.to_le_bytes();
        // Compact (0) | FlatPlain (0 << 1 = 0) or FlatInline (2 << 1 = 4)
        let inode_format = if nodes[i].is_inline { 4u16 } else { 0u16 };

        let inode = format::InodeCompact {
            format: LEU16::new(inode_format),
            xattr_icount: LEU16::new(0),
            mode: LEU16::new(mode),
            link_count: LEU16::new(link_count),
            size: LEU32::new(nodes[i].size as u32),
            reserved_1: [0; 4],
            i_u,
            ino: LEU32::new(nodes[i].nid as u32),
            uid: LEU16::new(100),
            gid: LEU16::new(200),
            reserved_2: [0; 4],
        };
        let inode_offset = nodes[i].inode_offset;
        image[inode_offset..inode_offset + INODE_SIZE].copy_from_slice(inode.as_bytes());

        match &nodes[i].contents {
            FlatNodeContents::Directory { blocks, .. } => {
                let offset = nodes[i].data_block as usize * BLOCK_SIZE;
                for (b_idx, block) in blocks.iter().enumerate() {
                    let block_offset = offset + b_idx * BLOCK_SIZE;
                    image[block_offset..block_offset + BLOCK_SIZE].copy_from_slice(block);
                }
            }
            FlatNodeContents::File { data } => {
                if nodes[i].is_inline {
                    let full_blocks_len = (data.len() / BLOCK_SIZE) * BLOCK_SIZE;
                    if full_blocks_len > 0 {
                        let offset = nodes[i].data_block as usize * BLOCK_SIZE;
                        image[offset..offset + full_blocks_len]
                            .copy_from_slice(&data[..full_blocks_len]);
                    }
                    let tail = &data[full_blocks_len..];
                    image[inode_offset + INODE_SIZE..inode_offset + INODE_SIZE + tail.len()]
                        .copy_from_slice(tail);
                } else if !data.is_empty() {
                    let offset = nodes[i].data_block as usize * BLOCK_SIZE;
                    image[offset..offset + data.len()].copy_from_slice(data);
                }
            }
            FlatNodeContents::Symlink { target } => {
                if !target.is_empty() {
                    let offset = nodes[i].data_block as usize * BLOCK_SIZE;
                    image[offset..offset + target.len()].copy_from_slice(target);
                }
            }
        }
    }

    image
}

#[cfg(test)]
mod tests {
    use super::*;
    use erofs::readers::VecReader;
    use erofs::{ErofsFilesystem, InodeDataLayout, Node};
    use std::sync::Arc;

    fn patterned_bytes(seed: usize, len: usize) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(31).wrapping_add(seed * 17)) as u8).collect()
    }

    fn assert_dir_recursive(
        fs: &ErofsFilesystem,
        expected_entries: &[SerializerNode],
        actual_dir: &erofs::DirectoryNode,
        expected_parent_nid: u64,
    ) {
        let mut actual_entries_buf = vec![erofs::DirectoryEntry::default(); 512];
        let filled = fs.read_directory(actual_dir, 0, &mut actual_entries_buf).unwrap();
        let mut actual_entries = actual_entries_buf[..filled].to_vec();

        // Verify '.' and '..' entries before filtering them out.
        let dot_entry =
            actual_entries.iter().find(|e| e.name == ".").expect("missing '.' directory entry");
        assert_eq!(dot_entry.nid, actual_dir.nid());
        assert_eq!(dot_entry.file_type, FileType::Dir);

        let dotdot_entry =
            actual_entries.iter().find(|e| e.name == "..").expect("missing '..' directory entry");
        assert_eq!(dotdot_entry.nid, expected_parent_nid);
        assert_eq!(dotdot_entry.file_type, FileType::Dir);

        actual_entries.retain(|e| e.name != "." && e.name != "..");

        assert_eq!(actual_entries.len(), expected_entries.len(), "Directory entry count mismatch");

        let mut sorted_expected: Vec<&SerializerNode> = expected_entries.iter().collect();
        sorted_expected.sort_by(|a, b| a.name().cmp(b.name()));

        for (i, expected_node) in sorted_expected.iter().enumerate() {
            let actual_entry = &actual_entries[i];
            assert_eq!(actual_entry.name, expected_node.name());

            let child_node = fs.node(actual_entry.nid).expect("failed to read child node");

            match expected_node {
                SerializerNode::Directory { entries, .. } => {
                    assert_eq!(actual_entry.file_type, FileType::Dir);
                    let actual_child_dir = match child_node {
                        Node::Directory(d) => d,
                        _ => panic!("Expected directory node for {}", expected_node.name()),
                    };
                    assert_dir_recursive(fs, entries, &actual_child_dir, actual_dir.nid());
                }
                SerializerNode::File { data, .. } => {
                    assert_eq!(actual_entry.file_type, FileType::RegFile);
                    let actual_child_file = match child_node {
                        Node::File(f) => f,
                        _ => panic!("Expected file node for {}", expected_node.name()),
                    };
                    assert_eq!(actual_child_file.size(), data.len() as u64);
                    let tail_len = data.len() % BLOCK_SIZE;
                    if tail_len > 0 && tail_len <= MAX_INLINE_TAIL_LEN {
                        assert_eq!(actual_child_file.data_layout(), InodeDataLayout::FlatInline);
                    } else {
                        assert_eq!(actual_child_file.data_layout(), InodeDataLayout::FlatPlain);
                    }
                    let mut file_buf = vec![0u8; data.len()];
                    fs.read_file_range(&actual_child_file, 0, &mut file_buf).unwrap();
                    assert_eq!(&file_buf, data);
                }
                SerializerNode::Symlink { target, .. } => {
                    assert_eq!(actual_entry.file_type, FileType::Symlink);
                    let actual_child_symlink = match child_node {
                        Node::Symlink(s) => s,
                        _ => panic!("Expected symlink node for {}", expected_node.name()),
                    };
                    assert_eq!(actual_child_symlink.size(), target.len() as u64);
                    let actual_target = fs.read_symlink(&actual_child_symlink).unwrap();
                    assert_eq!(&actual_target, target);
                }
            }
        }
    }

    #[fuchsia::test]
    fn test_serialize_and_parse() {
        let mut many_files = Vec::new();
        for i in 0..200 {
            let size = match i % 3 {
                0 => 512,
                1 => 4096,
                _ => 4608,
            };
            many_files.push(SerializerNode::File {
                name: format!("small_file_{i:03}"),
                data: patterned_bytes(i, size),
            });
        }
        many_files.push(SerializerNode::Symlink {
            name: "nested_symlink".to_string(),
            target: b"../../file1".to_vec(),
        });

        let tree = vec![
            SerializerNode::File { name: "file1".to_string(), data: b"hello world".to_vec() },
            SerializerNode::File {
                name: "exact_block".to_string(),
                data: patterned_bytes(301, 4096),
            },
            SerializerNode::File {
                name: "multi_block_inline".to_string(),
                data: patterned_bytes(302, 4608),
            },
            SerializerNode::Symlink { name: "symlink1".to_string(), target: b"file1".to_vec() },
            SerializerNode::Directory {
                name: "dir1".to_string(),
                entries: vec![
                    SerializerNode::File {
                        name: "file2".to_string(),
                        data: b"another file!".to_vec(),
                    },
                    SerializerNode::Directory { name: "subdir".to_string(), entries: many_files },
                ],
            },
        ];

        let image = serialize(&tree);
        let reader = Arc::new(VecReader::new(image));
        let fs = ErofsFilesystem::new(reader).expect("Failed to parse serialized EROFS image");

        let root = fs.root_node();
        assert_eq!(root.ino(), 0);

        assert_dir_recursive(&fs, &tree, &root, root.nid());
    }

    #[fuchsia::test]
    fn test_file_tail_boundaries() {
        // Test boundary sizes across empty, 32-byte slot boundaries, max inline tail (4064),
        // non-inlinable tail fallback to FlatPlain (4065..=4095), exact block (4096), and
        // multi-block variants.
        let sizes = [
            0,
            1,
            31,
            32,
            33,
            MAX_INLINE_TAIL_LEN - 1,
            MAX_INLINE_TAIL_LEN,
            MAX_INLINE_TAIL_LEN + 1,
            BLOCK_SIZE - 1,
            BLOCK_SIZE,
            BLOCK_SIZE + 1,
            BLOCK_SIZE + MAX_INLINE_TAIL_LEN,
            BLOCK_SIZE + MAX_INLINE_TAIL_LEN + 1,
            BLOCK_SIZE * 2 - 1,
            BLOCK_SIZE * 2,
            BLOCK_SIZE * 2 + 123,
        ];

        let tree: Vec<SerializerNode> = sizes
            .iter()
            .enumerate()
            .map(|(i, &size)| SerializerNode::File {
                name: format!("file_{size:05}"),
                data: patterned_bytes(i + 1, size),
            })
            .collect();

        let image = serialize(&tree);
        let reader = Arc::new(VecReader::new(image));
        let fs = ErofsFilesystem::new(reader).expect("Failed to parse serialized EROFS image");

        let root = fs.root_node();
        assert_dir_recursive(&fs, &tree, &root, root.nid());
    }

    #[fuchsia::test]
    fn test_empty_root_and_empty_subdir() {
        // 1. Empty root directory
        let empty_image = serialize(&[]);
        let empty_fs = ErofsFilesystem::new(Arc::new(VecReader::new(empty_image))).unwrap();
        let empty_root = empty_fs.root_node();
        assert_dir_recursive(&empty_fs, &[], &empty_root, empty_root.nid());

        // 2. Root directory with an empty subdirectory and an empty file
        let tree = vec![
            SerializerNode::Directory { name: "empty_dir".to_string(), entries: Vec::new() },
            SerializerNode::File { name: "empty_file".to_string(), data: Vec::new() },
        ];
        let image = serialize(&tree);
        let fs = ErofsFilesystem::new(Arc::new(VecReader::new(image))).unwrap();
        let root = fs.root_node();
        assert_dir_recursive(&fs, &tree, &root, root.nid());
    }

    #[fuchsia::test]
    fn test_directory_block_exact_boundary() {
        // Construct a directory where the first block's dirents (12B each) + names sum to
        // exactly 4096 bytes (no trailing zero padding in block 0), plus one additional entry
        // that spills into block 1:
        //   "."  -> 12 + 1 = 13 bytes
        //   ".." -> 12 + 2 = 14 bytes
        //   15 entries with 250-byte names -> 15 * (12 + 250) = 3930 bytes
        //   1 entry with 127-byte name -> 12 + 127 = 139 bytes
        //   Total for first 18 entries = 13 + 14 + 3930 + 139 = 4096 bytes.
        let mut entries = Vec::new();
        for i in 0..15 {
            let prefix = format!("entry_{i:02}_");
            let name = format!("{prefix}{}", "a".repeat(250 - prefix.len()));
            assert_eq!(name.len(), 250);
            entries.push(SerializerNode::File { name, data: patterned_bytes(i, 64) });
        }
        let prefix_15 = "entry_15_";
        let name_15 = format!("{prefix_15}{}", "b".repeat(127 - prefix_15.len()));
        assert_eq!(name_15.len(), 127);
        entries.push(SerializerNode::File { name: name_15, data: patterned_bytes(15, 64) });

        // 17th child entry spills into the second directory block.
        entries.push(SerializerNode::File {
            name: "entry_16_spill".to_string(),
            data: patterned_bytes(16, 64),
        });

        let image = serialize(&entries);
        let fs = ErofsFilesystem::new(Arc::new(VecReader::new(image))).unwrap();
        let root = fs.root_node();
        assert_eq!(root.size(), (2 * BLOCK_SIZE) as u64);
        assert_dir_recursive(&fs, &entries, &root, root.nid());
    }

    #[fuchsia::test]
    fn test_directory_blocks_precede_file_data_blocks() {
        let tree = vec![
            SerializerNode::Directory {
                name: "a_dir".to_string(),
                entries: vec![SerializerNode::File {
                    name: "first_file".to_string(),
                    data: patterned_bytes(1, BLOCK_SIZE),
                }],
            },
            SerializerNode::Directory {
                name: "z_dir".to_string(),
                entries: vec![SerializerNode::File {
                    name: "second_file".to_string(),
                    data: patterned_bytes(2, BLOCK_SIZE),
                }],
            },
        ];

        let image = serialize(&tree);
        let fs = ErofsFilesystem::new(Arc::new(VecReader::new(image))).unwrap();
        let root = fs.root_node();

        let mut root_entries = vec![erofs::DirectoryEntry::default(); 16];
        let count = fs.read_directory(&root, 0, &mut root_entries).unwrap();
        let find_child = |entries: &[erofs::DirectoryEntry], name: &str| {
            entries.iter().find(|e| e.name == name).unwrap().nid
        };

        let a_dir = match fs.node(find_child(&root_entries[..count], "a_dir")).unwrap() {
            Node::Directory(d) => d,
            _ => panic!("expected directory"),
        };
        let z_dir = match fs.node(find_child(&root_entries[..count], "z_dir")).unwrap() {
            Node::Directory(d) => d,
            _ => panic!("expected directory"),
        };

        let mut a_entries = vec![erofs::DirectoryEntry::default(); 16];
        let a_count = fs.read_directory(&a_dir, 0, &mut a_entries).unwrap();
        let first_file = match fs.node(find_child(&a_entries[..a_count], "first_file")).unwrap() {
            Node::File(f) => f,
            _ => panic!("expected file"),
        };

        assert!(
            z_dir.blkaddr_offset(BLOCK_SIZE as u64, 0).unwrap()
                < first_file.blkaddr_offset(BLOCK_SIZE as u64, 0).unwrap(),
            "all directory blocks must be placed before file data blocks"
        );
    }
}
