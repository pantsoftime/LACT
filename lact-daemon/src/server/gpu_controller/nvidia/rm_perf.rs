//! The RM performance-limit clients — what mVolt+ calls "boost limits".
//!
//! `PERF_LIMITS_GET_STATUS_V2` (`0x2080a079`, 0x14804 bytes) returns one
//! record per requested limit ID. The arbiter combines these clients per
//! clock domain; a populated ("valid") client carries the value it asks for
//! and the resulting clock. Command ID and size come from the size table in
//! `libnvidia-api.so`; the record layout was decoded on GB202 / R615 by
//! reading all 256 IDs at idle and under load:
//!
//! | offset  | meaning |
//! |---------|---------|
//! | +0x000  | limit ID (echo of the request) |
//! | +0x004  | type: 2 = frequency limit, 6 = voltage-policy limit, 1/3 = P-state style |
//! | +0x00c  | type 2: frequency kHz; type 6: 1 (NVVDD) / 3 (MSVDD) |
//! | +0x010  | type 2: `apiDomain` mask (same encoding as `CLK_MEASURE_FREQ`) |
//! | +0x020  | type 6: limit code, bit 8 = MSVDD rail |
//! | +0x034  | type 6: the policy voltage, µV (matches the rail STATUS limits) |
//! | +0x13c  | valid byte |
//! | +0x144  | resulting clock, kHz, or 0xffffffff |
//!
//! Requesting all IDs in one call is rejected; 128 per call is accepted. The
//! table extends past 0xff: on GB202 / R615 the clients 0x10f–0x114 are
//! populated, and 0x110 (GPC) / 0x111 (XBAR) are the **power-policy
//! limits** — they tracked a 400 W cap (3202 → 1642, 2880 → 2512 MHz) while
//! nothing below 0xff moved. IDs up to 0x13f are read; every ID up to 4095 is
//! accepted by the driver but none above 0x114 was populated.
//! Read-only. Which client *binds* is not flagged by the driver; the caller
//! reports the tightest maximum it can identify and lists the rest.

use anyhow::{Context, bail};
use tracing::debug;

use super::driver::DriverHandle;
use super::rm_perf_names::NVML_PERF_LIMIT_NAMES;

/// NVIDIA's own name for a limit client (without the `PERF_LIMIT_` prefix),
/// from the table inside `libnvidia-ml.so`. `None` for IDs NVML does not know
/// (the Blackwell clients above 0x10f).
pub fn nvml_name(id: u32) -> Option<&'static str> {
    NVML_PERF_LIMIT_NAMES
        .binary_search_by_key(&id, |(i, _)| *i)
        .ok()
        .map(|i| NVML_PERF_LIMIT_NAMES[i].1)
}

/// Readable rendering of an NVML limit name. "LOGIC" is the NVVDD rail and
/// "SRAM" the MSVDD rail in NVIDIA's terms; DOM_GRP_1 is the GPC clock group,
/// DOM_GRP_0 the P-state group; CLIENT_STRICT_* are the locked-clock requests.
pub fn friendly_name(nvml: &str) -> String {
    let rail = |s: &str| {
        if s.contains("LOGIC") {
            "NVVDD"
        } else if s.contains("SRAM") {
            "MSVDD"
        } else {
            "?"
        }
    };
    let domain = |s: &str| -> String {
        let last = s.rsplit('_').next().unwrap_or("");
        let body = s.trim_end_matches("_MIN").trim_end_matches("_MAX");
        let d = body.rsplit('_').next().unwrap_or("");
        match d {
            "1" if body.contains("DOM_GRP") => "GPC".to_owned(),
            "0" if body.contains("DOM_GRP") => "P-state".to_owned(),
            "GPC" | "XBAR" | "DRAM" | "NVD" | "DISP" | "SYS" => d.to_owned(),
            _ => last.to_owned(),
        }
    };
    let floor = nvml.ends_with("_MIN");
    if let Some(rest) = nvml.strip_prefix("RELIABILITY_ALT_") {
        return format!("Operating voltage limit ({}, ALT/OP)", rail(rest));
    }
    if let Some(rest) = nvml.strip_prefix("RELIABILITY_") {
        return format!("Reliability voltage limit ({})", rail(rest));
    }
    if let Some(rest) = nvml.strip_prefix("OVERVOLTAGE_") {
        return format!("Overvoltage ceiling ({})", rail(rest));
    }
    if let Some(rest) = nvml.strip_prefix("VMIN_") {
        return format!("Minimum voltage ({})", rail(rest));
    }
    if nvml.starts_with("PERF_CF_CONTROLLER_") {
        return format!(
            "Boost controller {} ({})",
            if floor { "floor" } else { "ceiling" },
            domain(nvml)
        );
    }
    if nvml.starts_with("THERM_POLICY_") {
        return format!("Thermal policy ({})", domain(nvml));
    }
    if nvml.starts_with("PWR_POLICY_") {
        return format!("Power policy ({})", domain(nvml));
    }
    if nvml.starts_with("CLIENT_STRICT_") || nvml.starts_with("CLIENT_LOW_STRICT_") {
        return format!(
            "Locked clock {} ({})",
            if floor { "minimum" } else { "maximum" },
            domain(nvml)
        );
    }
    if nvml.starts_with("PMU_DOM_GRP_") {
        return format!("PMU clock limit ({})", domain(nvml));
    }
    match nvml {
        "PMU_OVERRIDE" => "PMU override".to_owned(),
        "CUDA_MAX" => "CUDA context maximum".to_owned(),
        "ISMODEPOSSIBLE" => "Display mode requirement (P-state)".to_owned(),
        "ISMODEPOSSIBLE_DISP" => "Display mode requirement (display clock)".to_owned(),
        "PERFMON" => "Performance monitor".to_owned(),
        _ => {
            let words: Vec<String> = nvml
                .split('_')
                .map(|w| match w {
                    "LOGIC" => "NVVDD".to_owned(),
                    "SRAM" => "MSVDD".to_owned(),
                    "MIN" => "minimum".to_owned(),
                    "MAX" => "maximum".to_owned(),
                    w => {
                        let mut c = w.chars();
                        match c.next() {
                            Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase(),
                            None => String::new(),
                        }
                    }
                })
                .collect();
            words.join(" ")
        }
    }
}

