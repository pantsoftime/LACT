//! Read-only access to NVIDIA RM voltage-rail objects and the on-chip ADCs.
//!
//! `VOLT_RAILS` GET_INFO / GET_STATUS / GET_CONTROL and `CLK_ADC_DEVICES`
//! GET_INFO / GET_STATUS are private controls with no public header. The
//! command IDs and parameter sizes come from the size-validation switch inside
//! `libnvidia-api.so` (the same table the NvAPI layer uses), and the STATUS
//! record layout was mapped field by field from that library's own NvAPI
//! wrapper, which copies the RM record into the documented NvAPI structure.
//! Verified on GB202 with driver branches R610 and R615.
//!
//! The one write here is the rail-limit setter (`0x2080f214`). Its record was
//! mapped field by field on 2026-09-11 with single-field writes and exact
//! restores: +0x08 REL, +0x0c ALT/OP, +0x10 OV, +0x14 VMIN (µV deltas to the
//! driver's evaluated limits); +0x18 / +0x1c move no limit and are never
//! touched. The MSVDD rail carries −50 mV on REL at stock on GB202.
//!
//! Every probe fails closed: a rail count, object type or record index that
//! does not match the verified layout disables the whole feature rather than
//! reporting numbers from an unknown structure.

use anyhow::{Context, bail};
use tracing::debug;

use super::driver::DriverHandle;

const VOLT_RAILS_GET_INFO: u32 = 0x2080_b201;
const VOLT_RAILS_GET_STATUS: u32 = 0x2080_b202;
const VOLT_RAILS_GET_CONTROL: u32 = 0x2080_b213;
const VOLT_RAILS_SET_CONTROL: u32 = 0x2080_f214;
const VOLT_DEVICES_GET_INFO: u32 = 0x2080_b205;

const RAILS_INFO_SIZE: usize = 0x98c;
const RAILS_INFO_HEADER: usize = 0x0c;
const RAILS_INFO_STRIDE: usize = 0x4c;
const RAILS_STATUS_SIZE: usize = 0xd20;
const RAILS_STATUS_HEADER: usize = 0x20;
const RAILS_STATUS_STRIDE: usize = 0x68;
const RAILS_CONTROL_SIZE: usize = 0x40c;
const RAILS_CONTROL_HEADER: usize = 0x08;
const RAILS_CONTROL_STRIDE: usize = 0x20;

/// Object type byte of both rails on GB202.
const RAIL_TYPE: u8 = 5;
/// Exactly two rails (NVVDD, MSVDD) is the only layout verified.
const EXPECTED_RAIL_MASK: u32 = 0b11;
const RAIL_COUNT: usize = 2;

// STATUS record fields, as the NvAPI wrapper reads them.
const ST_TYPE: usize = 0x00;
const ST_DEFAULT_UV: usize = 0x04;
const ST_REL_LIMIT_UV: usize = 0x0c;
const ST_ALT_REL_LIMIT_UV: usize = 0x10;
const ST_OV_LIMIT_UV: usize = 0x14;
const ST_MAX_LIMIT_UV: usize = 0x18;
const ST_VMIN_LIMIT_UV: usize = 0x1c;
const ST_MARGIN_LIMIT_UV: usize = 0x20;
const ST_NOISE_UNAWARE_VMIN_UV: usize = 0x24;
const ST_SENSED_UV: usize = 0x28;
const ST_TARGET_UV: usize = 0x60;

// CONTROL record fields. +0x08 holds -50000 on the MSVDD rail at stock, which
// is the "-50 mV REL default" mVolt+ documents and matches the 50 mV gap
// between the two rails' REL limits; the remaining words are zero at stock
// and their meaning is not yet established.
const CT_INDEX: usize = 0x00;
const CT_TYPE: usize = 0x04;
const CT_REL_DELTA_UV: usize = 0x08;
const CT_ALT_REL_DELTA_UV: usize = 0x0c;
const CT_OV_DELTA_UV: usize = 0x10;
const CT_VMIN_DELTA_UV: usize = 0x14;

/// Bound on any limit delta the daemon writes, µV (mVolt+ uses ±250 mV).
pub const LIMIT_DELTA_BOUND_UV: i32 = 250_000;

