//! The ROM images are not redistributable, so these tests read them from the directory in
//! `LACT_VBIOS_TEST_DIR` and are skipped when it is unset.

use crate::*;
use std::{fs, path::PathBuf};

fn rom(name: &str) -> Option<Vec<u8>> {
    let dir = PathBuf::from(std::env::var_os("LACT_VBIOS_TEST_DIR")?);
    Some(fs::read(dir.join(name)).unwrap_or_else(|err| panic!("{name}: {err}")))
}

#[test]
fn garbage_is_rejected() {
    assert!(parse(&[0u8; 4096]).is_err());
    assert!(parse(&[]).is_err());
}

#[test]
fn matrix_full_dump() {
    let Some(data) = rom("Asus.RTX5090.32768.251013.rom") else {
        return;
    };
    let report = parse(&data).unwrap();

    assert_eq!(report.version.as_deref(), Some("98.02.2E.80.C9"));
    assert_eq!(report.device_id, 0x2b85);
    assert_eq!(report.family, VbiosFamily::Blackwell);
    assert_eq!(report.completeness, ImageCompleteness::Full);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert!(
        report.strings.iter().any(|s| s.contains("MATRIX")),
        "{:?}",
        report.strings
    );

    let integrity = report.integrity.as_ref().unwrap();
    assert!(integrity.directory_digest_valid);
    assert!(integrity.image_checksums_valid);

    let board = report.power_policy(PowerPolicyRole::BoardPower).unwrap();
    assert_eq!(
        (board.min, board.default, board.max),
        (400_000, 800_000, 800_000)
    );
    assert_eq!(
        report
            .power_policy(PowerPolicyRole::InputCurrent12V)
            .unwrap()
            .default,
        80_000
    );
    assert_eq!(
        report
            .power_policy(PowerPolicyRole::NvvddOutputCurrent)
            .unwrap()
            .default,
        480_000
    );
    assert_eq!(
        report
            .power_policy(PowerPolicyRole::MsvddOutputCurrent)
            .unwrap()
            .default,
        180_000
    );

    let dram = report.clock_range("DRAM").unwrap();
    assert_eq!((dram.min_mhz, dram.max_mhz), (-1000, 3000));
    let gpc = report.clock_range("GPC").unwrap();
    assert_eq!((gpc.min_mhz, gpc.max_mhz), (-1000, 1000));

    assert_eq!(report.rails.len(), 2);
    let nvvdd = &report.rails[0];
    assert_eq!(nvvdd.rail, "NVVDD");
    let rel = nvvdd
        .limits
        .iter()
        .find(|l| l.role == RailLimitRole::Rel)
        .unwrap();
    assert_eq!(rel.max_uv, Some(1_110_000.0));
    let ov = nvvdd
        .limits
        .iter()
        .find(|l| l.role == RailLimitRole::Ov)
        .unwrap();
    assert_eq!(ov.max_uv, Some(1_200_000.0));
}

#[test]
fn astral_xoc_dump() {
    let Some(data) = rom("281814.rom") else {
        return;
    };
    let report = parse(&data).unwrap();
    assert_eq!(report.version.as_deref(), Some("98.02.2E.80.50"));
    let board = report.power_policy(PowerPolicyRole::BoardPower).unwrap();
    assert_eq!(board.max, 2_001_000);
    assert_eq!(report.clock_range("GPC").unwrap().max_mhz, 8000);
    assert_eq!(report.clock_range("DRAM").unwrap().max_mhz, 5000);
    let rel = report.rails[0]
        .limits
        .iter()
        .find(|l| l.role == RailLimitRole::Rel)
        .unwrap();
    assert_eq!(rel.max_uv, Some(1_200_000.0));
}

#[test]
fn edited_image_keeps_consistent_hashes() {
    let Some(data) = rom("matrix_dram5000_tgp600.rom") else {
        return;
    };
    let report = parse(&data).unwrap();
    assert_eq!(report.clock_range("DRAM").unwrap().max_mhz, 5000);
    assert_eq!(
        report
            .power_policy(PowerPolicyRole::BoardPower)
            .unwrap()
            .default,
        600_000
    );
    assert!(
        report
            .integrity
            .as_ref()
            .unwrap()
            .bad_object_ids
            .iter()
            .all(|id| *id == 0xfb || *id == 0xbe)
    );
}

