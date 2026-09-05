//! NVIDIA RM "ClockClient" clock-domain controls.
//!
//! These reach the clock domains NVML does not expose (XBAR, SYS, video) and
//! the per-domain voltage-rail offsets, through the same RM control path as
//! the rest of [`DriverHandle`] — but with command IDs and a control-block
//! layout that are **not in the public headers**. The layout was established
//! empirically on GB202 with driver branch R610 (see LACT issue #1147 and the
//! FINDINGS.md that accompanies this work). Everything here self-checks the
//! layout it is about to write to and refuses to run on driver branches it
//! has not been verified on.
//!
//! Rules this module follows, learned the hard way:
//! - Domains are resolved by their `apiDomain` selector from `GET_INFO`,
//!   never by a hardcoded index — the index→domain map is not an identity.
//! - `SET_CONTROL` returning success proves nothing; every write is read back.
//! - A frequency offset that reads back correctly and never crashes can still
//!   produce wrong compute results; nothing here can detect that, and the
//!   validated ranges the GUI enforces come from a correctness harness, not
//!   from this interface.

use std::fmt;

use anyhow::{Context, bail};
use tracing::{debug, warn};

use super::driver::DriverHandle;

const CLK_MEASURE_FREQ: u32 = 0x2080_9006;
const CLK_DOMAINS_GET_INFO: u32 = 0x2080_9019;
const CLK_DOMAINS_GET_CONTROL: u32 = 0x2080_901b;
const CLK_DOMAINS_SET_CONTROL: u32 = 0x2080_d01c;

const CONTROL_SIZE: usize = 0x83c;
const CONTROL_HEADER: usize = 0x3c;
const CONTROL_STRIDE: usize = 0x40;
const CONTROLLABLE_MASK: u32 = 0x0000_00ff;

const INFO_SIZE: usize = 0x3030;
const INFO_HEADER: usize = 0x30;
const INFO_STRIDE: usize = 0x180;
const INFO_OFF_API_DOMAIN: usize = 0x04;
const INFO_OFF_OFFSET_RANGE_MHZ: usize = 0x28;

/// First u16 of every populated domain entry, in both blocks.
const ENTRY_TAG: u16 = 0x1010;

const OFF_FREQ_MODE: usize = 0x08;
const OFF_FREQ_KHZ: usize = 0x0c;
const OFF_RAIL_BASE: usize = 0x10;
const RAIL_COUNT: usize = 4;

/// The rail index that moves XBAR on the RTX 5090. SKU-dependent (reported
/// as rail 0 on the 5060), which is why this is a constant and not a guess
/// baked into the GUI.
pub const MSVDD_RAIL: usize = 1;

/// Driver branches the layout above has been verified against.
const VERIFIED_DRIVER_BRANCHES: &[&str] = &["610"];

/// `apiDomain` selector values as identified on GB202 / 610.57.04.
const API_DOMAIN_GPC: u32 = 0x0000_0001;
const API_DOMAIN_XBAR: u32 = 0x0000_0002;
const API_DOMAIN_SYS: u32 = 0x0000_0004;
const API_DOMAIN_MEMORY: u32 = 0x0000_0010;
const API_DOMAIN_VIDEO: u32 = 0x0010_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RmDomainKind {
    Gpc,
    Xbar,
    Sys,
    Video,
    Memory,
}

impl RmDomainKind {
    fn from_api_domain(api_domain: u32) -> Option<Self> {
        match api_domain {
            API_DOMAIN_GPC => Some(Self::Gpc),
            API_DOMAIN_XBAR => Some(Self::Xbar),
            API_DOMAIN_SYS => Some(Self::Sys),
            API_DOMAIN_MEMORY => Some(Self::Memory),
            API_DOMAIN_VIDEO => Some(Self::Video),
            _ => None,
        }
    }
}

impl fmt::Display for RmDomainKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Gpc => "GPCCLK",
            Self::Xbar => "XBARCLK",
            Self::Sys => "SYSCLK",
            Self::Video => "VIDCLK",
            Self::Memory => "DRAMCLK",
        })
    }
}

#[derive(Debug, Clone)]
pub struct RmClockDomain {
    /// Index into the control block. Not related to `api_domain`'s bit.
    pub index: u8,
    pub api_domain: u32,
    /// Driver-permitted frequency offset range, MHz. 0 = locked.
    pub offset_range_mhz: u32,
    pub controllable: bool,
    pub kind: Option<RmDomainKind>,
}