const DEV_INFO_SIZE: usize = 0x708;
const DEV_INFO_RECORD_BASE: usize = 0x08;
const DEV_INFO_STRIDE: usize = 0x38;
/// Voltage-device maximum, µV (1280 mV on the reference card's XOC vBIOS —
/// the "device max" mVolt+ uses as its XOC ceiling).
const DEV_INFO_OFF_MAX_UV: usize = 0x0c;
/// Used when no device reports a plausible maximum.
const DEVICE_MAX_FALLBACK_UV: u32 = 1_250_000;

const CLK_ADC_DEVICES_GET_INFO: u32 = 0x2080_90a0;
const CLK_ADC_DEVICES_GET_STATUS: u32 = 0x2080_90a1;
const ADC_INFO_SIZE: usize = 0x6a8;
const ADC_INFO_RECORD_BASE: usize = 0x28;
const ADC_INFO_STRIDE: usize = 0x34;
/// Per-ADC word that carries a single GPC bit for the core-rail ADCs and a
/// composite value for the fabric-rail one.
const ADC_INFO_OFF_GPC_MASK: usize = 0x08;
const ADC_STATUS_SIZE: usize = 0x40c;
const ADC_STATUS_RECORD_BASE: usize = 0x0c;
const ADC_STATUS_STRIDE: usize = 0x20;
const ADC_STATUS_TYPE: u16 = 0x0303;
const ADC_STATUS_OFF_SAMPLED_UV: usize = 0x08;

#[repr(C)]
#[derive(Clone, Copy)]
struct Bytes<const N: usize>([u8; N]);

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn rd_i32(buf: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn rd_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}

fn rm_call<const N: usize>(
    handle: &DriverHandle,
    cmd: u32,
    init: impl FnOnce(&mut [u8; N]),
) -> anyhow::Result<[u8; N]> {
    let mut buf = Bytes::<N>([0; N]);
    init(&mut buf.0);
    unsafe {
        handle
            .query_rm_control(cmd, &mut buf)
            .with_context(|| format!("RM control {cmd:#010x}"))?;
    }
    Ok(buf.0)
}

fn read_limit_deltas(control: &[u8; RAILS_CONTROL_SIZE]) -> LimitDeltas {
    let mut out = [[0i32; 4]; RAIL_COUNT];
    for (rail, deltas) in out.iter_mut().enumerate() {
        let base = RAILS_CONTROL_HEADER + rail * RAILS_CONTROL_STRIDE;
        for (limit, slot) in RailLimit::ALL.iter().zip(deltas.iter_mut()) {
            *slot = rd_i32(control, base + limit_offset(*limit));
        }
    }
    out
}

fn with_rail_mask<const N: usize>(buf: &mut [u8; N]) {
    buf[0..4].copy_from_slice(&EXPECTED_RAIL_MASK.to_le_bytes());
    buf[4..8].copy_from_slice(&EXPECTED_RAIL_MASK.to_le_bytes());
}

pub use lact_schema::RailLimit;

fn limit_offset(limit: RailLimit) -> usize {
    match limit {
        RailLimit::Vmin => CT_VMIN_DELTA_UV,
        RailLimit::Rel => CT_REL_DELTA_UV,
        RailLimit::AltRel => CT_ALT_REL_DELTA_UV,
        RailLimit::Ov => CT_OV_DELTA_UV,
    }
}

/// The four limit deltas of every rail, µV, indexed `[rail][RailLimit::ALL]`.
pub type LimitDeltas = [[i32; 4]; RAIL_COUNT];

pub fn limit_index(limit: RailLimit) -> usize {
    RailLimit::ALL.iter().position(|l| *l == limit).unwrap()
}

/// Rail order on GB202: 0 = NVVDD (core), 1 = MSVDD (fabric).
pub fn rail_name(index: u8) -> &'static str {
    match index {
        0 => "NVVDD",
        1 => "MSVDD",
        _ => "rail",
    }
}

