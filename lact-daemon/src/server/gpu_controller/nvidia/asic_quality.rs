//! Speedo / ASIC quality / IDDQ from the driver's VFE fuse variables (read-only).
//!
//! Ported from Panchovix's `nvidia-asic-quality` (MIT). The fuse values come from
//! `NvAPI_GPU_PerfVfeVarGetInfo`; the Speedo bounds the quality percentage is
//! normalised to come from `NvAPI_GPU_PerfVfeVarGetControl`. The percentage is
//! `100 * (speedo - low) / (high - low)` within those driver-reported bounds: it is not
//! a percentile or a performance score, and only compares cards of the same model on
//! the same vBIOS and driver.
//!
//! Only single-die layouts (variable types 0x7 and 0xD) are decoded; the dual-die type
//! 0xE variables of GB300 are skipped.

use lact_schema::NvidiaAsicQuality;

pub const INFO_VERSION: u32 = 0x002f_9568;
pub const INFO_SIZE: usize = 0x002b_9568;
pub const CONTROL_VERSION: u32 = 0x0002_9fc8;
pub const CONTROL_SIZE: usize = 0x9fc8;

const INFO_BASE: usize = 0x128;
const INFO_STRIDE: usize = 0x2bc0;
const CONTROL_BASE: usize = 0x68;
const CONTROL_STRIDE: usize = 0xa0;
/// The variable mask: 255 bits at bytes 4..0x24 of both structures.
pub const MASK_RANGE: std::ops::Range<usize> = 0x04..0x24;
const MAX_VARIABLES: usize = 255;

const VAR_TYPE_FUSE_LEGACY: u32 = 0x07;
const VAR_TYPE_FUSE: u32 = 0x0d;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fuse {
    Speedo,
    NvvddIddq,
    MsvddIddq,
}

