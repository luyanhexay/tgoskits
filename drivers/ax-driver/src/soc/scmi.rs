use alloc::format;
use core::sync::atomic::{AtomicBool, Ordering};

use arm_scmi_rs::{Scmi, Shmem, Smc};
use ax_kspin::SpinNoIrq as Mutex;
use fdt_edit::Phandle;
use log::{info, warn};

use crate::{DriverGeneric, mmio::iomap, probe::OnProbeError, register::ProbeFdt};

const SCMI_SHMEM_SIZE: usize = 0x100;
const RK3588_SCMI_SHMEM_BASE: usize = 0x10f000;

static SCMI: Mutex<Option<Scmi<Smc>>> = Mutex::new(None);
static SCMI_REGISTERED: AtomicBool = AtomicBool::new(false);

crate::model_register!(
    name: "ARM SCMI SMC",
    level: ProbeLevel::PostKernel,
    priority: ProbePriority::CLK,
    probe_kinds: &[
        ProbeKind::Fdt {
            compatibles: &["arm,scmi-smc"],
            on_probe: probe
        }
    ],
);

fn probe(probe: ProbeFdt<'_>) -> Result<(), OnProbeError> {
    let (info, plat_dev) = probe.into_parts();
    let smc_id = info
        .node
        .as_node()
        .get_property("arm,smc-id")
        .and_then(|prop| prop.get_u32())
        .ok_or_else(|| OnProbeError::other(format!("[{}] has no arm,smc-id", info.node.name())))?;
    let shmem_phandle = info
        .node
        .as_node()
        .get_property("shmem")
        .and_then(|prop| prop.get_u32_iter().next())
        .ok_or_else(|| OnProbeError::other(format!("[{}] has no shmem", info.node.name())))?;
    let (shmem_addr, shmem_size) = info
        .node
        .regs()
        .into_iter()
        .next()
        .map(|reg| {
            (
                reg.address as usize,
                reg.size.unwrap_or(SCMI_SHMEM_SIZE as u64) as usize,
            )
        })
        .unwrap_or_else(|| {
            warn!(
                "[{}] SCMI shmem phandle {} cannot be resolved by rdrive; using RK3588 shmem \
                 fallback {:#x}+{:#x}",
                info.node.name(),
                shmem_phandle,
                RK3588_SCMI_SHMEM_BASE,
                SCMI_SHMEM_SIZE
            );
            (RK3588_SCMI_SHMEM_BASE, SCMI_SHMEM_SIZE)
        });
    let shmem_base = iomap(shmem_addr, shmem_size)?;

    let shmem = Shmem {
        address: shmem_base,
        bus_address: shmem_addr,
        size: shmem_size,
    };
    let scmi = Scmi::new(Smc::new(smc_id, None), shmem);
    *SCMI.lock() = Some(scmi);
    SCMI_REGISTERED.store(true, Ordering::Release);
    plat_dev.register(ScmiDevice);
    info!(
        "SCMI SMC registered: smc_id={:#x}, shmem_phandle={}, shmem={:#x}+{:#x}",
        smc_id, shmem_phandle, shmem_addr, shmem_size
    );
    Ok(())
}

pub fn clock_rate(_phandle: Phandle, clock_id: u32) -> Option<u64> {
    if !SCMI_REGISTERED.load(Ordering::Acquire) {
        warn!(
            "SCMI clock rate requested before SCMI registration: clock_id={:#x}",
            clock_id
        );
        return None;
    }
    let mut guard = SCMI.lock();
    let scmi = guard.as_mut()?;
    match scmi.clock_rate_get_direct(clock_id) {
        Ok(rate) => {
            info!(
                "SCMI clock rate get: clock_id={:#x}, rate={} Hz",
                clock_id, rate
            );
            Some(rate)
        }
        Err(err) => {
            warn!(
                "SCMI clock rate get failed: clock_id={:#x}, {:?}",
                clock_id, err
            );
            None
        }
    }
}

pub fn set_clock_rate(_phandle: Phandle, clock_id: u32, rate: u64) -> Option<()> {
    if !SCMI_REGISTERED.load(Ordering::Acquire) {
        warn!(
            "SCMI clock rate set requested before SCMI registration: clock_id={:#x}, rate={} Hz",
            clock_id, rate
        );
        return None;
    }
    let mut guard = SCMI.lock();
    let scmi = guard.as_mut()?;
    match scmi.clock_rate_set_direct(clock_id, rate) {
        Ok(()) => {
            info!(
                "SCMI clock rate set: clock_id={:#x}, rate={} Hz",
                clock_id, rate
            );
            Some(())
        }
        Err(err) => {
            warn!(
                "SCMI clock rate set failed: clock_id={:#x}, rate={} Hz, {:?}",
                clock_id, rate, err
            );
            None
        }
    }
}

/// Log the list of SCMI protocols the platform firmware (ATF) actually exposes.
///
/// This decides the NPU DVFS implementation path: if PERF (0x13) is present we
/// can drive voltage-coupled DVFS over SCMI; if only CLOCK (0x14) is exposed,
/// real DVFS must drive the NPU regulator (RK8602 over I2C) directly.
pub fn log_supported_protocols() {
    if !SCMI_REGISTERED.load(Ordering::Acquire) {
        warn!("SCMI protocol list requested before SCMI registration");
        return;
    }
    let mut guard = SCMI.lock();
    let Some(scmi) = guard.as_mut() else {
        return;
    };
    let mut base = scmi.protocol_base();
    match base.discover_list_protocols(0) {
        Ok(protocols) => {
            info!("SCMI supported protocols (raw bytes): {:?}", protocols);
            let has = |p: u8| protocols.contains(&p);
            info!(
                "SCMI protocols decoded: CLOCK(0x14)={} PERF(0x13)={} POWER(0x11)={} \
                 SYSTEM(0x12)={} SENSOR(0x15)={} RESET(0x16)={} VOLTAGE(0x17)={}",
                has(0x14),
                has(0x13),
                has(0x11),
                has(0x12),
                has(0x15),
                has(0x16),
                has(0x17),
            );
        }
        Err(err) => warn!("SCMI discover_list_protocols failed: {:?}", err),
    }
}

struct ScmiDevice;

impl DriverGeneric for ScmiDevice {
    fn name(&self) -> &str {
        "arm-scmi-smc"
    }
}
