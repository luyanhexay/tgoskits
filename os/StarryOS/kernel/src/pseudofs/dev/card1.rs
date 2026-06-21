use alloc::borrow::Cow;
use core::{
    any::Any,
    convert::TryFrom,
    ffi::{CStr, c_char, c_ulong},
    mem,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    task::Context,
};

use ax_driver::rknpu::{self, RknpuAction, RknpuMemCreate, RknpuMemMap, RknpuMemSync, RknpuSubmit};
use ax_errno::{AxError, AxResult};
use ax_memory_addr::PhysAddrRange;
use ax_runtime::hal::{cpu::asm::user_copy, mem::virt_to_phys, time::monotonic_time_nanos};
use axfs_ng_vfs::{DeviceId, NodeFlags, VfsError, VfsResult};
use axpoll::{IoEvents, Pollable};
use linux_raw_sys::general::O_CLOEXEC;

use super::rknpu_drm::DrmVersion;
use crate::{
    file::FileLike,
    pseudofs::{
        DeviceOps,
        dev::rknpu_drm::{io_size, ioctl_nr, is_driver_ioctl},
        device::DeviceMmap,
    },
};

/// Driver name for DRM device
const DRM1_NAME: &CStr = c"rknpu";
/// Driver date for DRM device
const DRM1_DATE: &CStr = c"20240828";
/// Driver description for DRM device
const DRM1_DESC: &CStr = c"RKNPU driver";

/// Device ID for /dev/dri/card1
pub const CARD1_SYSTEM_DEVICE_ID: DeviceId = DeviceId::new(0xe2, 1);

/// Page shift constant (4KB pages)
const PAGE_SHIFT: u32 = 12;
/// Maximum ioctl command number
const MAX_IOCTL_NR: u32 = 0xcf;
/// Stack data buffer size
const STACK_DATA_SIZE: usize = 128;
/// DRM ioctl version command number
const DRM_IOCTL_VERSION_NR: u32 = 0;
/// DRM ioctl get unique command number
const DRM_IOCTL_GET_UNIQUE_NR: u32 = 1;
/// DRM ioctl gem flink command number
const DRM_IOCTL_GEM_FLINK_NR: u32 = 10;
/// DRM ioctl prime handle to fd command number
const DRM_IOCTL_PRIME_HANDLE_TO_FD_NR: u32 = 0x2d;
const RKNPU_ACTION_LOG_LIMIT: usize = 16;
const RKNPU_MEM_CREATE_LOG_LIMIT: usize = 16;
const RKNPU_MEM_SYNC_LOG_LIMIT: usize = 32;
const RKNPU_SUBMIT_LOG_LIMIT: usize = 16;
static RKNPU_ACTION_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static RKNPU_MEM_CREATE_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static RKNPU_MEM_SYNC_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);
static RKNPU_SUBMIT_LOG_COUNT: AtomicUsize = AtomicUsize::new(0);

// EXEC-3: partition the per-inference wall time across ioctl types + userspace
// gaps. EXEC-2 (§27) settled that the NPU hardware is busy only ~9.6% (≈22ms of
// 211ms); the other ~189ms is software. This breaks that ~189ms down by *where*
// it lives: inside each ioctl type's in-kernel handling (submit / mem_sync /
// mem_create / ...) vs the userspace gap between two consecutive rknpu ioctls
// (librknnrt processing — the kernel is not even on the CPU then). Pure reads of
// monotonic_time_nanos(); a cumulative summary printed ONCE every N driver
// ioctls (never per-call), so it never pollutes the measured runs. Indexed by
// the RknpuCmd discriminant 0..=5 (Action..MemSync).
const RKNPU_IOCTL_SUMMARY_EVERY: u64 = 4096;
const RKNPU_CMD_KINDS: usize = 6;
static RKNPU_IOCTL_TOTAL: AtomicU64 = AtomicU64::new(0);
static RKNPU_LAST_EXIT_NS: AtomicU64 = AtomicU64::new(0);
static RKNPU_GAP_NS: AtomicU64 = AtomicU64::new(0);
static RKNPU_GAP_COUNT: AtomicU64 = AtomicU64::new(0);
static RKNPU_CMD_COUNT: [AtomicU64; RKNPU_CMD_KINDS] =
    [const { AtomicU64::new(0) }; RKNPU_CMD_KINDS];
static RKNPU_CMD_NS: [AtomicU64; RKNPU_CMD_KINDS] = [const { AtomicU64::new(0) }; RKNPU_CMD_KINDS];

