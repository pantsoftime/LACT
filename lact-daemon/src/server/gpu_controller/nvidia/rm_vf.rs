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
//! Reading is ported from Panchovix/LACT commit c06c5f2 (2026-09-14), who
//! kept the curves read-only because on R610 the driver took point writes
//! without confirming them and one domain dropped a written point on the
//! next read.
//!
//! **Writing** (mVolt+ v0.47.1's "MSVDD curve editing") was probed on R615
//! on 2026-09-20 and behaves: GET_CONTROL (`0x20809023`, 0x1020c) holds one
//! 16-byte record per point from +0x108 — the point type word first and a
//! per-point frequency delta (i32 kHz) at +8, all zero without edits, the
//! domain's global offset living elsewhere — and SET_CONTROL (`0x2080d024`)
//! with only the changed points in the 640-bit mask is retained on every
//! later read while GET_STATUS shows exactly the delta on that point and
//! none on its neighbours, for XBAR, SYS and video alike. So a write here
//! is verified twice (the request was kept; the curve moved) and rolled
//! back to the pre-write records if the driver kept something else.

#![allow(clippy::doc_markdown)]

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

const CLK_VF_POINTS_GET_CONTROL: u32 = 0x2080_9023;
const CLK_VF_POINTS_SET_CONTROL: u32 = 0x2080_d024;
const CONTROL_SIZE: usize = 0x1_020c;
const CONTROL_BASE: usize = 0x108;
const CONTROL_STRIDE: usize = 0x10;
const CONTROL_TYPE_AT: usize = 0x00;
const CONTROL_DELTA_AT: usize = 0x08;
/// The low half of the INFO type word, which is what CONTROL carries.
const CONTROL_TYPE_VOLTAGE: u32 = TYPE_VOLTAGE & 0xffff;

#[repr(C)]
#[derive(Clone, Copy)]
struct Bytes<const N: usize>([u8; N]);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VfPoint {
    pub voltage_mv: u32,
    pub freq_mhz: u32,
    /// The per-point delta the driver holds, MHz (already part of `freq_mhz`)
    pub offset_mhz: i32,
}

/// The curve of one domain: its control-block index and its points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DomainCurve {
    pub domain_index: u8,
    /// Flat index of the curve's first point in the 640-point space
    pub bank_start: usize,
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
/// One group of the flat point space: `Some((first flat index, points))` for
/// a voltage-typed bank, `None` for a run of frequency-typed points.
type PointGroup = Option<(usize, Vec<VfPoint>)>;

