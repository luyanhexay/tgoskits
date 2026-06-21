use alloc::vec::Vec;

use log::info;
use rdrive::{probe::OnProbeError, register::ProbeFdt};
use rockchip_npu::{Rknpu, RknpuConfig, RknpuType};
pub use rockchip_npu::{
    RknpuAction,
    ioctrl::{RknpuMemCreate, RknpuMemMap, RknpuMemSync, RknpuSubmit},
};
use rockchip_pm::{PowerDomain, RockchipPM};

use crate::mmio::iomap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotFound,
    Busy,
    InvalidData,
}

crate::model_register!(
    name: "Rockchip NPU",
    level: ProbeLevel::PostKernel,
    priority: ProbePriority::DEFAULT,
    probe_kinds: &[
        ProbeKind::Fdt {
            compatibles: &["rockchip,rk3588-rknpu"],
            on_probe: probe
        }
    ],
);

fn probe(probe: ProbeFdt<'_>) -> Result<(), OnProbeError> {
    let (info, plat_dev) = probe.into_parts();
    let regs = info.node.regs();

    let config = RknpuConfig {
        rknpu_type: RknpuType::Rk3588,
    };

    let mut base_regs = Vec::new();
    let page_size = 0x1000;
    for reg in &regs {
        let start_raw = reg.address as usize;
        let end = start_raw + reg.size.unwrap_or(0x1000) as usize;

        let start = start_raw & !(page_size - 1);
        let offset = start_raw - start;
        let end = (end + page_size - 1) & !(page_size - 1);
        let size = end - start;

        base_regs.push(unsafe { iomap(start, size)?.add(offset) });
    }

    enable_pm();

    info!("NPU power enabled");

    // ===== NPU DVFS diagnostics — one-shot board experiment =====
    // rknn_run is ~812 ms (should be ~tens of ms). clk_npu set to 1 GHz via SCMI
    // reads back 1 GHz but gives ZERO speedup. This block gathers, in a single
    // boot, the data that decides the DVFS implementation path. See
    // docs/上板调试日志 §23.

    // (A) Which SCMI protocols does the platform firmware (ATF) actually expose?
    //     PERF (0x13) present  -> voltage-coupled DVFS is possible over SCMI.
    //     Only CLOCK (0x14)    -> real DVFS must drive the RK8602 regulator (I2C).
    crate::soc::scmi::log_supported_protocols();

    // (B) clk_npu (SCMI clock id 6): log boot-default, raise to OPP max, confirm.
    const NPU_MAX_HZ: u64 = 1_000_000_000; // opp-1000000000, RK3588 NPU max
    if let Some(clk) = info.find_clk_by_name("clk_npu") {
        let clock_id = clk.select().unwrap_or(0);
        let before = crate::soc::scmi::clock_rate(clk.phandle, clock_id).unwrap_or(0);
        info!("NPU clk_npu boot-default = {} Hz", before);
        match crate::soc::scmi::set_clock_rate(clk.phandle, clock_id, NPU_MAX_HZ) {
            Some(()) => {
                let got = crate::soc::scmi::clock_rate(clk.phandle, clock_id).unwrap_or(0);
                info!(
                    "NPU clk_npu set to {} Hz (read back {} Hz)",
                    NPU_MAX_HZ, got
                );
            }
            None => log::warn!("failed to set NPU clk_npu to {} Hz", NPU_MAX_HZ),
        }
    } else {
        log::warn!("NPU clk_npu not found in FDT; NPU clock left at boot default");
    }

    // (C) CRU data/compute clock CLK_NPU_DSU0 (id 304, max 1188 MHz): this is the
    //     CRU-settable NPU clock (plain MMIO, no ATF arbitration). If raising it
    //     speeds up rknn_run, the data clock — not the SCMI clk_npu — is the lever.
    const CLK_NPU_DSU0: u32 = 304;
    const NPU_DSU0_MAX_HZ: u64 = 1_188_000_000;
    match crate::soc::rk3588_get_clock_rate(CLK_NPU_DSU0) {
        Ok(hz) => info!("NPU CLK_NPU_DSU0 boot-default = {} Hz", hz),
        Err(err) => log::warn!("read CLK_NPU_DSU0 failed: {:?}", err),
    }
    match crate::soc::rk3588_set_clock_rate(CLK_NPU_DSU0, NPU_DSU0_MAX_HZ) {
        Ok(()) => {
            let got = crate::soc::rk3588_get_clock_rate(CLK_NPU_DSU0).unwrap_or(0);
            info!(
                "NPU CLK_NPU_DSU0 set to {} Hz (read back {} Hz)",
                NPU_DSU0_MAX_HZ, got
            );
        }
        Err(err) => log::warn!("set CLK_NPU_DSU0 failed: {:?}", err),
    }

    // (D) EXEC-2 read-only bus/control clock snapshot. The NPU YOLO load is
    //     memory-bandwidth heavy and the core-mask matrix showed adding cores
    //     does NOT change rknn_run time (not PE-compute-bound) — so the AXI data
    //     clock (aclk0/1/2) is the prime suspect. We only ever raised clk_npu /
    //     DSU0 (compute); these data/control clocks were never touched. Read-only,
    //     no writes. CRU ids: ACLK_NPU0=301 ACLK_NPU1=290 ACLK_NPU2=292
    //     HCLK_NPU_ROOT=303 PCLK_NPU_ROOT=305. NOTE: npu_get_rate() currently has
    //     no decode for ACLK_NPU*, so those are expected to log an error — that
    //     gap (cannot observe the bandwidth-critical clock) is itself a finding.
    const NPU_CLK_SNAPSHOT: [(&str, u32); 5] = [
        ("ACLK_NPU0", 301),
        ("ACLK_NPU1", 290),
        ("ACLK_NPU2", 292),
        ("HCLK_NPU_ROOT", 303),
        ("PCLK_NPU_ROOT", 305),
    ];
    for (name, id) in NPU_CLK_SNAPSHOT {
        match crate::soc::rk3588_get_clock_rate(id) {
            Ok(hz) => info!("NPU clock snapshot: {} (id {}) = {} Hz", name, id, hz),
            Err(err) => info!(
                "NPU clock snapshot: {} (id {}) = <unreadable: {:?}>",
                name, id, err
            ),
        }
    }
    // ===== end NPU DVFS diagnostics =====

    let dma = axklib::dma::device_with_mask(u32::MAX as u64);
    let npu = Rknpu::new(&base_regs, config, dma);
    plat_dev.register(npu);
    info!("NPU registered successfully");
    Ok(())
}

