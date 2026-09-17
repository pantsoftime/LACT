//! Thermal sensors and their simulation ("thermal inputs", mVolt+ v0.44).
//!
//! The 24-object sensor group of the private THERMAL range — GET_INFO
//! `0x2080853a`, GET_STATUS `0x2080853b`, GET_CONTROL `0x2080853c`,
//! SET_CONTROL `0x2080c53d` — is what NvAPI's thermal-reading entry point
//! (0x65FE3AAD) reads. Each object is one sensor: on GB202 object 1 is the
//! GPU die sensor NVML reports, object 2 a computed value about 10 °C above
//! it, and fifteen type-3 objects are the GDDR7 chips, which report nothing
//! on this driver.
//!
//! The 12-byte control record of an object has a *simulation* pair: byte +1
//! enables it and the u32 at +4 is the simulated NvTemp (24.8 fixed). With
//! it set the sensor reports the simulated value, NVML follows, and the VFE's
//! temperature term takes it — verified on 615.71.09 by pinning the GPU
//! sensor to 60 °C: the SM clock at idle fell from 3277 to ~1600 MHz. Both
//! the fan policies and the thermal-limit policies may read the same
//! channel, so a *low* simulated value can blind the card's own protection:
//! that is why the controller floors the value, refuses it alongside LACT's
//! fan control, clears it on every reset, and watches the sensors that did
//! not follow the simulation.
//!
//! Layout (E255 board-object group; verified by the absent index 3 reading
//! as zeros in every block and by tRC-style identities elsewhere in the
//! sweep): INFO header 0x34 then 0x3c per entry (byte 0 = type, byte 1 =
//! sensor id); STATUS header 0x28 then 0x38 per entry (NvTemp at +4,
//! `0xff00` = no reading); CONTROL / SET header 0x28 (type word, eight mask
//! words at +4, trailer) then 0xc per entry. The SET selects objects through
//! the mask words.
#![allow(clippy::doc_markdown)]

use anyhow::{Context, bail, ensure};
use tracing::{debug, info};

use super::driver::DriverHandle;

const GET_INFO: u32 = 0x2080_853a;
const GET_STATUS: u32 = 0x2080_853b;
const GET_CONTROL: u32 = 0x2080_853c;
const SET_CONTROL: u32 = 0x2080_c53d;

const INFO_SIZE: usize = 0x3bf8;
const STATUS_SIZE: usize = 0x37f0;
const CONTROL_SIZE: usize = 0xc1c;
const GROUP_TYPE: u32 = 2;
const MASK_AT: usize = 0x04;
const MASK_WORDS: usize = 8;
const MAX_OBJECTS: usize = 255;
const INFO_HDR: usize = 0x34;
const INFO_STRIDE: usize = 0x3c;
const IE_TYPE: usize = 0;
const IE_ID: usize = 1;
const STATUS_HDR: usize = 0x28;
const STATUS_STRIDE: usize = 0x38;
const SE_TEMP: usize = 0x04;
const CONTROL_HDR: usize = 0x28;
const CONTROL_STRIDE: usize = 0x0c;
const CE_SIM_ENABLE: usize = 0x01;
const CE_SIM_TEMP: usize = 0x04;
/// STATUS reports this (255.0 °C) when a sensor has no reading.
const NO_READING: u32 = 0xff00;
const GPU_SENSOR_TYPE: u8 = 2;
const GPU_SENSOR_ID: u8 = 0;
const COMPUTED_SENSOR_TYPE: u8 = 4;
const MEMORY_SENSOR_TYPE: u8 = 3;

/// Simulated values the daemon will write, °C. The floor keeps the fan and
/// thermal-limit policies from being told the die is cold.
pub const SIM_MIN_C: i32 = 20;
pub const SIM_MAX_C: i32 = 110;

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

fn nvtemp_to_c(v: u32) -> Option<f32> {
    // 24.8 fixed point; the no-reading sentinel and anything absurd are None.
    if v == NO_READING || v > 200 * 256 {
        None
    } else {
        #[allow(clippy::cast_precision_loss)]
        Some(v as f32 / 256.0)
    }
}

