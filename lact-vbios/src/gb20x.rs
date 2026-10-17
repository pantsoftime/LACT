//! Blackwell GB20x (vBIOS 98.xx) data tables.
//!
//! Layouts verified against full dumps of the ROG Matrix (98.02.2E.80.C9), Astral XOC
//! (98.02.2E.80.50) and MSI Lightning (98.02.2E.C0.0F) RTX 5090 images, and cross-checked
//! against the live RM values those images produce. Every table is gated on its header
//! bytes and on lying inside the directory object it belongs to; anything else is reported
//! as unsupported rather than guessed at.

use crate::{
    ClockOffsetRange, Integrity, PowerPolicyRole, PowerPolicyRow, PowerUnit, RailLimit,
    RailLimitRole, RailVoltageLimits,
    bit::Bit,
    image::{Chain, ImageHeaderKind},
    read::{f32_at, i16_at, u16_at, u32_at},
};
use sha2::{Digest, Sha384};

const DIRECTORY_ENTRY_SIZE: usize = 60;
const DIRECTORY_MARKER: [u8; 8] = [3, 0, 0, 0, 16, 0, 0, 0];
/// The directory SHA-384 lives at this offset into the signed metadata image.
const DIRECTORY_DIGEST_OFFSET: usize = 0x209;

pub const OBJ_CLOCK_RANGES: u32 = 0x03;
pub const OBJ_POWER_POLICY: u32 = 0x13;
pub const OBJ_VFE: u32 = 0x14;

/// BIT 'P' token pointer offsets.
const P_VFE: usize = 0x20;
const P_POWER_POLICY: usize = 0x2c;
const P_VOLTAGE_RAIL: usize = 0x68;
const P_POWER_PROFILES: usize = 0xf4;
/// BIT 'C' token pointer offset of the clock offset-range table.
const C_CLOCK_RANGES: usize = 0x08;

pub const POWER_ROW_SIZE: usize = 0x66;
pub const POWER_ROW_COUNT: usize = 20;
/// Offsets of the u32 limits in a power policy row.
pub const POWER_ROW_MIN: usize = 13;
pub const POWER_ROW_DEFAULT: usize = 17;
pub const POWER_ROW_MAX: usize = 21;

pub const EQUATION_SIZE: usize = 25;
/// Offsets of the f32 output clamp (µV for voltage equations) in a VFE equation.
pub const EQUATION_OUTPUT_MIN: usize = 3;
pub const EQUATION_OUTPUT_MAX: usize = 7;

/// Offsets of the s16 MHz bounds in a clock offset-range row.
pub const CLOCK_ROW_MIN: usize = 3;
pub const CLOCK_ROW_MAX: usize = 5;

/// Power override profiles: a fixed header, two selection stages, then 16 profiles of a
/// flag byte followed by 8 entries of `{u8 kind, u32 policy | field << 8, u32 value}`.
/// Field 0 is a policy's maximum and field 1 its default. Profiles 4..7 hold the limits
/// for the 12V-2x6 cable capability levels.
const PROFILE_HEADER: [u8; 10] = [0x10, 0x0a, 0x05, 0x04, 0x07, 0x08, 0x01, 0x10, 0x09, 0x08];
const PROFILES_OFFSET: usize = 0xfe;
const PROFILE_SIZE: usize = 0x49;
const PROFILE_COUNT: usize = 16;
const PROFILE_ENTRIES: usize = 8;
const PROFILE_ENTRY_SIZE: usize = 9;

#[derive(Debug, Clone)]
pub struct Object {
    pub id: u32,
    pub data: usize,
    pub size: usize,
    pub hash_offset: usize,
}

