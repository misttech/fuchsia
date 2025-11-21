// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::maps::{Map, MapKey, MapValueRef, RingBuffer, RingBufferWakeupPolicy};
use ebpf::{BpfValue, EbpfHelperImpl, EbpfProgramContext, FromBpfValue, HelperSet};
use inspect_stubs::track_stub;
use linux_uapi::{
    bpf_func_id_BPF_FUNC_get_current_pid_tgid, bpf_func_id_BPF_FUNC_get_current_uid_gid,
    bpf_func_id_BPF_FUNC_get_smp_processor_id, bpf_func_id_BPF_FUNC_get_socket_cookie,
    bpf_func_id_BPF_FUNC_get_socket_uid, bpf_func_id_BPF_FUNC_ktime_get_boot_ns,
    bpf_func_id_BPF_FUNC_ktime_get_coarse_ns, bpf_func_id_BPF_FUNC_ktime_get_ns,
    bpf_func_id_BPF_FUNC_map_delete_elem, bpf_func_id_BPF_FUNC_map_lookup_elem,
    bpf_func_id_BPF_FUNC_map_update_elem, bpf_func_id_BPF_FUNC_probe_read_str,
    bpf_func_id_BPF_FUNC_probe_read_user, bpf_func_id_BPF_FUNC_probe_read_user_str,
    bpf_func_id_BPF_FUNC_ringbuf_discard, bpf_func_id_BPF_FUNC_ringbuf_reserve,
    bpf_func_id_BPF_FUNC_ringbuf_submit, bpf_func_id_BPF_FUNC_sk_fullsock,
    bpf_func_id_BPF_FUNC_sk_storage_get, bpf_func_id_BPF_FUNC_skb_load_bytes_relative,
    bpf_func_id_BPF_FUNC_trace_printk, gid_t, pid_t, uid_t,
};
use std::slice;

pub trait MapsContext<'a> {
    fn on_map_access(&mut self, map: &Map);
    fn add_value_ref(&mut self, map_ref: MapValueRef<'a>);
}

pub trait MapsProgramContext: EbpfProgramContext {
    fn on_map_access(context: &mut Self::RunContext<'_>, map: &Map);
    fn add_value_ref<'a>(context: &mut Self::RunContext<'a>, map_ref: MapValueRef<'a>);
}

impl<C: EbpfProgramContext> MapsProgramContext for C
where
    for<'a> C::RunContext<'a>: MapsContext<'a>,
{
    fn on_map_access(context: &mut Self::RunContext<'_>, map: &Map) {
        context.on_map_access(map);
    }

    fn add_value_ref<'a>(context: &mut Self::RunContext<'a>, map_ref: MapValueRef<'a>) {
        context.add_value_ref(map_ref);
    }
}

fn bpf_map_lookup_elem<'a, C: MapsProgramContext>(
    context: &mut C::RunContext<'a>,
    map: BpfValue,
    key: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    // SAFETY: The `map` must be a reference to a `Map` object kept alive by the program itself.
    let map: &Map = unsafe { &*map.as_ptr::<Map>() };
    // SAFETY: safety is ensured by the verifier.
    let key =
        unsafe { std::slice::from_raw_parts(key.as_ptr::<u8>(), map.schema.key_size as usize) };

    C::on_map_access(context, map);

    let Some(value_ref) = map.lookup(key) else {
        return BpfValue::default();
    };

    let result: BpfValue = value_ref.ptr().raw_ptr().into();

    // If this is a map with ref-counted elements then save the reference for
    // the lifetime of the program.
    if value_ref.is_ref_counted() {
        C::add_value_ref(context, value_ref);
    }

    result
}