impl Fuse {
    /// Blackwell (type 0xD) variables are identified by fuse id and version field id;
    /// Ada and older (type 0x7) by fuse id only.
    fn identify(var_type: u32, field: u8, version_field: u8) -> Option<Self> {
        match (var_type, field, version_field) {
            (VAR_TYPE_FUSE, 0x00, 1) | (VAR_TYPE_FUSE_LEGACY, 0x07, _) => Some(Self::Speedo),
            (VAR_TYPE_FUSE, 0x18, 3) | (VAR_TYPE_FUSE_LEGACY, 0x10, _) => Some(Self::NvvddIddq),
            (VAR_TYPE_FUSE, 0x19, 3) | (VAR_TYPE_FUSE_LEGACY, 0x11, _) => Some(Self::MsvddIddq),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Reading {
    value: i64,
    valid: bool,
    overridden: bool,
    range: Option<(f32, f32)>,
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

fn f32_at(data: &[u8], at: usize) -> Option<f32> {
    Some(f32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// A fuse value is a u32, sign-extended when the variable flags it as signed.
fn value_at(data: &[u8], at: usize, signed_flag: usize) -> Option<i64> {
    let raw = u32_at(data, at)?;
    Some(if *data.get(signed_flag)? != 0 {
        i64::from(raw.cast_signed())
    } else {
        i64::from(raw)
    })
}

fn variables(info: &[u8]) -> impl Iterator<Item = usize> + '_ {
    let mask = &info[MASK_RANGE];
    (0..MAX_VARIABLES).filter(move |i| mask[i / 8] >> (i % 8) & 1 != 0)
}

fn read_fuse(info: &[u8], control: Option<&[u8]>, index: usize) -> Option<(Fuse, Reading)> {
    let entry = INFO_BASE + index * INFO_STRIDE;
    let var_type = u32_at(info, entry)?;
    let fuse = Fuse::identify(
        var_type,
        *info.get(entry + 0x654)?,
        *info.get(entry + 0x655)?,
    )?;

    let mut reading = Reading {
        value: value_at(info, entry + 0x11c, entry + 0x118)?,
        valid: *info.get(entry + 0x138)? != 0,
        overridden: *info.get(entry + 0x134)? != 0,
        range: None,
    };

    // A completed version check can still fall back to a default value on mismatch.
    let fuse_version = *info.get(entry + 0x114)?;
    let check = entry + 0x144;
    if *info.get(check + 2)? != 0 {
        let expected = *info.get(check)?;
        let version_matches = if *info.get(check + 1)? != 0 {
            fuse_version < 8 && expected & (1 << fuse_version) != 0
        } else {
            fuse_version == expected
        };
        reading.valid &= version_matches;
    }

    if let Some(control) = control {
        let entry = CONTROL_BASE + index * CONTROL_STRIDE;
        if let (Some(low), Some(high)) = (f32_at(control, entry + 4), f32_at(control, entry + 8))
            && low.is_finite()
            && high.is_finite()
            && low < high
            && high - low < 1e9
        {
            reading.range = Some((low, high));
        }
        reading.overridden |= u32_at(control, entry + 0x2c).is_some_and(|v| v != 0);
    }

    Some((fuse, reading))
}

/// Decodes the INFO (and optionally CONTROL) structures. Returns `None` when no Speedo
/// variable was recognised.
pub fn parse(info: &[u8], control: Option<&[u8]>) -> Option<NvidiaAsicQuality> {
    if info.len() < INFO_SIZE {
        return None;
    }
    let control = control.filter(|control| control.len() >= CONTROL_SIZE);

    // A fuse matched by more than one variable is ambiguous and left out.
    let mut found: Vec<(Fuse, Option<Reading>)> = Vec::new();
    for index in variables(info) {
        if let Some((fuse, reading)) = read_fuse(info, control, index) {
            match found.iter_mut().find(|(f, _)| *f == fuse) {
                Some((_, existing)) => *existing = None,
                None => found.push((fuse, Some(reading))),
            }
        }
    }
    let get = |fuse| {
        found
            .iter()
            .find(|(f, _)| *f == fuse)
            .and_then(|(_, reading)| *reading)
            .filter(|reading| reading.valid)
    };

    let speedo = get(Fuse::Speedo)?;
    // An IDDQ of 0 means the fuse is not programmed (e.g. no separate MSVDD), not zero leakage.
    let iddq = |fuse| get(fuse).map(|r| r.value).filter(|value| *value != 0);
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let quality_percent = speedo.range.and_then(|(low, high)| {
        let value = speedo.value as f32;
        (low..=high)
            .contains(&value)
            .then(|| 100.0 * (value - low) / (high - low))
    });

    Some(NvidiaAsicQuality {
        speedo: speedo.value,
        speedo_range: speedo.range,
        quality_percent,
        nvvdd_iddq: iddq(Fuse::NvvddIddq),
        msvdd_iddq: iddq(Fuse::MsvddIddq),
        fuse_override: [Fuse::Speedo, Fuse::NvvddIddq, Fuse::MsvddIddq]
            .into_iter()
            .filter_map(get)
            .any(|reading| reading.overridden),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_var(info: &mut [u8], index: usize, var_type: u32, field: u8, version: u8, value: u32) {
        info[MASK_RANGE.start + index / 8] |= 1 << (index % 8);
        let entry = INFO_BASE + index * INFO_STRIDE;
        info[entry..entry + 4].copy_from_slice(&var_type.to_le_bytes());
        info[entry + 0x654] = field;
        info[entry + 0x655] = version;
        info[entry + 0x11c..entry + 0x120].copy_from_slice(&value.to_le_bytes());
        info[entry + 0x138] = 1;
    }

    fn set_range(control: &mut [u8], index: usize, low: f32, high: f32) {
        let entry = CONTROL_BASE + index * CONTROL_STRIDE;
        control[entry + 4..entry + 8].copy_from_slice(&low.to_le_bytes());
        control[entry + 8..entry + 12].copy_from_slice(&high.to_le_bytes());
    }

    /// Values read from an RTX 5090 on 615.78.08 with the reference Python tool.
    #[test]
    fn blackwell_fuses() {
        let mut info = vec![0u8; INFO_SIZE];
        let mut control = vec![0u8; CONTROL_SIZE];
        set_var(&mut info, 3, VAR_TYPE_FUSE, 0x18, 3, 60650);
        set_var(&mut info, 7, VAR_TYPE_FUSE, 0x19, 3, 19800);
        set_var(&mut info, 14, VAR_TYPE_FUSE, 0x00, 1, 2215);
        set_range(&mut control, 14, 2018.94, 2284.87);

        let quality = parse(&info, Some(&control)).unwrap();
        assert_eq!(quality.speedo, 2215);
        assert_eq!(quality.nvvdd_iddq, Some(60650));
        assert_eq!(quality.msvdd_iddq, Some(19800));
        assert!((quality.quality_percent.unwrap() - 73.7).abs() < 0.05);
        assert!(!quality.fuse_override);
    }

    #[test]
    fn no_speedo_is_none() {
        let mut info = vec![0u8; INFO_SIZE];
        set_var(&mut info, 3, VAR_TYPE_FUSE, 0x18, 3, 60650);
        assert_eq!(parse(&info, None), None);
    }

    #[test]
    fn duplicate_speedo_is_ambiguous() {
        let mut info = vec![0u8; INFO_SIZE];
        set_var(&mut info, 1, VAR_TYPE_FUSE_LEGACY, 0x07, 0, 2000);
        set_var(&mut info, 2, VAR_TYPE_FUSE_LEGACY, 0x07, 0, 2100);
        assert_eq!(parse(&info, None), None);
    }

    #[test]
    fn version_mismatch_invalidates() {
        let mut info = vec![0u8; INFO_SIZE];
        set_var(&mut info, 14, VAR_TYPE_FUSE, 0x00, 1, 2215);
        let entry = INFO_BASE + 14 * INFO_STRIDE;
        info[entry + 0x114] = 2; // fuse version
        info[entry + 0x144] = 1; // expected version
        info[entry + 0x146] = 1; // check enabled
        assert_eq!(parse(&info, None), None);
    }
}

/// Reads the first GPU through the real driver: `cargo test -p lact-daemon -- --ignored asic_quality_live`
#[cfg(test)]
#[test]
#[ignore = "needs an NVIDIA GPU and libnvidia-api"]
fn asic_quality_live() {
    let nvapi = super::nvapi::NvApi::new().unwrap();
    let bus_id = std::env::var("LACT_TEST_NVIDIA_BUS").map_or(1, |bus| bus.parse().unwrap());
    let handle = nvapi
        .find_matching_gpu(bus_id)
        .unwrap()
        .expect("no GPU on that bus");
    let quality = nvapi.asic_quality(handle).unwrap().expect("no Speedo");
    println!("{quality:?}");
    assert!(quality.speedo > 0);
}