fn c_to_nvtemp(c: i32) -> u32 {
    #[allow(clippy::cast_sign_loss)]
    let v = (c.max(0) as u32) << 8;
    v
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SensorInfo {
    pub index: u8,
    pub kind: u8,
    pub id: u8,
}

impl SensorInfo {
    pub fn name(self) -> String {
        match (self.kind, self.id) {
            (GPU_SENSOR_TYPE, GPU_SENSOR_ID) => "GPU".to_owned(),
            (COMPUTED_SENSOR_TYPE, _) => "GPU (computed)".to_owned(),
            (MEMORY_SENSOR_TYPE, id) => format!("Memory chip {id:#04x}"),
            (kind, id) => format!("Sensor {} (type {kind}, id {id:#04x})", self.index),
        }
    }

    /// The sensor the driver, NVML and the fan policies treat as *the* GPU
    /// temperature.
    pub fn is_gpu(self) -> bool {
        self.kind == GPU_SENSOR_TYPE && self.id == GPU_SENSOR_ID
    }
}

/// One sensor's live state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SensorReading {
    pub index: u8,
    pub temp_c: Option<f32>,
    pub sim_enabled: bool,
    pub sim_c: Option<f32>,
}

pub struct ThermSensors {
    mask: [u32; MASK_WORDS],
    sensors: Vec<SensorInfo>,
    /// The control block found at daemon start: every restore targets it.
    start_control: Box<Bytes<CONTROL_SIZE>>,
}

fn mask_has(mask: &[u32; MASK_WORDS], index: usize) -> bool {
    index < MAX_OBJECTS && mask[index / 32] & (1 << (index % 32)) != 0
}

fn control_entry(block: &[u8], index: u8) -> &[u8] {
    let at = CONTROL_HDR + usize::from(index) * CONTROL_STRIDE;
    &block[at..at + CONTROL_STRIDE]
}

impl ThermSensors {
    pub fn probe(handle: &DriverHandle) -> anyhow::Result<Self> {
        let info = rm_call::<INFO_SIZE>(handle, GET_INFO, |_| {}).context("THERM sensors GET_INFO")?;
        ensure!(rd_u32(&info.0, 0) == GROUP_TYPE, "sensor group INFO has type {}", rd_u32(&info.0, 0));
        let mut mask = [0u32; MASK_WORDS];
        for (w, slot) in mask.iter_mut().enumerate() {
            *slot = rd_u32(&info.0, MASK_AT + 4 * w);
        }
        ensure!(mask.iter().any(|w| *w != 0), "sensor group is empty");
        let seed = |b: &mut [u8]| b[..0x40].copy_from_slice(&info.0[..0x40]);
        let status = rm_call::<STATUS_SIZE>(handle, GET_STATUS, |b| seed(b)).context("THERM sensors GET_STATUS")?;
        let control = rm_call::<CONTROL_SIZE>(handle, GET_CONTROL, |b| seed(b)).context("THERM sensors GET_CONTROL")?;
        for (name, block) in [("STATUS", &status.0[..]), ("CONTROL", &control.0[..])] {
            ensure!(rd_u32(block, 0) == GROUP_TYPE, "sensor group {name} has type {}", rd_u32(block, 0));
            ensure!(
                (0..MASK_WORDS).all(|w| rd_u32(block, MASK_AT + 4 * w) == mask[w]),
                "sensor group {name} mask differs from INFO"
            );
        }

        let mut sensors = Vec::new();
        let mut readable = 0;
        for index in 0..MAX_OBJECTS {
            if !mask_has(&mask, index) {
                continue;
            }
            let ie = INFO_HDR + index * INFO_STRIDE;
            let se = STATUS_HDR + index * STATUS_STRIDE;
            let ce = CONTROL_HDR + index * CONTROL_STRIDE;
            ensure!(
                ie + INFO_STRIDE <= INFO_SIZE && se + STATUS_STRIDE <= STATUS_SIZE && ce + CONTROL_STRIDE <= CONTROL_SIZE,
                "sensor {index} lies outside the blocks"
            );
            let enable = control.0[ce + CE_SIM_ENABLE];
            ensure!(enable <= 1, "sensor {index} control byte +1 is {enable}, not a flag");
            if nvtemp_to_c(rd_u32(&status.0, se + SE_TEMP)).is_some() {
                readable += 1;
            }
            #[allow(clippy::cast_possible_truncation)]
            sensors.push(SensorInfo {
                index: index as u8,
                kind: info.0[ie + IE_TYPE],
                id: info.0[ie + IE_ID],
            });
        }
        ensure!(readable > 0, "no sensor in the group reports a temperature");
        ensure!(
            sensors.iter().copied().any(SensorInfo::is_gpu),
            "the GPU sensor (type 2, id 0) is not in the group"
        );
        // An absent index must read as zeros in every block: the layout check.
        if let Some(absent) = (0..MAX_OBJECTS).find(|i| !mask_has(&mask, *i)) {
            let zero = |b: &[u8], at: usize, n: usize| b[at..at + n].iter().all(|x| *x == 0);
            ensure!(
                zero(&status.0, STATUS_HDR + absent * STATUS_STRIDE, STATUS_STRIDE)
                    && zero(&control.0, CONTROL_HDR + absent * CONTROL_STRIDE, CONTROL_STRIDE),
                "absent sensor {absent} is not blank; layout not recognised"
            );
        }
        info!(
            "thermal sensor group: {} sensors, {} with readings, simulation {}",
            sensors.len(),
            readable,
            if sensors.iter().any(|s| control.0[CONTROL_HDR + usize::from(s.index) * CONTROL_STRIDE + CE_SIM_ENABLE] == 1) {
                "ACTIVE at start"
            } else {
                "off"
            }
        );
        Ok(Self {
            mask,
            sensors,
            start_control: control,
        })
    }