const PERF_LIMITS_GET_STATUS_V2: u32 = 0x2080_a079;
const SIZE: usize = 0x14804;
const BASE: usize = 0x4;
const STRIDE: usize = 0x148;
const BATCH: usize = 128;
const ID_COUNT: usize = 0x140;

/// Clients identified by manipulation on GB202 (see the module docs).
pub const POWER_POLICY_IDS: [u32; 2] = [0x110, 0x111];

const OFF_TYPE: usize = 0x04;
const OFF_VALUE: usize = 0x0c;
const OFF_DOMAIN_MASK: usize = 0x10;
const OFF_LIMIT_CODE: usize = 0x20;
const OFF_VOLTAGE_UV: usize = 0x34;
const OFF_VALID: usize = 0x13c;
const OFF_RESULT_KHZ: usize = 0x144;

const TYPE_FREQUENCY: u32 = 2;
const TYPE_VOLTAGE: u32 = 6;
const NO_RESULT: u32 = 0xffff_ffff;

#[repr(C)]
#[derive(Clone, Copy)]
struct Bytes([u8; SIZE]);

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PerfLimitKind {
    /// A frequency cap or floor on the domains in the mask.
    Frequency { khz: u32, domain_mask: u32 },
    /// A voltage-policy limit (VMIN / REL / ALT / OV of a rail).
    Voltage { uv: u32, rail: u8 },
    /// P-state style or unknown; `value` is the raw word at +0x0c.
    Other { type_code: u32, value: u32 },
}

#[derive(Debug, Clone, Copy)]
pub struct PerfLimit {
    pub id: u32,
    pub kind: PerfLimitKind,
    pub result_khz: Option<u32>,
}

/// Marker that the status object reads and parses on this driver.
#[derive(Debug, Clone, Copy)]
pub struct PerfLimits;

impl PerfLimits {
    pub fn probe(handle: &DriverHandle) -> anyhow::Result<Self> {
        let limits = read_all(handle)?;
        let plausible = limits.iter().any(|l| {
            matches!(l.kind, PerfLimitKind::Frequency { khz, .. } if (100_000..=20_000_000).contains(&khz))
        });
        if !plausible {
            bail!("PERF_LIMITS status has no plausible frequency limit; layout not recognised");
        }
        debug!(
            "PERF limits: {} populated clients: {}",
            limits.len(),
            limits
                .iter()
                .map(|l| format!("{:#04x}", l.id))
                .collect::<Vec<_>>()
                .join(" ")
        );
        Ok(Self)
    }

    pub fn read(&self, handle: &DriverHandle) -> anyhow::Result<Vec<PerfLimit>> {
        read_all(handle)
    }
}

fn read_all(handle: &DriverHandle) -> anyhow::Result<Vec<PerfLimit>> {
    let mut out = Vec::new();
    for start in (0..ID_COUNT).step_by(BATCH) {
        let ids: Vec<u32> = (start..(start + BATCH).min(ID_COUNT)).map(|i| i as u32).collect();
        let mut buf = Bytes([0; SIZE]);
        buf.0[0..4].copy_from_slice(&(ids.len() as u32).to_le_bytes());
        for (i, id) in ids.iter().enumerate() {
            let r = BASE + i * STRIDE;
            buf.0[r..r + 4].copy_from_slice(&id.to_le_bytes());
        }
        unsafe {
            handle
                .query_rm_control(PERF_LIMITS_GET_STATUS_V2, &mut buf)
                .context("PERF_LIMITS_GET_STATUS_V2")?;
        }
        for (i, id) in ids.iter().enumerate() {
            let r = BASE + i * STRIDE;
            if rd_u32(&buf.0, r) != *id {
                bail!("PERF_LIMITS status entry {i} echoed a different ID");
            }
            if buf.0[r + OFF_VALID] == 0 {
                continue;
            }
            let type_code = rd_u32(&buf.0, r + OFF_TYPE);
            let value = rd_u32(&buf.0, r + OFF_VALUE);
            let kind = match type_code {
                TYPE_FREQUENCY => PerfLimitKind::Frequency {
                    khz: value,
                    domain_mask: rd_u32(&buf.0, r + OFF_DOMAIN_MASK),
                },
                TYPE_VOLTAGE => PerfLimitKind::Voltage {
                    uv: rd_u32(&buf.0, r + OFF_VOLTAGE_UV),
                    rail: u8::from(rd_u32(&buf.0, r + OFF_LIMIT_CODE) & 0x100 != 0),
                },
                _ => PerfLimitKind::Other { type_code, value },
            };
            let result = rd_u32(&buf.0, r + OFF_RESULT_KHZ);
            out.push(PerfLimit {
                id: *id,
                kind,
                result_khz: (result != NO_RESULT && result != 0).then_some(result),
            });
        }
    }
    Ok(out)
}