fn bpf_map_update_elem<C: MapsProgramContext>(
    context: &mut C::RunContext<'_>,
    map: BpfValue,
    key: BpfValue,
    value: BpfValue,
    flags: BpfValue,
    _: BpfValue,
) -> BpfValue {
    // SAFETY: The `map` must be a reference to a `Map` object kept alive by the program itself.
    let map: &Map = unsafe { &*map.as_ptr::<Map>() };
    // SAFETY: safety is ensured by the verifier.
    let key =
        unsafe { std::slice::from_raw_parts(key.as_ptr::<u8>(), map.schema.key_size as usize) };
    // SAFETY: safety is ensured by the verifier.
    let value =
        unsafe { std::slice::from_raw_parts(value.as_ptr::<u8>(), map.schema.value_size as usize) };
    let flags = flags.as_u64();

    let key = MapKey::from_slice(key);

    C::on_map_access(context, map);

    map.update(key, value, flags).map(|_| 0).unwrap_or(u64::MAX).into()
}

fn bpf_map_delete_elem<C: MapsProgramContext>(
    context: &mut C::RunContext<'_>,
    map: BpfValue,
    key: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    // SAFETY: The `map` must be a reference to a `Map` object kept alive by the program itself.
    let map: &Map = unsafe { &*map.as_ptr::<Map>() };
    // SAFETY: safety is ensured by the verifier.
    let key =
        unsafe { std::slice::from_raw_parts(key.as_ptr::<u8>(), map.schema.key_size as usize) };

    C::on_map_access(context, map);

    map.delete(key).map(|_| 0).unwrap_or(u64::MAX).into()
}

fn bpf_trace_printk<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _fmt: BpfValue,
    _fmt_size: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_trace_printk");
    0.into()
}

fn bpf_ktime_get_ns<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    zx::MonotonicInstant::get().into_nanos().into()
}

fn bpf_ringbuf_reserve<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    map: BpfValue,
    size: BpfValue,
    flags: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    // SAFETY: The safety of the operation is ensured by the bpf verifier. The `map` must be a
    // reference to a `Map` object kept alive by the program itself.
    let map: &Map = unsafe { &*map.as_ptr::<Map>() };
    let size = u32::from(size);
    let flags = u64::from(flags);
    map.ringbuf_reserve(size, flags).map(BpfValue::from).unwrap_or_else(|_| BpfValue::default())
}

fn bpf_ringbuf_submit<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    data: BpfValue,
    flags: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    let flags = RingBufferWakeupPolicy::from(u32::from(flags));

    // SAFETY: The safety of the operation is ensured by the bpf verifier. The data has to come from
    // the result of a reserve call.
    unsafe {
        RingBuffer::submit(u64::from(data), flags);
    }
    0.into()
}

fn bpf_ringbuf_discard<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    data: BpfValue,
    flags: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    let flags = RingBufferWakeupPolicy::from(u32::from(flags));

    // SAFETY: The safety of the operation is ensured by the bpf verifier. The data has to come from
    // the result of a reserve call.
    unsafe {
        RingBuffer::discard(u64::from(data), flags);
    }
    0.into()
}

fn bpf_ktime_get_boot_ns<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_ktime_get_boot_ns");
    0.into()
}

fn bpf_probe_read_user<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_probe_read_user");
    0.into()
}

fn bpf_probe_read_user_str<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_probe_read_user_str");
    0.into()
}

fn bpf_ktime_get_coarse_ns<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_ktime_get_coarse_ns");
    0.into()
}

fn bpf_probe_read_str<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_probe_read_str");
    0.into()
}

fn bpf_get_smp_processor_id<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_get_smp_processor_id");
    0.into()
}

pub trait CurrentTaskContext {
    fn get_uid_gid(&self) -> (uid_t, gid_t);
    fn get_tid_tgid(&self) -> (pid_t, pid_t);
}

pub trait CurrentTaskCompatibleProgramContext: EbpfProgramContext {
    fn get_uid_gid<'a>(context: &mut Self::RunContext<'a>) -> (uid_t, gid_t);
    fn get_tid_tgid<'a>(context: &mut Self::RunContext<'a>) -> (pid_t, pid_t);
}

impl<C: EbpfProgramContext> CurrentTaskCompatibleProgramContext for C
where
    for<'a> C::RunContext<'a>: CurrentTaskContext,
{
    fn get_uid_gid<'a>(context: &mut Self::RunContext<'a>) -> (uid_t, gid_t) {
        context.get_uid_gid()
    }
    fn get_tid_tgid<'a>(context: &mut Self::RunContext<'a>) -> (pid_t, pid_t) {
        context.get_tid_tgid()
    }
}