// EXEC-4c: per-inference "burst" timer + mid-run frequency raise, to measure the
// CPU-frequency speedup before/after within a SINGLE boot (no loop/transfer, just
// reliable single npu_smoke runs). librknnrt issues ~53 submits clustered inside
// one rknn_run, then a longer gap (outputs_get/inputs_set/init). We time each
// cluster's first→last submit span — freq-sensitive, since the inter-submit
// userspace gaps shrink as the CPU speeds up — and print it when the next cluster
// starts. After RAISE_AFTER_SUBMITS submits (~1 inference at boot freq) we raise
// the A76 clusters to max and re-probe, so successive bursts print slow (boot
// freq) then fast (raised).
const BURST_GAP_NS: u64 = 30_000_000; // >30ms idle between submits ⇒ inference boundary
const RAISE_AFTER_SUBMITS: u64 = 60;
static SUBMIT_LAST_NS: AtomicU64 = AtomicU64::new(0);
static SUBMIT_BURST_START_NS: AtomicU64 = AtomicU64::new(0);
static SUBMIT_BURST_COUNT: AtomicU64 = AtomicU64::new(0);
static SUBMIT_TOTAL: AtomicU64 = AtomicU64::new(0);
static FREQ_RAISED: AtomicBool = AtomicBool::new(false);

/// Human-readable name for an `RknpuCmd` discriminant, for the EXEC-3 summary.
fn rknpu_cmd_name(idx: usize) -> &'static str {
    match idx {
        0 => "action",
        1 => "submit",
        2 => "mem_create",
        3 => "mem_map",
        4 => "mem_destroy",
        5 => "mem_sync",
        _ => "?",
    }
}

/// EXEC-3 accounting: fold one completed driver ioctl into the cumulative
/// partition (per-cmd in-kernel time + userspace gap since the previous ioctl),
/// and print a cumulative summary every `RKNPU_IOCTL_SUMMARY_EVERY` ioctls. The
/// print happens outside any poll/hot loop, so it does not perturb timing.
fn account_rknpu_ioctl(op: RknpuCmd, entry_ns: u64, exit_ns: u64) {
    let idx = op as usize;
    if idx < RKNPU_CMD_KINDS {
        RKNPU_CMD_COUNT[idx].fetch_add(1, Ordering::Relaxed);
        RKNPU_CMD_NS[idx].fetch_add(exit_ns.saturating_sub(entry_ns), Ordering::Relaxed);
    }

    // Userspace gap = time from the previous ioctl's exit to this one's entry.
    let last_exit = RKNPU_LAST_EXIT_NS.swap(exit_ns, Ordering::Relaxed);
    if last_exit != 0 && entry_ns > last_exit {
        RKNPU_GAP_NS.fetch_add(entry_ns - last_exit, Ordering::Relaxed);
        RKNPU_GAP_COUNT.fetch_add(1, Ordering::Relaxed);
    }

    // EXEC-4c: per-inference burst timing + mid-run frequency raise (Submit only).
    if op == RknpuCmd::Submit {
        let last = SUBMIT_LAST_NS.swap(entry_ns, Ordering::Relaxed);
        if last == 0 {
            SUBMIT_BURST_START_NS.store(entry_ns, Ordering::Relaxed);
            SUBMIT_BURST_COUNT.store(1, Ordering::Relaxed);
        } else if entry_ns.saturating_sub(last) > BURST_GAP_NS {
            // Previous cluster ended at `last`; report its first→last span.
            let cnt = SUBMIT_BURST_COUNT.swap(1, Ordering::Relaxed);
            let start = SUBMIT_BURST_START_NS.swap(entry_ns, Ordering::Relaxed);
            if cnt >= 10 {
                let span_us = last.saturating_sub(start) / 1000;
                warn!(
                    "rknn burst: {} submits span {} us ({} us/submit)",
                    cnt,
                    span_us,
                    span_us / cnt
                );
            }
        } else {
            SUBMIT_BURST_COUNT.fetch_add(1, Ordering::Relaxed);
        }

        // After ~1 inference at the boot clock, raise the A76 clusters once and
        // re-probe, so later bursts print at the raised clock — before/after in
        // one boot.
        let sn = SUBMIT_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
        if sn >= RAISE_AFTER_SUBMITS && !FREQ_RAISED.swap(true, Ordering::Relaxed) {
            rknpu::set_cpu_clusters_max();
            measure_and_log_cpu_mhz();
        }
    }

    let n = RKNPU_IOCTL_TOTAL.fetch_add(1, Ordering::Relaxed) + 1;
    if !n.is_multiple_of(RKNPU_IOCTL_SUMMARY_EVERY) {
        return;
    }

    // kernel_us = Σ in-kernel time over all cmd types; gap_us = Σ userspace gaps.
    // kernel_us + gap_us ≈ total wall spent in the rknpu ioctl stream, so the
    // ratios say exactly where the ~189ms goes.
    let mut kernel_us = 0u64;
    for i in 0..RKNPU_CMD_KINDS {
        let cnt = RKNPU_CMD_COUNT[i].load(Ordering::Relaxed);
        if cnt == 0 {
            continue;
        }
        let total_us = RKNPU_CMD_NS[i].load(Ordering::Relaxed) / 1000;
        kernel_us += total_us;
        // warn! (not info!) so the partition table stays visible under the
        // board's measurement config `log="Warn"`, which suppresses the heavy
        // per-syscall Info spam that otherwise overflows the UART and corrupts
        // the console. Printed only every RKNPU_IOCTL_SUMMARY_EVERY ioctls.
        warn!(
            "rknpu part: {:>11} count={} total_us={} avg_us={}",
            rknpu_cmd_name(i),
            cnt,
            total_us,
            total_us / cnt,
        );
    }
    let gap_us = RKNPU_GAP_NS.load(Ordering::Relaxed) / 1000;
    let gap_cnt = RKNPU_GAP_COUNT.load(Ordering::Relaxed);
    let wall_us = kernel_us + gap_us;
    let kernel_permille = if wall_us > 0 {
        kernel_us.saturating_mul(1000) / wall_us
    } else {
        0
    };
    warn!(
        "rknpu part: TOTAL ioctls={} kernel_us={} gap_us={} (gap_cnt={} avg_us={}) wall_us={} \
         kernel={}.{}% gap={}.{}%",
        n,
        kernel_us,
        gap_us,
        gap_cnt,
        if gap_cnt > 0 { gap_us / gap_cnt } else { 0 },
        wall_us,
        kernel_permille / 10,
        kernel_permille % 10,
        (1000 - kernel_permille) / 10,
        (1000 - kernel_permille) % 10,
    );
}

