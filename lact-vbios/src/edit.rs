//! Offline editing of a small set of GB20x vBIOS fields.
//!
//! Only fields whose effect has been confirmed on hardware are offered (board power
//! limits, the NVVDD/MSVDD reliability voltage clamp, the DRAM offset ceiling), plus the
//! board power default, which is marked experimental. Everything else is left to a
//! dedicated editor.
//!
//! An edit never touches the input: it returns a new image in which only the requested
//! fields changed, the SHA-384 of every object those writes touched and the directory
//! digest were recomputed, and the checksum byte of each affected image was adjusted.
//! The result is parsed again and rejected if any other byte changed, if a field does
//! not read back as requested, or if the hashes and checksums are not consistent.
//!
//! The cryptographic signature cannot be regenerated: an edited image is only usable
//! through a driver that hands it to the GPU firmware without checking the signature
//! (a vBIOS override). It must never be flashed.

use crate::{
    RailLimitRole, VbiosError, VbiosFamily,
    bit::Bit,
    gb20x::{
        CLOCK_ROW_MAX, CLOCK_ROW_MIN, EQUATION_OUTPUT_MAX, Gb20x, POWER_ROW_DEFAULT, POWER_ROW_MAX,
        POWER_ROW_MIN, POWER_ROW_SIZE, PowerProfile,
    },
    image::{self, Chain},
    read::{i16_at, u32_at},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha384};
use std::collections::BTreeSet;

/// Lowest board power minimum this editor writes; the same floor the daemon's runtime cap
/// accepts.
const POWER_MIN_FLOOR_MW: u32 = 30_000;
/// The board power ceiling may be raised to at most this multiple of the board power default.
const POWER_MAX_DEFAULT_FACTOR: u32 = 2;
/// Highest DRAM offset ceiling (table MHz): what the ASUS XOC image ships and what has
/// been run on the RTX 5090. Images already above it keep their value as the cap.
const DRAM_MAX_CAP_TABLE_MHZ: i16 = 5000;

/// Power policy rows that carry the board power limits: the fixed board policies 2 and 7
/// and the total board power policy 8, with their expected row types.
const BOARD_ROWS: [(usize, u8); 3] = [(2, 0x01), (7, 0x0d), (8, 0x0c)];
const BOARD_POWER_ROW: usize = 8;
const FIELD_MAX: u8 = 0;
const FIELD_DEFAULT: u8 = 1;

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Rail {
    Nvvdd,
    Msvdd,
}

impl Rail {
    fn name(self) -> &'static str {
        match self {
            Self::Nvvdd => "NVVDD",
            Self::Msvdd => "MSVDD",
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "snake_case")]
pub enum EditField {
    BoardPowerMax,
    BoardPowerDefault,
    BoardPowerMin,
    RelVoltageMax(Rail),
    DramOffsetMax,
}