#[test]
fn stale_hash_is_reported() {
    let Some(data) = rom("matrix_dram5000_stalehash.rom") else {
        return;
    };
    let report = parse(&data).unwrap();
    assert!(
        report
            .integrity
            .unwrap()
            .bad_object_ids
            .contains(&gb20x::OBJ_CLOCK_RANGES)
    );
}

#[test]
fn sysfs_slice() {
    let (Some(slice), Some(full)) = (
        rom("matrix_sysfs.rom"),
        rom("Asus.RTX5090.32768.251013.rom"),
    ) else {
        return;
    };
    let report = parse(&slice).unwrap();
    assert_eq!(report.completeness, ImageCompleteness::PciSlice);
    assert_eq!(report.version.as_deref(), Some("98.02.2E.80.C9"));
    assert!(report.power_policies.is_empty());

    assert_eq!(same_pci_images(&full, &slice), Some(true));
    assert_eq!(pci_slice(&full), Some(&slice[..]));
    let xoc = rom("281814.rom").unwrap();
    assert_eq!(same_pci_images(&xoc, &slice), Some(false));
}

mod edit {
    use super::rom;
    use crate::edit::{EditField, Rail, edit, editable_fields};
    use crate::*;

    const MATRIX: &str = "Asus.RTX5090.32768.251013.rom";

    fn changed_ranges(a: &[u8], b: &[u8]) -> Vec<(usize, usize)> {
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for i in (0..a.len()).filter(|&i| a[i] != b[i]) {
            match ranges.last_mut() {
                Some((start, len)) if *start + *len == i => *len += 1,
                _ => ranges.push((i, 1)),
            }
        }
        ranges
    }

    #[test]
    fn matrix_fields() {
        let Some(data) = rom(MATRIX) else { return };
        let fields = editable_fields(&data).unwrap();
        assert!(fields.unavailable.is_empty(), "{:?}", fields.unavailable);
        let get = |field| fields.fields.iter().find(|f| f.field == field).unwrap();
        let max = get(EditField::BoardPowerMax);
        // Floor: the 600 W cable profile; cap: twice the 800 W default.
        assert_eq!(
            (max.current, max.min, max.max),
            (800_000, 800_000, 1_600_000)
        );
        let rel = get(EditField::RelVoltageMax(Rail::Nvvdd));
        assert_eq!(
            (rel.current, rel.min, rel.max),
            (1_110_000, 900_000, 1_200_000)
        );
        let dram = get(EditField::DramOffsetMax);
        assert_eq!((dram.current, dram.max), (3000, 5000));
    }

    /// The same bytes as the hand-built image that was booted through the override, except
    /// that the directory digest it left stale is now valid.
    #[test]
    fn dram_matches_known_good_image() {
        let (Some(data), Some(known)) = (rom(MATRIX), rom("matrix_dram5000.rom")) else {
            return;
        };
        let result = edit(&data, &[(EditField::DramOffsetMax, 5000)]).unwrap();
        let ranges = changed_ranges(&known, &result.data);
        // Only the 48-byte directory digest differs (one byte of it happens to match).
        assert!(
            ranges.iter().all(
                |&(start, len)| (0x5fc09..0x5fc39).contains(&start) && len <= 48
                    || start == 0x6f3ff
            ),
            "{ranges:x?}"
        );
        assert_eq!(
            result.summary.rehashed_objects,
            vec![gb20x::OBJ_CLOCK_RANGES]
        );
        let report = parse(&result.data).unwrap();
        assert!(report.integrity.as_ref().unwrap().consistent());
        assert_eq!(report.clock_range("DRAM").unwrap().max_mhz, 5000);
    }