fn enable_pm() {
    let mut pm = rdrive::get_one::<RockchipPM>()
        .expect("RockchipPM not found")
        .lock()
        .expect("RockchipPM lock failed");

    // RK3588 NPU power domain IDs (from rockchip-pm rk3588 variant)
    pm.power_domain_on(PowerDomain(9)).unwrap(); // NPUTOP
    pm.power_domain_on(PowerDomain(8)).unwrap(); // NPU
    pm.power_domain_on(PowerDomain(10)).unwrap(); // NPU1
    pm.power_domain_on(PowerDomain(11)).unwrap(); // NPU2
}

pub fn is_available() -> bool {
    rdrive::get_one::<Rknpu>().is_some()
}

/// EXEC-4b: raise the A76 big-core clusters to their 2.4GHz max via SCMI, logging
/// before/after. RK3588 CPU clusters are voltage-coupled DVFS arbitrated by ATF
/// over SCMI; the board DTB leaves them at 816MHz (`assigned-clock-rates`) and
/// EXEC-4 (§31) measured the cores running ~1.2GHz, so all userspace — including
/// the closed `librknnrt` that owns the ~187ms/inference bottleneck (§30/§31) —
/// runs ~2x slow. SCMI clock ids are read from the board DTB: A76 B0 cluster
/// (cpu@400/500)=2, B1 cluster (cpu@600/700)=3. The phandle arg to the SCMI
/// helpers is ignored (single registered SCMI instance), so a dummy is fine.
pub fn set_cpu_clusters_max() {
    const A76_MAX_HZ: u64 = 2_400_000_000;
    for (name, id) in [("A76_B0", 2u32), ("A76_B1", 3u32)] {
        let before = crate::soc::scmi::clock_rate(fdt_edit::Phandle::from(0u32), id).unwrap_or(0);
        match crate::soc::scmi::set_clock_rate(fdt_edit::Phandle::from(0u32), id, A76_MAX_HZ) {
            Some(()) => {
                let after =
                    crate::soc::scmi::clock_rate(fdt_edit::Phandle::from(0u32), id).unwrap_or(0);
                info!(
                    "EXEC-4b: {} (scmi clk {}) {} -> {} Hz",
                    name, id, before, after
                );
            }
            None => log::warn!(
                "EXEC-4b: failed to raise {} (scmi clk {}) to {} Hz",
                name,
                id,
                A76_MAX_HZ
            ),
        }
    }
}

pub fn obj_addr_and_size(handle: u32) -> Result<(usize, usize), Error> {
    with_npu(|npu| npu.get_obj_addr_and_size(handle).ok_or(Error::NotFound))
}

pub fn submit(args: &mut RknpuSubmit) -> Result<(), Error> {
    with_npu(|npu| npu.submit_ioctrl(args).map_err(|_| Error::InvalidData))
}

pub fn mem_create(args: &mut RknpuMemCreate) -> Result<(), Error> {
    with_npu(|npu| npu.create(args).map_err(|_| Error::InvalidData))
}

pub fn mem_sync(args: &mut RknpuMemSync) -> Result<(), Error> {
    with_npu(|npu| npu.mem_sync(args).map_err(|_| Error::InvalidData))
}

pub fn mem_map_offset(handle: u32) -> Result<u64, Error> {
    with_npu(|npu| {
        npu.get_phys_addr_and_size(handle)
            .map(|_| (handle as u64) << 12)
            .ok_or(Error::InvalidData)
    })
}

pub fn action(flags: RknpuAction) -> Result<u32, Error> {
    with_npu(|npu| npu.action(flags).map_err(|_| Error::InvalidData))
}

fn with_npu<F, R>(f: F) -> Result<R, Error>
where
    F: FnOnce(&mut Rknpu) -> Result<R, Error>,
{
    let mut npu = rdrive::get_one::<Rknpu>()
        .ok_or(Error::NotFound)?
        .try_lock()
        .map_err(|_| Error::Busy)?;
    f(&mut npu)
}
