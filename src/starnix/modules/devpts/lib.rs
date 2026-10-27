// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "512"]

use starnix_core::device::DeviceMode;
use starnix_core::device::kobject::DeviceMetadata;
pub use starnix_core::device::terminal::DEVPTS_COUNT;
use starnix_core::device::terminal::{Terminal, TerminalSide, TtyState, get_device_type_for_pts};
use starnix_core::fs::sysfs::build_device_directory;
use starnix_core::mm::MemoryAccessorExt;
use starnix_core::task::{CurrentTask, EventHandler, Kernel, WaitCanceler, Waiter};
use starnix_core::vfs::buffers::{InputBuffer, OutputBuffer};
use starnix_core::vfs::pseudo::vec_directory::{VecDirectory, VecDirectoryEntry};
use starnix_core::vfs::{
    CacheMode, DirEntry, DirectoryEntryType, FdFlags, FileHandle, FileObject, FileObjectState,
    FileOps, FileSystem, FileSystemHandle, FileSystemOps, FileSystemOptions, FsNode, FsNodeHandle,
    FsNodeInfo, FsNodeOps, FsStr, FsString, LookupContext, MountInfo, NamespaceNode, SpecialNode,
    SymlinkMode,
};
use starnix_core::{fileops_impl_nonseekable, fileops_impl_noop_sync, fs_node_impl_dir_readonly};
use starnix_logging::track_stub;
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_types::vfs::default_statfs;
use starnix_uapi::auth::FsCred;
use starnix_uapi::device_id::{DeviceId, TTY_ALT_MAJOR};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::mode;
use starnix_uapi::mount_flags::MountFlags;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::signals::SIGWINCH;
use starnix_uapi::termios::{
    into_termio, into_termios2, termios_from_termios2, termios2_from_termios,
};
use starnix_uapi::user_address::{UserAddress, UserRef};
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{
    DEVPTS_SUPER_MAGIC, FIOASYNC, FIONREAD, FIOQSIZE, TCFLSH, TCGETA, TCGETS, TCGETS2, TCGETX,
    TCSBRK, TCSBRKP, TCSETA, TCSETAF, TCSETAW, TCSETS, TCSETS2, TCSETSF, TCSETSF2, TCSETSW,
    TCSETSW2, TCSETX, TCSETXF, TCSETXW, TCXONC, TIOCCBRK, TIOCCONS, TIOCEXCL, TIOCGETD,
    TIOCGICOUNT, TIOCGLCKTRMIOS, TIOCGPGRP, TIOCGPTLCK, TIOCGPTN, TIOCGPTPEER, TIOCGRS485,
    TIOCGSERIAL, TIOCGSID, TIOCGSOFTCAR, TIOCGWINSZ, TIOCLINUX, TIOCMBIC, TIOCMBIS, TIOCMGET,
    TIOCMIWAIT, TIOCMSET, TIOCNOTTY, TIOCNXCL, TIOCOUTQ, TIOCPKT, TIOCSBRK, TIOCSCTTY,
    TIOCSERCONFIG, TIOCSERGETLSR, TIOCSERGETMULTI, TIOCSERGSTRUCT, TIOCSERGWILD, TIOCSERSETMULTI,
    TIOCSERSWILD, TIOCSETD, TIOCSLCKTRMIOS, TIOCSPGRP, TIOCSPTLCK, TIOCSRS485, TIOCSSERIAL,
    TIOCSSOFTCAR, TIOCSTI, TIOCSWINSZ, TIOCVHANGUP, errno, error, gid_t, ino_t, pid_t, statfs,
    uapi, uid_t,
};
use std::sync::{Arc, Weak};

// See https://www.kernel.org/doc/Documentation/admin-guide/devices.txt
const DEVPTS_FIRST_MAJOR: u32 = 136;
const DEVPTS_MAJOR_COUNT: u32 = 4;
// The block size of the node in the devpts file system. Value has been taken from
// https://github.com/google/gvisor/blob/master/test/syscalls/linux/pty.cc
const BLOCK_SIZE: usize = 1024;

// The node identifier of the different node in the devpts filesystem.
const ROOT_NODE_ID: ino_t = 1;
const PTMX_NODE_ID: ino_t = 2;
const FIRST_PTS_NODE_ID: ino_t = 3;

pub fn dev_pts_fs(
    current_task: &CurrentTask,
    options: FileSystemOptions,
) -> Result<FileSystemHandle, Errno> {
    new_pts_fs(&current_task.kernel(), options)
}

pub fn new_pts_fs(kernel: &Kernel, options: FileSystemOptions) -> Result<FileSystemHandle, Errno> {
    let state = if options.params.get(b"newinstance").is_some() {
        Arc::new(TtyState::default())
    } else {
        kernel.expando.get::<TtyState>()
    };

    new_pts_fs_with_state(kernel, options, state)
}

pub fn new_pts_fs_with_state(
    kernel: &Kernel,
    options: FileSystemOptions,
    state: Arc<TtyState>,
) -> Result<FileSystemHandle, Errno> {
    let parse_octal = |m: &str| u32::from_str_radix(m, 8);
    let uid = options.params.get_as::<uid_t>(b"uid")?;
    let gid = options.params.get_as::<gid_t>(b"gid")?;
    let mode = options.params.get_with(b"mode", parse_octal)?.unwrap_or(0o600);
    let ptmxmode = options.params.get_with(b"ptmxmode", parse_octal)?.unwrap_or(0);

    let dev_pts_fs = DevPtsFs { state: state.clone(), uid, gid, mode, ptmxmode };

    let fs = FileSystem::new(kernel, CacheMode::Uncached, dev_pts_fs, options)
        .expect("devpts filesystem constructed with valid options");
    fs.create_root(ROOT_NODE_ID, DevPtsRootDir { state });
    Ok(fs)
}

/// Creates a terminal and returns the main pty and an associated replica pts.
///
/// This function assumes that `/dev/ptmx` is the `DevPtmxFile` and that devpts
/// is mounted at `/dev/pts`. These assumptions are necessary so that the
/// `FileHandle` objects returned have appropriate `NamespaceNode` objects.
pub fn create_main_and_replica(
    current_task: &CurrentTask,
    window_size: uapi::winsize,
) -> Result<(FileHandle, FileHandle), Errno> {
    let pty_file = current_task.open_file("/dev/ptmx".into(), OpenFlags::RDWR)?;
    let pty = pty_file.downcast_file::<DevPtmxFile>().ok_or_else(|| errno!(ENOTTY))?;
    {
        let mut terminal = pty.terminal.write();
        terminal.line_discipline.locked = false;
        terminal.line_discipline.window_size = window_size;
    }
    let pts_path = FsString::from(format!("/dev/pts/{}", pty.terminal.id));
    let pts_file = current_task.open_file(pts_path.as_ref(), OpenFlags::RDWR)?;
    Ok((pty_file, pts_file))
}

pub fn tty_device_init(kernel: &Kernel) -> Result<(), Errno> {
    let registry = &kernel.device_registry;

    // Register /dev/pts/X device type.
    for n in 0..DEVPTS_MAJOR_COUNT {
        registry
            .register_major(
                "pts".into(),
                DeviceMode::Char,
                DEVPTS_FIRST_MAJOR + n,
                open_dev_pts_device,
            )
            .expect("can register pts{n} device");
    }

    // Register tty and ptmx device types.
    kernel
        .device_registry
        .register_major("/dev/tty".into(), DeviceMode::Char, TTY_ALT_MAJOR, open_dev_pts_device)
        .expect("can register tty device");

    let tty_class = registry.objects.tty_class();
    registry.add_device(
        kernel,
        "tty".into(),
        DeviceMetadata::new("tty".into(), DeviceId::TTY, DeviceMode::Char),
        /* parent = */ None,
        tty_class.clone(),
        build_device_directory,
    );
    registry.add_device(
        kernel,
        "ptmx".into(),
        DeviceMetadata::new("ptmx".into(), DeviceId::PTMX, DeviceMode::Char),
        /* parent = */ None,
        tty_class,
        build_device_directory,
    );
    Ok(())
}

struct DevPtsFs {
    state: Arc<TtyState>,
    uid: Option<uid_t>,
    gid: Option<gid_t>,
    mode: u32,
    ptmxmode: u32,
}

impl FileSystemOps for DevPtsFs {
    fn statfs(&self, _fs: &FileSystem, _current_task: &CurrentTask) -> Result<statfs, Errno> {
        Ok(default_statfs(DEVPTS_SUPER_MAGIC))
    }
    fn name(&self) -> &'static FsStr {
        "devpts".into()
    }

    fn uses_external_node_ids(&self) -> bool {
        false
    }
}

impl DevPtsFs {
    fn pty_creds_for(&self, current_task: &CurrentTask) -> FsCred {
        let creds = current_task.current_creds();
        let uid = self.uid.unwrap_or_else(|| creds.uid);
        let gid = self.gid.unwrap_or_else(|| creds.gid);
        FsCred { uid, gid }
    }
}

struct DevPtsRootDir {
    state: Arc<TtyState>,
}

