// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(test)]
mod ebpf_test {
    use ebpf_loader::{MapDefinition, ProgramDefinition};
    use libc::{
        self, IPPROTO_TCP, IPPROTO_UDP, SO_COOKIE, SO_MARK, SO_SNDBUF, SOCK_DGRAM, SOCK_STREAM,
        SOL_SOCKET,
    };
    use linux_uapi::{bpf_attr, bpf_map_type_BPF_MAP_TYPE_SK_STORAGE};
    use serial_test::serial;
    use std::ffi::CString;
    use std::fs::{File, OpenOptions};
    use std::io::Write as _;
    use std::net::UdpSocket;
    use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;
    use std::ptr::null_mut;
    use std::time::Duration;
    use test_case::test_case;
    use zerocopy::{FromBytes, Immutable, IntoBytes};

    macro_rules! root_required {
        () => {
            // SAFETY: geteuid() is always safe.
            let euid = unsafe { libc::geteuid() };
            if euid != 0 {
                eprintln!("eBPF tests require root privileges, skipping");
                return;
            }
        };
    }

    fn zero_bpf_attr() -> bpf_attr {
        bpf_attr::read_from_bytes(&[0; std::mem::size_of::<bpf_attr>()])
            .expect("Failed to create bpf_attr")
    }

    unsafe fn bpf(command: linux_uapi::bpf_cmd, attr: &bpf_attr) -> Result<i32, std::io::Error> {
        #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
        let result = unsafe {
            libc::syscall(
                linux_uapi::__NR_bpf.into(),
                command,
                attr as *const bpf_attr,
                std::mem::size_of_val(attr),
            )
        };
        (result >= 0).then_some(result as i32).ok_or_else(std::io::Error::last_os_error)
    }

    fn gettid() -> linux_uapi::pid_t {
        // SAFETY: gettid syscall is always safe.
        unsafe { libc::syscall(linux_uapi::__NR_gettid.into()) as linux_uapi::pid_t }
    }

