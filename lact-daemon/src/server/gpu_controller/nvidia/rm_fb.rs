//! GDDR7 timings from the FBPA registers (read-only; mVolt+ v0.44's
//! "memory timings").
//!
//! The VBIOS *Memory Tweak Table* (NVIDIA's public open-gpu-doc spec) packs
//! the timings into CONFIG words, and the driver writes those words verbatim
//! into each frame-buffer partition's register file: on Kepler nouveau wrote
//! `timing[0..]` to FBPA base + 0x290, and on GB202 the same two words sit at
//! **base + 0x290 (CONFIG0)** and **+0x294 (CONFIG1)**, unicast
//! `0x900000 + i·0x4000` per partition and broadcast `0x9a0000`. The decode is
//! self-checking: `tRC = tRAS + tRP` must hold, and it does on this card in
//! both memory P-states (84 = 56 + 28 at 17 GHz, 9 = 6 + 3 at 810 MHz).
//!
//! Reads go through the public `NV2080_CTRL_CMD_GPU_EXEC_REG_OPS`
//! (0x20800122), which the RM allows only for a privileged client (the daemon
//! runs as root; the GUI's embedded daemon does not), with both target
//! handles zero, `bNonTransactional` set so a refused offset reports
//! per-op instead of failing the call, and one register per call.
#![allow(clippy::doc_markdown)]

use anyhow::{Context, bail, ensure};
use tracing::{debug, info};

use super::driver::DriverHandle;

const EXEC_REG_OPS: u32 = 0x2080_0122;
const REG_OP_READ_32: u8 = 0;
const REG_TYPE_GLOBAL: u8 = 0;
const FBPA_UNICAST_BASE: u32 = 0x0090_0000;
const FBPA_STRIDE: u32 = 0x4000;
const FBPA_BROADCAST: u32 = 0x009a_0000;
const CONFIG0: u32 = 0x290;
const CONFIG1: u32 = 0x294;
const MAX_FBPA: u32 = 16;
/// The PRI error pattern a read of a non-existent partition returns.
const PRI_ERROR: u32 = 0xbadf_0000;

