//! The GPC→XBAR clock propagation ratio — what mVolt+ calls the "MSVDD clock
//! ratio".
//!
//! NVIDIA's clock arbiter propagates the requested core clock to the fabric
//! domains through a topology of relations. On GB202 the active topology has
//! one bidirectional *ratio* relation from GPC to XBAR (factory 0.8999); the
//! ratio is a U16.16 field in the relation's CONTROL object, readable and
//! writable at runtime through private RM controls (`CLK_PROP_TOPS_*`,
//! `CLK_CLK_PROP_TOP_RELS_*`). Two independent implementations (Loong0x00's
//! LACT branch, SHANAjam's Windows tool) established the layout and measured
//! the effect; the layout was re-verified here on R610 and R615.
//!
//! The ratio is a propagation *constraint*, not `XBAR = GPC × ratio`: raising
//! it lets XBAR/SYS follow the core higher when they are the binding
//! domains, and everything else (power, V/F, limits) still applies.
//!
//! Write discipline, same as the clock-domain block: fresh INFO + CONTROL,
//! locate the relation by its properties (never by a fixed index), change
//! only its ratio field, require exact readback, restore the preimage on any
//! mismatch.

use anyhow::{Context, bail};
use tracing::{debug, warn};

use super::driver::DriverHandle;

const CLK_PROP_TOPS_GET_INFO: u32 = 0x2080_907d;
const CLK_PROP_TOPS_GET_STATUS: u32 = 0x2080_907e;
const CLK_PROP_RELS_GET_INFO: u32 = 0x2080_9081;
const CLK_PROP_RELS_GET_CONTROL: u32 = 0x2080_9083;
const CLK_PROP_RELS_SET_CONTROL: u32 = 0x2080_d084;

const TOPS_INFO_SIZE: usize = 0x5f14;
const TOPS_INFO_ENTRY_BASE: usize = 0x0c;
const TOPS_INFO_ENTRY_STRIDE: usize = 0x2f8;
const TOPS_INFO_OFF_ID: usize = 0x05;
const TOPS_INFO_OFF_REL_MASK: usize = 0x08;
const TOPS_STATUS_SIZE: usize = 0x2c;
const TOPS_STATUS_OFF_ACTIVE_ID: usize = 0x09;

const RELS_INFO_SIZE: usize = 0x1518;
const RELS_INFO_ENTRY_BASE: usize = 0x128;
const RELS_INFO_ENTRY_STRIDE: usize = 0x14;
const RI_TYPE: usize = 0x00;
const RI_SRC: usize = 0x02;
const RI_DST: usize = 0x03;
const RI_BIDIR: usize = 0x04;
const RI_RATIO: usize = 0x0c;

const RELS_CONTROL_SIZE: usize = 0xc18;
/// The CONTROL request carries the INFO block's header (masks).
const RELS_CONTROL_HEADER: usize = 0x24;
const RELS_CONTROL_ENTRY_BASE: usize = 0x24;
const RELS_CONTROL_ENTRY_STRIDE: usize = 0x0c;
const RC_TYPE: usize = 0x00;
const RC_RATIO: usize = 0x08;

const REL_TYPE_RATIO: u8 = 3;

/// Bounds the daemon accepts, U16.16. 0.80 … 1.20. Loong0x00's adoption
/// envelope was 0.896 … 0.95 and their A/B at 1.20 raised XBAR by 174 MHz at
/// a 2500 MHz core without incident; the firmware itself ships alternative
/// topologies at 0.8, 1.2, 1.5 and 2.0. None of this is a stability claim.
pub const RATIO_MIN_RAW: u32 = 0x0000_cccd;
pub const RATIO_MAX_RAW: u32 = 0x0001_3333;

#[repr(C)]
#[derive(Clone, Copy)]
struct Bytes<const N: usize>([u8; N]);

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
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

pub fn raw_to_ratio(raw: u32) -> f64 {
    f64::from(raw) / 65536.0
}

/// Config value (ratio × 1000) to U16.16, bounds-checked.
pub fn ratio_milli_to_raw(milli: i32) -> anyhow::Result<u32> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let raw = (f64::from(milli) / 1000.0 * 65536.0).round() as i64;
    let raw = u32::try_from(raw).ok().filter(|r| (RATIO_MIN_RAW..=RATIO_MAX_RAW).contains(r));
    raw.with_context(|| {
        format!(
            "GPC→XBAR ratio {:.3} is outside the accepted {:.2}…{:.2}",
            f64::from(milli) / 1000.0,
            raw_to_ratio(RATIO_MIN_RAW),
            raw_to_ratio(RATIO_MAX_RAW)
        )
    })
}

/// The GPC→XBAR ratio relation of the active topology, as found by `probe`.
#[derive(Debug, Clone, Copy)]
pub struct PropRelation {
    pub index: u8,
    pub factory_raw: u32,
    pub topology_id: u8,
}