    fn bpf_map_create(map_def: &ebpf_loader::MapDefinition) -> Result<OwnedFd, std::io::Error> {
        let mut attr = zero_bpf_attr();

        // SAFETY: `attr` is zeroed, so it's safe to access any union variant.
        let create_map_attr = unsafe { &mut attr.__bindgen_anon_1 };
        create_map_attr.map_type = map_def.schema.map_type;
        create_map_attr.key_size = map_def.schema.key_size;
        create_map_attr.value_size = map_def.schema.value_size;
        create_map_attr.max_entries = map_def.schema.max_entries;
        create_map_attr.map_flags = map_def.schema.flags.bits();

        // If we have a name then copy it to the 0-terminated buffer, cropping
        // it to fit if necessary.
        if let Some(name) = map_def.name.as_ref() {
            let name_len = std::cmp::min(name.len(), create_map_attr.map_name.len() - 1);
            let name_bytes = create_map_attr.map_name.as_mut_bytes();
            name_bytes[..name_len].copy_from_slice(&name[..name_len]);
            name_bytes[name_len] = 0;
        }

        // SAFETY: `bpf()` syscall with valid arguments.
        let result = unsafe { bpf(linux_uapi::bpf_cmd_BPF_MAP_CREATE, &attr) };

        // SAFETY: result is an FD when non-negative.
        result.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn bpf_map_update_elem<K: IntoBytes + Immutable, V: IntoBytes + Immutable>(
        map_fd: BorrowedFd<'_>,
        key: K,
        value: V,
    ) -> Result<(), std::io::Error> {
        let mut attr = zero_bpf_attr();

        // SAFETY: `attr` is zeroed, so it's safe to access any union variant.
        let update_elem_attr = unsafe { &mut attr.__bindgen_anon_2 };
        update_elem_attr.map_fd = map_fd.as_raw_fd() as u32;
        update_elem_attr.key = key.as_bytes().as_ptr() as u64;

        // SAFETY: `attr` is zeroed, so it's safe to access any union variant.
        let value_field = &mut update_elem_attr.__bindgen_anon_1;
        value_field.value = value.as_bytes().as_ptr() as u64;

        // SAFETY: `bpf()` syscall with valid arguments.
        unsafe { bpf(linux_uapi::bpf_cmd_BPF_MAP_UPDATE_ELEM, &attr) }.map(|r| {
            assert!(r == 0);
        })
    }

    fn bpf_map_lookup_elem<K: IntoBytes + Immutable, T: FromBytes>(
        map_fd: BorrowedFd<'_>,
        key: K,
    ) -> Result<T, std::io::Error> {
        let mut attr = zero_bpf_attr();

        // SAFETY: `attr` is zeroed, so it's safe to access any union variant.
        let update_elem_attr = unsafe { &mut attr.__bindgen_anon_2 };
        update_elem_attr.map_fd = map_fd.as_raw_fd() as u32;
        update_elem_attr.key = key.as_bytes().as_ptr() as u64;

        let mut value = vec![0; std::mem::size_of::<T>()];

        // SAFETY: `attr` is zeroed, so it's safe to access any union variant.
        let value_field = &mut update_elem_attr.__bindgen_anon_1;
        value_field.value = value.as_mut_ptr() as u64;

        // SAFETY: `bpf()` syscall with valid arguments.
        unsafe { bpf(linux_uapi::bpf_cmd_BPF_MAP_LOOKUP_ELEM, &attr) }.map(|r| {
            assert!(r == 0);
            T::read_from_bytes(&value).expect("Failed to convert lookup result")
        })
    }

    fn bpf_prog_load(
        code: Vec<ebpf::EbpfInstruction>,
        prog_type: linux_uapi::bpf_prog_type,
        expected_attach_type: linux_uapi::bpf_attach_type,
    ) -> Result<OwnedFd, std::io::Error> {
        let mut attr = zero_bpf_attr();

        let license = b"N/A\0";

        // SAFETY: `attr` is zeroed, so it's safe to access any union variant.
        let load_prog_attr = unsafe { &mut attr.__bindgen_anon_3 };
        load_prog_attr.prog_type = prog_type;
        load_prog_attr.insns = code.as_ptr() as u64;
        load_prog_attr.insn_cnt = code.len() as u32;
        load_prog_attr.expected_attach_type = expected_attach_type;
        load_prog_attr.log_level = 0;
        load_prog_attr.log_size = 0;
        load_prog_attr.log_buf = 0;
        load_prog_attr.license = license.as_ptr() as u64;

        // SAFETY: `bpf()` syscall with valid arguments.
        let result = unsafe { bpf(linux_uapi::bpf_cmd_BPF_PROG_LOAD, &attr) };

        // SAFETY: result is an FD when non-negative.
        result.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn bpf_prog_attach(
        attach_type: linux_uapi::bpf_attach_type,
        attach_target: BorrowedFd<'_>,
        prog_fd: BorrowedFd<'_>,
    ) -> Result<(), std::io::Error> {
        let mut attr = zero_bpf_attr();

        // SAFETY: `attr` is zeroed, so it's safe to access any union variant.
        let attach_prog_attr = unsafe { &mut attr.__bindgen_anon_5 };
        attach_prog_attr.attach_bpf_fd = prog_fd.as_raw_fd() as u32;
        attach_prog_attr.attach_type = attach_type;
        attach_prog_attr.__bindgen_anon_1.target_fd = attach_target.as_raw_fd() as u32;

        // SAFETY: `bpf()` syscall with valid arguments.
        unsafe { bpf(linux_uapi::bpf_cmd_BPF_PROG_ATTACH, &attr) }.map(|r| {
            assert!(r == 0);
        })
    }

    fn bpf_prog_detach(
        attach_type: linux_uapi::bpf_attach_type,
        attach_target: BorrowedFd<'_>,
    ) -> Result<(), std::io::Error> {
        let mut attr = zero_bpf_attr();

        // SAFETY: `attr` is zeroed, so it's safe to access any union variant.
        let attach_prog_attr = unsafe { &mut attr.__bindgen_anon_5 };
        attach_prog_attr.attach_type = attach_type;
        attach_prog_attr.__bindgen_anon_1.target_fd = attach_target.as_raw_fd() as u32;

        // SAFETY: `bpf()` syscall with valid arguments.
        unsafe { bpf(linux_uapi::bpf_cmd_BPF_PROG_DETACH, &attr) }.map(|r| {
            assert!(r == 0);
            ()
        })
    }

    fn pollfd(
        fd: BorrowedFd<'_>,
        events: libc::c_short,
        timeout: Duration,
    ) -> Result<Option<libc::c_short>, std::io::Error> {
        let mut fds = [libc::pollfd { fd: fd.as_raw_fd(), events, revents: 0 }];

        // If the specified timeout is greater than i32::MAX milliseconds then
        // pass -1 to `poll()` to wait indefinitely.
        let timeout_ms = timeout.as_millis().try_into().unwrap_or(-1);

        // SAFETY: poll is safe to call with a valid pollfd array.
        let result = unsafe { libc::poll(fds.as_mut_ptr(), 1, timeout_ms) };

        if result < 0 {
            Err(std::io::Error::last_os_error())
        } else if result > 0 {
            Ok(Some(fds[0].revents))
        } else {
            Ok(None)
        }
    }

    fn get_socket_cookie(fd: BorrowedFd<'_>) -> Result<u64, std::io::Error> {
        let mut value: u64 = 0;
        let mut value_len: libc::socklen_t = std::mem::size_of_val(&value) as u32;
        // SAFETY: `getsockopt()` call with valid arguments.
        let result = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                SOL_SOCKET,
                SO_COOKIE,
                &mut value as *mut u64 as *mut libc::c_void,
                &mut value_len,
            )
        };

        if result < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            assert!(value_len == std::mem::size_of_val(&value) as u32);
            Ok(value)
        }
    }

    fn setsockopt(
        fd: BorrowedFd<'_>,
        level: libc::c_int,
        optname: libc::c_int,
        value: &[u8],
    ) -> Result<(), std::io::Error> {
        // SAFETY: `setsockopt()` call with valid arguments.
        let result = unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                level,
                optname,
                value.as_ptr() as *mut libc::c_void,
                value.len() as libc::socklen_t,
            )
        };

        (result >= 0).then_some(()).ok_or_else(|| std::io::Error::last_os_error())
    }

    fn getsockopt(
        fd: BorrowedFd<'_>,
        level: libc::c_int,
        optname: libc::c_int,
        buffer_size: usize,
    ) -> Result<Vec<u8>, std::io::Error> {
        let mut buf = vec![0; buffer_size];
        let mut optlen = buffer_size as libc::socklen_t;
        // SAFETY: `setsockopt()` call with valid arguments.
        let result = unsafe {
            libc::getsockopt(
                fd.as_raw_fd(),
                level,
                optname,
                buf.as_mut_ptr() as *mut libc::c_void,
                &mut optlen as *mut libc::socklen_t,
            )
        };

        (result >= 0)
            .then(|| {
                buf.resize(optlen as usize, 0);
                buf
            })
            .ok_or_else(|| std::io::Error::last_os_error())
    }

    fn getpagesize() -> usize {
        // SAFETY: `sysconf()` is safe to call.
        unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
    }

    // Names of the eBPF maps defined in `ebpf_test_progs.c`.
    const RINGBUF_MAP_NAME: &str = "ringbuf";
    const TARGET_COOKIE_MAP_NAME: &str = "target_cookie";
    const COUNT_MAP_NAME: &str = "count";
    const TEST_RESULT_MAP_NAME: &str = "test_result";
    const RO_ARRAY_MAP_NAME: &str = "ro_array_map";
    const RO_HASH_MAP_NAME: &str = "ro_hash_map";
    const RO_MMAP_MAP_NAME: &str = "ro_mmap_map";

    // LINT.IfChange
    #[repr(C)]
    #[derive(Debug, Immutable, FromBytes)]
    struct TestResult {
        uid_gid: u64,
        pid_tgid: u64,

        optlen: u64,
        optval_size: u64,
        retval: i32,
        get_retval: i32,

        ether_type: u32,
        ifindex: u32,
        mark: u32,

        sockaddr_family: u32,
        sockaddr_port: u32,
        sockaddr_ip: [u32; 4],

        sk_type: u32,
        sk_protocol: u32,
        sk_family: u32,
        sk_state: u32,
        socket_uid: u32,
        pkt_type: u32,
    }

    #[repr(C)]
    #[derive(Debug, Immutable, FromBytes)]
    struct GlobalVariables {
        global_counter1: u64,
        global_counter2: u64,
    }

    const TEST_SOCK_OPT: libc::c_int = 12345;
    // LINT.ThenChange(//src/starnix/tests/syscalls/rust/data/ebpf/ebpf_test_progs.c)

    #[derive(Default, Debug)]
    struct MapSet {
        maps: Vec<(MapDefinition, OwnedFd)>,
    }

    impl MapSet {
        fn new() -> Self {
            Self { maps: vec![] }
        }

        fn find_or_insert(&mut self, new_def: MapDefinition) -> BorrowedFd<'_> {
            let index = match self.maps.iter().position(|(def, _fd)| def.name == new_def.name) {
                Some(index) => {
                    assert_eq!(self.maps[index].0, new_def);
                    index
                }
                None => {
                    let fd = bpf_map_create(&new_def).expect("Failed to create map");
                    self.maps.push((new_def, fd));
                    self.maps.len() - 1
                }
            };
            self.maps[index].1.as_fd()
        }

        fn find(&self, name: &str, expected_type: linux_uapi::bpf_map_type) -> BorrowedFd<'_> {
            let (def, fd) = self
                .maps
                .iter()
                .find(|(def, _fd)| {
                    def.name.as_ref().map(|x| x == bstr::BStr::new(name)).unwrap_or(false)
                })
                .unwrap_or_else(|| panic!("Failed to find map {}", name));
            assert!(def.schema.map_type == expected_type, "Invalid map type for map {}", name);
            fd.as_fd()
        }

        fn rss(&self) -> BorrowedFd<'_> {
            let (def, fd) = self
                .maps
                .iter()
                .find(|(def, _fd)| def.name.is_none())
                .unwrap_or_else(|| panic!("Failed to find rss map"));
            assert!(
                def.schema.map_type == linux_uapi::bpf_map_type_BPF_MAP_TYPE_ARRAY,
                "Invalid map type for map rss"
            );
            fd.as_fd()
        }

        fn ringbuf(&self) -> BorrowedFd<'_> {
            self.find(RINGBUF_MAP_NAME, linux_uapi::bpf_map_type_BPF_MAP_TYPE_RINGBUF)
        }

        fn set_target_cookie(&self, cookie: u64) {
            let target_cookie_fd =
                self.find(TARGET_COOKIE_MAP_NAME, linux_uapi::bpf_map_type_BPF_MAP_TYPE_ARRAY);
            bpf_map_update_elem(target_cookie_fd, 0u32, cookie)
                .expect("Failed to set target_cookie");
        }

        fn get_count(&self) -> u32 {
            let count_map_fd =
                self.find(COUNT_MAP_NAME, linux_uapi::bpf_map_type_BPF_MAP_TYPE_ARRAY);
            bpf_map_lookup_elem(count_map_fd, 0u32).expect("Failed to get count")
        }

        fn get_test_result(&self) -> TestResult {
            let test_result_map_fd =
                self.find(TEST_RESULT_MAP_NAME, linux_uapi::bpf_map_type_BPF_MAP_TYPE_ARRAY);
            bpf_map_lookup_elem(test_result_map_fd, 0u32).expect("Failed to test_result")
        }

        fn get_global_variables(&self) -> GlobalVariables {
            let rss_map_fd = self.rss();
            bpf_map_lookup_elem(rss_map_fd, 0u32).expect("Failed to test_result")
        }
    }

    #[derive(Debug)]
    struct LoadedProgram {
        prog_fd: OwnedFd,
        attach_type: linux_uapi::bpf_attach_type,
    }

    impl LoadedProgram {
        fn try_new(
            name: &str,
            prog_type: linux_uapi::bpf_prog_type,
            attach_type: linux_uapi::bpf_attach_type,
            maps: &mut MapSet,
        ) -> Result<Self, std::io::Error> {
            let ProgramDefinition { mut code, maps: map_defs } =
                ebpf_loader::load_ebpf_program("data/ebpf_test_progs.o", ".text", name)
                    .expect("Failed to load program");

            let map_fds: Vec<_> = map_defs
                .into_iter()
                .map(|map_def| maps.find_or_insert(map_def).as_raw_fd())
                .collect();

            // Replace map indices with FDs.
            for inst in code.iter_mut() {
                if inst.code() == ebpf::BPF_LDDW && inst.src_reg() == ebpf::BPF_PSEUDO_MAP_IDX {
                    let map_index = inst.imm() as usize;
                    inst.set_src_reg(ebpf::BPF_PSEUDO_MAP_FD);
                    inst.set_imm(map_fds[map_index]);
                }
                if inst.code() == ebpf::BPF_LDDW && inst.src_reg() == ebpf::BPF_PSEUDO_MAP_IDX_VALUE
                {
                    let map_index = inst.imm() as usize;
                    inst.set_src_reg(ebpf::BPF_PSEUDO_MAP_VALUE);
                    inst.set_imm(map_fds[map_index]);
                }
            }

            let prog_fd = bpf_prog_load(code, prog_type, attach_type)?;

            Ok(Self { prog_fd, attach_type })
        }

        fn new(
            name: &str,
            prog_type: linux_uapi::bpf_prog_type,
            attach_type: linux_uapi::bpf_attach_type,
            maps: &mut MapSet,
        ) -> Self {
            Self::try_new(name, prog_type, attach_type, maps)
                .unwrap_or_else(|e| panic!("Failed to load program {name}: {e:?}"))
        }

        fn attach(&self) -> AttachedProgram {
            let cgroup = File::open("/sys/fs/cgroup").expect("Failed to open cgroup");
            bpf_prog_attach(self.attach_type, cgroup.as_fd(), self.prog_fd.as_fd())
                .expect("Failed to attach program");
            AttachedProgram { attach_type: self.attach_type, cgroup }
        }
    }

    struct AttachedProgram {
        attach_type: linux_uapi::bpf_attach_type,
        cgroup: File,
    }

    impl Drop for AttachedProgram {
        fn drop(&mut self) {
            let Self { attach_type, cgroup } = self;
            bpf_prog_detach(*attach_type, cgroup.as_fd()).expect("Failed to detach program");
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum IpFamily {
        V4,
        V6,
    }

    impl IpFamily {
        fn any_addr(&self) -> std::net::SocketAddr {
            match self {
                IpFamily::V4 => "0.0.0.0:0".parse().unwrap(),
                IpFamily::V6 => "[::]:0".parse().unwrap(),
            }
        }

        fn localhost_addr(&self) -> std::net::SocketAddr {
            match self {
                IpFamily::V4 => "127.0.0.1:0".parse().unwrap(),
                IpFamily::V6 => "[::1]:0".parse().unwrap(),
            }
        }

        fn multicast_addr(&self) -> std::net::SocketAddr {
            match self {
                IpFamily::V4 => "224.1.2.3:0".parse().unwrap(),
                IpFamily::V6 => "[ff0e::1]:0".parse().unwrap(),
            }
        }

        fn ether_type(&self) -> u16 {
            match self {
                IpFamily::V4 => libc::ETH_P_IP as u16,
                IpFamily::V6 => libc::ETH_P_IPV6 as u16,
            }
        }

        fn family(&self) -> libc::c_int {
            match self {
                IpFamily::V4 => libc::AF_INET,
                IpFamily::V6 => libc::AF_INET6,
            }
        }
    }

    fn get_loopback_ifindex() -> u32 {
        let lo_index = unsafe { libc::if_nametoindex(b"lo\0".as_ptr() as *const libc::c_char) };
        assert!(lo_index > 0, "Failed to get loopback interface index");
        lo_index
    }

    fn make_ifreq(name: &std::ffi::CStr) -> libc::ifreq {
        // SAFETY: zeroed() is valid for libc::ifreq as all fields are primitive integer/array types.
        let mut ifr: libc::ifreq = unsafe { std::mem::zeroed() };
        let bytes = name.to_bytes_with_nul();
        assert!(bytes.len() <= std::mem::size_of_val(&ifr.ifr_name));
        for (dst, src) in ifr.ifr_name.iter_mut().zip(bytes) {
            *dst = *src as libc::c_char;
        }
        ifr
    }

    /// A temporary non-loopback network interface (TAP device) with assigned IPv4
    /// and IPv6 addresses used to test multicast loopback traffic.
    ///
    /// When multicast packets are sent over the loopback interface (`lo`), Netstack3
    /// delivers the original packet directly to loopback without invoking the
    /// `IP_MULTICAST_LOOP` packet duplication path. Using a non-loopback interface
    /// forces Netstack3 to transmit the original packet out of the interface while
    /// creating a duplicate looped-back copy for local delivery, verifying that
    /// packet marks and metadata are properly preserved across the duplication.
    struct TestInterface {
        _tun: Option<File>,
        ifindex: u32,
        name: CString,
        ipv4_addr: std::net::Ipv4Addr,
    }

    static TAP_COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    impl TestInterface {
        fn new() -> Self {
            use libc::ioctl;

            let tun_file = OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/tun")
                .or_else(|_| OpenOptions::new().read(true).write(true).open("/dev/net/tun"))
                .expect("Failed to open tun device");

            let count = TAP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let name = CString::new(format!("tap{}", count)).expect("Invalid CString");

            // Create a TAP device.
            let mut ifr = make_ifreq(&name);
            // SAFETY: Setting ifru_flags and calling TUNSETIFF ioctl on the
            // open tun device file descriptor.
            let ret = unsafe {
                ifr.ifr_ifru.ifru_flags =
                    (linux_uapi::IFF_TAP | linux_uapi::IFF_NO_PI) as libc::c_short;
                ioctl(tun_file.as_raw_fd(), linux_uapi::TUNSETIFF as libc::c_ulong, &mut ifr)
            };
            assert_eq!(ret, 0, "ioctl(TUNSETIFF) failed: {:?}", std::io::Error::last_os_error());

            let sock = UdpSocket::bind("0.0.0.0:0").expect("Failed to create UDP socket");

            // Bring the interface up.
            let mut flags_ifr = make_ifreq(&name);
            for _ in 0..50 {
                // SAFETY: SIOCGIFFLAGS takes an open socket and a pointer to an
                // ifreq with ifr_name set.
                if unsafe { ioctl(sock.as_raw_fd(), libc::SIOCGIFFLAGS, &mut flags_ifr) } == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            // SAFETY: flags_ifr is a valid ifreq with ifr_name set;
            // SIOCSIFFLAGS sets the interface flags.
            let ret = unsafe {
                flags_ifr.ifr_ifru.ifru_flags |=
                    (libc::IFF_UP | libc::IFF_RUNNING | libc::IFF_MULTICAST) as libc::c_short;
                ioctl(sock.as_raw_fd(), libc::SIOCSIFFLAGS, &flags_ifr)
            };
            assert_eq!(ret, 0, "SIOCSIFFLAGS failed: {:?}", std::io::Error::last_os_error());

            // Assign an IPv4 address.
            let octet = (1 + (count % 200)) as u8;
            let ipv4_addr = std::net::Ipv4Addr::new(192, 168, 42, octet);
            let mut addr_ifr = make_ifreq(&name);
            // SAFETY: sockaddr_in and ifru_addr (sockaddr) have matching
            // layouts. We initialize the sockaddr_in with the IPv4 address and
            // call SIOCSIFADDR to assign it to the interface.
            let ret = unsafe {
                let sin = &mut addr_ifr.ifr_ifru.ifru_addr as *mut libc::sockaddr
                    as *mut libc::sockaddr_in;
                *sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as libc::sa_family_t,
                    sin_port: 0,
                    sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(ipv4_addr.octets()) },
                    sin_zero: [0; 8],
                };
                ioctl(sock.as_raw_fd(), libc::SIOCSIFADDR, &addr_ifr)
            };
            assert_eq!(ret, 0, "SIOCSIFADDR failed: {:?}", std::io::Error::last_os_error());

            // SAFETY: name is a valid null-terminated C string.
            let ifindex = unsafe { libc::if_nametoindex(name.as_ptr()) };
            assert!(ifindex > 0, "if_nametoindex failed");

            // Disable Duplicate Address Detection (DAD) on the interface before adding
            // an IPv6 address. Otherwise, the added address remains in the Tentative state
            // for the duration of the DAD probe (~1s), during which RFC 4862 prohibits
            // using it as a source address, causing immediate `sendto` calls to fail
            // with EADDRNOTAVAIL (NoSrcAddr). Setting `dad_transmits` to 0 disables DAD,
            // making the address immediately Assigned and usable.
            let dad_path =
                format!("/proc/sys/net/ipv6/conf/{}/dad_transmits", name.to_str().unwrap());
            let _ = std::fs::write(&dad_path, "0");

            let ipv6_addr = std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, octet as u16);
            Self::add_ipv6_addr(ifindex, ipv6_addr.octets());

            TestInterface { _tun: Some(tun_file), ifindex, name, ipv4_addr }
        }

        /// Assigns an IPv6 address to the interface using a Netlink `RTM_NEWADDR` request.
        fn add_ipv6_addr(ifindex: u32, addr: [u8; 16]) {
            // SAFETY: Creating an AF_NETLINK raw socket for route configuration.
            let fd = unsafe { libc::socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_ROUTE) };
            assert!(fd >= 0, "socket(AF_NETLINK) failed: {:?}", std::io::Error::last_os_error());
            // SAFETY: `fd` is valid and non-negative.
            let nl_sock = unsafe { OwnedFd::from_raw_fd(fd) };

            #[repr(C)]
            struct NlMsg {
                nlh: linux_uapi::nlmsghdr,
                ifa: linux_uapi::ifaddrmsg,
                nla_addr: linux_uapi::nlattr,
                addr: [u8; 16],
                nla_local: linux_uapi::nlattr,
                local: [u8; 16],
            }

            let msg = NlMsg {
                nlh: linux_uapi::nlmsghdr {
                    nlmsg_len: std::mem::size_of::<NlMsg>() as u32,
                    nlmsg_type: linux_uapi::RTM_NEWADDR as u16,
                    nlmsg_flags: (linux_uapi::NLM_F_REQUEST
                        | linux_uapi::NLM_F_ACK
                        | linux_uapi::NLM_F_CREATE) as u16,
                    nlmsg_seq: 1,
                    nlmsg_pid: 0,
                },
                ifa: linux_uapi::ifaddrmsg {
                    ifa_family: libc::AF_INET6 as u8,
                    ifa_prefixlen: 64,
                    ifa_flags: linux_uapi::IFA_F_PERMANENT as u8,
                    ifa_scope: 0,
                    ifa_index: ifindex,
                },
                nla_addr: linux_uapi::nlattr {
                    nla_len: (std::mem::size_of::<linux_uapi::nlattr>() + 16) as u16,
                    nla_type: linux_uapi::IFA_ADDRESS as u16,
                },
                addr,
                nla_local: linux_uapi::nlattr {
                    nla_len: (std::mem::size_of::<linux_uapi::nlattr>() + 16) as u16,
                    nla_type: linux_uapi::IFA_LOCAL as u16,
                },
                local: addr,
            };

            let msg_ptr = &msg as *const _ as *const libc::c_void;
            // SAFETY: Sending the NlMsg buffer to the valid netlink socket descriptor.
            let ret = unsafe {
                libc::send(nl_sock.as_raw_fd(), msg_ptr, std::mem::size_of::<NlMsg>(), 0)
            };
            assert_eq!(ret, std::mem::size_of::<NlMsg>() as isize, "send(nl_sock) failed");

            let mut buf = [0u8; 4096];
            // SAFETY: Receiving netlink reply into a valid, appropriately-sized stack buffer.
            let len = unsafe {
                libc::recv(nl_sock.as_raw_fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0)
            };
            assert!(len > 0, "recv(nl_sock) failed");

            let (nlh, rest) = linux_uapi::nlmsghdr::read_from_prefix(&buf[..len as usize])
                .expect("Failed to parse nlmsghdr");
            assert_eq!(nlh.nlmsg_type, linux_uapi::NLMSG_ERROR as u16);
            let (err_msg, _) =
                linux_uapi::nlmsgerr::read_from_prefix(rest).expect("Failed to parse nlmsgerr");
            assert_eq!(err_msg.error, 0, "netlink RTM_NEWADDR failed with error {}", err_msg.error);
        }

        /// Binds the given socket to this interface using `SO_BINDTODEVICE`.
        fn bind_socket_to_device(&self, fd: BorrowedFd<'_>) {
            setsockopt(fd, SOL_SOCKET, libc::SO_BINDTODEVICE, self.name.as_bytes_with_nul())
                .expect("Failed to set SO_BINDTODEVICE");
        }
    }

    impl Drop for TestInterface {
        fn drop(&mut self) {
            // Closing the TUN/TAP file destroys the device in the kernel.
            drop(self._tun.take());
            // Wait for the device to finish unregistering.
            for _ in 0..50 {
                // SAFETY: self.name is a valid null-terminated C string.
                if unsafe { libc::if_nametoindex(self.name.as_ptr()) } == 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }

    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    enum SocketTestKind {
        Udp,
        UdpMulticast,
        TcpListener,
        TcpConnected,
    }

    impl SocketTestKind {
        fn sk_type(&self) -> u32 {
            match self {
                SocketTestKind::Udp | SocketTestKind::UdpMulticast => SOCK_DGRAM as u32,
                SocketTestKind::TcpListener | SocketTestKind::TcpConnected => SOCK_STREAM as u32,
            }
        }

        fn protocol(&self) -> u32 {
            match self {
                SocketTestKind::Udp | SocketTestKind::UdpMulticast => IPPROTO_UDP as u32,
                SocketTestKind::TcpListener | SocketTestKind::TcpConnected => IPPROTO_TCP as u32,
            }
        }
    }

    fn tcp_connect_with_mark(addr: std::net::SocketAddr, mark: u32) -> std::net::TcpStream {
        let sin;
        let sin6;
        let (domain, sockaddr_ptr, addrlen): (_, *const libc::sockaddr, _) = match addr {
            std::net::SocketAddr::V4(v4) => {
                sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as u16,
                    sin_port: v4.port().to_be(),
                    sin_addr: libc::in_addr { s_addr: u32::from_ne_bytes(v4.ip().octets()) },
                    sin_zero: [0; 8],
                };
                (
                    libc::AF_INET,
                    &sin as *const _ as *const libc::sockaddr,
                    std::mem::size_of_val(&sin) as libc::socklen_t,
                )
            }
            std::net::SocketAddr::V6(v6) => {
                sin6 = libc::sockaddr_in6 {
                    sin6_family: libc::AF_INET6 as u16,
                    sin6_port: v6.port().to_be(),
                    sin6_flowinfo: v6.flowinfo(),
                    sin6_addr: libc::in6_addr { s6_addr: v6.ip().octets() },
                    sin6_scope_id: v6.scope_id(),
                };
                (
                    libc::AF_INET6,
                    &sin6 as *const _ as *const libc::sockaddr,
                    std::mem::size_of_val(&sin6) as libc::socklen_t,
                )
            }
        };

        // SAFETY: Creating a TCP socket with valid domain, type, and protocol.
        let fd = unsafe { libc::socket(domain, SOCK_STREAM, 0) };
        assert!(fd >= 0, "Failed to create TCP socket: {:?}", std::io::Error::last_os_error());
        // SAFETY: `fd` is valid and non-negative.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        setsockopt(fd.as_fd(), SOL_SOCKET, SO_MARK, &mark.to_ne_bytes())
            .expect("Failed to set SO_MARK");

        // SAFETY: `sockaddr_ptr` points to a valid sockaddr_in or sockaddr_in6 of length `addrlen`.
        let res = unsafe { libc::connect(fd.as_raw_fd(), sockaddr_ptr, addrlen) };
        assert_eq!(res, 0, "Failed to connect TCP socket: {:?}", std::io::Error::last_os_error());
        std::net::TcpStream::from(fd)
    }

    #[test_case(IpFamily::V4, IpFamily::V4, SocketTestKind::Udp; "udp_ipv4")]
    #[test_case(IpFamily::V4, IpFamily::V4, SocketTestKind::UdpMulticast; "udp_ipv4_multicast")]
    #[test_case(IpFamily::V6, IpFamily::V4, SocketTestKind::Udp; "udp_dual_stack_ipv4")]
    #[test_case(IpFamily::V6, IpFamily::V4, SocketTestKind::UdpMulticast; "udp_dual_stack_ipv4_multicast")]
    #[test_case(IpFamily::V6, IpFamily::V6, SocketTestKind::Udp; "udp_dual_stack_ipv6")]
    #[test_case(IpFamily::V6, IpFamily::V6, SocketTestKind::UdpMulticast; "udp_dual_stack_ipv6_multicast")]
    #[test_case(IpFamily::V4, IpFamily::V4, SocketTestKind::TcpConnected; "tcp_ipv4")]
    #[test_case(IpFamily::V6, IpFamily::V4, SocketTestKind::TcpConnected; "tcp_dual_stack_ipv4")]
    #[test_case(IpFamily::V6, IpFamily::V6, SocketTestKind::TcpConnected; "tcp_dual_stack_ipv6")]
    #[serial]
    fn ebpf_egress(socket_family: IpFamily, packet_family: IpFamily, kind: SocketTestKind) {
        root_required!();

        let mut maps = MapSet::new();
        let program = LoadedProgram::new(
            "skb_test_prog",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_CGROUP_SKB,
            linux_uapi::bpf_attach_type_BPF_CGROUP_INET_EGRESS,
            &mut maps,
        );

        // Check that the ring buffer is not signalled initially.
        let signaled = pollfd(maps.ringbuf(), libc::POLLIN, Duration::ZERO)
            .expect("Failed to poll ringbuffer FD");
        assert!(signaled == None);

        let _attached = program.attach();

        const MARK: u32 = 100;
        let (_sockets, expected_ifindex): (Box<dyn std::any::Any>, _) = match kind {
            SocketTestKind::Udp => {
                let socket =
                    UdpSocket::bind(socket_family.any_addr()).expect("Failed to create UDP socket");
                setsockopt(socket.as_fd(), SOL_SOCKET, SO_MARK, &MARK.to_ne_bytes())
                    .expect("Failed to set SO_MARK");
                let cookie = get_socket_cookie(socket.as_fd()).expect("Failed to get SO_COOKIE");
                maps.set_target_cookie(cookie);

                // Send a UDP packet.
                socket
                    .send_to(&[1, 2, 3], (packet_family.localhost_addr().ip(), 12345))
                    .expect("Failed to send UDP packet");
                (Box::new(socket), get_loopback_ifindex())
            }
            SocketTestKind::UdpMulticast => {
                let test_if = TestInterface::new();
                let socket =
                    UdpSocket::bind(socket_family.any_addr()).expect("Failed to create UDP socket");
                setsockopt(socket.as_fd(), libc::SOL_SOCKET, libc::SO_MARK, &MARK.to_ne_bytes())
                    .expect("Failed to set SO_MARK");
                test_if.bind_socket_to_device(socket.as_fd());
                let cookie = get_socket_cookie(socket.as_fd()).expect("Failed to get SO_COOKIE");
                maps.set_target_cookie(cookie);

                // Send a UDP packet.
                socket
                    .send_to(&[1, 2, 3], (packet_family.multicast_addr().ip(), 12345))
                    .expect("Failed to send UDP packet");
                let ifindex = test_if.ifindex;
                (Box::new((socket, test_if)), ifindex)
            }
            SocketTestKind::TcpConnected => {
                let listener = std::net::TcpListener::bind(socket_family.any_addr())
                    .expect("Failed to create TCP listener");
                let listener_addr = listener.local_addr().expect("Failed to get listener addr");
                let connect_addr = std::net::SocketAddr::new(
                    packet_family.localhost_addr().ip(),
                    listener_addr.port(),
                );
                let client =
                    std::net::TcpStream::connect(connect_addr).expect("Failed to connect TCP");
                let (mut accepted, _) = listener.accept().expect("Failed to accept TCP");
                setsockopt(accepted.as_fd(), SOL_SOCKET, SO_MARK, &MARK.to_ne_bytes())
                    .expect("Failed to set SO_MARK");
                let cookie = get_socket_cookie(accepted.as_fd()).expect("Failed to get SO_COOKIE");
                maps.set_target_cookie(cookie);

                accepted.write_all(&[1, 2, 3]).expect("Failed to send TCP packet");
                (Box::new((listener, client, accepted)), get_loopback_ifindex())
            }
            SocketTestKind::TcpListener => unreachable!(),
        };

        // The ring buffer FD should be signalled by the program.
        let signaled = pollfd(maps.ringbuf(), libc::POLLIN, Duration::MAX)
            .expect("Failed to poll ringbuffer FD");
        assert!(signaled == Some(libc::POLLIN));

        let test_result = maps.get_test_result();
        assert_eq!(test_result.ether_type, u16::to_be(packet_family.ether_type() as u16) as u32);
        assert_eq!(test_result.ifindex, expected_ifindex);
        assert_eq!(test_result.mark, MARK);
        assert_eq!(test_result.sk_type, kind.sk_type());
        assert_eq!(test_result.sk_protocol, kind.protocol());
        assert_eq!(test_result.sk_family, socket_family.family() as u32);
        assert_eq!(test_result.pkt_type, linux_uapi::PACKET_OUTGOING);

        // SAFETY: `getuid()` is always safe to call.
        let uid = unsafe { libc::getuid() };
        assert_eq!(test_result.socket_uid, uid);
    }

    // TODO(https://fxbug.dev/371573022): Add `udp_multicast_dual_stack_ipv4` once
    // Netstack3 implements support for IPv4 multicast in dual-stack sockets.
    #[test_case(IpFamily::V4, IpFamily::V4, SocketTestKind::Udp; "udp_ipv4")]
    #[test_case(IpFamily::V4, IpFamily::V4, SocketTestKind::UdpMulticast; "udp_multicast_ipv4")]
    #[test_case(IpFamily::V6, IpFamily::V4, SocketTestKind::Udp; "udp_dual_stack_ipv4")]
    #[test_case(IpFamily::V6, IpFamily::V6, SocketTestKind::Udp; "udp_dual_stack_ipv6")]
    #[test_case(IpFamily::V6, IpFamily::V6, SocketTestKind::UdpMulticast; "udp_multicast_dual_stack_ipv6")]
    #[test_case(IpFamily::V4, IpFamily::V4, SocketTestKind::TcpListener; "tcp_listener_ipv4")]
    #[test_case(IpFamily::V6, IpFamily::V4, SocketTestKind::TcpListener; "tcp_listener_dual_stack_ipv4")]
    #[test_case(IpFamily::V6, IpFamily::V6, SocketTestKind::TcpListener; "tcp_listener_dual_stack_ipv6")]
    #[test_case(IpFamily::V4, IpFamily::V4, SocketTestKind::TcpConnected; "tcp_connected_ipv4")]
    #[test_case(IpFamily::V6, IpFamily::V4, SocketTestKind::TcpConnected; "tcp_connected_dual_stack_ipv4")]
    #[test_case(IpFamily::V6, IpFamily::V6, SocketTestKind::TcpConnected; "tcp_connected_dual_stack_ipv6")]
    #[serial]
    fn ebpf_ingress(socket_family: IpFamily, packet_family: IpFamily, kind: SocketTestKind) {
        root_required!();

        let mut maps = MapSet::new();
        let program = LoadedProgram::new(
            "skb_test_prog",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_CGROUP_SKB,
            linux_uapi::bpf_attach_type_BPF_CGROUP_INET_INGRESS,
            &mut maps,
        );

        let _attached = program.attach();

        const SEND_MARK: u32 = 100;
        const RECV_MARK: u32 = 200;

        let (_sockets, expected_ifindex): (Box<dyn std::any::Any>, _) = match kind {
            SocketTestKind::Udp => {
                // Setup a listening UDP socket with RECV_MARK.
                let recv_socket =
                    UdpSocket::bind(socket_family.any_addr()).expect("Failed to create UDP socket");
                setsockopt(recv_socket.as_fd(), SOL_SOCKET, SO_MARK, &RECV_MARK.to_ne_bytes())
                    .expect("Failed to set SO_MARK");
                let recv_addr = recv_socket.local_addr().expect("Failed to get local socket addr");

                let cookie =
                    get_socket_cookie(recv_socket.as_fd()).expect("Failed to get SO_COOKIE");
                maps.set_target_cookie(cookie);

                // Send a UDP packet with SEND_MARK.
                let send_socket = UdpSocket::bind(packet_family.localhost_addr())
                    .expect("Failed to create UDP socket");
                setsockopt(send_socket.as_fd(), SOL_SOCKET, SO_MARK, &SEND_MARK.to_ne_bytes())
                    .expect("Failed to set SO_MARK");
                let send_to_addr = std::net::SocketAddr::new(
                    packet_family.localhost_addr().ip(),
                    recv_addr.port(),
                );
                send_socket.send_to(&[1, 2, 3], send_to_addr).expect("Failed to send UDP packet");
                (Box::new((recv_socket, send_socket)), get_loopback_ifindex())
            }
            SocketTestKind::UdpMulticast => {
                let test_if = TestInterface::new();
                let recv_socket =
                    UdpSocket::bind(socket_family.any_addr()).expect("Failed to create UDP socket");
                setsockopt(recv_socket.as_fd(), SOL_SOCKET, SO_MARK, &RECV_MARK.to_ne_bytes())
                    .expect("Failed to set SO_MARK");
                let recv_addr = recv_socket.local_addr().expect("Failed to get local socket addr");

                let mcast_ip = packet_family.multicast_addr().ip();
                match mcast_ip {
                    std::net::IpAddr::V4(mcast_v4) => {
                        recv_socket
                            .join_multicast_v4(&mcast_v4, &test_if.ipv4_addr)
                            .expect("Failed to join multicast v4");
                    }
                    std::net::IpAddr::V6(mcast_v6) => {
                        recv_socket
                            .join_multicast_v6(&mcast_v6, test_if.ifindex)
                            .expect("Failed to join multicast v6");
                    }
                }

                let cookie =
                    get_socket_cookie(recv_socket.as_fd()).expect("Failed to get SO_COOKIE");
                maps.set_target_cookie(cookie);

                // Send a UDP packet with SEND_MARK.
                let send_socket =
                    UdpSocket::bind(packet_family.any_addr()).expect("Failed to create UDP socket");
                setsockopt(send_socket.as_fd(), SOL_SOCKET, SO_MARK, &SEND_MARK.to_ne_bytes())
                    .expect("Failed to set SO_MARK");
                test_if.bind_socket_to_device(send_socket.as_fd());
                let send_to_addr = std::net::SocketAddr::new(mcast_ip, recv_addr.port());
                send_socket.send_to(&[1, 2, 3], send_to_addr).expect("Failed to send UDP packet");
                let ifindex = test_if.ifindex;
                (Box::new((recv_socket, send_socket, test_if)), ifindex)
            }
            SocketTestKind::TcpListener => {
                // Setup a listening TCP socket with RECV_MARK.
                let listener = std::net::TcpListener::bind(socket_family.any_addr())
                    .expect("Failed to create TCP listener");
                setsockopt(listener.as_fd(), SOL_SOCKET, SO_MARK, &RECV_MARK.to_ne_bytes())
                    .expect("Failed to set SO_MARK");
                let recv_addr = listener.local_addr().expect("Failed to get local socket addr");

                let cookie = get_socket_cookie(listener.as_fd()).expect("Failed to get SO_COOKIE");
                maps.set_target_cookie(cookie);

                // Connect to the listener from a client socket with SEND_MARK.
                let connect_addr = std::net::SocketAddr::new(
                    packet_family.localhost_addr().ip(),
                    recv_addr.port(),
                );
                let client = tcp_connect_with_mark(connect_addr, SEND_MARK);
                (Box::new((listener, client)), get_loopback_ifindex())
            }
            SocketTestKind::TcpConnected => {
                let listener = std::net::TcpListener::bind(socket_family.any_addr())
                    .expect("Failed to create TCP listener");
                let recv_addr = listener.local_addr().expect("Failed to get local socket addr");
                let connect_addr = std::net::SocketAddr::new(
                    packet_family.localhost_addr().ip(),
                    recv_addr.port(),
                );
                let mut client =
                    std::net::TcpStream::connect(connect_addr).expect("Failed to connect TCP");
                let (accepted, _) = listener.accept().expect("Failed to accept TCP");

                setsockopt(accepted.as_fd(), SOL_SOCKET, SO_MARK, &RECV_MARK.to_ne_bytes())
                    .expect("Failed to set SO_MARK");
                setsockopt(client.as_fd(), SOL_SOCKET, SO_MARK, &SEND_MARK.to_ne_bytes())
                    .expect("Failed to set SO_MARK");

                let cookie = get_socket_cookie(accepted.as_fd()).expect("Failed to get SO_COOKIE");
                maps.set_target_cookie(cookie);

                client.write_all(&[1, 2, 3]).expect("Failed to send TCP packet");
                (Box::new((listener, client, accepted)), get_loopback_ifindex())
            }
        };

        // The ring buffer FD should be signalled by the program.
        let signaled = pollfd(maps.ringbuf(), libc::POLLIN, Duration::MAX)
            .expect("Failed to poll ringbuffer FD");
        assert!(signaled == Some(libc::POLLIN));

        let test_result = maps.get_test_result();
        assert_eq!(test_result.ether_type, u16::to_be(packet_family.ether_type() as u16) as u32);
        assert_eq!(test_result.ifindex, expected_ifindex);
        // On loopback ingress, __sk_buff.mark should reflect the packet's SO_MARK (SEND_MARK)
        // rather than being overwritten by the destination socket's SO_MARK (RECV_MARK).
        assert_eq!(test_result.mark, SEND_MARK);
        assert_eq!(test_result.sk_type, kind.sk_type());
        assert_eq!(test_result.sk_protocol, kind.protocol());
        assert_eq!(test_result.sk_family, socket_family.family() as u32);
        let expected_pkt_type = match kind {
            SocketTestKind::UdpMulticast => linux_uapi::PACKET_LOOPBACK,
            SocketTestKind::Udp | SocketTestKind::TcpListener | SocketTestKind::TcpConnected => {
                linux_uapi::PACKET_HOST
            }
        };
        assert_eq!(test_result.pkt_type, expected_pkt_type);

        // SAFETY: `getuid()` is always safe to call.
        let uid = unsafe { libc::getuid() };
        assert_eq!(test_result.socket_uid, uid);
    }

    #[test]
    #[serial]
    fn ebpf_sock_create() {
        root_required!();

        let mut maps = MapSet::new();
        let program = LoadedProgram::new(
            "sock_create_prog",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_CGROUP_SOCK,
            linux_uapi::bpf_attach_type_BPF_CGROUP_INET_SOCK_CREATE,
            &mut maps,
        );
        let _attached = program.attach();

        // Verify that the counter is incremented when a new socket is created.
        let last_count = maps.get_count();
        let initial_variable = maps.get_global_variables();

        let _socket = UdpSocket::bind("0.0.0.0:0").expect("Failed to create UDP socket");

        let new_count = maps.get_count();
        let end_variable = maps.get_global_variables();

        assert!(new_count - last_count >= 1);
        let test_result = maps.get_test_result();
        assert_eq!(test_result.sk_type, SOCK_DGRAM as u32);
        assert_eq!(test_result.sk_protocol, IPPROTO_UDP as u32);
        assert_eq!(test_result.sk_family, libc::AF_INET as u32);
        assert_eq!(test_result.sk_state, linux_uapi::BPF_TCP_CLOSE);
        assert!(end_variable.global_counter1 - initial_variable.global_counter1 >= 1);
        assert!(end_variable.global_counter2 - initial_variable.global_counter2 >= 2);
        assert!(initial_variable.global_counter2 >= 2 * initial_variable.global_counter1);
        assert!(end_variable.global_counter2 >= 2 * end_variable.global_counter1);
    }

    // This test assumes that `close()` will destroy the corresponding object
    // before returning. This assumption may be violated if another threads
    // forks the current process (as in that case the socket FD gets duped to
    // the new process). The test is marked `serial` to workaround this issue.
    #[test]
    #[serial]
    fn sock_release_prog() {
        root_required!();

        let mut maps = MapSet::new();
        let program = LoadedProgram::new(
            "sock_release_prog",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_CGROUP_SOCK,
            linux_uapi::bpf_attach_type_BPF_CGROUP_INET_SOCK_RELEASE,
            &mut maps,
        );

        let socket = UdpSocket::bind("0.0.0.0:0").expect("Failed to create UDP socket");
        let cookie = get_socket_cookie(socket.as_fd()).expect("Failed to get SO_COOKIE");
        maps.set_target_cookie(cookie);

        let _attached = program.attach();

        // Verify that the counter is incremented when a new socket is released.
        let last_count = maps.get_count();
        std::mem::drop(socket);
        let new_count = maps.get_count();
        assert_eq!(new_count, last_count + 1);

        let test_result = maps.get_test_result();
        assert_eq!(test_result.sk_type, SOCK_DGRAM as u32);
        assert_eq!(test_result.sk_protocol, IPPROTO_UDP as u32);
        assert_eq!(test_result.sk_family, libc::AF_INET as u32);
        assert_eq!(test_result.sk_state, linux_uapi::BPF_TCP_CLOSE);

        // SAFETY: These libc functions are safe to call.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        assert_eq!(test_result.uid_gid & 0xFFFFFFFF, uid as u64);
        assert_eq!(test_result.uid_gid >> 32, gid as u64);

        assert_eq!(test_result.pid_tgid & 0xFFFFFFFF, gettid() as u64);
        assert_eq!(test_result.pid_tgid >> 32, std::process::id() as u64);
    }

    #[test]
    #[serial]
    fn ebpf_setsockopt() {
        root_required!();

        let mut maps = MapSet::new();
        let program = LoadedProgram::new(
            "setsockopt_prog",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_CGROUP_SOCKOPT,
            linux_uapi::bpf_attach_type_BPF_CGROUP_SETSOCKOPT,
            &mut maps,
        );
        let _attached = program.attach();

        let (socket, _peer) = UnixStream::pair().expect("Failed to create UNIX socket");

        let set_test_sock_opt =
            |optval| setsockopt(socket.as_fd(), SOL_SOCKET, TEST_SOCK_OPT, optval);

        // Set TEST_SOCK_OPT.
        let optval = vec![0; 10000];
        assert!(set_test_sock_opt(&optval).is_ok());

        // Since the `optval` is larger than one page the program will get
        // only the first page.
        let test_result = maps.get_test_result();
        assert_eq!(test_result.optlen, 10000);
        assert_eq!(test_result.optval_size, getpagesize() as u64);
        assert_eq!(test_result.sk_type, SOCK_STREAM as u32);
        assert_eq!(test_result.sk_protocol, 0);
        assert_eq!(test_result.sk_family, libc::AF_UNIX as u32);
        assert_eq!(test_result.sk_state, linux_uapi::BPF_TCP_ESTABLISHED);

        // Verify setsockopt on a listening TCP socket.
        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("Failed to bind TCP listener");
        assert!(setsockopt(listener.as_fd(), SOL_SOCKET, TEST_SOCK_OPT, &optval).is_ok());
        let test_result = maps.get_test_result();
        assert_eq!(test_result.sk_type, SOCK_STREAM as u32);
        assert_eq!(test_result.sk_protocol, IPPROTO_TCP as u32);
        assert_eq!(test_result.sk_family, libc::AF_INET as u32);
        assert_eq!(test_result.sk_state, linux_uapi::BPF_TCP_LISTEN);

        // Verify setsockopt on a connected TCP socket.
        let stream = std::net::TcpStream::connect(listener.local_addr().unwrap())
            .expect("Failed to connect TCP stream");
        assert!(setsockopt(stream.as_fd(), SOL_SOCKET, TEST_SOCK_OPT, &optval).is_ok());
        let test_result = maps.get_test_result();
        assert_eq!(test_result.sk_type, SOCK_STREAM as u32);
        assert_eq!(test_result.sk_protocol, IPPROTO_TCP as u32);
        assert_eq!(test_result.sk_family, libc::AF_INET as u32);
        assert!(test_result.sk_state == linux_uapi::BPF_TCP_ESTABLISHED);

        // Verify setsockopt on an accepted TCP socket.
        let (accepted_stream, _peer_addr) = listener.accept().expect("Failed to accept TCP stream");
        assert!(setsockopt(accepted_stream.as_fd(), SOL_SOCKET, TEST_SOCK_OPT, &optval).is_ok());
        let test_result = maps.get_test_result();
        assert_eq!(test_result.sk_type, SOCK_STREAM as u32);
        assert_eq!(test_result.sk_protocol, IPPROTO_TCP as u32);
        assert_eq!(test_result.sk_family, libc::AF_INET as u32);
        assert_eq!(test_result.sk_state, linux_uapi::BPF_TCP_ESTABLISHED);

        // Try again with the `optval[0]=1`. The program will set `optlen`
        // above buffer size, which should result in `EFAULT`.
        let optval = vec![1; 10];
        let err = set_test_sock_opt(&optval).expect_err("setsockopt expected to fail");
        assert_eq!(err.raw_os_error(), Some(libc::EFAULT));

        // The program rejects calls with `optval[0]=2`. `EPERM` is expected.
        let optval = vec![2; 10];
        let err = set_test_sock_opt(&optval).expect_err("setsockopt expected to fail");
        assert_eq!(err.raw_os_error(), Some(libc::EPERM));

        let set_sndbuf = |optval| setsockopt(socket.as_fd(), SOL_SOCKET, SO_SNDBUF, optval);

        let get_rcvbuf = || {
            let v =
                getsockopt(socket.as_fd(), SOL_SOCKET, SO_SNDBUF, 4).expect("getsockopt failed");
            libc::socklen_t::read_from_bytes(&v).expect("getsockopt returned invalid result")
        };

        // Try setting an option that's not handled by the program.
        let buffer_size: libc::socklen_t = 4096;
        assert!(set_sndbuf(buffer_size.as_bytes()).is_ok());
        assert_eq!(get_rcvbuf(), buffer_size * 2);

        // Try the same option with a larger buffer.
        let mut large_optval = vec![0; 10000];
        large_optval[0..4].copy_from_slice(buffer_size.as_bytes());
        assert!(set_sndbuf(&large_optval).is_ok());
        assert_eq!(get_rcvbuf(), buffer_size * 2);

        // The test program overrides rcvbuf=55555 with rcvbuf=66666.
        let buffer_size: libc::socklen_t = 55555;
        assert!(set_sndbuf(buffer_size.as_bytes()).is_ok());
        assert_eq!(get_rcvbuf(), 66666 * 2);
    }

    #[test]
    #[serial]
    fn ebpf_getsockopt() {
        root_required!();

        let mut maps = MapSet::new();
        let program = LoadedProgram::new(
            "getsockopt_prog",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_CGROUP_SOCKOPT,
            linux_uapi::bpf_attach_type_BPF_CGROUP_GETSOCKOPT,
            &mut maps,
        );
        let _attached = program.attach();

        let (socket, _peer) = UnixStream::pair().expect("Failed to create UNIX socket");

        let get_test_sock_opt =
            |optlen| getsockopt(socket.as_fd(), SOL_SOCKET, TEST_SOCK_OPT, optlen);

        // If the syscall fails and eBPF doesn't change `retval` then the
        // original error is returned.
        let err = get_test_sock_opt(2).expect_err("getsockopt expected to fail");
        assert_eq!(err.raw_os_error(), Some(libc::ENOPROTOOPT));

        // Verify that `bpf_sockopt.retval` is set correctly and
        // `bpf_get_retval()` returns the same value.
        let test_result = maps.get_test_result();
        assert_eq!(test_result.retval, -libc::ENOPROTOOPT);
        assert_eq!(test_result.get_retval, -libc::ENOPROTOOPT);
        assert_eq!(test_result.sk_type, SOCK_STREAM as u32);
        assert_eq!(test_result.sk_protocol, 0);
        assert_eq!(test_result.sk_family, libc::AF_UNIX as u32);
        assert_eq!(test_result.sk_state, linux_uapi::BPF_TCP_ESTABLISHED);

        // The original error is still returned if the program returns 0.
        let err = get_test_sock_opt(3).expect_err("getsockopt expected to fail");
        assert_eq!(err.raw_os_error(), Some(libc::ENOPROTOOPT));

        // eBPF program can override the result.
        let result = get_test_sock_opt(4).expect("getsockopt failed");
        assert_eq!(result, vec![42, 0, 0, 0]);

        // `optlen` cannot be set to -1.
        let err = get_test_sock_opt(5).expect_err("getsockopt expected to fail");
        assert_eq!(err.raw_os_error(), Some(libc::ENOPROTOOPT));

        let get_rcvbuf = |optlen| getsockopt(socket.as_fd(), SOL_SOCKET, SO_SNDBUF, optlen);

        // Verify that eBPF program can override the returned value.
        let result = get_rcvbuf(55).expect("getsockopt failed");
        assert_eq!(result.len(), 8);
        assert_eq!(u64::from_ne_bytes(result.try_into().unwrap()), 0x1234567890abcdef);

        // EPERM is returned if the program rejects the call by returning 0.
        let err = get_rcvbuf(56).expect_err("getsockopt expected to fail");
        assert_eq!(err.raw_os_error(), Some(libc::EPERM));

        // EFAULT is returned to the user if the program set `optlen` to -1.
        let err = get_rcvbuf(57).expect_err("getsockopt expected to fail");
        assert_eq!(err.raw_os_error(), Some(libc::EFAULT));

        // If the program changes retval then it's returned to the userspace.
        let err = get_rcvbuf(58).expect_err("getsockopt expected to fail");
        assert_eq!(err.raw_os_error(), Some(5));

        // Retval set by `bpf_set_retval` should be returned to the caller.
        let err = get_rcvbuf(59).expect_err("getsockopt expected to fail");
        assert_eq!(err.raw_os_error(), Some(5));

        // Set SO_SNDBUF for the two tests below.
        let buf_size: u32 = 65536;
        setsockopt(socket.as_fd(), SOL_SOCKET, SO_SNDBUF, &buf_size.to_ne_bytes())
            .expect("setsockopt(SO_SNDBUF)");

        // Original value returned if program just returns 1.
        let result = get_rcvbuf(60).expect("getsockopt failed");
        assert_eq!(result, (buf_size * 2).to_ne_bytes());

        // Original value returned if program sets `optlen = 0`.
        let result = get_rcvbuf(61).expect("getsockopt failed");
        assert_eq!(result, (buf_size * 2).to_ne_bytes());
    }

    #[test]
    #[serial]
    fn ebpf_udp_recv() {
        root_required!();

        let mut maps = MapSet::new();
        let program = LoadedProgram::new(
            "udprecv6_prog",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_CGROUP_SOCK_ADDR,
            linux_uapi::bpf_attach_type_BPF_CGROUP_UDP6_RECVMSG,
            &mut maps,
        );

        let recv_socket = UdpSocket::bind("[::]:0").expect("Failed to create UPD socket");
        let cookie = get_socket_cookie(recv_socket.as_fd()).expect("Failed to get SO_COOKIE");
        maps.set_target_cookie(cookie);

        let _attached = program.attach();

        let send_socket = UdpSocket::bind("127.0.0.1:0").expect("Failed to create UPD socket");

        // Send an IPv4 packet.
        let dst_port = recv_socket.local_addr().expect("Failed to get local ip").port();
        send_socket
            .send_to(&[1, 2, 3], ("127.0.0.1", dst_port))
            .expect("Failed to send UDP packet");

        // The program shouldn't be called until `recv()`.
        assert_eq!(maps.get_count(), 0);

        let mut buf = [0; 10];
        recv_socket.recv(&mut buf).expect("Failed to receive a UDP packet");

        assert_eq!(maps.get_count(), 1);
        let test_result = maps.get_test_result();
        let src_port = send_socket.local_addr().expect("Failed to get local ip").port();
        assert_eq!(test_result.sockaddr_port, src_port.to_be() as u32);
        assert_eq!(test_result.sockaddr_family, linux_uapi::AF_INET6);
        assert_eq!(test_result.sockaddr_ip, [0, 0, 0xffff0000, 0x0100007F]);
        assert_eq!(test_result.sk_type, SOCK_DGRAM as u32);
        assert_eq!(test_result.sk_protocol, IPPROTO_UDP as u32);
        assert_eq!(test_result.sk_family, linux_uapi::AF_INET6);
        assert_eq!(test_result.sk_state, linux_uapi::BPF_TCP_CLOSE);
    }

    // The following tests both attach eBPF programs to
    // `BPF_CGROUP_UDP4_SENDMSG`, so they have to be marked as `serial` to
    // avoid conflicts.
    #[test]
    #[serial]
    fn ebpf_udp_send() {
        root_required!();

        let mut maps = MapSet::new();
        let program = LoadedProgram::new(
            "udpsend4_prog",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_CGROUP_SOCK_ADDR,
            linux_uapi::bpf_attach_type_BPF_CGROUP_UDP4_SENDMSG,
            &mut maps,
        );

        let _attached = program.attach();

        let send_socket = UdpSocket::bind("127.0.0.1:0").expect("Failed to create UDP socket");

        let dst_port = 65535;

        // sendto() should still succeed.
        send_socket
            .send_to(&[1, 2, 3], ("127.0.0.1", dst_port))
            .expect("Failed to send UDP packet");

        // sendto() should be blocked once we set target socket cookie.
        let cookie = get_socket_cookie(send_socket.as_fd()).expect("Failed to get SO_COOKIE");
        maps.set_target_cookie(cookie);

        let err = send_socket
            .send_to(&[1, 2, 3], ("127.0.0.1", 65535))
            .expect_err("sendto expected to fail");
        assert_eq!(err.raw_os_error(), Some(libc::EPERM));

        assert_eq!(maps.get_count(), 1);
        let test_result = maps.get_test_result();
        assert_eq!(test_result.sockaddr_port, dst_port.to_be() as u32);
        assert_eq!(test_result.sockaddr_family, linux_uapi::AF_INET);
        assert_eq!(test_result.sockaddr_ip[0], 0x0100007F);
        assert_eq!(test_result.sk_type, SOCK_DGRAM as u32);
        assert_eq!(test_result.sk_protocol, IPPROTO_UDP as u32);
        assert_eq!(test_result.sk_family, libc::AF_INET as u32);
        assert_eq!(test_result.sk_state, linux_uapi::BPF_TCP_CLOSE);
    }

    #[test]
    #[serial]
    fn ebpf_udp_send_connected() {
        root_required!();

        let mut maps = MapSet::new();
        let program = LoadedProgram::new(
            "udpsend4_prog",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_CGROUP_SOCK_ADDR,
            linux_uapi::bpf_attach_type_BPF_CGROUP_UDP4_SENDMSG,
            &mut maps,
        );

        let _attached = program.attach();

        let recv_socket = UdpSocket::bind("127.0.0.1:0").expect("Failed to create UDP socket");
        let dst_port = recv_socket.local_addr().expect("Failed to get local ip").port();

        let send_socket = UdpSocket::bind("127.0.0.1:0").expect("Failed to create UDP socket");
        send_socket.connect(("127.0.0.1", dst_port)).expect("Failed to connect");

        let cookie = get_socket_cookie(send_socket.as_fd()).expect("Failed to get SO_COOKIE");
        maps.set_target_cookie(cookie);

        // `send()` should still succeed - the program is invoked only for
        // `sendmsg()` and `sendto()`.
        send_socket.send(&[1, 2, 3]).expect("Failed to send UDP packet");
    }

    #[test]
    #[serial]
    fn ebpf_sk_storage() {
        root_required!();

        let mut maps = MapSet::new();
        let sock_create_program = LoadedProgram::new(
            "sock_create_sk_storage_prog",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_CGROUP_SOCK,
            linux_uapi::bpf_attach_type_BPF_CGROUP_INET_SOCK_CREATE,
            &mut maps,
        );
        let _attached = sock_create_program.attach();

        let setsockopt_program = LoadedProgram::new(
            "setsockopt_sk_storage_prog",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_CGROUP_SOCKOPT,
            linux_uapi::bpf_attach_type_BPF_CGROUP_SETSOCKOPT,
            &mut maps,
        );
        let _attached = setsockopt_program.attach();

        let connect_program = LoadedProgram::new(
            "connect_sk_storage_prog",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_CGROUP_SOCK_ADDR,
            linux_uapi::bpf_attach_type_BPF_CGROUP_INET4_CONNECT,
            &mut maps,
        );
        let _attached = connect_program.attach();

        let socket = UdpSocket::bind("127.0.0.1:0").expect("Failed to create UDP socket");

        // Trigger setsockopt.
        assert!(setsockopt(socket.as_fd(), SOL_SOCKET, TEST_SOCK_OPT, &[0; 4]).is_ok());

        // Trigger connect.
        socket.connect("127.0.0.1:12345").expect("udp connect failed");

        let sk_storage_map_fd = maps.find("sk_storage_map", bpf_map_type_BPF_MAP_TYPE_SK_STORAGE);
        let value: i32 = bpf_map_lookup_elem(sk_storage_map_fd, socket.as_raw_fd())
            .expect("Failed to test_result");

        // The final value should be 1 + 2 + 4 = 7.
        const EXPECTED_VALUE: i32 = 7;
        assert_eq!(value, EXPECTED_VALUE);
    }

    #[test]
    #[serial]
    fn ebpf_ringbuf_reserve_overflow() {
        root_required!();

        let mut maps = MapSet::new();
        let program = LoadedProgram::new(
            "test_ringbuf_reserve_overflow_sock",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_CGROUP_SOCK,
            linux_uapi::bpf_attach_type_BPF_CGROUP_INET_SOCK_CREATE,
            &mut maps,
        );

        let _attached = program.attach();

        let _socket = UdpSocket::bind("0.0.0.0:0").expect("Failed to create UDP socket");

        let test_result = maps.get_test_result();

        // The program is expected to set retval to 1, indicating that
        // ringbuf_reserve failed as expected.
        assert_eq!(test_result.retval, 1);
    }

    #[test]
    #[serial]
    fn ebpf_sk_storage_clone() {
        root_required!();

        let clone_map_fd = bpf_map_create(&MapDefinition {
            name: Some("sk_storage_clone_map".into()),
            schema: ebpf::MapSchema {
                map_type: bpf_map_type_BPF_MAP_TYPE_SK_STORAGE,
                key_size: 4,
                value_size: 4,
                max_entries: 0,
                flags: ebpf::MapFlags::Clone,
            },
        })
        .expect("Failed to create clone map");

        let no_clone_map_fd = bpf_map_create(&MapDefinition {
            name: Some("sk_storage_no_clone_map".into()),
            schema: ebpf::MapSchema {
                map_type: bpf_map_type_BPF_MAP_TYPE_SK_STORAGE,
                key_size: 4,
                value_size: 4,
                max_entries: 0,
                flags: ebpf::MapFlags::empty(),
            },
        })
        .expect("Failed to create no-clone map");

        let listener =
            std::net::TcpListener::bind("127.0.0.1:0").expect("Failed to bind TCP listener");
        let listener_addr = listener.local_addr().expect("Failed to get local addr");

        bpf_map_update_elem(clone_map_fd.as_fd(), listener.as_raw_fd(), 42u32)
            .expect("Failed to update clone map");
        bpf_map_update_elem(no_clone_map_fd.as_fd(), listener.as_raw_fd(), 100u32)
            .expect("Failed to update no-clone map");

        let _client = std::net::TcpStream::connect(listener_addr).expect("Failed to connect");
        let (accepted_socket, _) = listener.accept().expect("Failed to accept");

        // The clone map should have the value cloned from the listener.
        let val: u32 = bpf_map_lookup_elem(clone_map_fd.as_fd(), accepted_socket.as_raw_fd())
            .expect("Failed to lookup clone map elem");
        assert_eq!(val, 42);

        // The no-clone map should not have an entry for the accepted socket.
        assert!(
            bpf_map_lookup_elem::<_, u32>(no_clone_map_fd.as_fd(), accepted_socket.as_raw_fd())
                .is_err()
        );

        // Mutating the accepted socket's storage should not affect the listener.
        bpf_map_update_elem(clone_map_fd.as_fd(), accepted_socket.as_raw_fd(), 43u32)
            .expect("Failed to update clone map on accepted socket");
        let listener_val: u32 = bpf_map_lookup_elem(clone_map_fd.as_fd(), listener.as_raw_fd())
            .expect("Failed to lookup clone map on listener");
        assert_eq!(listener_val, 42);
        let accepted_val: u32 =
            bpf_map_lookup_elem(clone_map_fd.as_fd(), accepted_socket.as_raw_fd())
                .expect("Failed to lookup clone map on accepted");
        assert_eq!(accepted_val, 43);
    }

    #[test]
    #[serial]
    fn ebpf_read_only_prog_map() {
        root_required!();

        let mut maps = MapSet::new();

        // Program reading from lookup pointer of read-only map must pass
        // verification.
        let _read_prog = LoadedProgram::try_new(
            "test_read_only_prog_map_read",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_SOCKET_FILTER,
            0,
            &mut maps,
        )
        .expect("reading from read-only map must pass verification");

        // Userspace write (update) should succeed on ProgReadOnly map.
        let ro_map_fd = maps.find(RO_ARRAY_MAP_NAME, linux_uapi::bpf_map_type_BPF_MAP_TYPE_ARRAY);
        let key = 0u32;
        let value = [0x55u8; 64];
        bpf_map_update_elem(ro_map_fd, key, value).expect("Userspace update should succeed");

        // Userspace read (lookup) should succeed and match the written value.
        let read_value: [u8; 64] =
            bpf_map_lookup_elem(ro_map_fd, key).expect("Userspace lookup should succeed");
        assert_eq!(value, read_value);

        // Program using a read-only hash map should also succeed.
        let _hash_prog = LoadedProgram::try_new(
            "test_read_only_prog_hash_map_read",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_SOCKET_FILTER,
            0,
            &mut maps,
        )
        .expect("program using read-only hash map must load successfully");
        let _ro_hash_fd = maps.find(RO_HASH_MAP_NAME, linux_uapi::bpf_map_type_BPF_MAP_TYPE_HASH);
    }

    #[test]
    #[serial]
    fn ebpf_read_only_prog_mmap() {
        root_required!();

        let mut maps = MapSet::new();
        let _mmap_prog = LoadedProgram::try_new(
            "test_read_only_prog_mmap",
            linux_uapi::bpf_prog_type_BPF_PROG_TYPE_SOCKET_FILTER,
            0,
            &mut maps,
        )
        .expect("program using mmapable read-only map must load successfully");

        let map_fd = maps.find(RO_MMAP_MAP_NAME, linux_uapi::bpf_map_type_BPF_MAP_TYPE_ARRAY);
        let page_size = getpagesize();

        // SAFETY: Calling mmap() with valid arguments.
        let call_mmap = |prot| unsafe {
            libc::mmap(null_mut(), page_size, prot, libc::MAP_SHARED, map_fd.as_raw_fd(), 0)
        };

        // 1. Writable mmap should fail with EACCES.
        let res = call_mmap(libc::PROT_READ | libc::PROT_WRITE);
        assert_eq!(res, libc::MAP_FAILED);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EACCES));

        // 2. Read-only mmap should succeed.
        let addr = call_mmap(libc::PROT_READ);
        assert_ne!(addr, libc::MAP_FAILED);

        // 3. mprotect to writable should fail with EACCES.
        // SAFETY: Calling mprotect() with valid arguments.
        let mprotect_res =
            unsafe { libc::mprotect(addr, page_size, libc::PROT_READ | libc::PROT_WRITE) };
        assert_eq!(mprotect_res, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EACCES));

        // SAFETY: Calling munmap() with valid arguments.
        let munmap_res = unsafe { libc::munmap(addr, page_size) };
        assert_eq!(munmap_res, 0);
    }

    #[test]
    #[serial]
    fn ebpf_read_only_prog_map_invalid_types() {
        root_required!();

        // SK_STORAGE with BPF_F_RDONLY_PROG should fail with EINVAL.
        let invalid_sk_result = bpf_map_create(&MapDefinition {
            name: Some("invalid_ro_sk".into()),
            schema: ebpf::MapSchema {
                map_type: linux_uapi::bpf_map_type_BPF_MAP_TYPE_SK_STORAGE,
                key_size: 4,
                value_size: 4,
                max_entries: 0,
                flags: ebpf::MapFlags::ProgReadOnly | ebpf::MapFlags::NoPrealloc,
            },
        });
        assert_eq!(invalid_sk_result.unwrap_err().raw_os_error(), Some(libc::EINVAL));

        // RINGBUF with BPF_F_RDONLY_PROG should fail with EINVAL.
        let invalid_ringbuf_result = bpf_map_create(&MapDefinition {
            name: Some("invalid_ro_ringbuf".into()),
            schema: ebpf::MapSchema {
                map_type: linux_uapi::bpf_map_type_BPF_MAP_TYPE_RINGBUF,
                key_size: 0,
                value_size: 0,
                max_entries: 4096,
                flags: ebpf::MapFlags::ProgReadOnly,
            },
        });
        assert_eq!(invalid_ringbuf_result.unwrap_err().raw_os_error(), Some(libc::EINVAL));
    }
}