fn bpf_get_current_uid_gid<C: CurrentTaskCompatibleProgramContext>(
    context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    let (uid, gid) = C::get_uid_gid(context);
    (uid as u64 + (gid as u64) << 32).into()
}

fn bpf_get_current_pid_tgid<C: CurrentTaskCompatibleProgramContext>(
    context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    let (pid, tgid) = C::get_tid_tgid(context);
    (pid as u64 + (tgid as u64) << 32).into()
}

pub trait SocketCookieContext<A> {
    fn get_socket_cookie(&self, arg: A) -> u64;
}

pub trait SocketCookieCompatibleProgramContext: EbpfProgramContext {
    fn get_socket_cookie<'a>(context: &mut Self::RunContext<'a>, arg: BpfValue) -> u64;
}

impl<C: EbpfProgramContext> SocketCookieCompatibleProgramContext for C
where
    for<'b, 'c> C::RunContext<'b>: SocketCookieContext<C::Arg1<'c>>,
    for<'b> C::Arg1<'b>: FromBpfValue<C::RunContext<'b>>,
{
    fn get_socket_cookie<'a>(context: &mut Self::RunContext<'a>, arg: BpfValue) -> u64 {
        // SAFETY: Verifier checks that the argument points at the same value
        // that was passed to the program as the first argument.
        let arg = unsafe { C::Arg1::from_bpf_value(context, arg) };
        context.get_socket_cookie(arg)
    }
}

fn bpf_get_socket_cookie<'a, C: SocketCookieCompatibleProgramContext>(
    context: &mut C::RunContext<'a>,
    arg: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    C::get_socket_cookie(context, arg).into()
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum LoadBytesBase {
    MacHeader,
    NetworkHeader,
}

pub trait SocketFilterContext<B>: SocketCookieContext<B> {
    fn get_socket_uid(&self, sk_buf: B) -> Option<uid_t>;
    fn load_bytes_relative(
        &self,
        sk_buf: B,
        base: LoadBytesBase,
        offset: usize,
        buf: &mut [u8],
    ) -> i64;
}

// Trait for EbpfProgramContext that is compatible with socket filter. The
// default blanket implementation is provided for all `EbpfProgramContext`
// where `RunContext` implements `SocketFilterContext`.
pub trait SocketFilterCompatibleProgramContext: EbpfProgramContext {
    fn get_socket_uid<'a>(context: &mut Self::RunContext<'a>, sk_buf: BpfValue) -> Option<uid_t>;
    fn skb_load_bytes_relative<'a>(
        context: &mut Self::RunContext<'a>,
        sk_buf: BpfValue,
        base: LoadBytesBase,
        offset: usize,
        buf: &mut [u8],
    ) -> i64;
}

impl<C: EbpfProgramContext> SocketFilterCompatibleProgramContext for C
where
    for<'b, 'c> C::RunContext<'b>: SocketFilterContext<C::Arg1<'c>>,
    for<'b> C::Arg1<'b>: FromBpfValue<C::RunContext<'b>>,
{
    fn get_socket_uid<'a>(context: &mut Self::RunContext<'a>, sk_buf: BpfValue) -> Option<uid_t> {
        // SAFETY: Verifier checks that the argument points at the same value
        // that was passed to the program as the first argument.
        let sk_buf = unsafe { C::Arg1::from_bpf_value(context, sk_buf) };
        context.get_socket_uid(sk_buf)
    }

    fn skb_load_bytes_relative<'a>(
        context: &mut Self::RunContext<'a>,
        sk_buf: BpfValue,
        base: LoadBytesBase,
        offset: usize,
        buf: &mut [u8],
    ) -> i64 {
        // SAFETY: Verifier checks that the argument points at the same value
        // that was passed to the program as the first argument.
        let sk_buf = unsafe { C::Arg1::from_bpf_value(context, sk_buf) };
        context.load_bytes_relative(sk_buf, base, offset, buf)
    }
}