impl PropRelation {
    /// Find the relation from live INFO/STATUS, verifying the layout. `gpc`
    /// and `xbar` are the clock-domain indices from `CLK_DOMAINS_GET_INFO`.
    pub fn probe(handle: &DriverHandle, gpc: u8, xbar: u8) -> anyhow::Result<Self> {
        let tops = rm_call::<TOPS_INFO_SIZE>(handle, CLK_PROP_TOPS_GET_INFO, |_| {})
            .context("CLK_PROP_TOPS_GET_INFO")?;
        let status = rm_call::<TOPS_STATUS_SIZE>(handle, CLK_PROP_TOPS_GET_STATUS, |_| {})
            .context("CLK_PROP_TOPS_GET_STATUS")?;
        let active_id = status[TOPS_STATUS_OFF_ACTIVE_ID];
        let tops_mask = rd_u32(&tops, 4);
        let mut active_rel_mask = None;
        for i in 0..32u8 {
            if tops_mask & (1 << i) == 0 {
                continue;
            }
            let base = TOPS_INFO_ENTRY_BASE + usize::from(i) * TOPS_INFO_ENTRY_STRIDE;
            if base + TOPS_INFO_ENTRY_STRIDE > TOPS_INFO_SIZE {
                bail!("topology {i} lies outside the GET_INFO block");
            }
            if tops[base + TOPS_INFO_OFF_ID] == active_id {
                active_rel_mask = Some(rd_u32(&tops, base + TOPS_INFO_OFF_REL_MASK));
            }
        }
        let active_rel_mask =
            active_rel_mask.with_context(|| format!("active topology id {active_id:#x} not in INFO"))?;

        let info = rm_call::<RELS_INFO_SIZE>(handle, CLK_PROP_RELS_GET_INFO, |_| {})
            .context("CLK_PROP_RELS_GET_INFO")?;
        let rel_mask = rd_u32(&info, 4);
        let mut found = None;
        for i in 0..32u8 {
            if rel_mask & (1 << i) == 0 || active_rel_mask & (1 << i) == 0 {
                continue;
            }
            let base = RELS_INFO_ENTRY_BASE + usize::from(i) * RELS_INFO_ENTRY_STRIDE;
            if base + RELS_INFO_ENTRY_STRIDE > RELS_INFO_SIZE {
                bail!("relation {i} lies outside the GET_INFO block");
            }
            if info[base + RI_TYPE] == REL_TYPE_RATIO
                && info[base + RI_SRC] == gpc
                && info[base + RI_DST] == xbar
                && info[base + RI_BIDIR] == 1
            {
                if found.is_some() {
                    bail!("more than one GPC→XBAR ratio relation in the active topology");
                }
                found = Some((i, rd_u32(&info, base + RI_RATIO)));
            }
        }
        let (index, factory_raw) = found.context("no bidirectional GPC→XBAR ratio relation in the active topology")?;
        if !(0x8000..=0x2_0000).contains(&factory_raw) {
            bail!("factory ratio {factory_raw:#x} is implausible");
        }

        let this = Self {
            index,
            factory_raw,
            topology_id: active_id,
        };
        let current = this.read_raw(handle)?;
        debug!(
            "GPC→XBAR propagation: relation {index} in topology {active_id:#x}, factory {:.4}, current {:.4}",
            raw_to_ratio(factory_raw),
            raw_to_ratio(current)
        );
        Ok(this)
    }

    fn control(&self, handle: &DriverHandle) -> anyhow::Result<[u8; RELS_CONTROL_SIZE]> {
        let info = rm_call::<RELS_INFO_SIZE>(handle, CLK_PROP_RELS_GET_INFO, |_| {})
            .context("CLK_PROP_RELS_GET_INFO")?;
        let control = rm_call::<RELS_CONTROL_SIZE>(handle, CLK_PROP_RELS_GET_CONTROL, |buf| {
            buf[..RELS_CONTROL_HEADER].copy_from_slice(&info[..RELS_CONTROL_HEADER]);
        })
        .context("CLK_PROP_RELS_GET_CONTROL")?;
        let base = self.entry();
        if control[base + RC_TYPE] != REL_TYPE_RATIO {
            bail!("relation {} CONTROL entry type changed; refusing", self.index);
        }
        Ok(control)
    }

    fn entry(&self) -> usize {
        RELS_CONTROL_ENTRY_BASE + usize::from(self.index) * RELS_CONTROL_ENTRY_STRIDE
    }

    pub fn read_raw(&self, handle: &DriverHandle) -> anyhow::Result<u32> {
        let control = self.control(handle)?;
        Ok(rd_u32(&control, self.entry() + RC_RATIO))
    }

    /// Write the ratio and verify the readback; restore the preimage if the
    /// result is not exactly what was requested.
    pub fn set_raw(&self, handle: &DriverHandle, raw: u32) -> anyhow::Result<()> {
        if !(RATIO_MIN_RAW..=RATIO_MAX_RAW).contains(&raw) && raw != self.factory_raw {
            bail!("ratio {raw:#x} outside the accepted range");
        }
        let preimage = self.control(handle)?;
        let mut wanted = preimage;
        let off = self.entry() + RC_RATIO;
        wanted[off..off + 4].copy_from_slice(&raw.to_le_bytes());

        let mut to_write = Bytes(wanted);
        unsafe {
            handle
                .query_rm_control(CLK_PROP_RELS_SET_CONTROL, &mut to_write)
                .context("CLK_PROP_RELS_SET_CONTROL")?;
        }
        let readback = self.control(handle)?;
        if readback[RELS_CONTROL_HEADER..] != wanted[RELS_CONTROL_HEADER..] {
            warn!("propagation ratio readback mismatch; restoring the previous control object");
            let mut restore = Bytes(preimage);
            unsafe {
                handle
                    .query_rm_control(CLK_PROP_RELS_SET_CONTROL, &mut restore)
                    .context("CLK_PROP_RELS_SET_CONTROL (restore)")?;
            }
            bail!("propagation ratio readback does not match what was written");
        }
        Ok(())
    }
}