#[derive(Debug, Clone)]
pub struct Directory {
    pub header: usize,
    pub data_base: usize,
    pub digest_offset: usize,
    pub objects: Vec<Object>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileEntry {
    pub offset: usize,
    pub policy: u8,
    /// 0 = maximum, 1 = default
    pub field: u8,
    pub value: u32,
}

#[derive(Debug, Clone)]
pub struct PowerProfile {
    pub index: usize,
    pub enabled: bool,
    pub entries: Vec<ProfileEntry>,
}

impl PowerProfile {
    pub fn entry(&self, policy: u8, field: u8) -> Option<&ProfileEntry> {
        self.entries
            .iter()
            .find(|entry| entry.policy == policy && entry.field == field)
    }
}

/// The equations one rail row selects, with their file offsets.
#[derive(Debug, Clone)]
pub struct RailEquations {
    pub name: &'static str,
    pub boot_voltage_uv: u32,
    /// (role, selector, equation offset); the offset is `None` for an absent selector or
    /// an equation outside object 0x14.
    pub roles: Vec<(RailLimitRole, Option<u16>, Option<usize>)>,
}

pub struct Gb20x<'a> {
    rom: &'a [u8],
    chain: &'a Chain,
    bit: &'a Bit,
    pub directory: Directory,
}

impl<'a> Gb20x<'a> {
    pub fn new(rom: &'a [u8], chain: &'a Chain, bit: &'a Bit) -> Result<Self, String> {
        let directory = find_directory(rom, chain)?;
        Ok(Self {
            rom,
            chain,
            bit,
            directory,
        })
    }

    pub fn chain(&self) -> &Chain {
        self.chain
    }

    /// Checks that `[at, at + len)` lies inside a directory object with the given id.
    fn owned_by(&self, at: usize, len: usize, id: u32) -> Result<(), String> {
        let owned = self.directory.objects.iter().any(|object| {
            object.id == id && object.data <= at && at + len <= object.data + object.size
        });
        if owned {
            Ok(())
        } else {
            Err(format!(
                "table at {at:#x} is not inside directory object {id:#04x}"
            ))
        }
    }

    pub fn integrity(&self) -> Integrity {
        let rom = self.rom;
        let directory = &self.directory;
        let bad_object_ids = directory
            .objects
            .iter()
            .filter(|object| {
                rom.get(object.data..object.data + object.size)
                    .zip(rom.get(object.hash_offset..object.hash_offset + 48))
                    .is_none_or(|(data, digest)| Sha384::digest(data).as_slice() != digest)
            })
            .map(|object| object.id)
            .collect();
        let directory_digest_valid = rom
            .get(directory.header..directory.data_base)
            .zip(rom.get(directory.digest_offset..directory.digest_offset + 48))
            .is_some_and(|(data, digest)| Sha384::digest(data).as_slice() == digest);
        let image_checksums_valid = std::iter::once(&self.chain.legacy)
            .chain(&self.chain.efi)
            .chain(&self.chain.data)
            .all(|image| checksum(&rom[image.start..image.end()]) == 0);

        Integrity {
            object_count: directory.objects.len(),
            bad_object_ids,
            directory_digest_valid,
            image_checksums_valid,
        }
    }

    /// File offset of the first power policy row.
    pub fn power_rows(&self) -> Result<usize, String> {
        const ROW_SIZE: usize = POWER_ROW_SIZE;
        const ROW_COUNT: usize = POWER_ROW_COUNT;

        let rom = self.rom;
        let at = self.bit.table(rom, self.chain, 'P', P_POWER_POLICY)?;
        let (version, header_size, row_size, count) = (
            rom[at],
            usize::from(rom[at + 1]),
            usize::from(rom[at + 2]),
            usize::from(rom[at + 3]),
        );
        if version != 0x40
            || !matches!(header_size, 0x38 | 0x3d)
            || row_size != ROW_SIZE
            || count != ROW_COUNT
        {
            return Err(format!(
                "unsupported power policy table (version {version:#x}, header {header_size:#x}, row {row_size:#x}, count {count})"
            ));
        }
        self.owned_by(at, header_size + ROW_SIZE * ROW_COUNT, OBJ_POWER_POLICY)?;
        Ok(at + header_size)
    }

