//! The V/F curves of every clock domain, read-only.
//!
//! `CLK_VF_POINTS` GET_INFO (`0x20809021`, 0x8208) carries a 640-bit mask of
//! populated points at +4 and one 8-byte record per point from +0x104 (a type
//! word, then a value); GET_STATUS (`0x20809022`, 0x98208) answers only when
//! that mask is copied into its request and holds a 0x98-byte record per
//! point from +0x100 with the voltage (µV) at +8 and the frequency (MHz) at
//! +0xc. All domains share one flat index space: the voltage-typed points
//! (`0xffff1111`) form 127-point banks, one per domain with a curve, and a
//! run of frequency-typed points (`0xffff0f0f`) is a domain described without
//! one (memory). Nothing in either response says which bank belongs to which
//! domain, so banks are matched to domains in the order `CLK_DOMAINS`
//! enumerates them — Panchovix confirmed that on hardware by offsetting one
//! domain at a time and seeing exactly one bank move. On GB202 / R615 that
//! gives GPC, XBAR, (memory, no curve), SYS, video, PWRCLK; the legacy,
//! display and hub domains carry no curve.
//!
//! Read-only on purpose: the driver takes writes to these points but offers
//! no way to check they were adopted, and one domain accepts a point write
//! and quietly drops it on the next read. Ported from Panchovix/LACT commit
//! c06c5f2 (2026-09-14); sizes and layout re-verified on R615.

use anyhow::{Context, bail};

use super::clock_client::RmClockDomain;
use super::driver::DriverHandle;

const CLK_VF_POINTS_GET_INFO: u32 = 0x2080_9021;
const CLK_VF_POINTS_GET_STATUS: u32 = 0x2080_9022;
const INFO_SIZE: usize = 0x8208;
const STATUS_SIZE: usize = 0x9_8208;
const MASK_AT: usize = 0x04;
const MASK_WORDS: usize = 20;
const INFO_BASE: usize = 0x104;
const INFO_STRIDE: usize = 8;
const TYPE_VOLTAGE: u32 = 0xffff_1111;
const TYPE_FREQUENCY: u32 = 0xffff_0f0f;
const STATUS_BASE: usize = 0x100;
const STATUS_STRIDE: usize = 0x98;
const STATUS_VOLTAGE_AT: usize = 0x08;
const STATUS_FREQ_AT: usize = 0x0c;
pub const POINTS_PER_BANK: usize = 127;

#[repr(C)]
#[derive(Clone, Copy)]
struct Bytes<const N: usize>([u8; N]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VfPoint {
    pub voltage_mv: u32,
    pub freq_mhz: u32,
}

/// The curve of one domain: its control-block index and its points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainCurve {
    pub domain_index: u8,
    pub points: Vec<VfPoint>,
}

fn rd_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().unwrap())
}

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

/// Split the flat point space into groups: `Some(bank)` for a voltage-typed
/// bank, `None` for a run of frequency-typed points. Fails closed on any
/// unknown point type.
fn group_points(info: &[u8], status: &[u8]) -> anyhow::Result<Vec<Option<Vec<VfPoint>>>> {
    let point_count: usize = (0..MASK_WORDS)
        .map(|w| rd_u32(info, MASK_AT + w * 4).count_ones() as usize)
        .sum();
    let mut groups: Vec<Option<Vec<VfPoint>>> = Vec::new();
    let mut bank: Vec<VfPoint> = Vec::new();
    let mut frequency_run = false;
    for index in 0..point_count {
        let at = INFO_BASE + index * INFO_STRIDE;
        if at + INFO_STRIDE > info.len() {
            bail!("point {index} lies outside the GET_INFO block");
        }
        match rd_u32(info, at) {
            TYPE_FREQUENCY => {
                if !bank.is_empty() {
                    groups.push(Some(std::mem::take(&mut bank)));
                }
                if !frequency_run {
                    groups.push(None);
                    frequency_run = true;
                }
            }
            TYPE_VOLTAGE => {
                frequency_run = false;
                let at = STATUS_BASE + index * STATUS_STRIDE;
                if at + STATUS_STRIDE > status.len() {
                    bail!("point {index} lies outside the GET_STATUS block");
                }
                bank.push(VfPoint {
                    voltage_mv: rd_u32(status, at + STATUS_VOLTAGE_AT) / 1000,
                    freq_mhz: rd_u32(status, at + STATUS_FREQ_AT),
                });
                if bank.len() == POINTS_PER_BANK {
                    groups.push(Some(std::mem::take(&mut bank)));
                }
            }
            other => bail!("point {index} has unknown type {other:#x}; layout not recognised"),
        }
    }
    if !bank.is_empty() {
        groups.push(Some(bank));
    }
    Ok(groups)
}