impl FsNodeOps for DevPtsRootDir {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let mut result = vec![];
        result.push(VecDirectoryEntry {
            entry_type: DirectoryEntryType::CHR,
            name: "ptmx".into(),
            inode: Some(PTMX_NODE_ID),
        });
        for (id, terminal) in self.state.terminals.read().iter() {
            if let Some(terminal) = terminal.upgrade() {
                if !terminal.read().is_main_closed() {
                    result.push(VecDirectoryEntry {
                        entry_type: DirectoryEntryType::CHR,
                        name: format!("{id}").into(),
                        inode: Some((*id as ino_t) + FIRST_PTS_NODE_ID),
                    });
                }
            }
        }
        Ok(VecDirectory::new_file(result))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        _current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        let fs = entry.node.fs();
        let devptsfs =
            fs.downcast_ops::<DevPtsFs>().expect("DevPts should only handle `DevPtsFs`s");
        let name = std::str::from_utf8(name).map_err(|_| errno!(ENOENT))?;
        if name == "ptmx" {
            let mut info = FsNodeInfo::new(mode!(IFCHR, devptsfs.ptmxmode), FsCred::root());
            info.rdev = DeviceId::PTMX;
            info.blksize = BLOCK_SIZE;
            let node = fs.create_node(PTMX_NODE_ID, SpecialNode, info);
            return Ok(node);
        }
        if let Ok(id) = name.parse::<u32>() {
            let terminal = self.state.terminals.read().get(&id).and_then(Weak::upgrade);
            if let Some(terminal) = terminal {
                if !terminal.read().is_main_closed() {
                    let ino = (id as ino_t) + FIRST_PTS_NODE_ID;
                    let mut info =
                        FsNodeInfo::new(mode!(IFCHR, devptsfs.mode), terminal.fscred.clone());
                    info.rdev = get_device_type_for_pts(id);
                    info.blksize = BLOCK_SIZE;
                    let node = fs.create_node(ino, SpecialNode, info);
                    return Ok(node);
                }
            }
        }
        error!(ENOENT)
    }
}

fn open_dev_pts_device(
    current_task: &CurrentTask,
    id: DeviceId,
    node: &NamespaceNode,
    flags: OpenFlags,
) -> Result<Box<dyn FileOps>, Errno> {
    match id {
        // /dev/ptmx and /dev/pts/ptmx
        DeviceId::PTMX => {
            let fs = node.entry.node.fs();
            let Some(devpts_fs) = fs.downcast_ops::<DevPtsFs>() else {
                // The device is not in ptmx, let try to find something at pts/ptmx relative to
                // the parent of the node
                let parent = node.parent().ok_or_else(|| errno!(EINVAL))?;
                let mut lookup_context = LookupContext::new(SymlinkMode::Follow);
                let ptmx_node =
                    current_task.lookup_path(&mut lookup_context, parent, "pts/ptmx".into())?;
                return open_dev_pts_device(current_task, id, &ptmx_node, flags);
            };

            let creds = devpts_fs.pty_creds_for(current_task);
            let terminal = devpts_fs.state.get_next_terminal(fs.root().clone(), creds)?;
            let name = FsString::from(terminal.id.to_string());
            let replica_dir_entry =
                fs.root().component_lookup(current_task, &MountInfo::detached(), name.as_ref())?;
            let replica_node =
                NamespaceNode { mount: node.mount.clone(), entry: replica_dir_entry };
            Ok(Box::new(DevPtmxFile::new(terminal, Some(replica_node))))
        }
        // /dev/tty
        DeviceId::TTY => {
            let controlling_terminal = current_task.thread_group().controlling_terminal();
            if let Some(controlling_terminal) = controlling_terminal {
                match controlling_terminal.side {
                    TerminalSide::Main => {
                        Ok(Box::new(DevPtmxFile::new(controlling_terminal.terminal, None)))
                    }
                    TerminalSide::Replica => {
                        Ok(Box::new(TtyFile::new(controlling_terminal.terminal)))
                    }
                }
            } else {
                error!(ENXIO)
            }
        }
        _ if id.major() < DEVPTS_FIRST_MAJOR
            || id.major() >= DEVPTS_FIRST_MAJOR + DEVPTS_MAJOR_COUNT =>
        {
            error!(ENODEV)
        }
        // /dev/pts/??
        _ => {
            let fs = node.entry.node.fs();
            let Some(devpts_fs) = fs.downcast_ops::<DevPtsFs>() else {
                return error!(ENOTSUP);
            };
            let pts_id = (id.major() - DEVPTS_FIRST_MAJOR) * 256 + id.minor();
            let terminal = devpts_fs
                .state
                .terminals
                .read()
                .get(&pts_id)
                .and_then(Weak::upgrade)
                .ok_or_else(|| errno!(EIO))?;
            if terminal.read().line_discipline.locked {
                return error!(EIO);
            }
            if !flags.contains(OpenFlags::NOCTTY) {
                // Opening a replica sets the process' controlling TTY when possible. An error indicates it cannot
                // be set, and is ignored silently.
                let _ = current_task.thread_group().set_controlling_terminal(
                    current_task,
                    &terminal,
                    TerminalSide::Replica,
                    false, /* steal */
                    flags.can_read(),
                );
            }
            Ok(Box::new(TtyFile::new(terminal)))
        }
    }
}

struct DevPtmxFile {
    terminal: Arc<Terminal>,

    /// The replica's NamespaceNode, used to implement `TIOCGPTPEER` when opened via `/dev/ptmx`.
    /// It is `None` if opened via `/dev/tty` redirecting to the main terminal. On Linux, `/dev/tty`
    /// always redirects to the replica PTY (even if `TIOCSCTTY` is called on the main PTY), where
    /// `TIOCGPTPEER` is unsupported.
    replica_node: Option<NamespaceNode>,
}

impl DevPtmxFile {
    pub fn new(terminal: Arc<Terminal>, replica_node: Option<NamespaceNode>) -> Self {
        terminal.main_open();
        Self { terminal, replica_node }
    }
}

impl FileOps for DevPtmxFile {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn close(self: Box<Self>, _file: &FileObjectState, _current_task: &CurrentTask) {
        self.terminal.disassociate_controlling_session();
        self.terminal.main_close();
    }

    fn read(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        file.blocking_op(current_task, FdEvents::POLLIN | FdEvents::POLLHUP, None, || {
            self.terminal.main_read(data)
        })
    }

    fn write(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        file.blocking_op(current_task, FdEvents::POLLOUT | FdEvents::POLLHUP, None, || {
            self.terminal.main_write(data)
        })
    }

    fn wait_async(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        Some(self.terminal.main_wait_async(waiter, events, handler))
    }

    fn query_events(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        Ok(self.terminal.main_query_events())
    }

    fn ioctl(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        let user_addr = UserAddress::from(arg);
        match request {
            TIOCGPTN => {
                // Get the terminal id.
                let value: u32 = self.terminal.id;
                current_task.write_object(UserRef::<u32>::new(user_addr), &value)?;
                Ok(SUCCESS)
            }
            TIOCGPTLCK => {
                // Get the lock status.
                let value = i32::from(self.terminal.read().line_discipline.locked);
                current_task.write_object(UserRef::<i32>::new(user_addr), &value)?;
                Ok(SUCCESS)
            }
            TIOCSPTLCK => {
                // Lock/Unlock the terminal.
                let value = current_task.read_object(UserRef::<i32>::new(user_addr))?;
                self.terminal.write().line_discipline.locked = value != 0;
                Ok(SUCCESS)
            }
            TIOCGPTPEER => {
                let Some(replica_node) = &self.replica_node else {
                    return error!(ENOTTY);
                };

                if replica_node.mount.flags().contains(MountFlags::NODEV) {
                    return error!(EACCES);
                }

                let flags = OpenFlags::from_bits_truncate(u32::from(arg));
                let replica_file = replica_node.open(current_task, flags)?;

                let fd_flags = if flags.contains(OpenFlags::CLOEXEC) {
                    FdFlags::CLOEXEC
                } else {
                    FdFlags::empty()
                };
                let fd = current_task.add_file(replica_file, fd_flags)?;
                Ok(fd.into())
            }
            _ => shared_ioctl(&self.terminal, true, file, current_task, request, arg),
        }
    }
}

pub struct TtyFile {
    terminal: Arc<Terminal>,
}

impl TtyFile {
    pub fn new(terminal: Arc<Terminal>) -> Self {
        terminal.replica_open();
        Self { terminal }
    }
}

impl FileOps for TtyFile {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn close(self: Box<Self>, _file: &FileObjectState, _current_task: &CurrentTask) {
        self.terminal.replica_close();
    }

    fn read(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        file.blocking_op(current_task, FdEvents::POLLIN | FdEvents::POLLHUP, None, || {
            self.terminal.replica_read(data)
        })
    }

    fn write(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        file.blocking_op(current_task, FdEvents::POLLOUT | FdEvents::POLLHUP, None, || {
            self.terminal.replica_write(data)
        })
    }

    fn wait_async(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        Some(self.terminal.replica_wait_async(waiter, events, handler))
    }

    fn query_events(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        Ok(self.terminal.replica_query_events())
    }