fn bpf_get_socket_uid<'a, C: SocketFilterProgramContext>(
    context: &mut C::RunContext<'a>,
    sk_buf: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    const OVERFLOW_UID: uid_t = 65534;
    C::get_socket_uid(context, sk_buf).unwrap_or(OVERFLOW_UID).into()
}

fn bpf_skb_load_bytes_relative<'a, C: SocketFilterProgramContext>(
    context: &mut C::RunContext<'a>,
    sk_buf: BpfValue,
    offset: BpfValue,
    to: BpfValue,
    len: BpfValue,
    start_header: BpfValue,
) -> BpfValue {
    let base = match start_header.as_u32() {
        0 => LoadBytesBase::MacHeader,
        1 => LoadBytesBase::NetworkHeader,
        _ => return u64::MAX.into(),
    };

    let Ok(offset) = offset.as_u64().try_into() else {
        return u64::MAX.into();
    };

    // SAFETY: The verifier ensures that `to` points to a valid buffer of at
    // least `len` bytes that the eBPF program has permission to access.
    let buf = unsafe { slice::from_raw_parts_mut(to.as_ptr::<u8>(), len.as_u64() as usize) };

    C::skb_load_bytes_relative(context, sk_buf, base, offset, buf).into()
}

fn bpf_sk_storage_get<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_sk_storage_get");
    0.into()
}

fn bpf_sk_fullsock<C: EbpfProgramContext>(
    _context: &mut C::RunContext<'_>,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
    _: BpfValue,
) -> BpfValue {
    track_stub!(TODO("https://fxbug.dev/287120494"), "bpf_sk_fullsock");
    0.into()
}

fn get_common_helpers<C: MapsProgramContext>() -> impl Iterator<Item = (u32, EbpfHelperImpl<C>)> {
    [
        (bpf_func_id_BPF_FUNC_ktime_get_boot_ns, EbpfHelperImpl(bpf_ktime_get_boot_ns)),
        (bpf_func_id_BPF_FUNC_ktime_get_coarse_ns, EbpfHelperImpl(bpf_ktime_get_coarse_ns)),
        (bpf_func_id_BPF_FUNC_ktime_get_ns, EbpfHelperImpl(bpf_ktime_get_ns)),
        (bpf_func_id_BPF_FUNC_map_delete_elem, EbpfHelperImpl(bpf_map_delete_elem)),
        (bpf_func_id_BPF_FUNC_map_lookup_elem, EbpfHelperImpl(bpf_map_lookup_elem)),
        (bpf_func_id_BPF_FUNC_map_update_elem, EbpfHelperImpl(bpf_map_update_elem)),
        (bpf_func_id_BPF_FUNC_probe_read_str, EbpfHelperImpl(bpf_probe_read_str)),
        (bpf_func_id_BPF_FUNC_probe_read_user, EbpfHelperImpl(bpf_probe_read_user)),
        (bpf_func_id_BPF_FUNC_probe_read_user_str, EbpfHelperImpl(bpf_probe_read_user_str)),
        (bpf_func_id_BPF_FUNC_ringbuf_discard, EbpfHelperImpl(bpf_ringbuf_discard)),
        (bpf_func_id_BPF_FUNC_ringbuf_reserve, EbpfHelperImpl(bpf_ringbuf_reserve)),
        (bpf_func_id_BPF_FUNC_ringbuf_submit, EbpfHelperImpl(bpf_ringbuf_submit)),
        (bpf_func_id_BPF_FUNC_trace_printk, EbpfHelperImpl(bpf_trace_printk)),
        (bpf_func_id_BPF_FUNC_get_smp_processor_id, EbpfHelperImpl(bpf_get_smp_processor_id)),
    ]
    .into_iter()
}