// Field names follow NV2080_CTRL_GPU_REG_OP.
#[allow(clippy::struct_field_names)]
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RegOp {
    reg_op: u8,
    reg_type: u8,
    reg_status: u8,
    reg_quad: u8,
    reg_group_mask: u32,
    reg_sub_group_mask: u32,
    reg_offset: u32,
    reg_value_hi: u32,
    reg_value_lo: u32,
    reg_and_n_mask_hi: u32,
    reg_and_n_mask_lo: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct GrRouteInfo {
    flags: u32,
    _pad: u32,
    route: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ExecRegOpsParams {
    h_client_target: u32,
    h_channel_target: u32,
    b_non_transactional: u32,
    reserved00: [u32; 2],
    reg_op_count: u32,
    reg_ops: u64,
    gr_route_info: GrRouteInfo,
}

const _: () = assert!(std::mem::size_of::<ExecRegOpsParams>() == 0x30);
const _: () = assert!(std::mem::size_of::<RegOp>() == 32);

/// One READ_32; `Ok(None)` when the RM refuses the offset.
fn read_reg(handle: &DriverHandle, offset: u32) -> anyhow::Result<Option<u32>> {
    let mut op = RegOp {
        reg_op: REG_OP_READ_32,
        reg_type: REG_TYPE_GLOBAL,
        reg_offset: offset,
        ..RegOp::default()
    };
    let mut params = ExecRegOpsParams {
        h_client_target: 0,
        h_channel_target: 0,
        b_non_transactional: 1,
        reserved00: [0; 2],
        reg_op_count: 1,
        reg_ops: std::ptr::addr_of_mut!(op) as u64,
        gr_route_info: GrRouteInfo::default(),
    };
    unsafe {
        handle
            .query_rm_control(EXEC_REG_OPS, &mut params)
            .with_context(|| format!("EXEC_REG_OPS read {offset:#010x}"))?;
    }
    if op.reg_status != 0 {
        debug!("EXEC_REG_OPS {offset:#010x}: op status {}", op.reg_status);
        return Ok(None);
    }
    Ok(Some(op.reg_value_lo))
}

/// The eight timings mVolt+ shows, in memory-controller clocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timings {
    pub cl: u32,
    pub wl: u32,
    pub rc: u32,
    pub rfc: u32,
    pub ras: u32,
    pub rp: u32,
    pub rd_rcd: u32,
    pub wr_rcd: u32,
}

/// Memory Tweak Table CONFIG0 / CONFIG1 packing.
pub fn decode(config0: u32, config1: u32) -> Timings {
    Timings {
        rc: config0 & 0xff,
        rfc: (config0 >> 8) & 0x1ff,
        ras: (config0 >> 17) & 0x7f,
        rp: (config0 >> 24) & 0x7f,
        cl: config1 & 0x7f,
        wl: (config1 >> 7) & 0x7f,
        rd_rcd: (config1 >> 14) & 0x3f,
        wr_rcd: (config1 >> 20) & 0x3f,
    }
}

fn is_populated(v: Option<u32>) -> bool {
    matches!(v, Some(v) if v != 0 && v != u32::MAX && v & 0xffff_0000 != PRI_ERROR)
}

pub struct MemoryTimings {
    fbpa_count: u32,
}

impl MemoryTimings {
    /// Count the partitions whose CONFIG0 reads, and require the broadcast
    /// word to satisfy the tRC identity before trusting the decode.
    pub fn probe(handle: &DriverHandle) -> anyhow::Result<Self> {
        let bc0 = read_reg(handle, FBPA_BROADCAST + CONFIG0).context("broadcast CONFIG0")?;
        let bc1 = read_reg(handle, FBPA_BROADCAST + CONFIG1).context("broadcast CONFIG1")?;
        let (Some(c0), Some(c1)) = (bc0, bc1) else {
            bail!("FBPA registers are not readable from this client (needs a privileged RM client)");
        };
        ensure!(is_populated(Some(c0)) && is_populated(Some(c1)), "broadcast CONFIG words are empty");
        let t = decode(c0, c1);
        ensure!(
            t.rc == t.ras + t.rp,
            "CONFIG0 {c0:#010x} does not satisfy tRC = tRAS + tRP ({} vs {} + {}); layout not recognised",
            t.rc,
            t.ras,
            t.rp
        );
        let mut fbpa_count = 0;
        for i in 0..MAX_FBPA {
            let v = read_reg(handle, FBPA_UNICAST_BASE + i * FBPA_STRIDE + CONFIG0)?;
            if !is_populated(v) {
                break;
            }
            fbpa_count = i + 1;
        }
        ensure!(fbpa_count > 0, "no unicast FBPA answers");
        info!("memory timings: {fbpa_count} FBPAs + broadcast; broadcast CL {} WL {} RC {} RFC {} RAS {} RP {}", t.cl, t.wl, t.rc, t.rfc, t.ras, t.rp);
        Ok(Self { fbpa_count })
    }

    /// `(name, timings)` for every partition and the broadcast window; a
    /// partition that fails to read this time is skipped.
    pub fn read(&self, handle: &DriverHandle) -> anyhow::Result<Vec<(String, Timings)>> {
        let mut rows = Vec::with_capacity(self.fbpa_count as usize + 1);
        for i in 0..self.fbpa_count {
            let base = FBPA_UNICAST_BASE + i * FBPA_STRIDE;
            if let (Some(c0), Some(c1)) = (read_reg(handle, base + CONFIG0)?, read_reg(handle, base + CONFIG1)?)
                && is_populated(Some(c0))
            {
                rows.push((format!("FBPA{i}"), decode(c0, c1)));
            }
        }
        if let (Some(c0), Some(c1)) = (
            read_reg(handle, FBPA_BROADCAST + CONFIG0)?,
            read_reg(handle, FBPA_BROADCAST + CONFIG1)?,
        ) {
            rows.push(("Broadcast".to_owned(), decode(c0, c1)));
        }
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_the_reference_card_in_both_pstates() {
        let full = decode(0x1c71_1054, 0x7978_85a5);
        assert_eq!(
            full,
            Timings { cl: 37, wl: 11, rc: 84, rfc: 272, ras: 56, rp: 28, rd_rcd: 34, wr_rcd: 23 }
        );
        assert_eq!(full.rc, full.ras + full.rp);
        let idle = decode(0x030c_1d09, 0x3831_0311);
        assert_eq!((idle.rc, idle.ras, idle.rp), (9, 6, 3));
        assert_eq!(idle.rc, idle.ras + idle.rp);
    }

    #[test]
    fn pri_error_and_blank_are_not_populated() {
        assert!(!is_populated(Some(0xbadf_5040)));
        assert!(!is_populated(Some(0)));
        assert!(!is_populated(None));
        assert!(is_populated(Some(0x1c71_1054)));
    }
}
