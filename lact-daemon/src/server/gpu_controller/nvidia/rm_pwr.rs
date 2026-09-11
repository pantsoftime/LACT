//! The RM PMGR power policies: the board TGP and the per-rail current limits
//! (what mVolt+ calls "OCP"), plus the live rail currents they are measured
//! against.
//!
//! `PWR_POLICIES` `GET_INFO` / `GET_STATUS` / `GET_CONTROL` / `SET_CONTROL` are
//! private controls with no public header. The IDs and sizes come from the
//! size table in `libnvidia-api.so`; the layouts were decoded on GB202 / R615
//! and confirmed by manipulation (the tooling repo's
//! `rm_probe_pwrpolicy_limit.py` runs of 2026-09-11):
//!
//! - INFO (`0x2080a618`, 0x5020): policy mask at +4; entry i at
//!   `0xd0 + i*0x104` starts with the boardobj header `{type, chIdx,
//!   limitUnit, numLimitInputs}` — limitUnit 0 = mW, 1 = mA — then limitMin /
//!   limitRated / limitMax. Header byte +0x98 is the TGP policy's index.
//! - CONTROL (`0x2080a61a`, 0x3634): policy mask at **+0x10**; entry i at
//!   `0x14 + i*0xc4`: +0 the policy's type byte (the boardobj header
//!   again), +4 the client limit. `SET_CONTROL`
//!   (`0x2080e61b`) takes the same block; it is what `NvAPI`'s power-policy
//!   setter writes, and the TGP entry reads NVML's power cap exactly.
//! - STATUS (`0x2080a619`, 0x60ef8): policy mask at +4; block i at
//!   `0xa0 + i*0x1730`: +0 the arbitrated limit, +4 the channel reading in
//!   the policy's unit.
//!
//! On the reference card the two milliamp policies with a finite rated limit
//! and an unlimited maximum are the rail OCPs: 480 A on the NVVDD channel
//! and 180 A on MSVDD (the larger rated limit is the core rail). Lowering the
//! NVVDD one to 100 A throttled the core within a second through the PMU's
//! GPC limit client (board power 558 → 150 W); lowering the MSVDD one to
//! 50 A did the same (613 → 250 W). Neither binds at the 620 W TGP, where
//! the readings sit near 307–373 A / 72 A under a steady load.
//!
//! Every probe fails closed, and the TGP entry is cross-checked against
//! NVML's power cap, so a layout change disables the feature rather than
//! writing into an unknown structure. The setter reads a fresh preimage,
//! changes only the two limit words, requires an exact readback and restores
//! the preimage on any mismatch.

use anyhow::{Context, bail};
use tracing::debug;

use super::driver::DriverHandle;
use super::rm_volt::rail_name;

const PWR_POLICIES_GET_INFO: u32 = 0x2080_a618;
const PWR_POLICIES_GET_STATUS: u32 = 0x2080_a619;
const PWR_POLICIES_GET_CONTROL: u32 = 0x2080_a61a;
const PWR_POLICIES_SET_CONTROL: u32 = 0x2080_e61b;

const INFO_SIZE: usize = 0x5020;
const INFO_MASK: usize = 0x04;
const INFO_TGP_INDEX: usize = 0x98;
const INFO_ENTRY_BASE: usize = 0xd0;
const INFO_ENTRY_STRIDE: usize = 0x104;
const IE_TYPE: usize = 0x00;
const IE_CHANNEL: usize = 0x01;
const IE_UNIT: usize = 0x02;
const IE_LIMIT_MIN: usize = 0x04;
const IE_LIMIT_RATED: usize = 0x08;
const IE_LIMIT_MAX: usize = 0x0c;

const CONTROL_SIZE: usize = 0x3634;
const CONTROL_MASK: usize = 0x10;
const CONTROL_ENTRY_BASE: usize = 0x14;
const CONTROL_ENTRY_STRIDE: usize = 0xc4;
const CE_TYPE: usize = 0x00;
const CE_LIMIT: usize = 0x04;