impl RmClockDomain {
    pub fn name(&self) -> String {
        match self.kind {
            Some(kind) => kind.to_string(),
            None => format!("DOMAIN_{}", self.index),
        }
    }
}

/// The 0x83c `CLK_DOMAINS_*_CONTROL` block. Opaque bytes with typed accessors
/// for the fields whose meaning has been established.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ControlBlock([u8; CONTROL_SIZE]);

#[repr(C)]
#[derive(Clone, Copy)]
struct InfoBlock([u8; INFO_SIZE]);

#[repr(C)]
#[derive(Clone, Copy)]
struct MeasureFreqParams {
    domain: u32,
    khz: u32,
}

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn rd_i32(buf: &[u8], off: usize) -> i32 {
    i32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

fn rd_u16(buf: &[u8], off: usize) -> u16 {
    u16::from_le_bytes(buf[off..off + 2].try_into().unwrap())
}

impl ControlBlock {
    fn entry(index: u8) -> usize {
        CONTROL_HEADER + usize::from(index) * CONTROL_STRIDE
    }

    pub fn freq_offset_khz(&self, index: u8) -> i32 {
        rd_i32(&self.0, Self::entry(index) + OFF_FREQ_KHZ)
    }

    pub fn set_freq_offset_khz(&mut self, index: u8, khz: i32) {
        let base = Self::entry(index);
        // Mode 0 is what the reference implementation writes; its other
        // values are unknown, so never carry one over.
        self.0[base + OFF_FREQ_MODE..base + OFF_FREQ_MODE + 4].copy_from_slice(&0u32.to_le_bytes());
        self.0[base + OFF_FREQ_KHZ..base + OFF_FREQ_KHZ + 4].copy_from_slice(&khz.to_le_bytes());
    }

    pub fn rail_offset_uv(&self, index: u8, rail: usize) -> i32 {
        rd_i32(&self.0, Self::entry(index) + OFF_RAIL_BASE + rail * 4)
    }

    pub fn set_rail_offset_uv(&mut self, index: u8, rail: usize, uv: i32) {
        let off = Self::entry(index) + OFF_RAIL_BASE + rail * 4;
        self.0[off..off + 4].copy_from_slice(&uv.to_le_bytes());
    }

    /// Every tunable field, for comparing a readback against what was written.
    fn tunables(&self) -> Vec<(i32, [i32; RAIL_COUNT])> {
        (0..8u8)
            .map(|i| {
                let mut rails = [0; RAIL_COUNT];
                for (r, slot) in rails.iter_mut().enumerate() {
                    *slot = self.rail_offset_uv(i, r);
                }
                (self.freq_offset_khz(i), rails)
            })
            .collect()
    }

    pub fn is_stock(&self) -> bool {
        self.tunables()
            .iter()
            .all(|(f, rails)| *f == 0 && rails.iter().all(|r| *r == 0))
    }

    fn zeroed(&self) -> Self {
        let mut out = *self;
        for i in 0..8u8 {
            out.set_freq_offset_khz(i, 0);
            for r in 0..RAIL_COUNT {
                out.set_rail_offset_uv(i, r, 0);
            }
        }
        out
    }
}

/// Whether the driver version string (e.g. `610.57.04`) belongs to a branch
/// this module's layout has been verified against.
pub fn driver_branch_verified(driver_version: &str) -> bool {
    driver_version
        .split('.')
        .next()
        .is_some_and(|branch| VERIFIED_DRIVER_BRANCHES.contains(&branch))
}

impl DriverHandle {
    /// Enumerate the RM clock domains, verifying the private block layout on
    /// the way. Fails closed on anything unexpected.
    pub fn clk_domains_get_info(&self) -> anyhow::Result<Vec<RmClockDomain>> {
        let mut info = InfoBlock([0; INFO_SIZE]);
        info.0[4..8].copy_from_slice(&CONTROLLABLE_MASK.to_le_bytes());
        unsafe {
            self.query_rm_control(CLK_DOMAINS_GET_INFO, &mut info)
                .context("CLK_DOMAINS_GET_INFO")?;
        }
        let controllable = rd_u32(&info.0, 0);
        let full_mask = rd_u32(&info.0, 4);
        if full_mask == 0 || full_mask.count_ones() > 32 {
            bail!("CLK_DOMAINS_GET_INFO returned an implausible domain mask {full_mask:#x}");
        }

        let mut domains = Vec::new();
        for index in 0..32u8 {
            if full_mask & (1 << index) == 0 {
                continue;
            }
            let base = INFO_HEADER + usize::from(index) * INFO_STRIDE;
            if base + INFO_STRIDE > INFO_SIZE {
                bail!("domain {index} lies outside the GET_INFO block");
            }
            let tag = rd_u16(&info.0, base);
            if tag != ENTRY_TAG {
                bail!(
                    "GET_INFO layout check failed: domain {index} tag {tag:#06x}, expected {ENTRY_TAG:#06x}. \
                     The control-block layout is driver-branch specific; refusing to continue"
                );
            }
            let api_domain = rd_u32(&info.0, base + INFO_OFF_API_DOMAIN);
            if api_domain.count_ones() != 1 {
                bail!("domain {index} has a non-single-bit apiDomain {api_domain:#x}");
            }
            domains.push(RmClockDomain {
                index,
                api_domain,
                offset_range_mhz: rd_u32(&info.0, base + INFO_OFF_OFFSET_RANGE_MHZ),
                controllable: controllable & (1 << index) != 0,
                kind: RmDomainKind::from_api_domain(api_domain),
            });
        }

        // The control block must agree with the info block on where entries
        // sit, or a write would land somewhere we did not intend.
        let control = self.clk_domains_get_control()?;
        for domain in domains.iter().filter(|d| d.controllable) {
            let tag = rd_u16(&control.0, ControlBlock::entry(domain.index));
            if tag != ENTRY_TAG {
                bail!(
                    "GET_CONTROL layout check failed: domain {} tag {tag:#06x}",
                    domain.index
                );
            }
        }

        debug!(
            "RM clock domains: {}",
            domains
                .iter()
                .map(|d| format!(
                    "{}[{}] api={:#x} range=±{} MHz{}",
                    d.name(),
                    d.index,
                    d.api_domain,
                    d.offset_range_mhz,
                    if d.controllable { "" } else { " (read-only)" }
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        Ok(domains)
    }

    pub fn clk_domains_get_control(&self) -> anyhow::Result<ControlBlock> {
        let mut block = ControlBlock([0; CONTROL_SIZE]);
        block.0[4..8].copy_from_slice(&CONTROLLABLE_MASK.to_le_bytes());
        unsafe {
            self.query_rm_control(CLK_DOMAINS_GET_CONTROL, &mut block)
                .context("CLK_DOMAINS_GET_CONTROL")?;
        }
        Ok(block)
    }

    /// Write a control block and verify every tunable field read back exactly.
    /// A successful status alone is not trusted.
    pub fn clk_domains_set_control(&self, block: &ControlBlock) -> anyhow::Result<()> {
        let mut to_write = *block;
        unsafe {
            self.query_rm_control(CLK_DOMAINS_SET_CONTROL, &mut to_write)
                .context("CLK_DOMAINS_SET_CONTROL")?;
        }
        let readback = self.clk_domains_get_control()?;
        if readback.tunables() != block.tunables() {
            bail!("clock control readback does not match what was written");
        }
        Ok(())
    }

    /// Measured frequency of a domain, in kHz, by its `apiDomain` selector.
    pub fn clk_measure_khz(&self, api_domain: u32) -> anyhow::Result<u32> {
        let mut params = MeasureFreqParams {
            domain: api_domain,
            khz: 0,
        };
        unsafe {
            self.query_rm_control(CLK_MEASURE_FREQ, &mut params)
                .context("CLK_MEASURE_FREQ")?;
        }
        Ok(params.khz)
    }

    /// Zero every frequency and rail offset if any is set.
    pub fn clk_domains_reset(&self) -> anyhow::Result<bool> {
        let current = self.clk_domains_get_control()?;
        if current.is_stock() {
            return Ok(false);
        }
        self.clk_domains_set_control(&current.zeroed())?;
        Ok(true)
    }
}

/// Try to enable the ClockClient interface for a driver handle. Returns the
/// domain list on success; logs and returns `None` on any refusal so the
/// caller simply does not offer the controls.
pub fn probe(handle: &DriverHandle, driver_version: &str) -> Option<Vec<RmClockDomain>> {
    if !driver_branch_verified(driver_version) {
        warn!(
            "RM clock domain controls disabled: driver {driver_version} is not a verified branch \
             ({VERIFIED_DRIVER_BRANCHES:?}). The control-block layout is private and must be \
             re-verified per branch."
        );
        return None;
    }
    match handle.clk_domains_get_info() {
        Ok(domains) => Some(domains),
        Err(err) => {
            warn!("RM clock domain controls disabled: {err:#}");
            None
        }
    }
}
