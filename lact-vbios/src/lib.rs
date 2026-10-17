//! Read-only NVIDIA vBIOS parser.
//!
//! Works on ROM files (a full flash dump, an override image starting at the legacy
//! option ROM, or the PCI expansion-ROM slice from sysfs) and never touches hardware.
//! Table decoding is limited to Blackwell GB20x images; other families get the
//! identity information only.

mod bit;
pub mod edit;
mod gb20x;
mod image;
mod read;

pub use image::{ImageHeaderKind, RomImage};

use bit::Bit;
use image::Chain;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VbiosFamily {
    /// GB20x, vBIOS 98.xx
    Blackwell,
    /// AD10x, vBIOS 95.xx
    Ada,
    /// GA10x, vBIOS 94.xx
    Ampere,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImageCompleteness {
    /// Legacy, EFI and data images: the tables are available.
    Full,
    /// Only the PCI-visible legacy (+ EFI) images, as read through the expansion-ROM BAR.
    /// Identity is available, the data tables are not.
    PciSlice,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Integrity {
    pub object_count: usize,
    /// Directory objects whose stored SHA-384 does not match their data.
    pub bad_object_ids: Vec<u32>,
    pub directory_digest_valid: bool,
    /// Every image in the chain sums to zero.
    pub image_checksums_valid: bool,
}

impl Integrity {
    /// Hashes and checksums are consistent. The cryptographic signature is never checked:
    /// edited images can be consistent here and still carry an invalid signature.
    pub fn consistent(&self) -> bool {
        self.bad_object_ids.is_empty() && self.directory_digest_valid && self.image_checksums_valid
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PowerUnit {
    Milliwatt,
    Milliamp,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PowerPolicyRole {
    /// Total board power (TGP); its limits are what NVML reports as the power limit range.
    BoardPower,
    #[serde(rename = "input_current_12v")]
    InputCurrent12V,
    NvvddOutputCurrent,
    MsvddOutputCurrent,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct PowerPolicyRow {
    pub index: usize,
    pub kind: u8,
    pub role: Option<PowerPolicyRole>,
    pub unit: PowerUnit,
    pub min: u32,
    pub default: u32,
    pub max: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ClockOffsetRange {
    pub index: usize,
    /// Set only when the whole table matches the known row layout.
    pub domain: Option<String>,
    pub descriptor: u16,
    pub min_mhz: i16,
    pub max_mhz: i16,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RailLimitRole {
    Rel,
    AltRel,
    Ov,
    Vmin,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RailLimit {
    pub role: RailLimitRole,
    /// VFE equation index; `None` when the rail has no such limit.
    pub selector: Option<u16>,
    /// Output clamp of the selected equation.
    pub min_uv: Option<f32>,
    pub max_uv: Option<f32>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct RailVoltageLimits {
    pub rail: String,
    pub boot_voltage_uv: u32,
    pub limits: Vec<RailLimit>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct VbiosReport {
    pub size: usize,
    pub vendor_id: u16,
    pub device_id: u16,
    pub version: Option<String>,
    /// Description and board identity strings from the legacy image header.
    pub strings: Vec<String>,
    pub family: VbiosFamily,
    pub completeness: ImageCompleteness,
    pub images: Vec<RomImage>,
    pub integrity: Option<Integrity>,
    pub power_policies: Vec<PowerPolicyRow>,
    pub clock_ranges: Vec<ClockOffsetRange>,
    pub rails: Vec<RailVoltageLimits>,
    /// Tables that could not be decoded, with the reason.
    pub warnings: Vec<String>,
}

impl VbiosReport {
    pub fn power_policy(&self, role: PowerPolicyRole) -> Option<&PowerPolicyRow> {
        self.power_policies
            .iter()
            .find(|row| row.role == Some(role))
    }

    pub fn clock_range(&self, domain: &str) -> Option<&ClockOffsetRange> {
        self.clock_ranges
            .iter()
            .find(|range| range.domain.as_deref() == Some(domain))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VbiosError(pub String);

impl std::fmt::Display for VbiosError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VbiosError {}

/// Parses a ROM image. Fails only when the image has no recognisable NVIDIA legacy
/// image and BIT; individual tables that cannot be decoded end up in `warnings`.
pub fn parse(rom: &[u8]) -> Result<VbiosReport, VbiosError> {
    let images = image::scan(rom);
    let chain = Chain::find(&images).map_err(VbiosError)?;
    let bit = Bit::parse(rom, &chain).map_err(VbiosError)?;
    let version = bit.version(rom);
    let family = family(version.as_deref());
    let completeness = if chain
        .data
        .iter()
        .any(|image| image.header_kind == ImageHeaderKind::Npds)
    {
        ImageCompleteness::Full
    } else {
        ImageCompleteness::PciSlice
    };

    let mut report = VbiosReport {
        size: rom.len(),
        vendor_id: chain.legacy.vendor_id,
        device_id: chain.legacy.device_id,
        version,
        strings: header_strings(rom, &chain),
        family,
        completeness,
        images,
        integrity: None,
        power_policies: vec![],
        clock_ranges: vec![],
        rails: vec![],
        warnings: vec![],
    };

    if completeness == ImageCompleteness::PciSlice {
        report
            .warnings
            .push("PCI expansion-ROM slice only: the data tables are not part of it".to_owned());
        return Ok(report);
    }
    if family != VbiosFamily::Blackwell {
        report
            .warnings
            .push("table decoding is only implemented for Blackwell GB20x images".to_owned());
        return Ok(report);
    }

    let tables = match gb20x::Gb20x::new(rom, &chain, &bit) {
        Ok(tables) => tables,
        Err(err) => {
            report.warnings.push(format!("object directory: {err}"));
            return Ok(report);
        }
    };
    report.integrity = Some(tables.integrity());
    match tables.power_policies(chain.legacy.device_id) {
        Ok(rows) => report.power_policies = rows,
        Err(err) => report.warnings.push(format!("power policies: {err}")),
    }
    match tables.clock_ranges() {
        Ok(rows) => report.clock_ranges = rows,
        Err(err) => report.warnings.push(format!("clock offset ranges: {err}")),
    }
    match tables.rail_limits() {
        Ok(rows) => report.rails = rows,
        Err(err) => report.warnings.push(format!("voltage rails: {err}")),
    }
    Ok(report)
}

/// Compares the PCI-visible part (legacy + EFI images) of two ROMs: a full image and the
/// slice the card exposes through its expansion-ROM BAR. Returns `None` when either side
/// has no recognisable image chain.
///
/// A match means the file carries the same option ROM as the card's flash, which in
/// practice identifies the same vBIOS build; the data images themselves cannot be
/// compared because the BAR does not expose them.
pub fn same_pci_images(full: &[u8], slice: &[u8]) -> Option<bool> {
    let pci_part = |rom: &[u8]| -> Option<std::ops::Range<usize>> {
        let chain = Chain::find(&image::scan(rom)).ok()?;
        Some(chain.legacy.start..chain.pci_end())
    };
    let (a, b) = (pci_part(full)?, pci_part(slice)?);
    Some(full[a] == slice[b])
}

/// Lowercase hex SHA-256 of `data`, for identifying source and edited images.
pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Extracts the PCI-visible slice (legacy + EFI images) from a ROM.
pub fn pci_slice(rom: &[u8]) -> Option<&[u8]> {
    let chain = Chain::find(&image::scan(rom)).ok()?;
    rom.get(chain.legacy.start..chain.pci_end())
}

fn family(version: Option<&str>) -> VbiosFamily {
    match version.and_then(|version| version.get(..3)) {
        Some("98.") => VbiosFamily::Blackwell,
        Some("95.") => VbiosFamily::Ada,
        Some("94.") => VbiosFamily::Ampere,
        _ => VbiosFamily::Unknown,
    }
}

/// The text lines printed by the option ROM: they precede the `Version ...` line near
/// the start of the legacy image (description such as "PG145 SKU 30 VGA BIOS", board
/// identity such as "ROG-MATRIX-RTX5090").
fn header_strings(rom: &[u8], chain: &Chain) -> Vec<String> {
    const SEARCH_LEN: usize = 0x2000;
    const LOOKBACK: usize = 0x80;

    let start = chain.legacy.start;
    let window = &rom[start..chain.legacy.end().min(start + SEARCH_LEN)];
    let Some(version_at) = window.windows(8).position(|w| w == b"Version ") else {
        return vec![];
    };
    let text = &window[version_at.saturating_sub(LOOKBACK)..version_at];
    text.split(|b| !(0x20..0x7f).contains(b))
        .filter(|run| run.len() >= 5)
        .map(|run| String::from_utf8_lossy(run).trim().to_owned())
        .filter(|s| s.len() >= 5)
        .collect()
}

#[cfg(test)]
mod tests;