const STATUS_SIZE: usize = 0x60ef8;
const STATUS_MASK: usize = 0x04;
const STATUS_BLOCK_BASE: usize = 0xa0;
const STATUS_BLOCK_STRIDE: usize = 0x1730;
const SB_LIMIT: usize = 0x00;
const SB_VALUE: usize = 0x04;

const UNIT_MW: u8 = 0;
const UNIT_MA: u8 = 1;
const MAX_POLICIES: usize = 32;
pub const RAIL_COUNT: usize = 2;

/// Lowest current limit the daemon will write, mA: below this a rail is
/// simply starved (100 A on NVVDD already leaves 150 W of board power).
pub const MIN_LIMIT_MA: u32 = 50_000;
/// Highest, as a multiple of the firmware's rated limit — NV-Voltelle's
/// rule. The driver itself accepts 5001 A.
pub const MAX_LIMIT_FACTOR: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
struct Bytes<const N: usize>([u8; N]);

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

/// One RM call on a heap-allocated request: the STATUS object is 400 KB.
fn rm_call<const N: usize>(
    handle: &DriverHandle,
    cmd: u32,
    init: impl FnOnce(&mut [u8; N]),
) -> anyhow::Result<Box<Bytes<N>>> {
    // SAFETY: an all-zero byte array is a valid `Bytes<N>`.
    let mut buf: Box<Bytes<N>> = unsafe { Box::<Bytes<N>>::new_zeroed().assume_init() };
    init(&mut buf.0);
    unsafe {
        handle
            .query_rm_control(cmd, &mut *buf)
            .with_context(|| format!("RM control {cmd:#010x}"))?;
    }
    Ok(buf)
}

fn read_control(handle: &DriverHandle, mask: u32) -> anyhow::Result<Box<Bytes<CONTROL_SIZE>>> {
    rm_call::<CONTROL_SIZE>(handle, PWR_POLICIES_GET_CONTROL, |b| {
        b[CONTROL_MASK..CONTROL_MASK + 4].copy_from_slice(&mask.to_le_bytes());
    })
    .context("PWR_POLICIES_GET_CONTROL")
}

fn control_limit_offset(index: u8) -> usize {
    CONTROL_ENTRY_BASE + usize::from(index) * CONTROL_ENTRY_STRIDE + CE_LIMIT
}

fn control_limit(control: &Bytes<CONTROL_SIZE>, index: u8) -> u32 {
    rd_u32(&control.0, control_limit_offset(index))
}

/// Rail label for a rail slot (`RAIL_COUNT` is 2, so this never truncates).
fn rail_label(rail: usize) -> &'static str {
    rail_name(u8::try_from(rail).unwrap_or(u8::MAX))
}

/// One power policy as described by `GET_INFO`.
#[derive(Debug, Clone, Copy)]
pub struct PolicyInfo {
    pub index: u8,
    pub type_code: u8,
    pub channel: u8,
    pub unit: u8,
    pub limit_min: u32,
    pub limit_rated: u32,
    pub limit_max: u32,
}

/// The verified power-policy objects of one GPU.
#[derive(Debug, Clone)]
pub struct PowerPolicies {
    mask: u32,
    policies: Vec<PolicyInfo>,
    /// Policy index per rail: `[NVVDD, MSVDD]`.
    rail_policy: [u8; RAIL_COUNT],
    /// Limits found when the daemon started, mA per rail: the firmware
    /// defaults unless a limit was left applied across a daemon restart.
    start_limits_ma: [u32; RAIL_COUNT],
}

