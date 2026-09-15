//! Power caps below the vBIOS minimum through the ordinary client of the RM
//! client power-policy group.
//!
//! NVML (and the `PWR_POLICIES` SET this fork uses for the OCP rails) refuse
//! any board power limit under the vBIOS minimum. Panchovix found that the
//! *client* policy group — `0x2080a630` GET_INFO (0x924; min / default / max
//! mW at +0x28 / +0x2c / +0x30), `0x2080a632` GET_CONTROL (0x328; mask 1 at
//! +4, entry 0 request mW at +0x2c, client selector byte at +0x30) and
//! `0x2080e633` SET_CONTROL — accepts requests below that minimum for the
//! ordinary client 0xFE, and the board then reports and holds the lower
//! value (100 W on a 250 W-minimum PRO 6000; a 30 W request settled at
//! ~74 W). It is a lower route only: requests above the maximum are accepted
//! but not honoured. The additional client 0xF8 is never written.
//!
//! Ported from Panchovix/LACT commit 62f735f (2026-09-14). His gate was the
//! exact driver 610.57.04; the layout was read back byte-identical on
//! 615.71.09 here, so this fork gates on the verified branch list and on the
//! cross-checks below (RM bounds and current request must agree with NVML).

use super::DriverHandle;
use anyhow::{Context, anyhow, ensure};

const GET_INFO: u32 = 0x2080_a630;
const GET_CONTROL: u32 = 0x2080_a632;
const SET_CONTROL: u32 = 0x2080_e633;
const INFO_SIZE: usize = 0x924;
const CONTROL_SIZE: usize = 0x328;
const REQUEST_AT: usize = 0x2c;
const CLIENT_AT: usize = 0x30;
const ORDINARY_CLIENT: u8 = 0xfe;
/// End of the client mask that follows the header: with only client 0
/// requested, the mask words after the first must read back zero.
const MASK_END: usize = 0x24;
/// The lowest request this fork will make, mW.
const LOWER_LIMIT_MW: u32 = 30_000;
/// Driver branches the layout was verified on (see the module docs).
const VERIFIED_BRANCHES: &[&str] = &["610", "615"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_field_names)]
pub struct PowerLimitBounds {
    pub min_mw: u32,
    pub default_mw: u32,
    pub max_mw: u32,
}

impl PowerLimitBounds {
    pub fn lower_min_mw(self) -> u32 {
        self.min_mw.min(LOWER_LIMIT_MW)
    }
}

impl DriverHandle {
    /// Advertise the private route only when its layout, units and GPU
    /// identity agree with NVML. Discovery issues no SET.
    pub fn probe_lower_power_limit(
        &self,
        driver_version: &str,
        nvml_bounds: PowerLimitBounds,
        nvml_current_mw: u32,
    ) -> anyhow::Result<Option<PowerLimitBounds>> {
        probe(driver_version, nvml_bounds, nvml_current_mw, |cmd, data| unsafe {
            self.query_rm_control_bytes(cmd, data)
        })
    }

    pub fn set_lower_power_limit(&self, limit_mw: u32, bounds: PowerLimitBounds) -> anyhow::Result<()> {
        set_limit(limit_mw, bounds, |cmd, data| unsafe {
            self.query_rm_control_bytes(cmd, data)
        })
    }
}

fn probe(
    driver_version: &str,
    nvml_bounds: PowerLimitBounds,
    nvml_current_mw: u32,
    mut query: impl FnMut(u32, &mut [u8]) -> anyhow::Result<()>,
) -> anyhow::Result<Option<PowerLimitBounds>> {
    let branch = driver_version.split('.').next().unwrap_or_default();
    if !VERIFIED_BRANCHES.contains(&branch) || !cfg!(target_endian = "little") {
        return Ok(None);
    }
    let bounds = read_bounds(&mut query)?;
    ensure!(bounds == nvml_bounds, "RM power bounds differ from NVML");
    let control = read_control(&mut query)?;
    ensure!(
        read_u32(&control, REQUEST_AT) == nvml_current_mw,
        "RM ordinary power request differs from NVML"
    );
    Ok(Some(bounds))
}