/// DRM_IOCTL_VERSION ioctl argument type
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct DrmUnique {
    /// Length of unique string identifier
    pub unique_len: c_ulong,
    /// Pointer to user-space buffer holding unique name for driver
    /// instantiation
    pub unique: *mut c_char,
}

/// RKNPU command types
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RknpuCmd {
    /// Action command
    Action     = 0x00,
    /// Submit command
    Submit     = 0x01,
    /// Memory create command
    MemCreate  = 0x02,
    /// Memory map command
    MemMap     = 0x03,
    /// Memory destroy command
    MemDestroy = 0x04,
    /// Memory sync command
    MemSync    = 0x05,
}

impl TryFrom<u32> for RknpuCmd {
    type Error = ();

    /// Tries to convert a u32 value to an RknpuCmd
    fn try_from(nr: u32) -> Result<Self, Self::Error> {
        match nr {
            0x00 | 0x40 => Ok(RknpuCmd::Action),
            0x01 | 0x41 => Ok(RknpuCmd::Submit),
            0x02 | 0x42 => Ok(RknpuCmd::MemCreate),
            0x03 | 0x43 => Ok(RknpuCmd::MemMap),
            0x04 | 0x44 => Ok(RknpuCmd::MemDestroy),
            0x05 | 0x45 => Ok(RknpuCmd::MemSync),
            _ => {
                warn!("Unknown ioctl nr: {nr:#x}",);
                Err(())
            }
        }
    }
}

/// Represents an RKNPU user action with flags and value
#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct RknpuUserAction {
    /// Action flags
    pub flags: RknpuAction,
    /// Action value
    pub value: u32,
}

impl RknpuUserAction {
    /// Creates a new RknpuUserAction with default values
    pub fn default() -> Self {
        Self {
            flags: RknpuAction::GetDrvVersion,
            value: 0,
        }
    }
}

/// DRM card1 device implementation
pub struct Card1;

impl Card1 {
    /// Creates a new /dev/dri/card1 device.
    pub fn new() -> Card1 {
        Self
    }
}

impl Default for Card1 {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceOps for Card1 {
    /// Reads data from the device (not supported for card1)
    fn read_at(&self, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        trace!("dri: read_at called");
        // card1 heap devices are not meant to be read directly
        Err(VfsError::InvalidInput)
    }

    /// Writes data to the device (not supported for card1)
    fn write_at(&self, _buf: &[u8], _offset: u64) -> VfsResult<usize> {
        trace!("dri: write_at called");
        // card1 heap devices are not meant to be written directly
        Err(VfsError::InvalidInput)
    }

    /// Handles ioctl commands for the device
    fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        if arg == 0 {
            warn!("[rknpu]: ioctl received null arg pointer");
            return Err(VfsError::InvalidData);
        }
        let nr = ioctl_nr(cmd);
        debug!("card1: cmd {cmd:#x}, nr {nr:#x}, arg {arg:#x}");

        let is_driver_ioctl = is_driver_ioctl(ioctl_nr(cmd));
        debug!("card1: is_driver_ioctl = {}", is_driver_ioctl);