impl PowerPolicies {
    /// Verify the layout of every object this module touches, or refuse.
    /// `tgp_cap_mw` is NVML's current power cap; when known, the TGP policy's
    /// CONTROL limit must read exactly that.
    pub fn probe(handle: &DriverHandle, tgp_cap_mw: Option<u32>) -> anyhow::Result<Self> {
        let info = rm_call::<INFO_SIZE>(handle, PWR_POLICIES_GET_INFO, |_| {})
            .context("PWR_POLICIES_GET_INFO")?;
        let mask = rd_u32(&info.0, INFO_MASK);
        let count = mask.count_ones() as usize;
        #[allow(clippy::cast_possible_truncation)]
        let contiguous = ((1u64 << count) - 1) as u32;
        if count == 0 || count > MAX_POLICIES || mask != contiguous {
            bail!("PWR_POLICIES mask {mask:#x} is not a contiguous set; layout not verified");
        }
        let mut policies = Vec::with_capacity(count);
        for i in 0..count {
            let base = INFO_ENTRY_BASE + i * INFO_ENTRY_STRIDE;
            #[allow(clippy::cast_possible_truncation)]
            let p = PolicyInfo {
                index: i as u8,
                type_code: info.0[base + IE_TYPE],
                channel: info.0[base + IE_CHANNEL],
                unit: info.0[base + IE_UNIT],
                limit_min: rd_u32(&info.0, base + IE_LIMIT_MIN),
                limit_rated: rd_u32(&info.0, base + IE_LIMIT_RATED),
                limit_max: rd_u32(&info.0, base + IE_LIMIT_MAX),
            };
            if p.unit > UNIT_MA
                || p.limit_max == 0
                || p.limit_min > p.limit_rated
                || p.limit_rated > p.limit_max
            {
                bail!(
                    "PWR_POLICIES entry {i} does not parse as a policy (unit {}, limits {} / {} / {})",
                    p.unit,
                    p.limit_min,
                    p.limit_rated,
                    p.limit_max
                );
            }
            policies.push(p);
        }

        // Each CONTROL entry repeats the policy's type byte; a mismatch means
        // the entries are not where this module thinks they are.
        let control = read_control(handle, mask)?;
        for p in &policies {
            let base = CONTROL_ENTRY_BASE + usize::from(p.index) * CONTROL_ENTRY_STRIDE;
            if control.0[base + CE_TYPE] != p.type_code {
                bail!(
                    "PWR_POLICIES CONTROL entry {} carries type {:#x}, INFO says {:#x}; layout not verified",
                    p.index,
                    control.0[base + CE_TYPE],
                    p.type_code
                );
            }
        }

        // The TGP entry pins the CONTROL layout: it must read NVML's cap.
        let tgp = usize::from(info.0[INFO_TGP_INDEX]);
        let Some(tgp_policy) = policies.get(tgp) else {
            bail!("TGP policy index {tgp} is out of range");
        };
        if tgp_policy.unit != UNIT_MW {
            bail!("TGP policy {tgp} is not in milliwatts");
        }
        if let Some(cap) = tgp_cap_mw {
            let limit = control_limit(&control, tgp_policy.index);
            if limit != cap {
                bail!("TGP policy limit {limit} mW does not match NVML's power cap {cap} mW; CONTROL layout not verified");
            }
        }

        // The rail OCPs: milliamps, a finite rated limit under an unlimited
        // maximum. The larger rated limit is the core rail.
        let mut leaves: Vec<&PolicyInfo> = policies
            .iter()
            .filter(|p| p.unit == UNIT_MA && p.limit_rated < p.limit_max && p.limit_rated >= MIN_LIMIT_MA)
            .collect();
        if leaves.len() != RAIL_COUNT {
            bail!(
                "expected {RAIL_COUNT} settable rail current-limit policies, found {}",
                leaves.len()
            );
        }
        leaves.sort_by_key(|p| std::cmp::Reverse(p.limit_rated));
        if leaves[0].limit_rated == leaves[1].limit_rated {
            bail!("the two rail current limits are equal; cannot tell NVVDD from MSVDD");
        }
        let rail_policy = [leaves[0].index, leaves[1].index];
        let start_limits_ma = rail_policy.map(|i| control_limit(&control, i));

        let this = Self {
            mask,
            policies,
            rail_policy,
            start_limits_ma,
        };
        let status = this.rail_status(handle).context("PWR_POLICIES_GET_STATUS")?;
        for (rail, (arbitrated, value)) in status.iter().enumerate() {
            let p = this.rail_policy(rail);
            if *arbitrated > p.limit_max || *value > p.limit_max {
                bail!(
                    "{} policy STATUS reads limit {arbitrated} / value {value} mA, above the maximum {}; layout not verified",
                    rail_label(rail),
                    p.limit_max
                );
            }
        }
        debug!(
            "RM power policies: {count} policies, TGP entry {tgp}, rail limits {:?} mA (rated {:?}, channels {:?}), readings {:?} mA",
            start_limits_ma,
            rail_policy.map(|i| this.policies[usize::from(i)].limit_rated),
            rail_policy.map(|i| this.policies[usize::from(i)].channel),
            status.map(|(_, v)| v)
        );
        Ok(this)
    }