/// Read every domain's curve. `domains` is the `CLK_DOMAINS` list in
/// enumeration order; domains beyond the last group carry no curve.
pub fn read_domain_curves(handle: &DriverHandle, domains: &[RmClockDomain]) -> anyhow::Result<Vec<DomainCurve>> {
    let info = rm_call::<INFO_SIZE>(handle, CLK_VF_POINTS_GET_INFO, |_| {}).context("CLK_VF_POINTS_GET_INFO")?;
    let mask: [u8; MASK_WORDS * 4] = info.0[MASK_AT..MASK_AT + MASK_WORDS * 4].try_into().unwrap();
    let status = rm_call::<STATUS_SIZE>(handle, CLK_VF_POINTS_GET_STATUS, |b| {
        b[MASK_AT..MASK_AT + MASK_WORDS * 4].copy_from_slice(&mask);
    })
    .context("CLK_VF_POINTS_GET_STATUS")?;
    let groups = group_points(&info.0, &status.0)?;
    let mut ordered: Vec<&RmClockDomain> = domains.iter().collect();
    ordered.sort_by_key(|d| d.index);
    Ok(ordered
        .into_iter()
        .zip(groups)
        .filter_map(|(domain, group)| {
            group.map(|points| DomainCurve {
                domain_index: domain.index,
                points,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth(types: &[u32]) -> (Vec<u8>, Vec<u8>) {
        let mut info = vec![0u8; INFO_SIZE];
        let mut status = vec![0u8; STATUS_SIZE];
        let mut in_bank = 0usize;
        for (i, t) in types.iter().enumerate() {
            let pos = if *t == TYPE_VOLTAGE {
                let p = in_bank % POINTS_PER_BANK;
                in_bank += 1;
                p
            } else {
                in_bank = 0;
                0
            };
            let word = MASK_AT + (i / 32) * 4;
            let mut m = rd_u32(&info, word);
            m |= 1 << (i % 32);
            info[word..word + 4].copy_from_slice(&m.to_le_bytes());
            let at = INFO_BASE + i * INFO_STRIDE;
            info[at..at + 4].copy_from_slice(&t.to_le_bytes());
            let at = STATUS_BASE + i * STATUS_STRIDE;
            status[at + STATUS_VOLTAGE_AT..at + STATUS_VOLTAGE_AT + 4]
                .copy_from_slice(&(450_000 + pos as u32 * 6250).to_le_bytes());
            status[at + STATUS_FREQ_AT..at + STATUS_FREQ_AT + 4].copy_from_slice(&(i as u32 * 10).to_le_bytes());
        }
        (info, status)
    }

    #[test]
    fn banks_and_frequency_runs_group_like_the_reference_card() {
        // GPC, XBAR, memory (5 frequency points), SYS, video, PWRCLK.
        let mut types = vec![TYPE_VOLTAGE; 254];
        types.extend([TYPE_FREQUENCY; 5]);
        types.extend([TYPE_VOLTAGE; 381]);
        let (info, status) = synth(&types);
        let groups = group_points(&info, &status).unwrap();
        assert_eq!(groups.len(), 6);
        assert!(groups[2].is_none());
        for (i, g) in groups.iter().enumerate() {
            if let Some(bank) = g {
                assert_eq!(bank.len(), POINTS_PER_BANK, "group {i}");
                assert_eq!(bank[0].voltage_mv, 450);
                assert_eq!(bank[POINTS_PER_BANK - 1].voltage_mv, 450 + 126 * 6250 / 1000);
            }
        }
        // The fourth bank starts at flat index 259.
        assert_eq!(groups[3].as_ref().unwrap()[0].freq_mhz, 2590);
    }

    #[test]
    fn unknown_point_type_fails_closed() {
        let (info, status) = synth(&[TYPE_VOLTAGE, 0x1234_5678]);
        assert!(group_points(&info, &status).is_err());
    }
}