        if is_driver_ioctl {
            if let Ok(op) = RknpuCmd::try_from(nr) {
                rknpu_driver_ioctl(op, arg)?;
            } else {
                warn!("Unknown RKNPU cmd: {:#x}", cmd);
                return Err(VfsError::NotATty);
            }
        } else {
            assert!(nr <= MAX_IOCTL_NR, "card1: unsupported ioctl nr {nr}");
            let mut stack_data = [0u8; STACK_DATA_SIZE];

            let in_size = io_size(cmd) as usize;
            let out_size = in_size;

            copy_from_user(stack_data.as_mut_ptr(), arg as _, in_size)?;
            match nr {
                DRM_IOCTL_VERSION_NR => {
                    debug!("drm get version");
                    drm_version(&mut stack_data)?;
                }
                DRM_IOCTL_GET_UNIQUE_NR => {
                    debug!("drm get unique");
                    drm_get_unique(&mut stack_data)?;
                }
                DRM_IOCTL_GEM_FLINK_NR => {
                    drm_gem_flink_ioctl(&mut stack_data)?;
                }
                DRM_IOCTL_PRIME_HANDLE_TO_FD_NR => {
                    drm_prime_handle_to_fd_ioctl(&mut stack_data)?;
                }

                _ => {
                    panic!("card1: unsupported ioctl nr {nr:#x}");
                }
            }
            copy_to_user(arg as _, stack_data.as_mut_ptr(), out_size)?;
        }

        Ok(0)
    }

    /// Returns a reference to the object as Any for dynamic type checking
    fn as_any(&self) -> &dyn Any {
        self
    }

    /// Returns the node flags for the device
    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }

    /// Maps an exported GEM buffer selected by `handle << PAGE_SHIFT`.
    fn mmap(&self, offset: u64, _length: u64) -> DeviceMmap {
        let Some(handle) = map_handle_from_offset(offset) else {
            warn!("card1: mmap received invalid offset {offset:#x}");
            return DeviceMmap::None;
        };
        let Ok(exported) = exported_gem_buffer(handle) else {
            warn!("card1: mmap could not resolve handle {handle}");
            return DeviceMmap::None;
        };
        DeviceMmap::Physical(exported.range, None)
    }
}

struct ExportedGemBuffer {
    range: PhysAddrRange,
}

impl ExportedGemBuffer {
    fn new(range: PhysAddrRange) -> Self {
        Self { range }
    }
}

impl FileLike for ExportedGemBuffer {
    fn path(&self) -> Cow<'_, str> {
        "anon_inode:[rknpu-gem]".into()
    }

    fn device_mmap(&self, _offset: u64, _length: u64) -> AxResult<DeviceMmap> {
        Ok(DeviceMmap::Physical(self.range, None))
    }
}