#[derive(Debug, Clone, Copy)]
pub struct RailStatus {
    pub index: u8,
    pub default_uv: u32,
    pub target_uv: u32,
    pub vmin_limit_uv: u32,
    pub rel_limit_uv: u32,
    pub alt_rel_limit_uv: u32,
    pub ov_limit_uv: u32,
    pub max_limit_uv: u32,
    #[allow(dead_code)]
    pub margin_limit_uv: u32,
    #[allow(dead_code)]
    pub noise_unaware_vmin_uv: u32,
    /// Zero on the verified drivers; the ADCs are the sensed source.
    #[allow(dead_code)]
    pub sensed_uv: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdcKind {
    /// Core rail: the INFO record carries one GPC bit.
    Nvvdd,
    /// Fabric rail: identified on GB202 by lifting the XBAR voltage demand
    /// under load and watching which ADC followed the MSVDD target.
    Msvdd,
}

#[derive(Debug, Clone, Copy)]
struct AdcDevice {
    index: u8,
    kind: AdcKind,
}

/// The verified voltage-rail and ADC objects of one GPU.
#[derive(Debug, Clone)]
pub struct VoltRails {
    adc_mask: u32,
    adcs: Vec<AdcDevice>,
    /// Limit deltas found when the daemon started: the firmware defaults,
    /// unless a delta was left applied across a daemon restart (then they are
    /// that delta until the next boot — the driver keeps no other record).
    start_deltas_uv: LimitDeltas,
    device_max_uv: u32,
}

impl VoltRails {
    /// Verify the layout of every object this module reads, or refuse.
    pub fn probe(handle: &DriverHandle) -> anyhow::Result<Self> {
        let info = rm_call::<RAILS_INFO_SIZE>(handle, VOLT_RAILS_GET_INFO, |_| {})
            .context("VOLT_RAILS_GET_INFO")?;
        let rail_mask = rd_u32(&info, 4);
        if rail_mask != EXPECTED_RAIL_MASK {
            bail!("VOLT_RAILS reports rail mask {rail_mask:#x}; only two rails (NVVDD, MSVDD) are verified");
        }
        for rail in 0..RAIL_COUNT {
            let base = RAILS_INFO_HEADER + rail * RAILS_INFO_STRIDE;
            if info[base] != RAIL_TYPE {
                bail!("VOLT_RAILS INFO rail {rail} has type {:#x}, expected {RAIL_TYPE:#x}", info[base]);
            }
        }

        let status = rm_call::<RAILS_STATUS_SIZE>(handle, VOLT_RAILS_GET_STATUS, with_rail_mask)
            .context("VOLT_RAILS_GET_STATUS")?;
        for rail in 0..RAIL_COUNT {
            let base = RAILS_STATUS_HEADER + rail * RAILS_STATUS_STRIDE;
            if status[base + ST_TYPE] != RAIL_TYPE {
                bail!("VOLT_RAILS STATUS rail {rail} layout check failed");
            }
            let target = rd_u32(&status, base + ST_TARGET_UV);
            if !(300_000..=1_600_000).contains(&target) {
                bail!("VOLT_RAILS STATUS rail {rail} target {target} µV is implausible");
            }
        }

        let control = rm_call::<RAILS_CONTROL_SIZE>(handle, VOLT_RAILS_GET_CONTROL, with_rail_mask)
            .context("VOLT_RAILS_GET_CONTROL")?;
        for rail in 0..RAIL_COUNT {
            let base = RAILS_CONTROL_HEADER + rail * RAILS_CONTROL_STRIDE;
            if rd_u32(&control, base + CT_INDEX) != rail as u32 || control[base + CT_TYPE] != RAIL_TYPE {
                bail!("VOLT_RAILS CONTROL rail {rail} layout check failed");
            }
        }

        let adc_info = rm_call::<ADC_INFO_SIZE>(handle, CLK_ADC_DEVICES_GET_INFO, |_| {})
            .context("CLK_ADC_DEVICES_GET_INFO")?;
        let adc_mask = rd_u32(&adc_info, 4);
        let mut adcs = Vec::new();
        for index in 0..32u8 {
            if adc_mask & (1 << index) == 0 {
                continue;
            }
            let base = ADC_INFO_RECORD_BASE + usize::from(index) * ADC_INFO_STRIDE;
            if base + ADC_INFO_STRIDE > ADC_INFO_SIZE {
                bail!("ADC {index} lies outside the GET_INFO block");
            }
            let gpc_mask = rd_u32(&adc_info, base + ADC_INFO_OFF_GPC_MASK);
            let kind = if gpc_mask.count_ones() == 1 && gpc_mask <= 0xffff {
                AdcKind::Nvvdd
            } else {
                AdcKind::Msvdd
            };
            adcs.push(AdcDevice { index, kind });
        }
        if adcs.iter().filter(|a| a.kind == AdcKind::Nvvdd).count() < 2 {
            bail!("ADC inventory does not look like the verified layout ({} devices)", adcs.len());
        }

        let start_deltas_uv = read_limit_deltas(&control);
        let device_max_uv = rm_call::<DEV_INFO_SIZE>(handle, VOLT_DEVICES_GET_INFO, |_| {})
            .ok()
            .and_then(|dev| {
                let mask = rd_u32(&dev, 4);
                (0..32u8)
                    .filter(|i| mask & (1 << i) != 0)
                    .map(|i| DEV_INFO_RECORD_BASE + usize::from(i) * DEV_INFO_STRIDE)
                    .filter(|base| base + DEV_INFO_STRIDE <= DEV_INFO_SIZE)
                    .map(|base| rd_u32(&dev, base + DEV_INFO_OFF_MAX_UV))
                    .filter(|uv| (1_000_000..=2_000_000).contains(uv))
                    .min()
            })
            .unwrap_or(DEVICE_MAX_FALLBACK_UV);

        let this = Self {
            adc_mask,
            adcs,
            start_deltas_uv,
            device_max_uv,
        };
        // One live read of the status to make sure the record type matches.
        this.adc_sampled(handle).context("CLK_ADC_DEVICES_GET_STATUS")?;
        debug!(
            "RM voltage rails: start limit deltas {:?} µV, device max {} mV",
            start_deltas_uv,
            device_max_uv / 1000
        );
        debug!(
            "RM voltage rails: targets {:?} mV, {} ADCs ({} NVVDD, {} MSVDD)",
            this.rails_status(handle)
                .map(|s| s.iter().map(|r| r.target_uv / 1000).collect::<Vec<_>>())
                .unwrap_or_default(),
            this.adcs.len(),
            this.adcs.iter().filter(|a| a.kind == AdcKind::Nvvdd).count(),
            this.adcs.iter().filter(|a| a.kind == AdcKind::Msvdd).count(),
        );
        Ok(this)
    }