fn read_bounds(query: &mut impl FnMut(u32, &mut [u8]) -> anyhow::Result<()>) -> anyhow::Result<PowerLimitBounds> {
    let mut info = [0; INFO_SIZE];
    query(GET_INFO, &mut info)?;
    validate_header(&info)?;
    let bounds = PowerLimitBounds {
        min_mw: read_u32(&info, 0x28),
        default_mw: read_u32(&info, 0x2c),
        max_mw: read_u32(&info, 0x30),
    };
    ensure!(
        bounds.min_mw > 0 && bounds.min_mw <= bounds.default_mw && bounds.default_mw <= bounds.max_mw,
        "Invalid RM power limit bounds"
    );
    Ok(bounds)
}

fn read_control(query: &mut impl FnMut(u32, &mut [u8]) -> anyhow::Result<()>) -> anyhow::Result<[u8; CONTROL_SIZE]> {
    let mut control = [0; CONTROL_SIZE];
    control[4..8].copy_from_slice(&1u32.to_le_bytes());
    control[CLIENT_AT] = ORDINARY_CLIENT;
    query(GET_CONTROL, &mut control)?;
    validate_header(&control)?;
    ensure!(control[CLIENT_AT] == ORDINARY_CLIENT, "Unexpected power client");
    ensure!(
        !matches!(read_u32(&control, REQUEST_AT), 0 | u32::MAX),
        "No ordinary power request available"
    );
    Ok(control)
}

fn validate_header(data: &[u8]) -> anyhow::Result<()> {
    // The extended layout carries the requested-client mask in the words
    // after +4; anything set beyond the first word means either a different
    // layout or a request for more than the ordinary client (Panchovix
    // cf5ecbe, 2026-09-15).
    ensure!(
        read_u32(data, 0) == 0xff && read_u32(data, 4) == 1 && data[8..MASK_END].iter().all(|byte| *byte == 0),
        "Unrecognized RM power client layout"
    );
    Ok(())
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap())
}