    pub fn sensors(&self) -> &[SensorInfo] {
        &self.sensors
    }

    fn seed(&self, b: &mut [u8]) {
        b[..4].copy_from_slice(&GROUP_TYPE.to_le_bytes());
        for (w, m) in self.mask.iter().enumerate() {
            b[MASK_AT + 4 * w..MASK_AT + 4 * w + 4].copy_from_slice(&m.to_le_bytes());
        }
    }

    fn read_control(&self, handle: &DriverHandle) -> anyhow::Result<Box<Bytes<CONTROL_SIZE>>> {
        rm_call::<CONTROL_SIZE>(handle, GET_CONTROL, |b| self.seed(b)).context("THERM sensors GET_CONTROL")
    }

    /// Every sensor's reading and simulation state.
    pub fn readings(&self, handle: &DriverHandle) -> anyhow::Result<Vec<SensorReading>> {
        let status = rm_call::<STATUS_SIZE>(handle, GET_STATUS, |b| self.seed(b)).context("THERM sensors GET_STATUS")?;
        let control = self.read_control(handle)?;
        Ok(self
            .sensors
            .iter()
            .map(|s| {
                let se = STATUS_HDR + usize::from(s.index) * STATUS_STRIDE;
                let ce = control_entry(&control.0, s.index);
                let sim_enabled = ce[CE_SIM_ENABLE] == 1;
                SensorReading {
                    index: s.index,
                    temp_c: nvtemp_to_c(rd_u32(&status.0, se + SE_TEMP)),
                    sim_enabled,
                    sim_c: sim_enabled.then(|| nvtemp_to_c(rd_u32(ce, CE_SIM_TEMP))).flatten(),
                }
            })
            .collect())
    }