    pub fn adc_count(&self) -> usize {
        self.adcs.len()
    }

    pub fn rails_status(&self, handle: &DriverHandle) -> anyhow::Result<Vec<RailStatus>> {
        let status = rm_call::<RAILS_STATUS_SIZE>(handle, VOLT_RAILS_GET_STATUS, with_rail_mask)?;
        Ok((0..RAIL_COUNT)
            .map(|rail| {
                let base = RAILS_STATUS_HEADER + rail * RAILS_STATUS_STRIDE;
                let f = |off| rd_u32(&status, base + off);
                RailStatus {
                    index: rail as u8,
                    default_uv: f(ST_DEFAULT_UV),
                    target_uv: f(ST_TARGET_UV),
                    vmin_limit_uv: f(ST_VMIN_LIMIT_UV),
                    rel_limit_uv: f(ST_REL_LIMIT_UV),
                    alt_rel_limit_uv: f(ST_ALT_REL_LIMIT_UV),
                    ov_limit_uv: f(ST_OV_LIMIT_UV),
                    max_limit_uv: f(ST_MAX_LIMIT_UV),
                    margin_limit_uv: f(ST_MARGIN_LIMIT_UV),
                    noise_unaware_vmin_uv: f(ST_NOISE_UNAWARE_VMIN_UV),
                    sensed_uv: f(ST_SENSED_UV),
                }
            })
            .collect())
    }

    /// The four limit deltas of each rail currently in the control object, µV.
    pub fn limit_deltas_uv(&self, handle: &DriverHandle) -> anyhow::Result<LimitDeltas> {
        let control = rm_call::<RAILS_CONTROL_SIZE>(handle, VOLT_RAILS_GET_CONTROL, with_rail_mask)?;
        Ok(read_limit_deltas(&control))
    }