fn set_limit(
    limit_mw: u32,
    expected_bounds: PowerLimitBounds,
    mut query: impl FnMut(u32, &mut [u8]) -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let bounds = read_bounds(&mut query)?;
    ensure!(bounds == expected_bounds, "RM power bounds changed since discovery");
    ensure!(
        (bounds.lower_min_mw()..=bounds.max_mw).contains(&limit_mw),
        "Power limit is outside the supported range"
    );

    let before = read_control(&mut query)?;
    if read_u32(&before, REQUEST_AT) == limit_mw {
        return Ok(());
    }
    // Keep the whole payload and change only entry 0's request; mask 1 and
    // selector FE keep every other entry and the F8 client untouched.
    let mut expected = before;
    expected[REQUEST_AT..REQUEST_AT + 4].copy_from_slice(&limit_mw.to_le_bytes());
    let applied = (|| -> anyhow::Result<()> {
        let mut request = expected;
        query(SET_CONTROL, &mut request).context("Could not set ordinary power request")?;
        ensure!(read_control(&mut query)? == expected, "Power request readback differs");
        Ok(())
    })();

    if let Err(apply_error) = applied {
        // A failed SET can have side effects; restore through FE so a previous
        // below-minimum request can be put back too.
        let restored = (|| -> anyhow::Result<()> {
            let mut restore = before;
            query(SET_CONTROL, &mut restore)?;
            ensure!(read_control(&mut query)? == before, "Restored power request differs");
            Ok(())
        })();
        return match restored {
            Ok(()) => Err(apply_error.context("Previous power request restored")),
            Err(restore_error) => Err(anyhow!(
                "Power request failed: {apply_error:#}; restoration also failed: {restore_error:#}"
            )),
        };
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOUNDS: PowerLimitBounds = PowerLimitBounds {
        min_mw: 250_000,
        default_mw: 300_000,
        max_mw: 325_000,
    };

    struct FakeRm {
        control: [u8; CONTROL_SIZE],
        writes: Vec<Vec<u8>>,
        fail_first_write: bool,
        fail_readback: bool,
        fail_restore: bool,
    }

    impl FakeRm {
        fn new(current: u32) -> Self {
            let mut control = [0; CONTROL_SIZE];
            control[..8].copy_from_slice(&[0xff, 0, 0, 0, 1, 0, 0, 0]);
            control[0x28..0x2c].copy_from_slice(&[0x67, 0x67, 0, 0]);
            control[0x2c..0x30].copy_from_slice(&current.to_le_bytes());
            control[0x30] = 0xfe;
            Self {
                control,
                writes: Vec::new(),
                fail_first_write: false,
                fail_readback: false,
                fail_restore: false,
            }
        }

        fn query(&mut self, cmd: u32, data: &mut [u8]) -> anyhow::Result<()> {
            match cmd {
                GET_INFO => {
                    assert_eq!(data.len(), INFO_SIZE);
                    data[..8].copy_from_slice(&[0xff, 0, 0, 0, 1, 0, 0, 0]);
                    for (offset, value) in [(0x28, 250_000u32), (0x2c, 300_000), (0x30, 325_000)] {
                        data[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
                    }
                }
                GET_CONTROL => {
                    assert_eq!(data[0x30], 0xfe);
                    if self.fail_readback && self.writes.len() == 1 {
                        anyhow::bail!("readback unavailable");
                    }
                    data.copy_from_slice(&self.control);
                }
                SET_CONTROL => {
                    assert_eq!(data.len(), CONTROL_SIZE);
                    assert_eq!(&data[4..8], &[1, 0, 0, 0]);
                    assert_eq!(data[0x30], 0xfe);
                    assert!(
                        data.iter()
                            .zip(self.control)
                            .enumerate()
                            .all(|(i, (a, b))| (0x2c..0x30).contains(&i) || *a == b)
                    );
                    self.writes.push(data.to_vec());
                    if self.fail_restore && self.writes.len() > 1 {
                        anyhow::bail!("restore unavailable");
                    }
                    self.control.copy_from_slice(data);
                    if self.fail_first_write && self.writes.len() == 1 {
                        anyhow::bail!("SET failed after modifying hardware");
                    }
                }
                _ => panic!("unexpected command {cmd:x}"),
            }
            Ok(())
        }
    }

    #[test]
    fn unknown_branch_does_not_probe_and_nvml_mismatch_rejects_support() {
        assert_eq!(
            probe("596.49.00", BOUNDS, 250_000, |_, _| panic!("unverified branch")).unwrap(),
            None
        );
        let mut rm = FakeRm::new(250_000);
        assert!(probe("615.71.09", BOUNDS, 300_000, |c, d| rm.query(c, d)).is_err());
        assert!(rm.writes.is_empty());
    }

    #[test]
    fn extra_mask_bits_reject_the_layout_before_any_write() {
        let mut rm = FakeRm::new(250_000);
        rm.control[0x20] = 1;
        assert!(probe("615.71.09", BOUNDS, 250_000, |c, d| rm.query(c, d)).is_err());
        assert!(set_limit(200_000, BOUNDS, |c, d| rm.query(c, d)).is_err());
        assert!(rm.writes.is_empty());
    }

    #[test]
    fn changes_only_fe_request_and_keeps_vbios_maximum() {
        let mut rm = FakeRm::new(250_000);
        let bounds = probe("615.71.09", BOUNDS, 250_000, |c, d| rm.query(c, d))
            .unwrap()
            .unwrap();
        for cap in [150_000, 30_000, 250_000] {
            set_limit(cap, bounds, |c, d| rm.query(c, d)).unwrap();
            assert_eq!(read_u32(&rm.control, 0x2c), cap);
        }
        let writes = rm.writes.len();
        for cap in [0, 29_999, 325_001, 350_000, u32::MAX] {
            assert!(set_limit(cap, bounds, |c, d| rm.query(c, d)).is_err());
        }
        assert_eq!(rm.writes.len(), writes);
    }

    #[test]
    fn restores_previous_below_minimum_request_after_set_or_readback_failure() {
        for fail_set in [false, true] {
            let mut rm = FakeRm::new(100_000);
            let original = rm.control;
            rm.fail_first_write = fail_set;
            rm.fail_readback = !fail_set;
            assert!(set_limit(150_000, BOUNDS, |c, d| rm.query(c, d)).is_err());
            assert_eq!(rm.writes.len(), 2);
            assert_eq!(rm.control, original);
        }
    }

    #[test]
    fn reports_restore_failure_and_rejects_wrong_client_before_writing() {
        let mut rm = FakeRm::new(100_000);
        rm.fail_first_write = true;
        rm.fail_restore = true;
        let err = set_limit(150_000, BOUNDS, |c, d| rm.query(c, d)).unwrap_err();
        assert!(err.to_string().contains("restoration also failed"));
        let mut rm = FakeRm::new(250_000);
        rm.control[0x30] = 0xf8;
        assert!(set_limit(150_000, BOUNDS, |c, d| rm.query(c, d)).is_err());
        assert!(rm.writes.is_empty());
    }
}