    fn rail_policy(&self, rail: usize) -> &PolicyInfo {
        &self.policies[usize::from(self.rail_policy[rail])]
    }

    /// The firmware's rated limit of a rail, mA.
    pub fn rated_ma(&self, rail: usize) -> u32 {
        self.rail_policy(rail).limit_rated
    }

    pub fn start_limits_ma(&self) -> &[u32; RAIL_COUNT] {
        &self.start_limits_ma
    }

    /// The range the daemon accepts for a rail, mA.
    pub fn accepted_range_ma(&self, rail: usize) -> (u32, u32) {
        let p = self.rail_policy(rail);
        (
            MIN_LIMIT_MA.max(p.limit_min),
            p.limit_rated.saturating_mul(MAX_LIMIT_FACTOR).min(p.limit_max),
        )
    }

    /// The client limit of each rail policy in the control object, mA.
    pub fn limits_ma(&self, handle: &DriverHandle) -> anyhow::Result<[u32; RAIL_COUNT]> {
        let control = read_control(handle, self.mask)?;
        Ok(self.rail_policy.map(|i| control_limit(&control, i)))
    }

    /// The arbitrated limit and the live channel reading of each rail policy, mA.
    pub fn rail_status(&self, handle: &DriverHandle) -> anyhow::Result<[(u32, u32); RAIL_COUNT]> {
        let mask = self.mask;
        let status = rm_call::<STATUS_SIZE>(handle, PWR_POLICIES_GET_STATUS, |b| {
            b[STATUS_MASK..STATUS_MASK + 4].copy_from_slice(&mask.to_le_bytes());
        })?;
        Ok(self.rail_policy.map(|i| {
            let base = STATUS_BLOCK_BASE + usize::from(i) * STATUS_BLOCK_STRIDE;
            (
                rd_u32(&status.0, base + SB_LIMIT),
                rd_u32(&status.0, base + SB_VALUE),
            )
        }))
    }

    /// Write the two rail limits (only their limit words) and verify the
    /// readback; restore the preimage on any mismatch. Returns whether
    /// anything was written. A value outside the accepted range is refused
    /// unless it is the one found at start.
    pub fn set_limits_ma(&self, handle: &DriverHandle, wanted: [u32; RAIL_COUNT]) -> anyhow::Result<bool> {
        let preimage = read_control(handle, self.mask)?;
        let current = self.rail_policy.map(|i| control_limit(&preimage, i));
        if current == wanted {
            return Ok(false);
        }
        let mut block = preimage.clone();
        for (rail, ma) in wanted.iter().enumerate() {
            let (lo, hi) = self.accepted_range_ma(rail);
            if !(lo..=hi).contains(ma) && *ma != self.start_limits_ma[rail] {
                bail!(
                    "{} current limit {} A is outside the accepted {}…{} A",
                    rail_label(rail),
                    ma / 1000,
                    lo / 1000,
                    hi / 1000
                );
            }
            let off = control_limit_offset(self.rail_policy[rail]);
            block.0[off..off + 4].copy_from_slice(&ma.to_le_bytes());
        }
        let wanted_block = block.clone();
        unsafe {
            handle
                .query_rm_control(PWR_POLICIES_SET_CONTROL, &mut *block)
                .context("PWR_POLICIES_SET_CONTROL")?;
        }
        let readback = read_control(handle, self.mask)?;
        if readback.0[CONTROL_ENTRY_BASE..] != wanted_block.0[CONTROL_ENTRY_BASE..] {
            let mut restore = preimage;
            unsafe {
                handle
                    .query_rm_control(PWR_POLICIES_SET_CONTROL, &mut *restore)
                    .context("PWR_POLICIES_SET_CONTROL (restore)")?;
            }
            bail!("rail current limit readback does not match what was written; previous control object restored");
        }
        Ok(true)
    }
}