    pub fn start_deltas_uv(&self) -> &LimitDeltas {
        &self.start_deltas_uv
    }

    pub fn device_max_uv(&self) -> u32 {
        self.device_max_uv
    }

    /// Write the limit deltas (only the four mapped words of each rail record)
    /// and verify the readback; restore the preimage on any mismatch. Returns
    /// whether anything was written. Bounds are the caller's job.
    pub fn set_limit_deltas(&self, handle: &DriverHandle, wanted: &LimitDeltas) -> anyhow::Result<bool> {
        let preimage = rm_call::<RAILS_CONTROL_SIZE>(handle, VOLT_RAILS_GET_CONTROL, with_rail_mask)?;
        if read_limit_deltas(&preimage) == *wanted {
            return Ok(false);
        }
        let mut block = preimage;
        for (rail, deltas) in wanted.iter().enumerate() {
            let base = RAILS_CONTROL_HEADER + rail * RAILS_CONTROL_STRIDE;
            for (limit, uv) in RailLimit::ALL.iter().zip(deltas) {
                if uv.abs() > LIMIT_DELTA_BOUND_UV {
                    bail!("rail {rail} {} delta {uv} µV exceeds the ±{LIMIT_DELTA_BOUND_UV} µV bound", limit.label());
                }
                let off = base + limit_offset(*limit);
                block[off..off + 4].copy_from_slice(&uv.to_le_bytes());
            }
        }
        let mut to_write = Bytes(block);
        unsafe {
            handle
                .query_rm_control(VOLT_RAILS_SET_CONTROL, &mut to_write)
                .context("VOLT_RAILS_SET_CONTROL")?;
        }
        let readback = rm_call::<RAILS_CONTROL_SIZE>(handle, VOLT_RAILS_GET_CONTROL, with_rail_mask)?;
        if readback[RAILS_CONTROL_HEADER..] != block[RAILS_CONTROL_HEADER..] {
            let mut restore = Bytes(preimage);
            unsafe {
                handle
                    .query_rm_control(VOLT_RAILS_SET_CONTROL, &mut restore)
                    .context("VOLT_RAILS_SET_CONTROL (restore)")?;
            }
            bail!("rail limit readback does not match what was written; previous control object restored");
        }
        Ok(true)
    }

    /// Sampled voltage of every ADC, µV.
    pub fn adc_sampled(&self, handle: &DriverHandle) -> anyhow::Result<Vec<(AdcKind, u32)>> {
        let mask = self.adc_mask;
        let status = rm_call::<ADC_STATUS_SIZE>(handle, CLK_ADC_DEVICES_GET_STATUS, |buf| {
            buf[4..8].copy_from_slice(&mask.to_le_bytes());
        })?;
        let mut out = Vec::with_capacity(self.adcs.len());
        for adc in &self.adcs {
            let base = ADC_STATUS_RECORD_BASE + usize::from(adc.index) * ADC_STATUS_STRIDE;
            if rd_u16(&status, base) != ADC_STATUS_TYPE {
                bail!("ADC {} status record type {:#x} unexpected", adc.index, rd_u16(&status, base));
            }
            out.push((adc.kind, rd_u32(&status, base + ADC_STATUS_OFF_SAMPLED_UV)));
        }
        Ok(out)
    }

    /// Mean sensed voltage per rail in millivolts: `(NVVDD, MSVDD)`.
    pub fn sensed_mv(&self, handle: &DriverHandle) -> (Option<u32>, Option<u32>) {
        let Ok(samples) = self.adc_sampled(handle) else {
            return (None, None);
        };
        let mean = |kind: AdcKind| {
            let values: Vec<u64> = samples
                .iter()
                .filter(|(k, uv)| *k == kind && *uv != 0)
                .map(|(_, uv)| u64::from(*uv))
                .collect();
            if values.is_empty() {
                None
            } else {
                u32::try_from(values.iter().sum::<u64>() / values.len() as u64 / 1000).ok()
            }
        };
        (mean(AdcKind::Nvvdd), mean(AdcKind::Msvdd))
    }
}