    #[test]
    fn power_edits_keep_the_matching_profile_in_step() {
        let Some(data) = rom(MATRIX) else { return };
        let result = edit(
            &data,
            &[
                (EditField::BoardPowerMax, 900_000),
                (EditField::BoardPowerDefault, 850_000),
            ],
        )
        .unwrap();
        assert_eq!(result.summary.applied[0].profiles, vec![7]);
        let report = parse(&result.data).unwrap();
        let rows = &report.power_policies;
        assert_eq!((rows[8].default, rows[8].max), (850_000, 900_000));
        assert_eq!(rows[2].max, 900_000);
        assert_eq!(rows[7].max, 900_000);
        // Rows 2 and 7 keep their default; only the board row's default moves.
        assert_eq!(rows[2].default, 800_000);
        let fields = editable_fields(&result.data).unwrap();
        assert!(fields.unavailable.is_empty(), "{:?}", fields.unavailable);
    }

    #[test]
    fn rel_limit() {
        let Some(data) = rom(MATRIX) else { return };
        let result = edit(&data, &[(EditField::RelVoltageMax(Rail::Msvdd), 1_150_000)]).unwrap();
        let report = parse(&result.data).unwrap();
        let rel = |rail: usize| {
            report.rails[rail]
                .limits
                .iter()
                .find(|l| l.role == RailLimitRole::Rel)
                .unwrap()
                .max_uv
        };
        assert_eq!(rel(1), Some(1_150_000.0));
        assert_eq!(rel(0), Some(1_110_000.0));
    }

    #[test]
    fn out_of_bounds_and_inconsistent_requests_are_refused() {
        let Some(data) = rom(MATRIX) else { return };
        assert!(edit(&data, &[(EditField::RelVoltageMax(Rail::Nvvdd), 1_300_000)]).is_err());
        assert!(edit(&data, &[(EditField::DramOffsetMax, 6000)]).is_err());
        assert!(edit(&data, &[(EditField::BoardPowerMax, 1_700_000)]).is_err());
        // Below the 600 W cable profile.
        assert!(
            edit(
                &data,
                &[
                    (EditField::BoardPowerMax, 500_000),
                    (EditField::BoardPowerDefault, 500_000)
                ]
            )
            .is_err()
        );
        assert!(edit(&data, &[(EditField::BoardPowerDefault, 900_000)]).is_err());
        assert!(edit(&data, &[(EditField::BoardPowerMin, 10_000)]).is_err());
        assert!(
            edit(
                &data,
                &[
                    (EditField::DramOffsetMax, 4000),
                    (EditField::DramOffsetMax, 4500)
                ]
            )
            .is_err()
        );
    }

    #[test]
    fn xoc_power_is_left_to_a_dedicated_editor() {
        let Some(data) = rom("281814.rom") else {
            return;
        };
        let fields = editable_fields(&data).unwrap();
        // Its board power comes from a profile that does not match the 2001 W base rows,
        // and its NVVDD REL equation is shared with ALT and OV.
        let unavailable = fields
            .unavailable
            .iter()
            .map(|u| u.field)
            .collect::<Vec<_>>();
        assert!(unavailable.contains(&EditField::BoardPowerMax));
        assert!(unavailable.contains(&EditField::RelVoltageMax(Rail::Nvvdd)));
        assert!(!unavailable.contains(&EditField::RelVoltageMax(Rail::Msvdd)));
        assert!(edit(&data, &[(EditField::BoardPowerMax, 1_000_000)]).is_err());
    }

    #[test]
    fn stale_source_is_refused() {
        let Some(data) = rom("matrix_dram5000_stalehash.rom") else {
            return;
        };
        assert!(edit(&data, &[(EditField::DramOffsetMax, 4000)]).is_err());
    }

    #[test]
    fn stale_directory_digest_is_repaired() {
        let Some(data) = rom("matrix_dram5000.rom") else {
            return;
        };
        let result = edit(&data, &[(EditField::BoardPowerDefault, 600_000)]).unwrap();
        assert!(result.summary.directory_digest_repaired);
        assert!(parse(&result.data).unwrap().integrity.unwrap().consistent());
    }

    #[test]
    fn slice_is_refused() {
        let Some(data) = rom("matrix_sysfs.rom") else {
            return;
        };
        assert!(editable_fields(&data).is_err());
    }
}