/// Returns helper implementations that depend on `CurrentTask`.
pub fn get_current_task_helpers<C: CurrentTaskCompatibleProgramContext>()
-> impl Iterator<Item = (u32, EbpfHelperImpl<C>)> {
    [
        (bpf_func_id_BPF_FUNC_get_current_uid_gid, EbpfHelperImpl(bpf_get_current_uid_gid)),
        (bpf_func_id_BPF_FUNC_get_current_pid_tgid, EbpfHelperImpl(bpf_get_current_pid_tgid)),
    ]
    .into_iter()
}

// Trait for `EbpfProgramContext` implementations that are used for
// `BPF_PROG_TYPE_CGROUP_SOCK` programs.
pub trait CgroupSockProgramContext:
    MapsProgramContext + SocketCookieCompatibleProgramContext + CurrentTaskCompatibleProgramContext
{
    fn get_helpers() -> HelperSet<Self> {
        [
            (bpf_func_id_BPF_FUNC_get_socket_cookie, EbpfHelperImpl(bpf_get_socket_cookie)),
            (bpf_func_id_BPF_FUNC_sk_storage_get, EbpfHelperImpl(bpf_sk_storage_get)),
        ]
        .into_iter()
        .chain(get_common_helpers())
        .chain(get_current_task_helpers())
        .collect()
    }
}

// Trait for `EbpfProgramContext` implementations that are used for
// `BPF_PROG_TYPE_CGROUP_SOCKADDR` programs.
pub trait CgroupSockAddrProgramContext:
    MapsProgramContext + SocketCookieCompatibleProgramContext + CurrentTaskCompatibleProgramContext
{
    fn get_helpers() -> HelperSet<Self> {
        [
            (bpf_func_id_BPF_FUNC_get_socket_cookie, EbpfHelperImpl(bpf_get_socket_cookie)),
            (bpf_func_id_BPF_FUNC_sk_storage_get, EbpfHelperImpl(bpf_sk_storage_get)),
        ]
        .into_iter()
        .chain(get_common_helpers())
        .chain(get_current_task_helpers())
        .collect()
    }
}

// Trait for `EbpfProgramContext` implementations that are used for
// `BPF_PROG_TYPE_CGROUP_SOCKOPT` programs.
pub trait CgroupSockOptProgramContext:
    MapsProgramContext + CurrentTaskCompatibleProgramContext
{
    fn get_helpers() -> HelperSet<Self> {
        get_common_helpers().chain(get_current_task_helpers()).collect()
    }
}

// Trait for `EbpfProgramContext` implementations that are used for socket filter programs.
pub trait SocketFilterProgramContext:
    MapsProgramContext + SocketFilterCompatibleProgramContext + SocketCookieCompatibleProgramContext
{
    fn get_helpers() -> HelperSet<Self> {
        [
            (bpf_func_id_BPF_FUNC_get_socket_uid, EbpfHelperImpl(bpf_get_socket_uid)),
            (bpf_func_id_BPF_FUNC_get_socket_cookie, EbpfHelperImpl(bpf_get_socket_cookie)),
            (
                bpf_func_id_BPF_FUNC_skb_load_bytes_relative,
                EbpfHelperImpl(bpf_skb_load_bytes_relative),
            ),
            (bpf_func_id_BPF_FUNC_sk_storage_get, EbpfHelperImpl(bpf_sk_storage_get)),
            (bpf_func_id_BPF_FUNC_sk_fullsock, EbpfHelperImpl(bpf_sk_fullsock)),
        ]
        .into_iter()
        .chain(get_common_helpers())
        .collect()
    }
}

/// Macro used to declare program type for a `EbpfProgramContext` implementation.
/// Implements `StaticHelperSet` trait for the context type.
///
/// # Example
///
/// The following example declares that `MyEbpfProgramContext` is used to run
/// socket filter programs:
///
/// ```
/// ebpf_program_context_type!(MyEbpfProgramContext, SocketFilterProgramContext);
/// ```
#[macro_export]
macro_rules! ebpf_program_context_type {
    ($context:ty, $subtrait:ty) => {
        impl $subtrait for $context {}
        ebpf::static_helper_set!($context, <$context as $subtrait>::get_helpers());
    };
}