fn group_points(info: &[u8], status: &[u8], control: Option<&[u8]>) -> anyhow::Result<Vec<PointGroup>> {
    let point_count: usize = (0..MASK_WORDS)
        .map(|w| rd_u32(info, MASK_AT + w * 4).count_ones() as usize)
        .sum();
    let mut groups: Vec<PointGroup> = Vec::new();
    let mut bank: Vec<VfPoint> = Vec::new();
    let mut bank_start = 0usize;
    let mut frequency_run = false;
    for index in 0..point_count {
        let at = INFO_BASE + index * INFO_STRIDE;
        if at + INFO_STRIDE > info.len() {
            bail!("point {index} lies outside the GET_INFO block");
        }
        match rd_u32(info, at) {
            TYPE_FREQUENCY => {
                if !bank.is_empty() {
                    groups.push(Some((bank_start, std::mem::take(&mut bank))));
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
                if bank.is_empty() {
                    bank_start = index;
                }
                bank.push(VfPoint {
                    voltage_mv: rd_u32(status, at + STATUS_VOLTAGE_AT) / 1000,
                    freq_mhz: rd_u32(status, at + STATUS_FREQ_AT),
                    offset_mhz: control.map_or(0, |c| point_delta_khz(c, index) / 1000),
                });
                if bank.len() == POINTS_PER_BANK {
                    groups.push(Some((bank_start, std::mem::take(&mut bank))));
                }
            }
            other => bail!("point {index} has unknown type {other:#x}; layout not recognised"),
        }
    }
    if !bank.is_empty() {
        groups.push(Some((bank_start, bank)));
    }
    Ok(groups)
}

fn control_record_at(index: usize) -> usize {
    CONTROL_BASE + index * CONTROL_STRIDE
}

fn point_delta_khz(control: &[u8], index: usize) -> i32 {
    let at = control_record_at(index) + CONTROL_DELTA_AT;
    if at + 4 > control.len() {
        return 0;
    }
    i32::from_le_bytes(control[at..at + 4].try_into().unwrap())
}

fn read_control(handle: &DriverHandle, mask: &[u8; MASK_WORDS * 4]) -> anyhow::Result<Box<Bytes<CONTROL_SIZE>>> {
    rm_call::<CONTROL_SIZE>(handle, CLK_VF_POINTS_GET_CONTROL, |b| {
        b[MASK_AT..MASK_AT + MASK_WORDS * 4].copy_from_slice(mask);
    })
    .context("CLK_VF_POINTS_GET_CONTROL")
}

fn read_status(handle: &DriverHandle, mask: &[u8; MASK_WORDS * 4]) -> anyhow::Result<Box<Bytes<STATUS_SIZE>>> {
    rm_call::<STATUS_SIZE>(handle, CLK_VF_POINTS_GET_STATUS, |b| {
        b[MASK_AT..MASK_AT + MASK_WORDS * 4].copy_from_slice(mask);
    })
    .context("CLK_VF_POINTS_GET_STATUS")
}

fn status_freq_mhz(status: &[u8], index: usize) -> u32 {
    rd_u32(status, STATUS_BASE + index * STATUS_STRIDE + STATUS_FREQ_AT)
}

/// Write per-point offsets (MHz, by index within the bank) to one curve;
/// every point of the bank not listed is written back to zero. Only points
/// whose delta changes are selected in the SET mask. After the write the
/// control block must hold exactly the requested deltas, or the pre-write
/// records are put back and the call fails; the status frequencies are then
/// checked against the pre-write curve and any point that did not move by
/// its delta is reported (the driver keeps a curve monotonic, so that is a
/// warning, not a failure). Returns the number of points written.
pub fn write_bank_offsets(
    handle: &DriverHandle,
    bank_start: usize,
    bank_len: usize,
    offsets_mhz: &[(u8, i32)],
) -> anyhow::Result<usize> {
    let info = rm_call::<INFO_SIZE>(handle, CLK_VF_POINTS_GET_INFO, |_| {}).context("CLK_VF_POINTS_GET_INFO")?;
    let mask: [u8; MASK_WORDS * 4] = info.0[MASK_AT..MASK_AT + MASK_WORDS * 4].try_into().unwrap();
    let before = read_control(handle, &mask)?;
    let status_before = read_status(handle, &mask)?;

    let mut wanted = vec![0i32; bank_len];
    for (point, mhz) in offsets_mhz {
        let slot = wanted
            .get_mut(usize::from(*point))
            .with_context(|| format!("curve point {point} is outside the {bank_len}-point curve"))?;
        *slot = mhz.checked_mul(1000).context("curve offset overflows")?;
    }
    let mut changed = Vec::new();
    for (i, khz) in wanted.iter().enumerate() {
        let index = bank_start + i;
        let at = control_record_at(index);
        if at + CONTROL_STRIDE > CONTROL_SIZE {
            bail!("curve point {index} lies outside the CONTROL block");
        }
        let kind = rd_u32(&before.0, at + CONTROL_TYPE_AT) & 0xffff;
        if kind != CONTROL_TYPE_VOLTAGE {
            bail!("CONTROL record {index} has type {kind:#x}, not a voltage-typed curve point; layout not recognised");
        }
        if point_delta_khz(&before.0, index) != *khz {
            changed.push((index, *khz));
        }
    }
    if changed.is_empty() {
        return Ok(0);
    }

    let build = |deltas: &[(usize, i32)]| {
        let mut req = before.clone();
        req.0[MASK_AT..MASK_AT + MASK_WORDS * 4].fill(0);
        for (index, khz) in deltas {
            let word = MASK_AT + (index / 32) * 4;
            let bits = rd_u32(&req.0, word) | (1 << (index % 32));
            req.0[word..word + 4].copy_from_slice(&bits.to_le_bytes());
            let at = control_record_at(*index) + CONTROL_DELTA_AT;
            req.0[at..at + 4].copy_from_slice(&khz.to_le_bytes());
        }
        req
    };
    let mut request = build(&changed);
    let written = unsafe { handle.query_rm_control(CLK_VF_POINTS_SET_CONTROL, &mut *request) };
    let after = read_control(handle, &mask);
    let retained = after
        .as_ref()
        .is_ok_and(|after| changed.iter().all(|(index, khz)| point_delta_khz(&after.0, *index) == *khz));
    if written.is_err() || !retained {
        let previous: Vec<(usize, i32)> = changed
            .iter()
            .map(|(index, _)| (*index, point_delta_khz(&before.0, *index)))
            .collect();
        let mut restore = build(&previous);
        let restored = unsafe { handle.query_rm_control(CLK_VF_POINTS_SET_CONTROL, &mut *restore) };
        let what = match &written {
            Err(err) => format!("SET failed ({err:#})"),
            Ok(()) => "the driver did not retain the requested deltas".to_owned(),
        };
        match restored {
            Ok(()) => bail!("V/F curve write: {what}; previous deltas restored"),
            Err(err) => bail!("V/F curve write: {what}, and the restore failed too ({err:#})"),
        }
    }

    let status_after = read_status(handle, &mask)?;
    for (index, khz) in &changed {
        let moved = i64::from(status_freq_mhz(&status_after.0, *index)) - i64::from(status_freq_mhz(&status_before.0, *index));
        let expected = i64::from(khz - point_delta_khz(&before.0, *index)) / 1000;
        if moved != expected {
            tracing::warn!(
                "V/F curve point {index}: control holds the delta but the curve moved {moved} MHz, not {expected} (the driver keeps curves monotonic)"
            );
        }
    }
    Ok(changed.len())
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
    // The control block is optional for reading: without it the curves still
    // plot, they just show no per-point offsets.
    let control = read_control(handle, &mask).ok();
    let groups = group_points(&info.0, &status.0, control.as_ref().map(|c| &c.0[..]))?;
    let mut ordered: Vec<&RmClockDomain> = domains.iter().collect();
    ordered.sort_by_key(|d| d.index);
    Ok(ordered
        .into_iter()
        .zip(groups)
        .filter_map(|(domain, group)| {
            group.map(|(bank_start, points)| DomainCurve {
                domain_index: domain.index,
                bank_start,
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
        let groups = group_points(&info, &status, None).unwrap();
        assert_eq!(groups.len(), 6);
        assert!(groups[2].is_none());
        for (i, g) in groups.iter().enumerate() {
            if let Some((_, bank)) = g {
                assert_eq!(bank.len(), POINTS_PER_BANK, "group {i}");
                assert_eq!(bank[0].voltage_mv, 450);
                assert_eq!(bank[POINTS_PER_BANK - 1].voltage_mv, 450 + 126 * 6250 / 1000);
            }
        }
        // The fourth bank starts at flat index 259.
        assert_eq!(groups[3].as_ref().unwrap().1[0].freq_mhz, 2590);
        assert_eq!(groups[3].as_ref().unwrap().0, 259);
        assert_eq!(groups[1].as_ref().unwrap().0, 127);
    }

    #[test]
    fn control_deltas_are_read_per_point() {
        let (info, status) = synth(&vec![TYPE_VOLTAGE; 254]);
        let mut control = vec![0u8; CONTROL_SIZE];
        let at = control_record_at(190) + CONTROL_DELTA_AT;
        control[at..at + 4].copy_from_slice(&(-15_000i32).to_le_bytes());
        let groups = group_points(&info, &status, Some(&control)).unwrap();
        let (start, xbar) = groups[1].as_ref().unwrap();
        assert_eq!(*start, 127);
        assert_eq!(xbar[190 - 127].offset_mhz, -15);
        assert_eq!(xbar[0].offset_mhz, 0);
        assert_eq!(control_record_at(254), 0x10e8);
    }

    #[test]
    fn unknown_point_type_fails_closed() {
        let (info, status) = synth(&[TYPE_VOLTAGE, 0x1234_5678]);
        assert!(group_points(&info, &status, None).is_err());
    }
}