    fn ioctl(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        shared_ioctl(&self.terminal, false, file, current_task, request, arg)
    }
}

/// The ioctl behaviour common to main and replica terminal file descriptors.
fn shared_ioctl(
    terminal: &Terminal,
    is_main: bool,
    file: &FileObject,
    current_task: &CurrentTask,
    request: u32,
    arg: SyscallArg,
) -> Result<SyscallResult, Errno> {
    let user_addr = UserAddress::from(arg);
    match request {
        FIONREAD => {
            // Get the main terminal available bytes for reading.
            let value = terminal.read().get_available_read_size(is_main) as u32;
            current_task.write_object(UserRef::<u32>::new(user_addr), &value)?;
            Ok(SUCCESS)
        }
        TIOCSCTTY => {
            // Make the given terminal the controlling terminal of the calling process.
            let steal = bool::from(arg);
            current_task.thread_group().set_controlling_terminal(
                current_task,
                terminal,
                TerminalSide::from(is_main),
                steal,
                file.can_read(),
            )?;
            Ok(SUCCESS)
        }
        TIOCNOTTY => {
            // Release the controlling terminal.
            current_task.thread_group().release_controlling_terminal(
                current_task,
                terminal,
                TerminalSide::from(is_main),
            )?;
            Ok(SUCCESS)
        }
        TIOCGPGRP => {
            // Get the foreground process group.
            let pgid = current_task.thread_group().get_foreground_process_group(terminal)?;
            current_task.write_object(UserRef::<pid_t>::new(user_addr), &pgid)?;
            Ok(SUCCESS)
        }
        TIOCSPGRP => {
            // Set the foreground process group.
            let pgid = current_task.read_object(UserRef::<pid_t>::new(user_addr))?;
            if pgid < 0 {
                return error!(EINVAL);
            }
            let pgid = current_task.kernel().pids.get(pgid)?;
            current_task.thread_group().set_foreground_process_group(
                current_task,
                terminal,
                &pgid,
            )?;
            Ok(SUCCESS)
        }
        TIOCGWINSZ => {
            // Get the window size
            current_task.write_object(
                UserRef::<uapi::winsize>::new(user_addr),
                &terminal.read().line_discipline.window_size,
            )?;
            Ok(SUCCESS)
        }
        TIOCSWINSZ => {
            // If the window is already the required size, do nothing.
            let new_winsize = current_task.read_object(UserRef::<uapi::winsize>::new(user_addr))?;
            if terminal.read().line_discipline.window_size == new_winsize {
                return Ok(SUCCESS);
            }
            // Set the window size
            terminal.write().line_discipline.window_size = new_winsize;

            // Send a SIGWINCH signal to the foreground process group.
            terminal.send_signals(&[SIGWINCH]);
            Ok(SUCCESS)
        }
        TCGETA => {
            let termio = into_termio(terminal.read().termios());
            current_task.write_object(UserRef::<uapi::termio>::new(user_addr), &termio)?;
            Ok(SUCCESS)
        }
        TCGETS => {
            // N.B. TCGETS on the main terminal actually returns the configuration of the replica
            // end.
            let termios = termios_from_termios2(terminal.read().termios());
            current_task.write_object(UserRef::<uapi::termios>::new(user_addr), &termios)?;
            Ok(SUCCESS)
        }
        TCGETS2 => {
            current_task.write_object(
                UserRef::<uapi::termios2>::new(user_addr),
                terminal.read().termios(),
            )?;
            Ok(SUCCESS)
        }
        TCSETA => {
            let termio = current_task.read_object(UserRef::<uapi::termio>::new(user_addr))?;
            terminal.set_termios(into_termios2(termio));
            Ok(SUCCESS)
        }
        TCSETS => {
            // N.B. TCSETS on the main terminal actually affects the configuration of the replica
            // end.
            let termios = current_task.read_object(UserRef::<uapi::termios>::new(user_addr))?;
            terminal.set_termios(termios2_from_termios(&termios));
            Ok(SUCCESS)
        }
        TCSETS2 => {
            let termios2 = current_task.read_object(UserRef::<uapi::termios2>::new(user_addr))?;
            terminal.set_termios(termios2);
            Ok(SUCCESS)
        }
        TCSETAF => {
            // This should drain the output queue and discard the pending input first.
            let termio = current_task.read_object(UserRef::<uapi::termio>::new(user_addr))?;
            terminal.set_termios(into_termios2(termio));
            Ok(SUCCESS)
        }
        TCSETSF => {
            // This should drain the output queue and discard the pending input first.
            let termios = current_task.read_object(UserRef::<uapi::termios>::new(user_addr))?;
            terminal.set_termios(termios2_from_termios(&termios));
            Ok(SUCCESS)
        }
        TCSETSF2 => {
            // This should drain the output queue and discard the pending input first.
            let termios2 = current_task.read_object(UserRef::<uapi::termios2>::new(user_addr))?;
            terminal.set_termios(termios2);
            Ok(SUCCESS)
        }
        TCSETAW => {
            track_stub!(TODO("https://fxbug.dev/322873281"), "TCSETAW drain output queue first");
            let termio = current_task.read_object(UserRef::<uapi::termio>::new(user_addr))?;
            terminal.set_termios(into_termios2(termio));
            Ok(SUCCESS)
        }
        TCSETSW => {
            track_stub!(TODO("https://fxbug.dev/322873281"), "TCSETSW drain output queue first");
            let termios = current_task.read_object(UserRef::<uapi::termios>::new(user_addr))?;
            terminal.set_termios(termios2_from_termios(&termios));
            Ok(SUCCESS)
        }
        TCSETSW2 => {
            track_stub!(TODO("https://fxbug.dev/322873281"), "TCSETSW2 drain output queue first");
            let termios2 = current_task.read_object(UserRef::<uapi::termios2>::new(user_addr))?;
            terminal.set_termios(termios2);
            Ok(SUCCESS)
        }
        TIOCSETD => {
            track_stub!(
                TODO("https://fxbug.dev/322874060"),
                "devpts setting line discipline",
                is_main
            );
            error!(EINVAL)
        }
        TCSBRK => Ok(SUCCESS),
        TCXONC => {
            track_stub!(TODO("https://fxbug.dev/322892912"), "devpts ioctl TCXONC", is_main);
            error!(ENOSYS)
        }
        TCFLSH => {
            terminal.flush(is_main, u32::from(arg))?;
            Ok(SUCCESS)
        }
        TIOCEXCL => {
            track_stub!(TODO("https://fxbug.dev/322893449"), "devpts ioctl TIOCEXCL", is_main);
            error!(ENOSYS)
        }
        TIOCNXCL => {
            track_stub!(TODO("https://fxbug.dev/322893393"), "devpts ioctl TIOCNXCL", is_main);
            error!(ENOSYS)
        }
        TIOCOUTQ => {
            track_stub!(TODO("https://fxbug.dev/322893723"), "devpts ioctl TIOCOUTQ", is_main);
            error!(ENOSYS)
        }
        TIOCSTI => {
            track_stub!(TODO("https://fxbug.dev/322893780"), "devpts ioctl TIOCSTI", is_main);
            error!(ENOSYS)
        }
        TIOCMGET => {
            track_stub!(TODO("https://fxbug.dev/322893681"), "devpts ioctl TIOCMGET", is_main);
            error!(ENOSYS)
        }
        TIOCMBIS => {
            track_stub!(TODO("https://fxbug.dev/322893709"), "devpts ioctl TIOCMBIS", is_main);
            error!(ENOSYS)
        }
        TIOCMBIC => {
            track_stub!(TODO("https://fxbug.dev/322893610"), "devpts ioctl TIOCMBIC", is_main);
            error!(ENOSYS)
        }
        TIOCMSET => {
            track_stub!(TODO("https://fxbug.dev/322893211"), "devpts ioctl TIOCMSET", is_main);
            error!(ENOSYS)
        }
        TIOCGSOFTCAR => {
            track_stub!(TODO("https://fxbug.dev/322893365"), "devpts ioctl TIOCGSOFTCAR", is_main);
            error!(ENOSYS)
        }
        TIOCSSOFTCAR => {
            track_stub!(TODO("https://fxbug.dev/322894074"), "devpts ioctl TIOCSSOFTCAR", is_main);
            error!(ENOSYS)
        }
        TIOCLINUX => {
            track_stub!(TODO("https://fxbug.dev/322893147"), "devpts ioctl TIOCLINUX", is_main);
            error!(ENOSYS)
        }
        TIOCCONS => {
            track_stub!(TODO("https://fxbug.dev/322893267"), "devpts ioctl TIOCCONS", is_main);
            error!(ENOSYS)
        }
        TIOCGSERIAL => {
            track_stub!(TODO("https://fxbug.dev/322893503"), "devpts ioctl TIOCGSERIAL", is_main);
            error!(ENOSYS)
        }
        TIOCSSERIAL => {
            track_stub!(TODO("https://fxbug.dev/322893663"), "devpts ioctl TIOCSSERIAL", is_main);
            error!(ENOSYS)
        }
        TIOCPKT => {
            if !is_main {
                return error!(ENOTTY);
            }
            let value = current_task.read_object(UserRef::<i32>::new(user_addr))?;
            terminal.write().set_packet_mode(value != 0);
            Ok(SUCCESS)
        }
        TIOCGETD => {
            track_stub!(TODO("https://fxbug.dev/322893974"), "devpts ioctl TIOCGETD", is_main);
            error!(ENOSYS)
        }
        TCSBRKP => Ok(SUCCESS),
        TIOCSBRK => {
            track_stub!(TODO("https://fxbug.dev/322893936"), "devpts ioctl TIOCSBRK", is_main);
            error!(ENOSYS)
        }
        TIOCCBRK => {
            track_stub!(TODO("https://fxbug.dev/322893213"), "devpts ioctl TIOCCBRK", is_main);
            error!(ENOSYS)
        }
        TIOCGSID => {
            if !is_main
                && !current_task
                    .thread_group()
                    .controlling_terminal()
                    .as_ref()
                    .is_some_and(|ct| std::ptr::eq(terminal, Arc::as_ptr(&ct.terminal)))
            {
                return error!(ENOTTY);
            }
            let sid = terminal
                .read()
                .controlling_session()
                .map(|sid| sid.id)
                .ok_or_else(|| errno!(ENOTTY))?;
            current_task.write_object(UserRef::<pid_t>::new(user_addr), &sid)?;
            Ok(SUCCESS)
        }
        TIOCGRS485 => {
            track_stub!(TODO("https://fxbug.dev/322893728"), "devpts ioctl TIOCGRS485", is_main);
            error!(ENOSYS)
        }
        TIOCSRS485 => {
            track_stub!(TODO("https://fxbug.dev/322893783"), "devpts ioctl TIOCSRS485", is_main);
            error!(ENOSYS)
        }
        TCGETX => {
            track_stub!(TODO("https://fxbug.dev/322893327"), "devpts ioctl TCGETX", is_main);
            error!(ENOSYS)
        }
        TCSETX => {
            track_stub!(TODO("https://fxbug.dev/322893741"), "devpts ioctl TCSETX", is_main);
            error!(ENOSYS)
        }
        TCSETXF => {
            track_stub!(TODO("https://fxbug.dev/322893937"), "devpts ioctl TCSETXF", is_main);
            error!(ENOSYS)
        }
        TCSETXW => {
            track_stub!(TODO("https://fxbug.dev/322893899"), "devpts ioctl TCSETXW", is_main);
            error!(ENOSYS)
        }
        TIOCVHANGUP => {
            track_stub!(TODO("https://fxbug.dev/322893742"), "devpts ioctl TIOCVHANGUP", is_main);
            error!(ENOSYS)
        }
        FIOASYNC => {
            track_stub!(TODO("https://fxbug.dev/322893269"), "devpts ioctl FIOASYNC", is_main);
            error!(ENOSYS)
        }
        TIOCSERCONFIG => {
            track_stub!(TODO("https://fxbug.dev/322893881"), "devpts ioctl TIOCSERCONFIG", is_main);
            error!(ENOSYS)
        }
        TIOCSERGWILD => {
            track_stub!(TODO("https://fxbug.dev/322893686"), "devpts ioctl TIOCSERGWILD", is_main);
            error!(ENOSYS)
        }
        TIOCSERSWILD => {
            track_stub!(TODO("https://fxbug.dev/322893837"), "devpts ioctl TIOCSERSWILD", is_main);
            error!(ENOSYS)
        }
        TIOCGLCKTRMIOS => {
            track_stub!(
                TODO("https://fxbug.dev/322894114"),
                "devpts ioctl TIOCGLCKTRMIOS",
                is_main
            );
            error!(ENOSYS)
        }
        TIOCSLCKTRMIOS => {
            track_stub!(
                TODO("https://fxbug.dev/322893711"),
                "devpts ioctl TIOCSLCKTRMIOS",
                is_main
            );
            error!(ENOSYS)
        }
        TIOCSERGSTRUCT => {
            track_stub!(
                TODO("https://fxbug.dev/322893828"),
                "devpts ioctl TIOCSERGSTRUCT",
                is_main
            );
            error!(ENOSYS)
        }
        TIOCSERGETLSR => {
            track_stub!(TODO("https://fxbug.dev/322894083"), "devpts ioctl TIOCSERGETLSR", is_main);
            error!(ENOSYS)
        }
        TIOCSERGETMULTI => {
            track_stub!(
                TODO("https://fxbug.dev/322893962"),
                "devpts ioctl TIOCSERGETMULTI",
                is_main
            );
            error!(ENOSYS)
        }
        TIOCSERSETMULTI => {
            track_stub!(
                TODO("https://fxbug.dev/322893273"),
                "devpts ioctl TIOCSERSETMULTI",
                is_main
            );
            error!(ENOSYS)
        }
        TIOCMIWAIT => {
            track_stub!(TODO("https://fxbug.dev/322894005"), "devpts ioctl TIOCMIWAIT", is_main);
            error!(ENOSYS)
        }
        TIOCGICOUNT => {
            track_stub!(TODO("https://fxbug.dev/322893862"), "devpts ioctl TIOCGICOUNT", is_main);
            error!(ENOSYS)
        }
        FIOQSIZE => {
            track_stub!(TODO("https://fxbug.dev/322893770"), "devpts ioctl FIOQSIZE", is_main);
            error!(ENOSYS)
        }
        other => {
            track_stub!(TODO("https://fxbug.dev/322893712"), "devpts unknown ioctl", other);
            error!(ENOTTY)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tty_device_init;
    use starnix_core::fs::tmpfs::TmpFs;
    use starnix_core::task::{ExitStatus, KernelFeatures};
    use starnix_core::testing::*;
    use starnix_core::vfs::buffers::{VecInputBuffer, VecOutputBuffer};
    use starnix_core::vfs::fs_args::MountParams;
    use starnix_core::vfs::{MountInfo, NamespaceNode, OpenAccessCheck};
    use starnix_types::ownership::Releasable;
    use starnix_uapi::auth::Credentials;
    use starnix_uapi::file_mode::FileMode;
    use starnix_uapi::signals::{
        SIGCHLD, SIGCONT, SIGHUP, SIGINT, SIGQUIT, SIGTSTP, SIGTTOU, SigSet,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    fn new_pts_fs(kernel: &Kernel) -> FileSystemHandle {
        let mut options = FileSystemOptions::default();
        options.params = MountParams::parse("ptmxmode=666".into()).expect("parse option");
        super::new_pts_fs(&kernel, options).expect("create new_pts_fs")
    }

    fn ioctl<T: zerocopy::IntoBytes + zerocopy::FromBytes + zerocopy::Immutable + Copy>(
        current_task: &CurrentTask,
        file: &FileHandle,
        command: u32,
        value: &T,
    ) -> Result<T, Errno> {
        let address =
            map_memory(current_task, UserAddress::default(), std::mem::size_of::<T>() as u64);
        let address_ref = UserRef::<T>::new(address);
        current_task.write_object(address_ref, value)?;
        file.ioctl(current_task, command, address.into())?;
        current_task.read_object(address_ref)
    }

    fn set_controlling_terminal(
        current_task: &CurrentTask,
        file: &FileHandle,
        steal: bool,
    ) -> Result<SyscallResult, Errno> {
        #[allow(clippy::bool_to_int_with_if)]
        file.ioctl(current_task, TIOCSCTTY, steal.into())
    }

    fn lookup_node(
        task: &CurrentTask,
        fs: &FileSystemHandle,
        name: &FsStr,
    ) -> Result<NamespaceNode, Errno> {
        let root = NamespaceNode::new_anonymous(fs.root().clone());
        root.lookup_child(task, &mut Default::default(), name)
    }

    fn open_file_with_flags(
        current_task: &CurrentTask,
        fs: &FileSystemHandle,
        name: &FsStr,
        flags: OpenFlags,
    ) -> Result<FileHandle, Errno> {
        let node = lookup_node(current_task, fs, name)?;
        node.open(current_task, flags)
    }

    fn open_file(
        current_task: &CurrentTask,
        fs: &FileSystemHandle,
        name: &FsStr,
    ) -> Result<FileHandle, Errno> {
        open_file_with_flags(current_task, fs, name, OpenFlags::RDWR | OpenFlags::NOCTTY)
    }

    fn open_ptmx_and_unlock(
        current_task: &CurrentTask,
        fs: &FileSystemHandle,
    ) -> Result<FileHandle, Errno> {
        let file = open_file_with_flags(current_task, fs, "ptmx".into(), OpenFlags::RDWR)?;

        // Unlock terminal
        ioctl::<i32>(current_task, &file, TIOCSPTLCK, &0)?;

        Ok(file)
    }

    #[fuchsia::test]
    async fn opening_ptmx_creates_pts() {
        spawn_kernel_and_run(async |task| {
            let kernel = task.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            let fs = new_pts_fs(kernel);
            lookup_node(task, &fs, "0".into()).unwrap_err();
            let _ptmx = open_ptmx_and_unlock(task, &fs).expect("ptmx");
            lookup_node(task, &fs, "0".into()).expect("pty");
        })
        .await;
    }

    #[fuchsia::test]
    async fn closing_ptmx_closes_pts() {
        spawn_kernel_and_run(async |task| {
            let kernel = task.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            let fs = new_pts_fs(kernel);
            lookup_node(task, &fs, "0".into()).unwrap_err();
            let ptmx = open_ptmx_and_unlock(task, &fs).expect("ptmx");
            let _pts = open_file(task, &fs, "0".into()).expect("open file");
            std::mem::drop(ptmx);
            task.trigger_delayed_releaser();
            lookup_node(task, &fs, "0".into()).unwrap_err();
        })
        .await;
    }

    #[fuchsia::test]
    async fn pts_are_reused() {
        spawn_kernel_and_run(async |task| {
            let kernel = task.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            let fs = new_pts_fs(kernel);

            let _ptmx0 = open_ptmx_and_unlock(task, &fs).expect("ptmx");
            let mut _ptmx1 = open_ptmx_and_unlock(task, &fs).expect("ptmx");
            let _ptmx2 = open_ptmx_and_unlock(task, &fs).expect("ptmx");

            lookup_node(task, &fs, "0".into()).expect("component_lookup");
            lookup_node(task, &fs, "1".into()).expect("component_lookup");
            lookup_node(task, &fs, "2".into()).expect("component_lookup");

            std::mem::drop(_ptmx1);
            task.trigger_delayed_releaser();

            lookup_node(task, &fs, "1".into()).unwrap_err();

            _ptmx1 = open_ptmx_and_unlock(task, &fs).expect("ptmx");
            lookup_node(task, &fs, "1".into()).expect("component_lookup");
        })
        .await;
    }

    #[fuchsia::test]
    async fn opening_inexistant_replica_fails() {
        spawn_kernel_and_run(async |task| {
            let kernel = task.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            // Initialize pts devices
            new_pts_fs(kernel);
            let fs = TmpFs::new_fs(kernel);
            let mount = MountInfo::detached();
            let pts = fs
                .root()
                .create_entry(task, &mount, "custom_pts".into(), |dir, mount, name| {
                    dir.create_node(
                        task,
                        mount,
                        name,
                        mode!(IFCHR, 0o666),
                        DeviceId::new(DEVPTS_FIRST_MAJOR, 0),
                        FsCred::root(),
                    )
                })
                .expect("custom_pts");
            let node = NamespaceNode::new_anonymous(pts.clone());
            assert!(node.open(task, OpenAccessCheck::skip(OpenFlags::RDONLY)).is_err());
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_open_tty() {
        spawn_kernel_with_features_and_run(
            async |task| {
                let kernel = task.kernel();
                tty_device_init(kernel).expect("tty_device_init");
                let fs = new_pts_fs(kernel);
                let devfs = starnix_core::fs::devtmpfs::DevTmpFs::from_kernel(kernel);

                let ptmx = open_ptmx_and_unlock(task, &fs).expect("ptmx");
                set_controlling_terminal(task, &ptmx, false).expect("set_controlling_terminal");
                let tty =
                    open_file_with_flags(task, &devfs, "tty".into(), OpenFlags::RDWR).expect("tty");
                // Check that tty is the main terminal by calling the ioctl TIOCGPTN and checking it is
                // has the same result as on ptmx.
                assert_eq!(
                    ioctl::<i32>(task, &tty, TIOCGPTN, &0),
                    ioctl::<i32>(task, &ptmx, TIOCGPTN, &0)
                );

                // Detach the controlling terminal.
                ioctl::<i32>(task, &ptmx, TIOCNOTTY, &0).expect("detach terminal");
                let pts = open_file(task, &fs, "0".into()).expect("open file");
                set_controlling_terminal(task, &pts, false).expect("set_controlling_terminal");
                let tty =
                    open_file_with_flags(task, &devfs, "tty".into(), OpenFlags::RDWR).expect("tty");
                // TIOCGPTN is not implemented on replica terminals
                assert!(ioctl::<i32>(task, &tty, TIOCGPTN, &0).is_err());
            },
            KernelFeatures { devtmpfs: true, ..Default::default() },
        )
        .await;
    }

    #[fuchsia::test]
    async fn test_unknown_ioctl() {
        spawn_kernel_and_run(async |task| {
            let kernel = task.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            let fs = new_pts_fs(kernel);

            let ptmx = open_ptmx_and_unlock(task, &fs).expect("ptmx");
            assert_eq!(ptmx.ioctl(task, 42, Default::default()), error!(ENOTTY));

            let pts_file = open_file(task, &fs, "0".into()).expect("open file");
            assert_eq!(pts_file.ioctl(task, 42, Default::default()), error!(ENOTTY));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_tiocgptn_ioctl() {
        spawn_kernel_and_run(async |task| {
            let kernel = task.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            let fs = new_pts_fs(kernel);
            let ptmx0 = open_ptmx_and_unlock(task, &fs).expect("ptmx");
            let ptmx1 = open_ptmx_and_unlock(task, &fs).expect("ptmx");

            let pts0 = ioctl::<u32>(task, &ptmx0, TIOCGPTN, &0).expect("ioctl");
            assert_eq!(pts0, 0);

            let pts1 = ioctl::<u32>(task, &ptmx1, TIOCGPTN, &0).expect("ioctl");
            assert_eq!(pts1, 1);
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_new_terminal_is_locked() {
        spawn_kernel_and_run(async |task| {
            let kernel = task.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            let fs = new_pts_fs(kernel);
            let _ptmx_file = open_file(task, &fs, "ptmx".into()).expect("open file");

            let pts = lookup_node(task, &fs, "0".into()).expect("component_lookup");
            assert_eq!(pts.open(task, OpenFlags::RDONLY).map(|_| ()), error!(EIO));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_lock_ioctls() {
        spawn_kernel_and_run(async |task| {
            let kernel = task.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            let fs = new_pts_fs(kernel);
            let ptmx = open_ptmx_and_unlock(task, &fs).expect("ptmx");
            let pts = lookup_node(task, &fs, "0".into()).expect("component_lookup");

            // Check that the lock is not set.
            assert_eq!(ioctl::<i32>(task, &ptmx, TIOCGPTLCK, &0), Ok(0));
            // /dev/pts/0 can be opened
            pts.open(task, OpenFlags::RDONLY).expect("open");

            // Lock the terminal
            ioctl::<i32>(task, &ptmx, TIOCSPTLCK, &42).expect("ioctl");
            // Check that the lock is set.
            assert_eq!(ioctl::<i32>(task, &ptmx, TIOCGPTLCK, &0), Ok(1));
            // /dev/pts/0 cannot be opened
            assert_eq!(pts.open(task, OpenFlags::RDONLY).map(|_| ()), error!(EIO));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_ptmx_stats() {
        spawn_kernel_and_run(async |task| {
            let kernel = task.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            task.set_creds(Credentials::with_ids(22, 22));
            let fs = new_pts_fs(kernel);
            let ptmx = open_ptmx_and_unlock(task, &fs).expect("ptmx");
            let ptmx_stat = ptmx.node().stat(task).expect("stat");
            assert_eq!(ptmx_stat.st_blksize as usize, BLOCK_SIZE);
            let pts = open_file(task, &fs, "0".into()).expect("open file");
            let pts_stats = pts.node().stat(task).expect("stat");
            assert_eq!(pts_stats.st_mode & FileMode::PERMISSIONS.bits(), 0o600);
            assert_eq!(pts_stats.st_uid, 22);
            // TODO(qsr): Check that gid is tty.
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_attach_terminal_when_open() {
        spawn_kernel_and_run(async |task| {
            let kernel = task.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            let fs = new_pts_fs(kernel);
            let _opened_main = open_ptmx_and_unlock(task, &fs).expect("ptmx");
            // Opening the main terminal should not set the terminal of the session.
            assert!(task.thread_group().controlling_terminal().is_none());
            // Opening the terminal should not set the terminal of the session with the NOCTTY flag.
            let _opened_replica2 =
                open_file_with_flags(task, &fs, "0".into(), OpenFlags::RDWR | OpenFlags::NOCTTY)
                    .expect("open file");
            assert!(task.thread_group().controlling_terminal().is_none());

            // Opening the replica terminal should set the terminal of the session.
            let _opened_replica2 =
                open_file_with_flags(task, &fs, "0".into(), OpenFlags::RDWR).expect("open file");
            assert!(task.thread_group().controlling_terminal().is_some());
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_attach_terminal() {
        spawn_kernel_and_run(async |task1| {
            let kernel = task1.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            let task2 = task1.clone_task_for_test(0, Some(SIGCHLD));
            task2.thread_group().setsid().expect("setsid");

            let fs = new_pts_fs(kernel);
            let opened_main = open_ptmx_and_unlock(task1, &fs).expect("ptmx");
            let opened_replica = open_file(&task2, &fs, "0".into()).expect("open file");

            assert_eq!(ioctl::<i32>(task1, &opened_main, TIOCGPGRP, &0), error!(ENOTTY));
            assert_eq!(ioctl::<i32>(&task2, &opened_replica, TIOCGPGRP, &0), error!(ENOTTY));

            set_controlling_terminal(task1, &opened_main, false).unwrap();
            assert_eq!(
                ioctl::<i32>(task1, &opened_main, TIOCGPGRP, &0),
                Ok(task1.thread_group().pgid().id)
            );
            assert_eq!(ioctl::<i32>(&task2, &opened_replica, TIOCGPGRP, &0), error!(ENOTTY));

            // Cannot steal terminal using the replica.
            assert_eq!(set_controlling_terminal(&task2, &opened_replica, false), error!(EPERM));
            assert_eq!(ioctl::<i32>(&task2, &opened_replica, TIOCGPGRP, &0), error!(ENOTTY));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_steal_terminal() {
        spawn_kernel_and_run(async |task1| {
            let kernel = task1.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            task1.set_creds(Credentials::with_ids(1, 1));

            let task2 = task1.clone_task_for_test(0, Some(SIGCHLD));

            let fs = new_pts_fs(kernel);
            let _opened_main = open_ptmx_and_unlock(task1, &fs).expect("ptmx");
            let wo_opened_replica =
                open_file_with_flags(task1, &fs, "0".into(), OpenFlags::WRONLY | OpenFlags::NOCTTY)
                    .expect("open file");
            assert!(!wo_opened_replica.can_read());

            // FD must be readable for setting the terminal.
            assert_eq!(set_controlling_terminal(task1, &wo_opened_replica, false), error!(EPERM));

            let opened_replica = open_file(&task2, &fs, "0".into()).expect("open file");
            // Task must be session leader for setting the terminal.
            assert_eq!(set_controlling_terminal(&task2, &opened_replica, false), error!(EPERM));

            // Associate terminal to task1.
            set_controlling_terminal(task1, &opened_replica, false)
                .expect("Associate terminal to task1");

            // Redundant association of the same terminal should succeed (no-op)
            set_controlling_terminal(task1, &opened_replica, false)
                .expect("Redundant association should succeed");

            task2.thread_group().setsid().expect("setsid");

            // One cannot associate a terminal that is already associated with another process.
            assert_eq!(set_controlling_terminal(&task2, &opened_replica, false), error!(EPERM));

            // One cannot steal a terminal without the CAP_SYS_ADMIN capacility
            assert_eq!(set_controlling_terminal(&task2, &opened_replica, true), error!(EPERM));

            // One can steal a terminal with the CAP_SYS_ADMIN capacility
            task2.set_creds(Credentials::with_ids(0, 0));
            // But not without specifying that one wants to steal it.
            assert_eq!(set_controlling_terminal(&task2, &opened_replica, false), error!(EPERM));
            set_controlling_terminal(&task2, &opened_replica, true)
                .expect("Associate terminal to task2");

            assert!(task1.thread_group().controlling_terminal().is_none());

            // If task2 already owns a controlling terminal, attempting to steal a second
            // terminal from task1 must fail with EPERM without disassociating task1's terminal.
            let _opened_main2 = open_ptmx_and_unlock(task1, &fs).expect("ptmx 1");
            let opened_replica2 = open_file(task1, &fs, "1".into()).expect("open file 1");
            set_controlling_terminal(task1, &opened_replica2, false)
                .expect("Associate second terminal to task1");
            assert!(task1.thread_group().controlling_terminal().is_some());
            assert_eq!(set_controlling_terminal(&task2, &opened_replica2, true), error!(EPERM));
            assert!(task1.thread_group().controlling_terminal().is_some());
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_tiocgsid() {
        spawn_kernel_and_run(async |task1| {
            let kernel = task1.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            let mut task2 = task1.clone_task_for_test(0, Some(SIGCHLD));
            task2.set_mm_for_test(task1.mm().unwrap().clone());
            task2.thread_group().setsid().expect("setsid");

            let fs = new_pts_fs(kernel);
            let opened_main = open_ptmx_and_unlock(task1, &fs).expect("ptmx");
            let opened_replica = open_file(&task2, &fs, "0".into()).expect("open file");

            // Unattached terminal returns ENOTTY on both main and replica.
            assert_eq!(ioctl::<pid_t>(task1, &opened_main, TIOCGSID, &0), error!(ENOTTY));
            assert_eq!(ioctl::<pid_t>(&task2, &opened_replica, TIOCGSID, &0), error!(ENOTTY));

            // Attach controlling session to task2.
            set_controlling_terminal(&task2, &opened_replica, false)
                .expect("set controlling terminal");
            let expected_sid = task2.thread_group().read().session.id;

            // Calling TIOCGSID on both main and replica from the controlling session returns SID.
            assert_eq!(ioctl::<pid_t>(&task2, &opened_main, TIOCGSID, &0), Ok(expected_sid));
            assert_eq!(ioctl::<pid_t>(&task2, &opened_replica, TIOCGSID, &0), Ok(expected_sid));

            // From a task in a different session, TIOCGSID on the replica returns ENOTTY,
            // whereas TIOCGSID on the main terminal succeeds and returns the controlling SID.
            assert_eq!(ioctl::<pid_t>(task1, &opened_replica, TIOCGSID, &0), error!(ENOTTY));
            assert_eq!(ioctl::<pid_t>(task1, &opened_main, TIOCGSID, &0), Ok(expected_sid));

            // After detaching (TIOCNOTTY), TIOCGSID returns ENOTTY.
            ioctl::<i32>(&task2, &opened_replica, TIOCNOTTY, &0).expect("detach terminal");
            assert_eq!(ioctl::<pid_t>(&task2, &opened_main, TIOCGSID, &0), error!(ENOTTY));
            assert_eq!(ioctl::<pid_t>(&task2, &opened_replica, TIOCGSID, &0), error!(ENOTTY));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_set_foreground_process() {
        spawn_kernel_and_run(async |init| {
            let kernel = init.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            let mut task1 = init.clone_task_for_test(0, Some(SIGCHLD));
            // Share the init task's address space so ioctl usercopy operations can
            // access memory while running on this test thread.
            task1.set_mm_for_test(init.mm().unwrap().clone());
            task1.thread_group().setsid().expect("setsid");
            let pre_existing_task = task1.clone_task_for_test(0, Some(SIGCHLD));

            let fs = new_pts_fs(kernel);
            let _opened_main = open_ptmx_and_unlock(init, &fs).expect("ptmx");
            let opened_replica = open_file(&pre_existing_task, &fs, "0".into()).expect("open file");

            // Cannot change the foreground process group if the terminal is not the controlling
            // terminal.
            assert_eq!(
                ioctl::<i32>(&task1, &opened_replica, TIOCSPGRP, &task1.pid.id),
                error!(ENOTTY)
            );

            // Attach terminal to task1; pre-existing session members do not retroactively gain
            // the controlling terminal, whereas newly cloned children inherit it.
            set_controlling_terminal(&task1, &opened_replica, false).unwrap();
            assert!(pre_existing_task.thread_group().controlling_terminal().is_none());

            let mut task2 = task1.clone_task_for_test(0, Some(SIGCHLD));
            task2.set_mm_for_test(init.mm().unwrap().clone());
            task2.thread_group().setpgid(&task2, &task2, &task2.pid).expect("setpgid");
            let task2_pgid = task2.thread_group().pgid();
            assert_ne!(task2_pgid, task1.thread_group().pgid());

            // The foreground process group should be the one of task1
            assert_eq!(
                ioctl::<i32>(&task1, &opened_replica, TIOCGPGRP, &0),
                Ok(task1.thread_group().pgid().id)
            );

            // Cannot change the foreground process group to a negative pid.
            assert_eq!(ioctl::<i32>(&task2, &opened_replica, TIOCSPGRP, &-1), error!(EINVAL));

            // Cannot change the foreground process group to a invalid process group.
            assert_eq!(ioctl::<i32>(&task2, &opened_replica, TIOCSPGRP, &255), error!(ESRCH));

            // Cannot change the foreground process group to a process group in another session.
            let init_pgid = init.thread_group().pgid();
            assert_eq!(
                ioctl::<i32>(&task2, &opened_replica, TIOCSPGRP, &init_pgid.id),
                error!(EPERM)
            );

            // Changing the foreground process while being in background generates SIGTTOU and fails.
            assert_eq!(
                ioctl::<i32>(&task2, &opened_replica, TIOCSPGRP, &task2_pgid.id),
                error!(EINTR)
            );
            assert!(task2.read().has_signal_pending(SIGTTOU));

            // Set the foreground process to task2 process group
            ioctl::<i32>(&task1, &opened_replica, TIOCSPGRP, &task2_pgid.id).unwrap();

            // Check that the foreground process has been changed.
            let terminal =
                Arc::clone(&task1.thread_group().controlling_terminal().as_ref().unwrap().terminal);
            assert_eq!(
                terminal.read().controlling_session.as_ref().unwrap().foreground_process_group(),
                &task2_pgid
            );
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_detach_session() {
        spawn_kernel_and_run(async |task1| {
            let kernel = task1.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            let mut task2 = task1.clone_task_for_test(0, Some(SIGCHLD));
            task2.thread_group().setsid().expect("setsid");

            let fs = new_pts_fs(kernel);
            let _opened_main = open_ptmx_and_unlock(task1, &fs).expect("ptmx");
            let opened_replica = open_file(task1, &fs, "0".into()).expect("open file");

            // Cannot detach the controlling terminal when none is attached terminal
            assert_eq!(ioctl::<i32>(task1, &opened_replica, TIOCNOTTY, &0), error!(ENOTTY));

            set_controlling_terminal(&task2, &opened_replica, false)
                .expect("set controlling terminal");

            // Cannot detach the controlling terminal when not in the controlling session.
            assert_eq!(ioctl::<i32>(task1, &opened_replica, TIOCNOTTY, &0), error!(ENOTTY));

            // A non-session-leader child in the same session can detach its own controlling
            // terminal via TIOCNOTTY without detaching the session leader.
            let mut task3 = task2.clone_task_for_test(0, Some(SIGCHLD));
            task3.set_mm_for_test(task1.mm().unwrap().clone());
            task2.set_mm_for_test(task1.mm().unwrap().clone());
            assert!(task3.thread_group().controlling_terminal().is_some());
            assert_eq!(ioctl::<i32>(&task3, &opened_replica, TIOCGPGRP, &0), Ok(task2.pid.id));

            ioctl::<i32>(&task3, &opened_replica, TIOCNOTTY, &0)
                .expect("non-leader detach terminal");
            assert!(task3.thread_group().controlling_terminal().is_none());
            assert!(task2.thread_group().controlling_terminal().is_some());
            assert_eq!(ioctl::<i32>(&task3, &opened_replica, TIOCGPGRP, &0), error!(ENOTTY));
            assert_eq!(
                ioctl::<i32>(&task3, &opened_replica, TIOCSPGRP, &task2.pid.id),
                error!(ENOTTY)
            );
            assert_eq!(ioctl::<i32>(&task2, &opened_replica, TIOCGPGRP, &0), Ok(task2.pid.id));

            // Detach the terminal from the session leader.
            ioctl::<i32>(&task2, &opened_replica, TIOCNOTTY, &0).expect("detach terminal");
            assert!(task2.thread_group().controlling_terminal().is_none());
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_send_data_back_and_forth() {
        spawn_kernel_and_run(async |task| {
            let kernel = task.kernel();
            tty_device_init(kernel).expect("tty_device_init");
            let fs = new_pts_fs(kernel);
            let ptmx = open_ptmx_and_unlock(task, &fs).expect("ptmx");
            let pts = open_file(task, &fs, "0".into()).expect("open file");

            let has_data_ready_to_read = |fd: &FileHandle| {
                fd.query_events(task).expect("query_events").contains(FdEvents::POLLIN)
            };

            let write_and_assert = |fd: &FileHandle, data: &[u8]| {
                assert_eq!(
                    fd.write(task, &mut VecInputBuffer::new(data)).expect("write"),
                    data.len()
                );
            };

            let read_and_check = |fd: &FileHandle, data: &[u8]| {
                assert!(has_data_ready_to_read(fd));
                let mut buffer = VecOutputBuffer::new(data.len() + 1);
                assert_eq!(fd.read(task, &mut buffer).expect("read"), data.len());
                assert_eq!(data, buffer.data());
            };

            let hello_buffer = b"hello\n";
            let hello_transformed_buffer = b"hello\r\n";

            // Main to replica
            write_and_assert(&ptmx, hello_buffer);
            read_and_check(&pts, hello_buffer);

            // Data has been echoed
            read_and_check(&ptmx, hello_transformed_buffer);

            // Replica to main
            write_and_assert(&pts, hello_buffer);
            read_and_check(&ptmx, hello_transformed_buffer);

            // Data has not been echoed
            assert!(!has_data_ready_to_read(&pts));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_foreground_pgrp_persistence_and_signal_noop_on_exit() {
        spawn_kernel_and_run(async |init| {
            let kernel = init.kernel();
            tty_device_init(kernel).expect("tty_device_init");

            let mut task1 = init.clone_task_for_test(0, Some(SIGCHLD));
            task1.set_mm_for_test(init.mm().unwrap().clone());
            task1.thread_group().setsid().expect("setsid");
            let task1_pgid = task1.thread_group().pgid();

            let fs = new_pts_fs(kernel);
            let _opened_main = open_ptmx_and_unlock(init, &fs).expect("ptmx");
            let opened_replica = open_file(&task1, &fs, "0".into()).expect("open replica");
            set_controlling_terminal(&task1, &opened_replica, false)
                .expect("set controlling terminal");

            let terminal =
                Arc::clone(&task1.thread_group().controlling_terminal().as_ref().unwrap().terminal);

            // Spawn two tasks in a new foreground process group.
            let mut task2 = task1.clone_task_for_test(0, Some(SIGCHLD));
            task2.set_mm_for_test(init.mm().unwrap().clone());
            task2.thread_group().setpgid(&task2, &task2, &task2.pid).expect("setpgid task2");
            let task2_pgid = task2.thread_group().pgid();
            let task2_pgid_id = task2_pgid.id;

            let mut task3 = task1.clone_task_for_test(0, Some(SIGCHLD));
            task3.set_mm_for_test(init.mm().unwrap().clone());
            task1.thread_group().setpgid(&task1, &task3, &task2_pgid).expect("setpgid task3");

            // Set task2's process group as the foreground process group.
            ioctl::<i32>(&task1, &opened_replica, TIOCSPGRP, &task2_pgid_id)
                .expect("set foreground process group");
            assert_eq!(ioctl::<i32>(&task1, &opened_replica, TIOCGPGRP, &0), Ok(task2_pgid_id));

            // Mask SIGWINCH on all three tasks so default-ignore disposition does not discard it.
            task1.write().set_signal_mask(SigSet::from(SIGWINCH));
            task2.write().set_signal_mask(SigSet::from(SIGWINCH));
            task3.write().set_signal_mask(SigSet::from(SIGWINCH));

            // Deliver SIGWINCH to the foreground group while both tasks are alive.
            terminal.send_signals(&[SIGWINCH]);
            assert!(task2.thread_group().has_signal_queued(SIGWINCH));
            assert!(task3.thread_group().has_signal_queued(SIGWINCH));
            assert!(!task1.thread_group().has_signal_queued(SIGWINCH));

            // Terminate the process group leader (task2) while task3 remains alive in the group.
            task2.thread_group().kill(ExitStatus::Exit(0), None);
            std::mem::drop(task2);

            // Foreground PGID remains task2_pgid_id, and signals reach the surviving member task3.
            assert_eq!(ioctl::<i32>(&task1, &opened_replica, TIOCGPGRP, &0), Ok(task2_pgid_id));
            terminal.send_signals(&[SIGINT]);
            assert!(task3.thread_group().has_signal_queued(SIGINT));
            assert!(!task1.thread_group().has_signal_queued(SIGINT));

            // Terminate the final member (task3) and reap both zombies so the foreground process
            // group is completely reclaimed from PidTable.
            task3.thread_group().kill(ExitStatus::Exit(0), None);
            std::mem::drop(task3);
            {
                let mut pids = kernel.pids.lock();
                for zombie in task1.thread_group().write().zombie_children.drain(..) {
                    zombie.release(&mut pids);
                }
            }
            assert_eq!(kernel.pids.get(task2_pgid_id), error!(ESRCH));

            // Verify tcgetpgrp (TIOCGPGRP) still returns the exited group's PGID.
            assert_eq!(ioctl::<i32>(&task1, &opened_replica, TIOCGPGRP, &0), Ok(task2_pgid_id));

            // Verify signal dispatch to the exited foreground process group is a safe no-op.
            terminal.send_signals(&[SIGINT, SIGQUIT, SIGTSTP, SIGWINCH, SIGHUP]);
            assert!(!task1.thread_group().has_signal_queued(SIGINT));
            assert!(!task1.thread_group().has_signal_queued(SIGQUIT));
            assert!(!task1.thread_group().has_signal_queued(SIGTSTP));
            assert!(!task1.thread_group().has_signal_queued(SIGWINCH));
            assert!(!task1.thread_group().has_signal_queued(SIGHUP));

            // Mask SIGTTOU on task1 and reclaim the foreground process group.
            task1.write().set_signal_mask(SigSet::from(SIGTTOU) | SigSet::from(SIGWINCH));
            ioctl::<i32>(&task1, &opened_replica, TIOCSPGRP, &task1_pgid.id)
                .expect("reclaim foreground process group");
            assert_eq!(ioctl::<i32>(&task1, &opened_replica, TIOCGPGRP, &0), Ok(task1_pgid.id));

            // Move foreground to another short-lived process group, exit it, and disassociate.
            let mut task4 = task1.clone_task_for_test(0, Some(SIGCHLD));
            task4.set_mm_for_test(init.mm().unwrap().clone());
            task4.thread_group().setpgid(&task4, &task4, &task4.pid).expect("setpgid task4");
            let task4_pgid_id = task4.pid.id;
            ioctl::<i32>(&task1, &opened_replica, TIOCSPGRP, &task4_pgid_id)
                .expect("set foreground to task4");
            task4.thread_group().kill(ExitStatus::Exit(0), None);
            std::mem::drop(task4);

            terminal.disassociate_controlling_session();
            assert!(task1.thread_group().controlling_terminal().is_none());
            assert!(!task1.thread_group().has_signal_queued(SIGHUP));
            assert!(!task1.thread_group().has_signal_queued(SIGCONT));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_concurrent_terminal_job_control_stress() {
        spawn_kernel_and_run(async |init| {
            let kernel = init.kernel();
            tty_device_init(kernel).expect("tty_device_init");

            let mut task1 = init.clone_task_for_test(0, Some(SIGCHLD));
            task1.set_mm_for_test(init.mm().unwrap().clone());
            task1.thread_group().setsid().expect("setsid task1");
            task1.write().set_signal_mask(SigSet::from(SIGTTOU));
            let task1_pgid = task1.thread_group().pgid();

            let mut child_a = task1.clone_task_for_test(0, Some(SIGCHLD));
            child_a.set_mm_for_test(init.mm().unwrap().clone());
            child_a
                .thread_group()
                .setpgid(&child_a, &child_a, &child_a.pid)
                .expect("setpgid child_a");
            let child_a_pgid = child_a.pid.clone();

            let mut child_b = task1.clone_task_for_test(0, Some(SIGCHLD));
            child_b.set_mm_for_test(init.mm().unwrap().clone());
            child_b
                .thread_group()
                .setpgid(&child_b, &child_b, &child_b.pid)
                .expect("setpgid child_b");
            let child_b_pgid = child_b.pid.clone();

            let mut child_m = task1.clone_task_for_test(0, Some(SIGCHLD));
            child_m.set_mm_for_test(init.mm().unwrap().clone());

            let fs = new_pts_fs(kernel);
            let _opened_main = open_ptmx_and_unlock(init, &fs).expect("ptmx 0");
            let opened_replica = open_file(&task1, &fs, "0".into()).expect("open replica 0");
            set_controlling_terminal(&task1, &opened_replica, false)
                .expect("set controlling terminal 0");
            let terminal =
                Arc::clone(&task1.thread_group().controlling_terminal().as_ref().unwrap().terminal);

            // Create a second session leader and terminal to stress concurrent disassociation.
            let mut task_s2 = init.clone_task_for_test(0, Some(SIGCHLD));
            task_s2.set_mm_for_test(init.mm().unwrap().clone());
            task_s2.thread_group().setsid().expect("setsid task_s2");
            task_s2.write().set_signal_mask(SigSet::from(SIGTTOU));
            let task_s2_pgid = task_s2.pid.clone();

            let _opened_main2 = open_ptmx_and_unlock(init, &fs).expect("ptmx 1");
            let opened_replica2 = open_file(&task_s2, &fs, "1".into()).expect("open replica 1");
            set_controlling_terminal(&task_s2, &opened_replica2, false)
                .expect("set controlling terminal 1");
            let terminal2 = Arc::clone(
                &task_s2.thread_group().controlling_terminal().as_ref().unwrap().terminal,
            );

            let running = Arc::new(AtomicBool::new(true));

            // Thread 1: Concurrently dispatch signals to the foreground process groups.
            let sig_term1 = Arc::clone(&terminal);
            let sig_term2 = Arc::clone(&terminal2);
            let sig_running = Arc::clone(&running);
            let signal_thread = std::thread::spawn(move || {
                while sig_running.load(Ordering::Relaxed) {
                    sig_term1.send_signals(&[SIGWINCH, SIGCONT]);
                    sig_term2.send_signals(&[SIGWINCH, SIGCONT]);
                }
            });

            // Thread 2: Concurrently read the foreground process group and verify PGID validity.
            let get_tg1 = Arc::clone(task1.thread_group());
            let get_term1 = Arc::clone(&terminal);
            let get_running = Arc::clone(&running);
            let valid_pgids = [task1_pgid.id, child_a_pgid.id, child_b_pgid.id];
            let getter_thread = std::thread::spawn(move || {
                while get_running.load(Ordering::Relaxed) {
                    if let Ok(pgid) = get_tg1.get_foreground_process_group(&get_term1) {
                        assert!(valid_pgids.contains(&pgid));
                    }
                }
            });

            // Thread 3: Concurrently invoke setsid on group leaders (acquiring PidTableLock).
            let setsid_tg1 = Arc::clone(task1.thread_group());
            let setsid_tga = Arc::clone(child_a.thread_group());
            let setsid_running = Arc::clone(&running);
            let setsid_thread = std::thread::spawn(move || {
                while setsid_running.load(Ordering::Relaxed) {
                    assert_eq!(setsid_tg1.setsid(), error!(EPERM));
                    assert_eq!(setsid_tga.setsid(), error!(EPERM));
                }
            });

            // Thread 4: Concurrently disassociate the controlling session on terminal2.
            let disassoc_term2 = Arc::clone(&terminal2);
            let disassoc_running = Arc::clone(&running);
            let disassoc_thread = std::thread::spawn(move || {
                while disassoc_running.load(Ordering::Relaxed) {
                    disassoc_term2.disassociate_controlling_session();
                }
            });

            // Main thread: Interleave TIOCSPGRP, setpgid task migration, and terminal attach/detach.
            for _ in 0..75 {
                let _ = ioctl::<i32>(&task1, &opened_replica, TIOCSPGRP, &child_a_pgid.id);
                task1
                    .thread_group()
                    .setpgid(&task1, &child_m, &child_a_pgid)
                    .expect("migrate child_m to child_a");

                let _ = ioctl::<i32>(&task1, &opened_replica, TIOCSPGRP, &child_b_pgid.id);
                task1
                    .thread_group()
                    .setpgid(&task1, &child_m, &child_b_pgid)
                    .expect("migrate child_m to child_b");

                let _ = ioctl::<i32>(&task1, &opened_replica, TIOCSPGRP, &task1_pgid.id);

                let _ = set_controlling_terminal(&task_s2, &opened_replica2, false);
                let _ = task_s2.thread_group().set_foreground_process_group(
                    &task_s2,
                    &terminal2,
                    &task_s2_pgid,
                );
                let _ = ioctl::<i32>(&task_s2, &opened_replica2, TIOCNOTTY, &0);
            }

            // Disassociate primary terminal while background threads are still active.
            terminal.disassociate_controlling_session();

            running.store(false, Ordering::Relaxed);
            signal_thread.join().expect("signal_thread join");
            getter_thread.join().expect("getter_thread join");
            setsid_thread.join().expect("setsid_thread join");
            disassoc_thread.join().expect("disassoc_thread join");

            assert!(task1.thread_group().controlling_terminal().is_none());
            assert!(task_s2.thread_group().controlling_terminal().is_none());
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_tiocnotty_non_leader_only_detaches_caller() {
        spawn_kernel_and_run(async |init| {
            let kernel = init.kernel();
            tty_device_init(kernel).expect("tty_device_init");

            let mut leader = init.clone_task_for_test(0, Some(SIGCHLD));
            leader.set_mm_for_test(init.mm().unwrap().clone());
            leader.thread_group().setsid().expect("setsid");

            let fs = new_pts_fs(kernel);
            let _opened_main = open_ptmx_and_unlock(init, &fs).expect("ptmx");
            let opened_replica = open_file(&leader, &fs, "0".into()).expect("open replica");
            set_controlling_terminal(&leader, &opened_replica, false)
                .expect("set controlling terminal");

            // Spawn a non-leader process in the same session.
            let mut child = leader.clone_task_for_test(0, Some(SIGCHLD));
            child.set_mm_for_test(init.mm().unwrap().clone());
            assert!(leader.thread_group().controlling_terminal().is_some());
            assert!(child.thread_group().controlling_terminal().is_some());

            let terminal = Arc::clone(
                &leader.thread_group().controlling_terminal().as_ref().unwrap().terminal,
            );

            // Non-leader calling TIOCNOTTY only detaches its own controlling terminal without
            // disassociating the session or signaling the foreground process group.
            ioctl::<i32>(&child, &opened_replica, TIOCNOTTY, &0)
                .expect("non-leader TIOCNOTTY should succeed");
            assert!(child.thread_group().controlling_terminal().is_none());
            assert!(leader.thread_group().controlling_terminal().is_some());
            assert_eq!(terminal.read().controlling_session(), Some(&leader.pid));
            assert!(!leader.thread_group().has_signal_queued(SIGHUP));
            assert!(!child.thread_group().has_signal_queued(SIGHUP));

            // A second TIOCNOTTY from the already-detached non-leader fails with ENOTTY.
            assert_eq!(ioctl::<i32>(&child, &opened_replica, TIOCNOTTY, &0), error!(ENOTTY));

            // Session leader calling TIOCNOTTY disassociates the entire session.
            ioctl::<i32>(&leader, &opened_replica, TIOCNOTTY, &0)
                .expect("leader TIOCNOTTY should succeed");
            assert!(leader.thread_group().controlling_terminal().is_none());
            assert!(terminal.read().controlling_session().is_none());
        })
        .await;
    }
}