    pub fn power_policies(&self, device_id: u16) -> Result<Vec<PowerPolicyRow>, String> {
        let rom = self.rom;
        let rows = self.power_rows()?;

        // The role assignments hold for desktop boards; notebook images move the board
        // budget to another row, so only raw rows are reported for them.
        let desktop = !NOTEBOOK_DEVICE_IDS.contains(&device_id);

        (0..POWER_ROW_COUNT)
            .map(|index| {
                let row = rows + POWER_ROW_SIZE * index;
                let kind = rom[row];
                let read = |offset| u32_at(rom, row + offset).ok_or("truncated power policy row");
                let unit = if (9..=14).contains(&index) {
                    PowerUnit::Milliamp
                } else {
                    PowerUnit::Milliwatt
                };
                let role = match (index, kind) {
                    _ if !desktop => None,
                    (8, 0x0c) => Some(PowerPolicyRole::BoardPower),
                    (9, 0x0b) => Some(PowerPolicyRole::InputCurrent12V),
                    (13, 0x07) => Some(PowerPolicyRole::NvvddOutputCurrent),
                    (14, 0x07) => Some(PowerPolicyRole::MsvddOutputCurrent),
                    _ => None,
                };
                Ok(PowerPolicyRow {
                    index,
                    kind,
                    role,
                    unit,
                    min: read(POWER_ROW_MIN)?,
                    default: read(POWER_ROW_DEFAULT)?,
                    max: read(POWER_ROW_MAX)?,
                })
            })
            .collect()
    }

    /// The power override profiles. Fails on an unknown header or entry encoding, so
    /// callers editing power limits can refuse images whose profiles they cannot account for.
    pub fn power_profiles(&self) -> Result<Vec<PowerProfile>, String> {
        let rom = self.rom;
        let at = self.bit.table(rom, self.chain, 'P', P_POWER_PROFILES)?;
        if rom.get(at..at + PROFILE_HEADER.len()) != Some(&PROFILE_HEADER[..]) {
            return Err("unsupported power profile table header".to_owned());
        }
        let len = PROFILES_OFFSET + PROFILE_SIZE * PROFILE_COUNT;
        if !self
            .directory
            .objects
            .iter()
            .any(|object| object.data <= at && at + len <= object.data + object.size)
        {
            return Err(format!(
                "power profile table at {at:#x} is not inside one directory object"
            ));
        }

        (0..PROFILE_COUNT)
            .map(|index| {
                let profile = at + PROFILES_OFFSET + PROFILE_SIZE * index;
                let enabled = match rom[profile] {
                    0 => false,
                    1 => true,
                    flag => {
                        return Err(format!("power profile {index} has unknown flag {flag:#x}"));
                    }
                };
                let mut entries = Vec::new();
                for slot in 0..PROFILE_ENTRIES {
                    let offset = profile + 1 + PROFILE_ENTRY_SIZE * slot;
                    let kind = rom[offset];
                    let selector = u32_at(rom, offset + 1).ok_or("truncated power profile")?;
                    let value = u32_at(rom, offset + 5).ok_or("truncated power profile")?;
                    match kind {
                        0 if selector == 0 && value == 0 => {}
                        1 if selector >> 16 == 0 && selector >> 8 <= 1 => {
                            entries.push(ProfileEntry {
                                offset,
                                #[allow(clippy::cast_possible_truncation)]
                                policy: selector as u8,
                                #[allow(clippy::cast_possible_truncation)]
                                field: (selector >> 8) as u8,
                                value,
                            })
                        }
                        _ => {
                            return Err(format!(
                                "power profile {index} entry {slot} has an unknown encoding"
                            ));
                        }
                    }
                }
                Ok(PowerProfile {
                    index,
                    enabled,
                    entries,
                })
            })
            .collect()
    }

    /// File offsets of the clock offset-range rows, and whether they match the known
    /// domain layout (only then are rows 0..5 GPC, XBAR, DRAM, SYS, VID).
    pub fn clock_rows(&self) -> Result<(Vec<usize>, bool), String> {
        /// Row descriptors (u16 at row+9) of the known nine-row layout; the first five
        /// rows are the domains NVML and the RM clock client expose offsets for.
        const DESCRIPTORS: [u16; 9] = [0x43, 0x84, 0x182, 0xc5, 0x106, 0x147, 0x8, 0x200, 0x1c1];

        let rom = self.rom;
        let at = self.bit.table(rom, self.chain, 'C', C_CLOCK_RANGES)?;
        let header = rom.get(at..at + 6).ok_or("truncated clock range table")?;
        let (version, header_size, entry_size, count, sub_count, sub_size) = (
            header[0],
            usize::from(header[1]),
            usize::from(header[2]),
            usize::from(header[3]),
            usize::from(header[4]),
            usize::from(header[5]),
        );
        if version != 0x50
            || header_size != 13
            || entry_size != 15
            || count != 9
            || sub_count != 3
            || !matches!(sub_size, 1 | 2)
        {
            return Err(format!(
                "unsupported clock range table (version {version:#x}, header {header_size}, entry {entry_size}, count {count})"
            ));
        }
        let stride = entry_size + sub_count * sub_size;
        self.owned_by(at, header_size + stride * count, OBJ_CLOCK_RANGES)?;

        let rows = (0..count)
            .map(|index| at + header_size + stride * index)
            .collect::<Vec<_>>();
        // Bit 12 of the DRAM descriptor is set on notebook images; the row is otherwise identical.
        let known_layout = rows.iter().zip(DESCRIPTORS).all(|(&row, expected)| {
            rom[row] == 2
                && u16_at(rom, row + 9).is_some_and(|descriptor| descriptor & !0x1000 == expected)
        });
        Ok((rows, known_layout))
    }