impl Pollable for ExportedGemBuffer {
    fn poll(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}

fn prime_fd_cloexec(flags: u32) -> bool {
    flags & O_CLOEXEC != 0
}

fn map_handle_from_offset(offset: u64) -> Option<u32> {
    if offset & ((1 << PAGE_SHIFT) - 1) != 0 {
        return None;
    }
    let handle = u32::try_from(offset >> PAGE_SHIFT).ok()?;
    (handle != 0).then_some(handle)
}

fn exported_gem_buffer(handle: u32) -> AxResult<ExportedGemBuffer> {
    let (obj_addr, size) = rknpu::obj_addr_and_size(handle)
        .map_err(map_rknpu_err)
        .map_err(|_| AxError::NotFound)?;
    let paddr = virt_to_phys(obj_addr.into());
    Ok(ExportedGemBuffer::new(PhysAddrRange::from_start_size(
        paddr, size,
    )))
}

fn map_rknpu_err(err: rknpu::Error) -> VfsError {
    match err {
        rknpu::Error::NotFound => VfsError::NotFound,
        rknpu::Error::Busy => VfsError::AlreadyExists,
        rknpu::Error::InvalidData => VfsError::InvalidData,
    }
}

fn elapsed_us(start_ns: u64, end_ns: u64) -> u64 {
    end_ns.saturating_sub(start_ns) / 1000
}

/// Copies data from user space to kernel space
pub fn copy_from_user(dst: *mut u8, src: *const u8, size: usize) -> Result<(), VfsError> {
    let ret = unsafe { user_copy(dst, src, size) };

    if ret != 0 {
        warn!("[rknpu]: copy_from_user failed, ret={}", ret);
        return Err(VfsError::InvalidData);
    }
    Ok(())
}

/// Copies data from kernel space to user space
pub fn copy_to_user(dst: *mut u8, src: *const u8, size: usize) -> Result<(), VfsError> {
    let ret = unsafe { user_copy(dst, src, size) };

    if ret != 0 {
        warn!("[rknpu]: copy_to_user failed, ret={}", ret);
        return Err(VfsError::InvalidData);
    }
    Ok(())
}

// EXEC-4 (#1): one-shot empirical CPU-frequency probe. EXEC-3 §30 localized the
// ~187ms/inference to userspace librknnrt; the lead hypothesis is the A76 cluster
// runs well below its 2.4GHz max (back-of-envelope from §29 put it ~1GHz). This
// measures the *actual* clock the way that matters — how fast code runs — by
// timing a fixed-length dependent decrement loop against the monotonic clock,
// which needs no SCMI/CRU clock IDs. Fired once on the first NPU ioctl (in the
// inference process context), so the ~0.1-0.2s loop happens before the measured
// runs and never perturbs them.
static CPU_MHZ_PROBED: AtomicBool = AtomicBool::new(false);

#[cfg(target_arch = "aarch64")]
fn measure_and_log_cpu_mhz() {
    const ITERS: u64 = 200_000_000;
    let t0 = monotonic_time_nanos();
    let mut n = ITERS;
    // A tight `subs; b.ne` loop: the subs->subs dependency is the critical path,
    // so it retires at ~1 cycle/iteration on A76/A55 — good enough to tell 1.0GHz
    // from 2.4GHz. SAFETY: pure register arithmetic, no memory/stack access.
    unsafe {
        core::arch::asm!(
            "2:",
            "subs {n}, {n}, #1",
            "b.ne 2b",
            n = inout(reg) n,
            options(nomem, nostack),
        );
    }
    core::hint::black_box(n);
    let dt_ns = monotonic_time_nanos().saturating_sub(t0).max(1);
    // MHz = cycles / microseconds = ITERS*1000 / dt_ns (assuming ~1 cyc/iter).
    let mhz = ITERS.saturating_mul(1000) / dt_ns;
    warn!(
        "cpufreq probe: {} dependent-sub iters in {} ns -> ~{} MHz (assume ~1 cyc/iter; RK3588 \
         A76 max 2400, A55 max 1800)",
        ITERS, dt_ns, mhz
    );
}

#[cfg(not(target_arch = "aarch64"))]
fn measure_and_log_cpu_mhz() {}

/// Handles RKNPU driver ioctls, timing the whole call so EXEC-3 accounting can
/// partition wall time across ioctl types and userspace gaps. The real work is
/// in `rknpu_driver_ioctl_inner`; this shell brackets it with a monotonic clock
/// read on entry and exit (covering every path, including early `Err` returns)
/// and folds the result into the cumulative partition.
pub fn rknpu_driver_ioctl(op: RknpuCmd, arg: usize) -> VfsResult<usize> {
    if !CPU_MHZ_PROBED.swap(true, Ordering::Relaxed) {
        // EXEC-4c: probe the BOOT clock here (~1209 of §31). The raise to max now
        // happens mid-run (after RAISE_AFTER_SUBMITS submits, in account_rknpu_ioctl)
        // so one boot captures the before/after burst timing.
        measure_and_log_cpu_mhz();
    }
    let entry_ns = monotonic_time_nanos();
    let result = rknpu_driver_ioctl_inner(op, arg);
    let exit_ns = monotonic_time_nanos();
    account_rknpu_ioctl(op, entry_ns, exit_ns);
    result
}

/// Handles RKNPU action ioctl commands
fn rknpu_driver_ioctl_inner(op: RknpuCmd, arg: usize) -> VfsResult<usize> {
    debug!("rknpu_driver_ioctl: op = {:?}", op);
    match op {
        RknpuCmd::Submit => {
            let mut submit_args = RknpuSubmit::default();
            copy_from_user(
                &mut submit_args as *mut _ as *mut u8,
                arg as *const u8,
                mem::size_of::<RknpuSubmit>(),
            )?;
            let log_index = RKNPU_SUBMIT_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
            if log_index < RKNPU_SUBMIT_LOG_LIMIT {
                debug!(
                    "rknpu submit ioctl[{log_index}]: flags={:#x} timeout={} task_start={} \
                     task_number={} task_counter={} core_mask={:#x} task_obj_addr={:#x} \
                     task_base_addr={:#x} subcore_task={:?}",
                    submit_args.flags,
                    submit_args.timeout,
                    submit_args.task_start,
                    submit_args.task_number,
                    submit_args.task_counter,
                    submit_args.core_mask,
                    submit_args.task_obj_addr,
                    submit_args.task_base_addr,
                    submit_args.subcore_task
                );
            }
            debug!("rknpu submit ioctl {submit_args:#x?}");

            let submit_start_ns = monotonic_time_nanos();
            match rknpu::submit(&mut submit_args).map_err(map_rknpu_err) {
                Ok(()) => {
                    let submit_end_ns = monotonic_time_nanos();
                    if log_index < RKNPU_SUBMIT_LOG_LIMIT {
                        debug!(
                            "rknpu submit ioctl[{log_index}] done: task_counter={} \
                             hw_elapse_time={} core_mask={:#x} elapsed_us={}",
                            submit_args.task_counter,
                            submit_args.hw_elapse_time,
                            submit_args.core_mask,
                            elapsed_us(submit_start_ns, submit_end_ns)
                        );
                    }
                }
                Err(e) => {
                    let submit_end_ns = monotonic_time_nanos();
                    warn!("rknpu submit ioctl failed: {:?}", e);
                    if log_index < RKNPU_SUBMIT_LOG_LIMIT {
                        warn!(
                            "rknpu submit ioctl[{log_index}] failed: task_counter={} \
                             hw_elapse_time={} core_mask={:#x} elapsed_us={}",
                            submit_args.task_counter,
                            submit_args.hw_elapse_time,
                            submit_args.core_mask,
                            elapsed_us(submit_start_ns, submit_end_ns)
                        );
                    }
                }
            }
            debug!("rknpu submit ioctl result: {:#x?}", submit_args);

            copy_to_user(
                arg as *mut u8,
                &submit_args as *const _ as *const u8,
                mem::size_of::<RknpuSubmit>(),
            )?;
        }
        RknpuCmd::MemCreate => {
            debug!("rknpu mem_create ioctl");
            let mut mem_create_args = RknpuMemCreate::default();

            copy_from_user(
                &mut mem_create_args as *mut _ as *mut u8,
                arg as *const u8,
                mem::size_of::<RknpuMemCreate>(),
            )?;

            let log_index = RKNPU_MEM_CREATE_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
            if log_index < RKNPU_MEM_CREATE_LOG_LIMIT {
                debug!(
                    "rknpu mem_create ioctl[{log_index}]: flags={:#x} size={} core_mask={:#x}",
                    mem_create_args.flags, mem_create_args.size, mem_create_args.core_mask
                );
            }

            let create_start_ns = monotonic_time_nanos();
            match rknpu::mem_create(&mut mem_create_args).map_err(map_rknpu_err) {
                Ok(()) => {
                    let create_end_ns = monotonic_time_nanos();
                    if log_index < RKNPU_MEM_CREATE_LOG_LIMIT {
                        debug!(
                            "rknpu mem_create ioctl[{log_index}] done: handle={} flags={:#x} \
                             size={} sram_size={} obj_addr={:#x} dma_addr={:#x} \
                             iommu_domain_id={} core_mask={:#x} elapsed_us={}",
                            mem_create_args.handle,
                            mem_create_args.flags,
                            mem_create_args.size,
                            mem_create_args.sram_size,
                            mem_create_args.obj_addr,
                            mem_create_args.dma_addr,
                            mem_create_args.iommu_domain_id,
                            mem_create_args.core_mask,
                            elapsed_us(create_start_ns, create_end_ns)
                        );
                    }
                }
                Err(e) => {
                    let create_end_ns = monotonic_time_nanos();
                    warn!("rknpu mem_create ioctl failed: {:?}", e);
                    if log_index < RKNPU_MEM_CREATE_LOG_LIMIT {
                        warn!(
                            "rknpu mem_create ioctl[{log_index}] failed: flags={:#x} size={} \
                             core_mask={:#x} elapsed_us={}",
                            mem_create_args.flags,
                            mem_create_args.size,
                            mem_create_args.core_mask,
                            elapsed_us(create_start_ns, create_end_ns)
                        );
                    }
                }
            }

            copy_to_user(
                arg as *mut u8,
                &mem_create_args as *const _ as *const u8,
                mem::size_of::<RknpuMemCreate>(),
            )?;
        }
        RknpuCmd::MemMap => {
            debug!("rknpu mem_map ioctl");
            let mut mem_map = RknpuMemMap::default();
            copy_from_user(
                &mut mem_map as *mut _ as *mut u8,
                arg as *const u8,
                mem::size_of::<RknpuMemMap>(),
            )?;

            match rknpu::mem_map_offset(mem_map.handle).map_err(map_rknpu_err) {
                Ok(offset) => {
                    mem_map.offset = offset;
                    debug!(
                        "mem_map: handle={} -> offset=0x{:x}",
                        mem_map.handle, mem_map.offset
                    );
                }
                Err(e) => {
                    warn!("mem_map: invalid handle={}", mem_map.handle);
                    warn!("rknpu mem_map ioctl failed: {:?}", e);
                    return Err(e);
                }
            }

            copy_to_user(
                arg as *mut u8,
                &mem_map as *const _ as *const u8,
                mem::size_of::<RknpuMemMap>(),
            )?;
        }
        RknpuCmd::MemDestroy => {
            debug!("rknpu mem_destroy ioctl");
        }
        RknpuCmd::MemSync => {
            let mut mem_sync = RknpuMemSync::default();
            copy_from_user(
                &mut mem_sync as *mut _ as *mut u8,
                arg as *const u8,
                mem::size_of::<RknpuMemSync>(),
            )?;
            let log_index = RKNPU_MEM_SYNC_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
            if log_index < RKNPU_MEM_SYNC_LOG_LIMIT {
                debug!(
                    "rknpu mem_sync ioctl[{log_index}]: flags={:#x} obj_addr={:#x} offset={} \
                     size={}",
                    mem_sync.flags, mem_sync.obj_addr, mem_sync.offset, mem_sync.size
                );
            }
            debug!("rknpu mem_sync ioctl {mem_sync:#x?}");

            let sync_start_ns = monotonic_time_nanos();
            match rknpu::mem_sync(&mut mem_sync).map_err(map_rknpu_err) {
                Ok(()) => {
                    let sync_end_ns = monotonic_time_nanos();
                    if log_index < RKNPU_MEM_SYNC_LOG_LIMIT {
                        debug!(
                            "rknpu mem_sync ioctl[{log_index}] done: flags={:#x} offset={} \
                             size={} elapsed_us={}",
                            mem_sync.flags,
                            mem_sync.offset,
                            mem_sync.size,
                            elapsed_us(sync_start_ns, sync_end_ns)
                        );
                    }
                }
                Err(e) => {
                    let sync_end_ns = monotonic_time_nanos();
                    warn!("rknpu mem_sync ioctl failed: {:?}", e);
                    if log_index < RKNPU_MEM_SYNC_LOG_LIMIT {
                        warn!(
                            "rknpu mem_sync ioctl[{log_index}] failed: flags={:#x} offset={} \
                             size={} elapsed_us={}",
                            mem_sync.flags,
                            mem_sync.offset,
                            mem_sync.size,
                            elapsed_us(sync_start_ns, sync_end_ns)
                        );
                    }
                    return Err(e);
                }
            }

            copy_to_user(
                arg as *mut u8,
                &mem_sync as *const _ as *const u8,
                mem::size_of::<RknpuMemSync>(),
            )?;
        }
        RknpuCmd::Action => {
            debug!("rknpu action ioctl");
            let mut action = RknpuUserAction::default();
            copy_from_user(
                &mut action as *mut _ as *mut u8,
                arg as *const u8,
                mem::size_of::<RknpuUserAction>(),
            )?;

            let log_index = RKNPU_ACTION_LOG_COUNT.fetch_add(1, Ordering::Relaxed);
            let value_in = action.value;
            debug!(
                "rknpu action ioctl: flags = {:?}, value = {}",
                action.flags, action.value
            );

            match rknpu::action(action.flags).map_err(map_rknpu_err) {
                Ok(val) => {
                    action.value = val;
                    if log_index < RKNPU_ACTION_LOG_LIMIT {
                        debug!(
                            "rknpu action ioctl[{log_index}]: flags={:?} value_in={} \
                             result=Ok({}) value_out={}",
                            action.flags, value_in, val, action.value
                        );
                    }
                }
                Err(e) => {
                    warn!("rknpu action ioctl failed: {:?}", e);
                    if log_index < RKNPU_ACTION_LOG_LIMIT {
                        warn!(
                            "rknpu action ioctl[{log_index}]: flags={:?} value_in={} \
                             result=Err({:?}) value_out={}",
                            action.flags, value_in, e, action.value
                        );
                    }
                }
            }

            copy_to_user(
                arg as *mut u8,
                &action as *const _ as *const u8,
                mem::size_of::<RknpuUserAction>(),
            )?;
        }
    }
    Ok(0)
}

/// DRM_IOCTL_GEM_FLINK ioctl argument type
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
struct DrmGemFlink {
    /// GEM handle
    handle: u32,
    /// GEM name
    name: u32,
}

/// Handles DRM GEM flink ioctl command
fn drm_gem_flink_ioctl(data: &mut [u8]) -> VfsResult<usize> {
    let data = unsafe { &mut *(data.as_mut_ptr() as *mut DrmGemFlink) };
    debug!("drm_gem_flink_ioctl called: {:#?}", data);
    Err(VfsError::NotFound)
}

/// DRM prime handle structure
#[repr(C)]
#[derive(Debug, Clone, Copy)]
struct DrmPrimeHande {
    /// Handle
    handle: u32,
    /// Flags
    flags: u32,
    /// File descriptor
    fd: i32,
}

/// Handles DRM prime handle to fd ioctl command
fn drm_prime_handle_to_fd_ioctl(data: &mut [u8]) -> VfsResult<usize> {
    let data = unsafe { &mut *(data.as_mut_ptr() as *mut DrmPrimeHande) };
    debug!("drm_prime_handle_to_fd_ioctl {data:#x?}");
    let exported = exported_gem_buffer(data.handle).map_err(|err| {
        warn!(
            "drm_prime_handle_to_fd_ioctl: invalid handle {}: {err}",
            data.handle
        );
        VfsError::NotFound
    })?;
    data.fd = exported
        .add_to_fd_table(prime_fd_cloexec(data.flags))
        .map_err(|err| {
            warn!("drm_prime_handle_to_fd_ioctl: failed to allocate fd: {err}");
            VfsError::NoMemory
        })?;
    Ok(0)
}

/// Rust implementation of Linux kernel's drm_copy_field function
///
/// This function safely copies a string value to user space buffer,
/// similar to the Linux kernel implementation with proper error handling.
unsafe fn drm_copy_field(
    buf: *mut u8,
    buf_len: &mut c_ulong,
    value: *const u8,
) -> Result<(), VfsError> {
    // Handle NULL value case - same as kernel's WARN_ONCE check
    if value.is_null() {
        warn!("[drm_copy_field] BUG: the value to copy was not set!");
        *buf_len = 0;
        return Ok(());
    }

    // Calculate actual string length using C string semantics
    let mut len = 0;
    unsafe {
        let mut ptr = value;
        while *ptr != 0 {
            len += 1;
            ptr = ptr.add(1);
        }
    }

    // Get the original buffer size
    let original_buf_len = *buf_len;

    // Update user's buffer length with actual string length (same as kernel)
    *buf_len = len;

    // Don't overflow user buffer - limit copy to available space
    let copy_len = if len > original_buf_len {
        original_buf_len
    } else {
        len
    };

    // Finally, try filling in the userbuf (same logic as kernel)
    if copy_len > 0 && !buf.is_null() {
        copy_to_user(buf as _, value, copy_len as _)?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use ax_memory_addr::PhysAddrRange;

    use super::*;

    #[test]
    fn prime_export_honors_cloexec_flag() {
        assert!(prime_fd_cloexec(linux_raw_sys::general::O_CLOEXEC as u32));
        assert!(!prime_fd_cloexec(0));
    }

    #[test]
    fn mem_map_offset_decodes_to_handle() {
        assert_eq!(map_handle_from_offset(0x1000), Some(1));
        assert_eq!(map_handle_from_offset(0x2000), Some(2));
        assert_eq!(map_handle_from_offset(0), None);
        assert_eq!(map_handle_from_offset(0x1001), None);
    }

    #[test]
    fn exported_buffer_reports_physical_device_mmap() {
        let range = PhysAddrRange::from_start_size(0x1234_5000.into(), 0x4000);
        let exported = ExportedGemBuffer::new(range);

        assert!(
            matches!(exported.device_mmap(0, 0).unwrap(), DeviceMmap::Physical(actual, None) if actual == range)
        );
    }
}

/// Sets the DRM version information for the device
pub fn drm_version(data: &mut [u8]) -> VfsResult<()> {
    let data = unsafe { &mut *(data.as_mut_ptr() as *mut DrmVersion) };
    debug!("drm_version called: {:?}", data);

    // Set version information
    data.version_major = 0;
    data.version_minor = 9;
    data.version_patchlevel = 8;

    // Use drm_copy_field to handle string copying properly
    unsafe {
        // Copy driver name
        let ret = drm_copy_field(
            data.name.cast(),
            &mut data.name_len,
            DRM1_NAME.as_ptr().cast(),
        );
        if let Err(e) = ret {
            warn!("[drm_version] Failed to copy driver name: {:?}", e);
            return Err(VfsError::InvalidData);
        }

        // Copy driver date
        let ret = drm_copy_field(
            data.date as *mut u8,
            &mut data.date_len,
            DRM1_DATE.as_ptr() as *const u8,
        );
        if let Err(e) = ret {
            warn!("[drm_version] Failed to copy driver date: {:?}", e);
            return Err(VfsError::InvalidData);
        }

        // Copy driver description
        let ret = drm_copy_field(
            data.desc.cast(),
            &mut data.desc_len,
            DRM1_DESC.as_ptr().cast(),
        );
        if let Err(e) = ret {
            warn!("[drm_version] Failed to copy driver description: {:?}", e);
            return Err(VfsError::InvalidData);
        }
    }

    debug!(
        "[drm_version] Set driver info: name_len={}, date_len={}, desc_len={}",
        data.name_len, data.date_len, data.desc_len
    );

    Ok(())
}

/// DRM_GET_UNIQUE ioctl handler
///
/// This function handles DRM_IOCTL_GET_UNIQUE requests, returning the unique
/// identifier for the DRM device (typically a bus ID or similar identifier).
pub fn drm_get_unique(data: &mut [u8]) -> VfsResult<()> {
    let unique_data = unsafe { &mut *(data.as_mut_ptr() as *mut DrmUnique) };
    debug!("drm_get_unique called: {:?}", unique_data);

    unique_data.unique_len = 0;

    Ok(())
}