    /// Write one object's control entry with only that object selected,
    /// read back, and on any disagreement put the start-of-daemon entry back.
    fn write_entry(&self, handle: &DriverHandle, index: u8, entry: &[u8]) -> anyhow::Result<()> {
        let before = self.read_control(handle)?;
        let mut req = before.clone();
        for w in 0..MASK_WORDS {
            req.0[MASK_AT + 4 * w..MASK_AT + 4 * w + 4].copy_from_slice(&0u32.to_le_bytes());
        }
        let word = usize::from(index) / 32;
        let bit = 1u32 << (index % 32);
        req.0[MASK_AT + 4 * word..MASK_AT + 4 * word + 4].copy_from_slice(&bit.to_le_bytes());
        let at = CONTROL_HDR + usize::from(index) * CONTROL_STRIDE;
        req.0[at..at + CONTROL_STRIDE].copy_from_slice(entry);
        debug!("THERM sensor {index} SET_CONTROL entry {:02x?}", entry);
        let written = unsafe { handle.query_rm_control(SET_CONTROL, &mut *req) };
        let back = self.read_control(handle);
        let ok = written.is_ok() && back.as_ref().is_ok_and(|b| control_entry(&b.0, index) == entry);
        if ok {
            return Ok(());
        }
        // Restore what the daemon started with, then report.
        let mut restore = before.clone();
        for w in 0..MASK_WORDS {
            restore.0[MASK_AT + 4 * w..MASK_AT + 4 * w + 4].copy_from_slice(&0u32.to_le_bytes());
        }
        restore.0[MASK_AT + 4 * word..MASK_AT + 4 * word + 4].copy_from_slice(&bit.to_le_bytes());
        restore.0[at..at + CONTROL_STRIDE].copy_from_slice(control_entry(&self.start_control.0, index));
        let restored = unsafe { handle.query_rm_control(SET_CONTROL, &mut *restore) };
        match (written, restored) {
            (Err(err), Ok(())) => bail!("sensor {index} SET_CONTROL failed ({err:#}); start value restored"),
            (Err(err), Err(rerr)) => bail!("sensor {index} SET_CONTROL failed ({err:#}) and the restore failed too ({rerr:#})"),
            (Ok(()), Ok(())) => bail!("sensor {index} readback differs from the request; start value restored"),
            (Ok(()), Err(rerr)) => bail!("sensor {index} readback differs and the restore failed ({rerr:#})"),
        }
    }

    /// Set (`Some(°C)`) or clear the simulation on one sensor. Clearing
    /// writes the entry the daemon found at start for that object.
    pub fn set_simulation(&self, handle: &DriverHandle, index: u8, temp_c: Option<i32>) -> anyhow::Result<()> {
        ensure!(
            self.sensors.iter().any(|s| s.index == index),
            "sensor {index} is not in the group"
        );
        let current = self.read_control(handle)?;
        let mut entry = [0u8; CONTROL_STRIDE];
        match temp_c {
            Some(c) => {
                ensure!((SIM_MIN_C..=SIM_MAX_C).contains(&c), "simulated {c} °C is outside {SIM_MIN_C}…{SIM_MAX_C} °C");
                entry.copy_from_slice(control_entry(&current.0, index));
                entry[CE_SIM_ENABLE] = 1;
                entry[CE_SIM_TEMP..CE_SIM_TEMP + 4].copy_from_slice(&c_to_nvtemp(c).to_le_bytes());
            }
            None => entry.copy_from_slice(control_entry(&self.start_control.0, index)),
        }
        if control_entry(&current.0, index) == entry {
            return Ok(());
        }
        self.write_entry(handle, index, &entry)
    }

    /// Clear every simulation the daemon may have set.
    pub fn restore_all(&self, handle: &DriverHandle) -> anyhow::Result<()> {
        let current = self.read_control(handle)?;
        let mut first_err = None;
        for s in &self.sensors {
            let start = control_entry(&self.start_control.0, s.index);
            if control_entry(&current.0, s.index) != start
                && let Err(err) = self.write_entry(handle, s.index, start)
                && first_err.is_none()
            {
                first_err = Some(err);
            }
        }
        first_err.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvtemp_conversions() {
        assert_eq!(nvtemp_to_c(0x3c00), Some(60.0));
        assert_eq!(nvtemp_to_c(0x1b68), Some(27.406_25));
        assert_eq!(nvtemp_to_c(NO_READING), None);
        assert_eq!(c_to_nvtemp(60), 0x3c00);
    }

    #[test]
    fn names_and_gpu_flag() {
        let gpu = SensorInfo { index: 1, kind: 2, id: 0 };
        assert!(gpu.is_gpu());
        assert_eq!(gpu.name(), "GPU");
        let mem = SensorInfo { index: 9, kind: 3, id: 0x2c };
        assert!(!mem.is_gpu());
        assert_eq!(mem.name(), "Memory chip 0x2c");
    }

    #[test]
    fn mask_helper() {
        let mut m = [0u32; MASK_WORDS];
        m[0] = 0xff_ffff;
        assert!(mask_has(&m, 23));
        assert!(!mask_has(&m, 24));
        assert!(!mask_has(&m, 255));
    }
}