impl EditField {
    pub const ALL: [Self; 6] = [
        Self::BoardPowerMax,
        Self::BoardPowerDefault,
        Self::BoardPowerMin,
        Self::RelVoltageMax(Rail::Nvvdd),
        Self::RelVoltageMax(Rail::Msvdd),
        Self::DramOffsetMax,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::BoardPowerMax => "Board power maximum",
            Self::BoardPowerDefault => "Board power default",
            Self::BoardPowerMin => "Board power minimum",
            Self::RelVoltageMax(Rail::Nvvdd) => "NVVDD reliability voltage limit",
            Self::RelVoltageMax(Rail::Msvdd) => "MSVDD reliability voltage limit",
            Self::DramOffsetMax => "Memory offset maximum",
        }
    }

    pub fn unit(self) -> EditUnit {
        match self {
            Self::BoardPowerMax | Self::BoardPowerDefault | Self::BoardPowerMin => {
                EditUnit::Milliwatt
            }
            Self::RelVoltageMax(_) => EditUnit::Microvolt,
            Self::DramOffsetMax => EditUnit::TableMhz,
        }
    }

    /// Not confirmed on hardware yet.
    pub fn experimental(self) -> bool {
        matches!(self, Self::BoardPowerDefault)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EditUnit {
    Milliwatt,
    Microvolt,
    /// vBIOS table MHz; the NVML/LACT memory offset is twice this value.
    TableMhz,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct EditableField {
    pub field: EditField,
    pub current: i64,
    /// Bounds this editor accepts: format and consistency limits plus LACT's own caps.
    /// They are not a statement that the hardware tolerates the whole range.
    pub min: i64,
    pub max: i64,
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct UnavailableField {
    pub field: EditField,
    pub reason: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct EditableFields {
    pub fields: Vec<EditableField>,
    pub unavailable: Vec<UnavailableField>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AppliedEdit {
    pub field: EditField,
    pub before: i64,
    pub after: i64,
    /// Power override profiles whose entries were updated along with the base table.
    pub profiles: Vec<usize>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct EditSummary {
    pub applied: Vec<AppliedEdit>,
    pub changed_bytes: usize,
    pub rehashed_objects: Vec<u32>,
    /// The source's directory digest was stale and is now valid.
    pub directory_digest_repaired: bool,
}

#[derive(Debug, Clone)]
pub struct EditResult {
    pub data: Vec<u8>,
    pub summary: EditSummary,
}

struct Context<'a> {
    tables: Gb20x<'a>,
    power_rows: Result<usize, String>,
    profiles: Result<Vec<PowerProfile>, String>,
}

impl Context<'_> {
    fn power_row(&self, index: usize) -> Result<usize, String> {
        Ok(*self.power_rows.as_ref().map_err(Clone::clone)? + POWER_ROW_SIZE * index)
    }
}

fn with_context<T>(
    rom: &[u8],
    f: impl FnOnce(&[u8], &Context<'_>) -> Result<T, String>,
) -> Result<T, VbiosError> {
    let report = crate::parse(rom)?;
    if report.family != VbiosFamily::Blackwell
        || report.completeness != crate::ImageCompleteness::Full
    {
        return Err(VbiosError(
            "editing needs a full Blackwell GB20x image".to_owned(),
        ));
    }
    let chain = Chain::find(&image::scan(rom)).map_err(VbiosError)?;
    let bit = Bit::parse(rom, &chain).map_err(VbiosError)?;
    let tables = Gb20x::new(rom, &chain, &bit).map_err(VbiosError)?;
    let power_rows = tables.power_rows();
    let profiles = tables.power_profiles();
    let context = Context {
        tables,
        power_rows,
        profiles,
    };
    f(rom, &context).map_err(VbiosError)
}

/// The fields this editor can change in `rom`, with their accepted bounds, and the
/// reason for each field it cannot change.
pub fn editable_fields(rom: &[u8]) -> Result<EditableFields, VbiosError> {
    with_context(rom, |rom, context| {
        let mut result = EditableFields::default();
        for field in EditField::ALL {
            match describe(rom, context, field) {
                Ok(described) => result.fields.push(described),
                Err(reason) => result.unavailable.push(UnavailableField { field, reason }),
            }
        }
        Ok(result)
    })
}

/// Applies `changes` (values in each field's stored unit) to a copy of `rom`.
pub fn edit(rom: &[u8], changes: &[(EditField, i64)]) -> Result<EditResult, VbiosError> {
    let mut seen = BTreeSet::new();
    if let Some((field, _)) = changes.iter().find(|(field, _)| !seen.insert(*field)) {
        return Err(VbiosError(format!("{} is requested twice", field.label())));
    }

    let source = crate::parse(rom)?;
    let integrity = source
        .integrity
        .clone()
        .ok_or_else(|| VbiosError("the image's object directory could not be read".to_owned()))?;
    if !integrity.bad_object_ids.is_empty() || !integrity.image_checksums_valid {
        return Err(VbiosError(
            "the image has stale object hashes or bad checksums; refusing to edit it".to_owned(),
        ));
    }

    let (data, applied, rehashed_objects) = with_context(rom, |rom, context| {
        let mut data = rom.to_vec();
        let mut writes: Vec<(usize, usize)> = Vec::new();
        let mut applied = Vec::new();
        // Independent fields are checked against the source's bounds; the board power fields
        // against each other on the combined result, since raising the maximum and the
        // default together is one consistent change.
        check_power_request(rom, context, changes)?;
        for &(field, value) in changes {
            let described = describe(rom, context, field)?;
            let power = matches!(
                field,
                EditField::BoardPowerMax | EditField::BoardPowerDefault | EditField::BoardPowerMin
            );
            if !power && (value < described.min || value > described.max) {
                return Err(format!(
                    "{}: {value} is outside {}..={}",
                    field.label(),
                    described.min,
                    described.max
                ));
            }
            let profiles = write_field(rom, context, &mut data, &mut writes, field, value)?;
            applied.push(AppliedEdit {
                field,
                before: described.current,
                after: value,
                profiles,
            });
        }
        check_power_consistency(&data, context)?;
        let rehashed = repair(&mut data, context, &writes)?;
        Ok((data, applied, rehashed))
    })?;

    verify(rom, &data, changes)?;

    let changed_bytes = rom.iter().zip(&data).filter(|(a, b)| a != b).count();
    Ok(EditResult {
        data,
        summary: EditSummary {
            applied: applied
                .into_iter()
                .filter(|edit| edit.before != edit.after)
                .collect(),
            changed_bytes,
            rehashed_objects,
            directory_digest_repaired: !integrity.directory_digest_valid && changed_bytes > 0,
        },
    })
}

fn read_u32(rom: &[u8], at: usize) -> Result<u32, String> {
    u32_at(rom, at).ok_or_else(|| format!("read outside the image at {at:#x}"))
}

/// The board rows' (min, default, max), after checking each row's type.
fn board_rows(rom: &[u8], context: &Context<'_>) -> Result<Vec<(usize, [u32; 3])>, String> {
    BOARD_ROWS
        .iter()
        .map(|&(index, kind)| {
            let row = context.power_row(index)?;
            if rom[row] != kind {
                return Err(format!(
                    "power policy row {index} has an unexpected type {:#x}",
                    rom[row]
                ));
            }
            Ok((
                index,
                [
                    read_u32(rom, row + POWER_ROW_MIN)?,
                    read_u32(rom, row + POWER_ROW_DEFAULT)?,
                    read_u32(rom, row + POWER_ROW_MAX)?,
                ],
            ))
        })
        .collect()
}

/// Enabled profiles that override any board policy, split into those carrying exactly the
/// base table's maximum and default for every board policy (they are kept in step with
/// the base on edits) and the rest (lower cable-capability levels, left alone).
fn board_profiles<'p>(
    rom: &[u8],
    context: &'p Context<'_>,
) -> Result<(Vec<&'p PowerProfile>, Vec<&'p PowerProfile>), String> {
    let profiles = context.profiles.as_ref().map_err(Clone::clone)?;
    let rows = board_rows(rom, context)?;
    let mut matching = Vec::new();
    let mut other = Vec::new();
    for profile in profiles.iter().filter(|profile| profile.enabled) {
        let board_entries = profile
            .entries
            .iter()
            .filter(|entry| {
                rows.iter()
                    .any(|(index, _)| usize::from(entry.policy) == *index)
            })
            .count();
        if board_entries == 0 {
            continue;
        }
        let complete = board_entries == rows.len() * 2
            && rows.iter().all(|&(index, [_, default, max])| {
                #[allow(clippy::cast_possible_truncation)]
                let policy = index as u8;
                profile
                    .entry(policy, FIELD_MAX)
                    .is_some_and(|entry| entry.value == max)
                    && profile
                        .entry(policy, FIELD_DEFAULT)
                        .is_some_and(|entry| entry.value == default)
            });
        if complete {
            matching.push(profile);
        } else {
            other.push(profile);
        }
    }
    if matching.is_empty() && !other.is_empty() {
        return Err(
            "the board power is set by override profiles that do not match the base table (XOC-style image)"
                .to_owned(),
        );
    }
    Ok((matching, other))
}

/// Board power values above the lower cable-capability profiles' (the ceiling may not drop
/// below them) and the LACT cap on the ceiling.
fn power_limits(rom: &[u8], context: &Context<'_>) -> Result<(u32, u32), String> {
    let rows = board_rows(rom, context)?;
    let (_, other) = board_profiles(rom, context)?;
    let [_, default, max] = rows
        .iter()
        .find(|(index, _)| *index == BOARD_POWER_ROW)
        .map(|(_, values)| *values)
        .ok_or("no board power row")?;
    let profile_floor = other
        .iter()
        .flat_map(|profile| &profile.entries)
        .filter(|entry| {
            BOARD_ROWS
                .iter()
                .any(|(index, _)| usize::from(entry.policy) == *index)
        })
        .map(|entry| entry.value)
        .max()
        .unwrap_or(0);
    Ok((
        profile_floor,
        max.max(default.saturating_mul(POWER_MAX_DEFAULT_FACTOR)),
    ))
}

/// Checks the requested board power values together: min <= default <= max on the result,
/// the minimum not below the floor, the maximum within the LACT cap and not below the
/// lower cable-capability profiles.
fn check_power_request(
    rom: &[u8],
    context: &Context<'_>,
    changes: &[(EditField, i64)],
) -> Result<(), String> {
    let requested = |field| {
        changes
            .iter()
            .find(|(f, _)| *f == field)
            .map(|(_, value)| *value)
    };
    if [
        EditField::BoardPowerMax,
        EditField::BoardPowerDefault,
        EditField::BoardPowerMin,
    ]
    .iter()
    .all(|field| requested(*field).is_none())
    {
        return Ok(());
    }
    let current = |field| describe(rom, context, field).map(|described| described.current);
    let max = requested(EditField::BoardPowerMax)
        .map_or_else(|| current(EditField::BoardPowerMax), Ok)?;
    let default = requested(EditField::BoardPowerDefault)
        .map_or_else(|| current(EditField::BoardPowerDefault), Ok)?;
    let min = requested(EditField::BoardPowerMin)
        .map_or_else(|| current(EditField::BoardPowerMin), Ok)?;
    let (profile_floor, cap) = power_limits(rom, context)?;
    let source_min = current(EditField::BoardPowerMin)?;
    let w = |mw: i64| mw / 1000;

    if min < i64::from(POWER_MIN_FLOOR_MW).min(source_min) {
        return Err(format!(
            "board power minimum {} W is below {} W",
            w(min),
            POWER_MIN_FLOOR_MW / 1000
        ));
    }
    if !(min <= default && default <= max) {
        return Err(format!(
            "board power must keep min <= default <= max (got {} / {} / {} W)",
            w(min),
            w(default),
            w(max)
        ));
    }
    if max > i64::from(cap) {
        return Err(format!(
            "board power maximum {} W is above LACT's cap of {} W",
            w(max),
            cap / 1000
        ));
    }
    if max < i64::from(profile_floor) {
        return Err(format!(
            "board power maximum {} W is below a lower cable-capability profile ({} W)",
            w(max),
            profile_floor / 1000
        ));
    }
    Ok(())
}

fn describe(rom: &[u8], context: &Context<'_>, field: EditField) -> Result<EditableField, String> {
    match field {
        EditField::BoardPowerMax | EditField::BoardPowerDefault | EditField::BoardPowerMin => {
            let rows = board_rows(rom, context)?;
            let (_, other) = board_profiles(rom, context)?;
            let [min, default, max] = rows
                .iter()
                .find(|(index, _)| *index == BOARD_POWER_ROW)
                .map(|(_, values)| *values)
                .ok_or("no board power row")?;
            let note = (!other.is_empty())
                .then(|| "lower cable-capability profiles keep their own limits".to_owned());
            let (current, low, high) = match field {
                EditField::BoardPowerMax => {
                    if rows.iter().any(|(_, [_, _, row_max])| *row_max != max) {
                        return Err(
                            "the board power rows 2, 7 and 8 have different maximums".to_owned()
                        );
                    }
                    let (profile_floor, cap) = power_limits(rom, context)?;
                    (max, profile_floor.max(default), cap)
                }
                EditField::BoardPowerDefault => (default, min, max),
                _ => (min, POWER_MIN_FLOOR_MW.min(min), default),
            };
            Ok(EditableField {
                field,
                current: current.into(),
                min: low.into(),
                max: high.into(),
                note,
            })
        }
        EditField::RelVoltageMax(rail) => {
            let rails = context.tables.rail_equations()?;
            let row = rails
                .iter()
                .find(|row| row.name == rail.name())
                .ok_or_else(|| format!("the image has no {} rail", rail.name()))?;
            let (_, selector, equation) = row
                .roles
                .iter()
                .find(|(role, ..)| *role == RailLimitRole::Rel)
                .copied()
                .ok_or("no REL selector")?;
            let selector = selector.ok_or("the rail has no REL limit")?;
            let equation = equation.ok_or("the REL equation lies outside the VFE object")?;
            // The clamp belongs to the equation: one shared with another limit would move both.
            let shared = rails
                .iter()
                .flat_map(|other| other.roles.iter().map(move |role| (other.name, role)))
                .any(|(name, &(role, other_selector, _))| {
                    other_selector == Some(selector)
                        && !(name == row.name && role == RailLimitRole::Rel)
                });
            if shared {
                return Err(format!(
                    "the REL equation {selector} is shared with another limit"
                ));
            }
            if !matches!(rom[equation], 1 | 7) {
                return Err(format!(
                    "the REL equation has an unknown kind {:#x}",
                    rom[equation]
                ));
            }
            let (low, current) = context
                .tables
                .voltage_clamp(equation)
                .ok_or("the REL equation is not a voltage output")?;
            // LACT cap: the reliability limit stays at or below the rail's overvoltage limit.
            let ov = row
                .roles
                .iter()
                .find(|(role, ..)| *role == RailLimitRole::Ov)
                .and_then(|(_, _, equation)| equation.and_then(|e| context.tables.voltage_clamp(e)))
                .map(|(_, max)| max)
                .ok_or("the rail has no readable OV limit to cap the REL limit with")?;
            #[allow(clippy::cast_possible_truncation)]
            let (low, current, high) = (
                low.round() as i64,
                current.round() as i64,
                ov.max(current).round() as i64,
            );
            Ok(EditableField {
                field,
                current,
                min: low,
                max: high,
                note: Some("capped at the rail's overvoltage (OV) limit".to_owned()),
            })
        }
        EditField::DramOffsetMax => {
            let row = dram_row(context)?;
            let min = i16_at(rom, row + CLOCK_ROW_MIN).ok_or("truncated clock range row")?;
            let max = i16_at(rom, row + CLOCK_ROW_MAX).ok_or("truncated clock range row")?;
            Ok(EditableField {
                field,
                current: max.into(),
                min: min.max(0).into(),
                max: max.max(DRAM_MAX_CAP_TABLE_MHZ).into(),
                note: Some(
                    "only raises the ceiling; the offset itself is still set at runtime".to_owned(),
                ),
            })
        }
    }
}

fn dram_row(context: &Context<'_>) -> Result<usize, String> {
    let (rows, known_layout) = context.tables.clock_rows()?;
    if !known_layout {
        return Err("the clock range table does not match the known domain layout".to_owned());
    }
    rows.get(2).copied().ok_or_else(|| "no DRAM row".to_owned())
}

fn put(data: &mut [u8], writes: &mut Vec<(usize, usize)>, at: usize, bytes: &[u8]) {
    data[at..at + bytes.len()].copy_from_slice(bytes);
    writes.push((at, bytes.len()));
}

/// Writes one field into `data`; returns the profiles updated with it.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn write_field(
    rom: &[u8],
    context: &Context<'_>,
    data: &mut [u8],
    writes: &mut Vec<(usize, usize)>,
    field: EditField,
    value: i64,
) -> Result<Vec<usize>, String> {
    match field {
        EditField::BoardPowerMax | EditField::BoardPowerDefault | EditField::BoardPowerMin => {
            let value = u32::try_from(value).map_err(|_| "power value out of range")?;
            let (matching, _) = board_profiles(rom, context)?;
            let (rows, offset, profile_field): (&[usize], usize, Option<u8>) = match field {
                EditField::BoardPowerMax => (&[2, 7, 8], POWER_ROW_MAX, Some(FIELD_MAX)),
                EditField::BoardPowerDefault => (&[8], POWER_ROW_DEFAULT, Some(FIELD_DEFAULT)),
                _ => (&[8], POWER_ROW_MIN, None),
            };
            for &index in rows {
                put(
                    data,
                    writes,
                    context.power_row(index)? + offset,
                    &value.to_le_bytes(),
                );
            }
            let mut updated = Vec::new();
            if let Some(profile_field) = profile_field {
                for profile in matching {
                    for &index in rows {
                        let entry = profile
                            .entry(index as u8, profile_field)
                            .ok_or("matching profile lost an entry")?;
                        put(data, writes, entry.offset + 5, &value.to_le_bytes());
                    }
                    updated.push(profile.index);
                }
            }
            Ok(updated)
        }
        EditField::RelVoltageMax(rail) => {
            let rails = context.tables.rail_equations()?;
            let equation = rails
                .iter()
                .find(|row| row.name == rail.name())
                .and_then(|row| {
                    row.roles
                        .iter()
                        .find(|(role, ..)| *role == RailLimitRole::Rel)
                })
                .and_then(|(_, _, equation)| *equation)
                .ok_or("no REL equation")?;
            let microvolts = value as f32;
            if f64::from(microvolts) != value as f64 {
                return Err("voltage is not exactly representable".to_owned());
            }
            put(
                data,
                writes,
                equation + EQUATION_OUTPUT_MAX,
                &microvolts.to_le_bytes(),
            );
            Ok(vec![])
        }
        EditField::DramOffsetMax => {
            let value = i16::try_from(value).map_err(|_| "memory offset out of range")?;
            put(
                data,
                writes,
                dram_row(context)? + CLOCK_ROW_MAX,
                &value.to_le_bytes(),
            );
            Ok(vec![])
        }
    }
}

/// Board row 8 must keep min <= default <= max, and every enabled profile must keep each
/// policy's default at or below its maximum.
fn check_power_consistency(data: &[u8], context: &Context<'_>) -> Result<(), String> {
    let Ok(row) = context.power_row(BOARD_POWER_ROW) else {
        return Ok(());
    };
    let min = read_u32(data, row + POWER_ROW_MIN)?;
    let default = read_u32(data, row + POWER_ROW_DEFAULT)?;
    let max = read_u32(data, row + POWER_ROW_MAX)?;
    if !(min <= default && default <= max) {
        return Err(format!(
            "board power would end up inconsistent: min {} W, default {} W, max {} W",
            min / 1000,
            default / 1000,
            max / 1000
        ));
    }
    if let Ok(profiles) = &context.profiles {
        for profile in profiles.iter().filter(|profile| profile.enabled) {
            for entry in profile
                .entries
                .iter()
                .filter(|entry| entry.field == FIELD_DEFAULT)
            {
                let read = |offset| read_u32(data, offset + 5);
                if let Some(max_entry) = profile.entry(entry.policy, FIELD_MAX)
                    && read(entry.offset)? > read(max_entry.offset)?
                {
                    return Err(format!(
                        "profile {} would have policy {}'s default above its maximum",
                        profile.index, entry.policy
                    ));
                }
            }
        }
    }
    Ok(())
}

fn checksum(data: &[u8]) -> u8 {
    data.iter().fold(0u8, |sum, b| sum.wrapping_add(*b))
}

/// Recomputes the hashes of every object a write touched and the directory digest, then
/// fixes the checksum byte (the last byte) of every data image that changed. Returns the
/// ids of the rehashed objects.
fn repair(
    data: &mut [u8],
    context: &Context<'_>,
    writes: &[(usize, usize)],
) -> Result<Vec<u32>, String> {
    let directory = &context.tables.directory;
    let chain = context.tables.chain();

    let mut touched = Vec::new();
    for &(at, len) in writes {
        let owners = directory
            .objects
            .iter()
            .filter(|object| object.data < at + len && at < object.data + object.size)
            .collect::<Vec<_>>();
        if owners.is_empty() {
            return Err(format!("write at {at:#x} is outside every hashed object"));
        }
        for owner in owners {
            if !touched
                .iter()
                .any(|o: &&crate::gb20x::Object| std::ptr::eq(*o, owner))
            {
                touched.push(owner);
            }
        }
    }

    // Repair targets must not be covered by an object themselves, or a later hash would
    // invalidate an earlier one.
    let checksum_bytes = chain
        .data
        .iter()
        .map(|image| image.end() - 1)
        .collect::<Vec<_>>();
    let overlaps = |at: usize, len: usize, start: usize, end: usize| start < at + len && at < end;
    let covered = |at: usize, len: usize| {
        directory
            .objects
            .iter()
            .any(|object| overlaps(at, len, object.data, object.data + object.size))
    };
    // Hash slots live in the directory; the digest and checksum bytes must lie outside it.
    for object in &touched {
        if covered(object.hash_offset, 48) {
            return Err(format!(
                "hash slot {:#x} overlaps hashed data",
                object.hash_offset
            ));
        }
    }
    for (at, len) in [(directory.digest_offset, 48)]
        .into_iter()
        .chain(checksum_bytes.iter().map(|&at| (at, 1)))
    {
        if covered(at, len) || overlaps(at, len, directory.header, directory.data_base) {
            return Err(format!("repair target {at:#x} overlaps hashed data"));
        }
    }
    if checksum_bytes
        .iter()
        .any(|&at| overlaps(at, 1, directory.digest_offset, directory.digest_offset + 48))
    {
        return Err("a checksum byte overlaps the directory digest".to_owned());
    }

    for object in &touched {
        let digest = Sha384::digest(&data[object.data..object.data + object.size]);
        data[object.hash_offset..object.hash_offset + 48].copy_from_slice(&digest);
    }
    let digest = Sha384::digest(&data[directory.header..directory.data_base]);
    data[directory.digest_offset..directory.digest_offset + 48].copy_from_slice(&digest);

    for image in &chain.data {
        let last = image.end() - 1;
        data[last] = data[last].wrapping_sub(checksum(&data[image.start..image.end()]));
    }
    if [&chain.legacy].into_iter().chain(&chain.efi).any(|image| {
        writes
            .iter()
            .any(|&(at, len)| at < image.end() && image.start < at + len)
    }) {
        return Err("a write landed in the legacy or EFI image".to_owned());
    }

    let mut ids = touched.iter().map(|object| object.id).collect::<Vec<_>>();
    ids.sort_unstable();
    Ok(ids)
}

/// Parses the edited image again: its hashes and checksums must be consistent, every
/// requested field must read back as requested, and every changed byte must be accounted
/// for by a declared write or a repair.
fn verify(rom: &[u8], data: &[u8], changes: &[(EditField, i64)]) -> Result<(), VbiosError> {
    let report = crate::parse(data)?;
    if !report
        .integrity
        .as_ref()
        .is_some_and(crate::Integrity::consistent)
    {
        return Err(VbiosError(
            "the edited image does not verify; nothing was produced".to_owned(),
        ));
    }

    let fields = editable_fields(data)?;
    for &(field, value) in changes {
        let current = fields
            .fields
            .iter()
            .find(|described| described.field == field)
            .map(|described| described.current);
        if current != Some(value) {
            return Err(VbiosError(format!(
                "{} did not read back as {value}",
                field.label()
            )));
        }
    }

    // Allowed byte changes: the declared fields, hash slots, the digest, checksum bytes.
    // Recompute them from the source's layout and compare.
    let allowed = with_context(rom, |rom, context| {
        let mut writes = Vec::new();
        let mut scratch = rom.to_vec();
        for &(field, value) in changes {
            write_field(rom, context, &mut scratch, &mut writes, field, value)?;
        }
        let mut allowed = writes
            .iter()
            .flat_map(|&(at, len)| at..at + len)
            .collect::<BTreeSet<_>>();
        let directory = &context.tables.directory;
        for object in &directory.objects {
            allowed.extend(object.hash_offset..object.hash_offset + 48);
        }
        allowed.extend(directory.digest_offset..directory.digest_offset + 48);
        allowed.extend(
            context
                .tables
                .chain()
                .data
                .iter()
                .map(|image| image.end() - 1),
        );
        Ok(allowed)
    })?;
    if let Some(at) = (0..rom.len()).find(|&at| rom[at] != data[at] && !allowed.contains(&at)) {
        return Err(VbiosError(format!(
            "undeclared byte change at {at:#x}; nothing was produced"
        )));
    }
    if rom.len() != data.len() {
        return Err(VbiosError("the edited image changed size".to_owned()));
    }
    Ok(())
}