    pub fn clock_ranges(&self) -> Result<Vec<ClockOffsetRange>, String> {
        const NAMES: [&str; 5] = ["GPC", "XBAR", "DRAM", "SYS", "VID"];

        let rom = self.rom;
        let (rows, known_layout) = self.clock_rows()?;
        rows.into_iter()
            .enumerate()
            .map(|(index, row)| {
                Ok(ClockOffsetRange {
                    index,
                    domain: known_layout
                        .then(|| NAMES.get(index).map(|name| (*name).to_owned()))
                        .flatten(),
                    descriptor: u16_at(rom, row + 9).ok_or("truncated clock range row")?,
                    min_mhz: i16_at(rom, row + CLOCK_ROW_MIN).ok_or("truncated clock range row")?,
                    max_mhz: i16_at(rom, row + CLOCK_ROW_MAX).ok_or("truncated clock range row")?,
                })
            })
            .collect()
    }

    /// The VFE equations each rail row selects for its REL (reliability), ALT
    /// (alternate reliability), OV (overvoltage) and VMIN limits.
    pub fn rail_equations(&self) -> Result<Vec<RailEquations>, String> {
        const RAIL_ROW_SIZE: usize = 0x41;
        const VFE_HEADER: [u8; 5] = [0x30, 0x0e, 0x1d, 0xff, 0x19];
        const ROLES: [(RailLimitRole, usize); 4] = [
            (RailLimitRole::Rel, 0x0d),
            (RailLimitRole::AltRel, 0x0f),
            (RailLimitRole::Ov, 0x11),
            (RailLimitRole::Vmin, 0x21),
        ];

        let rom = self.rom;
        let rail = self.bit.table(rom, self.chain, 'P', P_VOLTAGE_RAIL)?;
        let header = rom.get(rail..rail + 5).ok_or("truncated rail table")?;
        let count = usize::from(header[3]);
        if header[..3] != [0x30, 0x05, 0x41]
            || !matches!(count, 1 | 2)
            || rail + 5 + RAIL_ROW_SIZE * count > rom.len()
        {
            return Err(format!(
                "unsupported voltage rail table header {header:02x?}"
            ));
        }

        let vfe = self.bit.table(rom, self.chain, 'P', P_VFE)?;
        if rom.get(vfe..vfe + 5) != Some(&VFE_HEADER[..]) {
            return Err("unsupported VFE table header".to_owned());
        }
        let equations =
            vfe + usize::from(rom[vfe + 1]) + usize::from(rom[vfe + 2]) * usize::from(rom[vfe + 3]);
        self.owned_by(vfe, equations - vfe, OBJ_VFE)?;

        (0..count)
            .map(|index| {
                let row = rail + 5 + RAIL_ROW_SIZE * index;
                let name = match (rom[row], rom[row + 1]) {
                    (1, 1) => "NVVDD",
                    (1, 2) => "MSVDD",
                    other => return Err(format!("unrecognised rail identity {other:02x?}")),
                };
                let roles = ROLES
                    .iter()
                    .map(|&(role, offset)| {
                        let selector = u16_at(rom, row + offset).ok_or("truncated rail row")?;
                        let selector = (selector != 0xffff).then_some(selector);
                        let equation = selector
                            .map(|selector| equations + usize::from(selector) * EQUATION_SIZE)
                            .filter(|&equation| {
                                self.owned_by(equation, EQUATION_SIZE, OBJ_VFE).is_ok()
                            });
                        Ok((role, selector, equation))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                Ok(RailEquations {
                    name,
                    boot_voltage_uv: u32_at(rom, row + 0x19).ok_or("truncated rail row")?,
                    roles,
                })
            })
            .collect()
    }

    /// Output clamp of a voltage-output equation (unit code 2), in µV.
    pub fn voltage_clamp(&self, equation: usize) -> Option<(f32, f32)> {
        let rom = self.rom;
        if rom.get(equation + 23)? & 0x0f != 2 {
            return None;
        }
        let min = f32_at(rom, equation + EQUATION_OUTPUT_MIN).filter(|v| v.is_finite())?;
        let max = f32_at(rom, equation + EQUATION_OUTPUT_MAX).filter(|v| v.is_finite())?;
        (min <= max).then_some((min, max))
    }

    pub fn rail_limits(&self) -> Result<Vec<RailVoltageLimits>, String> {
        Ok(self
            .rail_equations()?
            .into_iter()
            .map(|rail| RailVoltageLimits {
                rail: rail.name.to_owned(),
                boot_voltage_uv: rail.boot_voltage_uv,
                limits: rail
                    .roles
                    .iter()
                    .map(|&(role, selector, equation)| {
                        let clamp = equation.and_then(|equation| self.voltage_clamp(equation));
                        RailLimit {
                            role,
                            selector,
                            min_uv: clamp.map(|(min, _)| min),
                            max_uv: clamp.map(|(_, max)| max),
                        }
                    })
                    .collect(),
            })
            .collect())
    }
}

/// Notebook GB20x device IDs whose power policy rows use a different role layout.
const NOTEBOOK_DEVICE_IDS: [u16; 1] = [0x2c59];

fn checksum(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |sum, b| sum.wrapping_add(*b))
}

/// Locates the object hash directory inside the signed metadata image (the first data
/// image after EFI). Each 60-byte entry is `{id, offset, size}` followed by the SHA-384
/// of the object; offsets are relative to the end of the entry array.
fn find_directory(rom: &[u8], chain: &Chain) -> Result<Directory, String> {
    let meta = chain
        .data
        .first()
        .filter(|image| image.header_kind == ImageHeaderKind::Pcir)
        .ok_or("no signed metadata image after EFI")?;
    let data_end = chain
        .data
        .iter()
        .find(|image| image.header_kind == ImageHeaderKind::Npds)
        .ok_or("no NPDS data image")?
        .end();

    let mut candidates = Vec::new();
    for at in meta.start..meta.header.saturating_sub(16) {
        if rom[at..at + 8] != DIRECTORY_MARKER {
            continue;
        }
        let (Some(header_size), Some(count), Some(device)) = (
            u32_at(rom, at + 4),
            u32_at(rom, at + 8),
            u32_at(rom, at + 12),
        ) else {
            continue;
        };
        let (header_size, count) = (header_size as usize, count as usize);
        if !(2..=1024).contains(&count) || device != u32::from(chain.legacy.device_id) {
            continue;
        }
        let data_base = at + header_size + count * DIRECTORY_ENTRY_SIZE;
        if data_base > meta.header {
            continue;
        }
        let objects: Option<Vec<Object>> = (0..count)
            .map(|index| {
                let entry = at + header_size + index * DIRECTORY_ENTRY_SIZE;
                let id = u32_at(rom, entry)?;
                let data = data_base + u32_at(rom, entry + 4)? as usize;
                let size = u32_at(rom, entry + 8)? as usize;
                (size != 0 && data + size <= data_end).then_some(Object {
                    id,
                    data,
                    size,
                    hash_offset: entry + 12,
                })
            })
            .collect();
        let Some(objects) = objects else { continue };
        if objects[0].id != 0xfb || objects[1].id != 0xbe || objects[1].data != meta.header {
            continue;
        }
        candidates.push(Directory {
            header: at,
            data_base,
            digest_offset: meta.start + DIRECTORY_DIGEST_OFFSET,
            objects,
        });
    }
    match candidates.len() {
        1 => Ok(candidates.remove(0)),
        n => Err(format!("expected one object directory, found {n}")),
    }
}
