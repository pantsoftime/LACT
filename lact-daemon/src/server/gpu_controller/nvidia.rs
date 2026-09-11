pub mod clock_client;
mod driver;
pub mod nvapi;
mod rm_perf;
mod rm_prop;
mod rm_volt;

use super::{CommonControllerInfo, FanControlHandle, GpuController};
use crate::{
    bindings::nvidia::NvPhysicalGpuHandle,
    server::gpu_controller::{
        common::{fan_control::FanCurveExt, resolve_process_name},
        nvidia::nvapi::{
            CLOCK_CLIENT_CLK_VF_POINT_TYPE_PROG, ClockClientClkVfPointInfoV1,
            ClockClientClkVfPointsControlV1, ClockClientClkVfPointsInfoV1,
            ClockClientClkVfPointsStatusV3, NvGpuClockDomainId,
        },
    },
};
use amdgpu_sysfs::{
    gpu_handle::{PowerLevelId, fan_control::FanInfo, power_profile_mode::PowerProfileModesTable},
    hw_mon::Temperature,
};
use anyhow::{Context, anyhow, bail, ensure};
use clock_client::{ControlBlock, RmClockDomain, RmDomainKind};
use lact_schema::{
    NvidiaPropagationRatio, NvidiaRailLimitDelta, NvidiaVoltageRail, PerfLimitEntry, RailLimit,
};
use rm_perf::{PerfLimitKind, PerfLimits};
use rm_prop::PropRelation;
use rm_volt::VoltRails;
use driver::DriverHandle;
use futures::{FutureExt, future::LocalBoxFuture};
use indexmap::IndexMap;
use lact_schema::{
    ActivePowerStates, CacheInfo, ClocksInfo, ClocksTable, ClockspeedStats, DeviceApiInfo,
    DeviceFlag, DeviceInfo, DeviceStats, DeviceType, DrmInfo, DrmMemoryInfo, FanControlMode,
    FanStats, IntelDrmInfo, LinkInfo, NvidiaClockOffset, NvidiaClocksTable, NvidiaThermalInfo,
    NvidiaVfPoint, NvidiaVoltageBoost, PmfwInfo, PowerState, PowerStates, PowerStats, ProcessInfo,
    ProcessList, ProcessType, ProcessUtilizationType, TemperatureEntry, VoltageStats, VramStats,
    config::{FanControlSettings, FanCurve, GpuConfig, NvidiaCurvePoint},
};
use lact_schema::{NvidiaRmClockDomain, config::ClocksConfiguration};
use nvapi::NvApi;
use nvml_wrapper::{
    Device, Nvml,
    bitmasks::device::{PowerMizerModes, ThrottleReasons},
    enum_wrappers::device::{Clock, PerformanceState, TemperatureSensor, TemperatureThreshold},
    enums::device::{GpuLockedClocksSetting, PowerMizerMode, UsedGpuMemory},
    error::NvmlError,
};
use std::{
    cell::{Cell, RefCell},
    cmp,
    collections::{BTreeMap, HashMap, btree_map::Entry},
    fmt::Write,
    ops::RangeInclusive,
    rc::Rc,
    time::{Duration, Instant},
};
use tokio::{select, sync::Notify, time::sleep};
use tracing::{debug, error, trace, warn};

const SUPPORTED_UTIL_TYPES: &[ProcessUtilizationType] = &[
    ProcessUtilizationType::Graphics,
    ProcessUtilizationType::Memory,
    ProcessUtilizationType::Encode,
    ProcessUtilizationType::Decode,
];

const VOLTAGE_BOOST_RANGE: RangeInclusive<i32> = 0..=100;

pub struct NvidiaGpuController {
    nvml: Rc<Nvml>,
    common: CommonControllerInfo,
    fan_control_handle: RefCell<Option<FanControlHandle>>,
    initial_target_temp: Option<u32>,

    nvapi: Option<(Rc<NvApi>, NvPhysicalGpuHandle)>,
    driver_handle: Option<DriverHandle>,
    /// RM ClockClient domains, if the private interface passed its self-check
    rm_clock_domains: Option<Vec<RmClockDomain>>,
    /// RM voltage rails + ADCs (read-only telemetry), if their layout checked out
    rm_volt: Option<VoltRails>,
    /// The GPC→XBAR propagation ratio relation, if found in the active topology
    rm_prop: Option<PropRelation>,
    /// The arbiter's limit clients ("boost limits"), if the status parsed
    rm_perf: Option<PerfLimits>,
    nvapi_therm_channel_mask: Option<i32>,

    last_util_timestamp: Cell<Option<u64>>,
    // Store last applied offsets as a workaround when the driver doesn't tell us the current offset
    last_applied_offsets: RefCell<HashMap<Clock, HashMap<PerformanceState, i32>>>,
    last_applied_gpu_locked_clocks: RefCell<Option<(u32, u32)>>,
    last_applied_vram_locked_clocks: RefCell<Option<(u32, u32)>>,
    // Check if reset is needed to avoid unnecessarily going to nvapi
    vf_curve_written: Cell<bool>,
    voltage_boost_written: Cell<bool>,
}

impl NvidiaGpuController {
    pub fn new(
        common: CommonControllerInfo,
        nvml: Rc<Nvml>,
        nvapi: Option<Rc<NvApi>>,
    ) -> anyhow::Result<Self> {
        let device = nvml
            .device_by_pci_bus_id(common.pci_slot_name.as_str())
            .with_context(|| {
                format!(
                    "Could not get PCI device '{}' from NVML",
                    common.pci_slot_name
                )
            })?;

        let (nvapi_handle, nvapi_therm_channel_mask) = match nvapi.as_ref() {
            Some(nvapi) => {
                let bus_id = common.get_slot_info()?.bus;
                let gpu_handle = nvapi
                    .find_matching_gpu(u32::from(bus_id))
                    .inspect_err(|err| error!("Could not get NvAPI GPU handle: {err}"))
                    .ok()
                    .flatten();

                let therm_channel_mask = gpu_handle.and_then(|handle| unsafe {
                    nvapi
                        .calculate_therm_channel_mask(handle)
                        .inspect(|mask| {
                            debug!("calculated NvAPI therm channel mask {mask:x}");
                        })
                        .inspect_err(|err| {
                            error!("could not calculate NvAPI therm channel mask: {err:#}");
                        })
                        .ok()
                });

                (gpu_handle, therm_channel_mask)
            }
            None => (None, None),
        };
        debug!("initialized NvAPI device handle {nvapi_handle:?}");

        let minor_number = device.minor_number()?;

        let driver_handle = match DriverHandle::open(minor_number) {
            Ok(handle) => {
                debug!("opened Nvidia driver handle");
                Some(handle)
            }
            Err(err) => {
                error!("could not get Nvidia driver handle: {err:#}");
                None
            }
        };

        let target_temp = device
            .temperature_threshold(TemperatureThreshold::AcousticCurr)
            .ok();

        let rm_clock_domains = driver_handle.as_ref().and_then(|handle| {
            let version = nvml.sys_driver_version().unwrap_or_default();
            clock_client::probe(handle, &version)
        });
        if rm_clock_domains.is_some() {
            tracing::info!("RM clock domain controls (XBAR/SYS/video, MSVDD) enabled");
        }
        let (rm_volt, rm_prop) = match (driver_handle.as_ref(), rm_clock_domains.as_deref()) {
            (Some(handle), Some(domains)) => {
                let volt = match VoltRails::probe(handle) {
                    Ok(volt) => {
                        tracing::info!(
                            "RM voltage rail telemetry enabled ({} ADCs)",
                            volt.adc_count()
                        );
                        Some(volt)
                    }
                    Err(err) => {
                        warn!("RM voltage rail telemetry disabled: {err:#}");
                        None
                    }
                };
                let index_of = |kind| {
                    domains
                        .iter()
                        .find(|d| d.kind == Some(kind))
                        .map(|d| d.index)
                };
                let prop = match (index_of(RmDomainKind::Gpc), index_of(RmDomainKind::Xbar)) {
                    (Some(gpc), Some(xbar)) => match PropRelation::probe(handle, gpc, xbar) {
                        Ok(rel) => {
                            tracing::info!(
                                "GPC→XBAR propagation ratio control enabled (relation {} of topology {:#x}, factory {:.4})",
                                rel.index,
                                rel.topology_id,
                                rm_prop::raw_to_ratio(rel.factory_raw)
                            );
                            Some(rel)
                        }
                        Err(err) => {
                            warn!("GPC→XBAR propagation ratio control disabled: {err:#}");
                            None
                        }
                    },
                    _ => None,
                };
                (volt, prop)
            }
            _ => (None, None),
        };
        let rm_perf = match (driver_handle.as_ref(), rm_clock_domains.as_ref()) {
            (Some(handle), Some(_)) => match PerfLimits::probe(handle) {
                Ok(perf) => {
                    tracing::info!("RM performance-limit (boost limit) telemetry enabled");
                    Some(perf)
                }
                Err(err) => {
                    warn!("RM performance-limit telemetry disabled: {err:#}");
                    None
                }
            },
            _ => None,
        };

        Ok(Self {
            nvml,
            nvapi: nvapi.zip(nvapi_handle),
            common,
            driver_handle,
            rm_clock_domains,
            rm_volt,
            rm_prop,
            rm_perf,
            nvapi_therm_channel_mask,
            initial_target_temp: target_temp,
            last_util_timestamp: Cell::new(None),
            fan_control_handle: RefCell::new(None),
            last_applied_offsets: RefCell::new(HashMap::new()),
            last_applied_gpu_locked_clocks: RefCell::new(None),
            last_applied_vram_locked_clocks: RefCell::new(None),
            vf_curve_written: Cell::new(false),
            voltage_boost_written: Cell::new(false),
        })
    }

    // ---- RM ClockClient (XBAR / SYS / video offsets, MSVDD rail offset) ----

    fn rm(&self) -> Option<(&DriverHandle, &[RmClockDomain])> {
        Some((
            self.driver_handle.as_ref()?,
            self.rm_clock_domains.as_deref()?,
        ))
    }

    fn rm_domain(&self, kind: RmDomainKind) -> Option<&RmClockDomain> {
        self.rm_clock_domains
            .as_deref()?
            .iter()
            .find(|d| d.kind == Some(kind) && d.controllable && d.offset_range_mhz > 0)
    }

    fn rm_measure_mhz(&self, kind: RmDomainKind) -> Option<u64> {
        let (handle, domains) = self.rm()?;
        let domain = domains.iter().find(|d| d.kind == Some(kind))?;
        handle
            .clk_measure_khz(domain.api_domain)
            .ok()
            .map(|khz| u64::from(khz) / 1000)
    }

    /// Offset entry (current / min / max, MHz) for a frequency domain.
    fn rm_freq_offset(
        &self,
        block: &ControlBlock,
        kind: RmDomainKind,
    ) -> Option<NvidiaClockOffset> {
        let domain = self.rm_domain(kind)?;
        let range = i32::try_from(domain.offset_range_mhz).ok()?;
        Some(NvidiaClockOffset {
            current: block.freq_offset_khz(domain.index) / 1000,
            min: -range,
            max: range,
        })
    }

    /// A domain's voltage demand offset on its own rail (from the INFO rail
    /// mask), in millivolts. The driver publishes no range; the bound is
    /// deliberately tight: +20 mV on the XBAR domain lowered XBAR ~31 MHz and
    /// moved the arbitrated MSVDD target by only 5 mV under load, because
    /// another client is the rail winner. Not free headroom.
    fn rm_rail_offset(&self, block: &ControlBlock, kind: RmDomainKind) -> Option<NvidiaClockOffset> {
        let domain = self.rm_domain(kind)?;
        let rail = domain.rail_index()?;
        Some(NvidiaClockOffset {
            current: block.rail_offset_uv(domain.index, rail) / 1000,
            min: -50,
            max: 50,
        })
    }

    fn rm_propagation_ratio(&self) -> Option<NvidiaPropagationRatio> {
        let (handle, rel) = (self.driver_handle.as_ref()?, self.rm_prop.as_ref()?);
        let current = rel.read_raw(handle).ok()?;
        Some(NvidiaPropagationRatio {
            current: rm_prop::raw_to_ratio(current),
            factory: rm_prop::raw_to_ratio(rel.factory_raw),
            min: rm_prop::raw_to_ratio(rm_prop::RATIO_MIN_RAW),
            max: rm_prop::raw_to_ratio(rm_prop::RATIO_MAX_RAW),
        })
    }

    fn rm_voltage_rails(&self) -> Vec<NvidiaVoltageRail> {
        let (Some(handle), Some(volt)) = (self.driver_handle.as_ref(), self.rm_volt.as_ref()) else {
            return Vec::new();
        };
        let status = match volt.rails_status(handle) {
            Ok(status) => status,
            Err(err) => {
                warn!("could not read voltage rail status: {err:#}");
                return Vec::new();
            }
        };
        let deltas = volt.limit_deltas_uv(handle).ok();
        let (nvvdd_sensed, msvdd_sensed) = volt.sensed_mv(handle);
        let bound = rm_volt::LIMIT_DELTA_BOUND_UV / 1000;
        status
            .into_iter()
            .map(|s| NvidiaVoltageRail {
                index: s.index,
                name: rm_volt::rail_name(s.index).to_owned(),
                target_mv: s.target_uv / 1000,
                default_mv: s.default_uv / 1000,
                vmin_limit_mv: s.vmin_limit_uv / 1000,
                rel_limit_mv: s.rel_limit_uv / 1000,
                alt_rel_limit_mv: s.alt_rel_limit_uv / 1000,
                ov_limit_mv: s.ov_limit_uv / 1000,
                max_limit_mv: s.max_limit_uv / 1000,
                sensed_mv: match s.index {
                    0 => nvvdd_sensed,
                    1 => msvdd_sensed,
                    _ => None,
                },
                limit_deltas: deltas
                    .as_ref()
                    .map(|d| {
                        let rail = usize::from(s.index);
                        RailLimit::ALL
                            .iter()
                            .map(|limit| {
                                let i = rm_volt::limit_index(*limit);
                                NvidiaRailLimitDelta {
                                    limit: *limit,
                                    current_mv: d[rail][i] / 1000,
                                    default_mv: volt.start_deltas_uv()[rail][i] / 1000,
                                    min_mv: -bound,
                                    max_mv: bound,
                                    limit_mv: match limit {
                                        RailLimit::Vmin => s.vmin_limit_uv,
                                        RailLimit::Rel => s.rel_limit_uv,
                                        RailLimit::AltRel => s.alt_rel_limit_uv,
                                        RailLimit::Ov => s.ov_limit_uv,
                                    } / 1000,
                                }
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                device_max_mv: Some(volt.device_max_uv() / 1000),
            })
            .collect()
    }

    /// Apply the configured rail limit deltas; `None` = the value found at
    /// daemon start. A raised limit is held under the voltage device's
    /// maximum, and the driver's evaluated MAX is what actually binds.
    fn apply_rm_rail_limits(&self, clocks: &ClocksConfiguration) -> anyhow::Result<()> {
        let (Some(handle), Some(volt)) = (self.driver_handle.as_ref(), self.rm_volt.as_ref()) else {
            if clocks.any_rail_limit_delta() {
                warn!("rail limit deltas in the config were not applied: the rail objects are not available on this driver");
            }
            return Ok(());
        };
        let current = volt.limit_deltas_uv(handle)?;
        let status = volt.rails_status(handle)?;
        let mut wanted = *volt.start_deltas_uv();
        for (rail, deltas) in wanted.iter_mut().enumerate() {
            for (i, limit) in RailLimit::ALL.iter().enumerate() {
                let Some(mv) = clocks.rail_limit_delta(rail as u8, *limit) else {
                    continue;
                };
                let uv = mv * 1000;
                if uv.abs() > rm_volt::LIMIT_DELTA_BOUND_UV {
                    bail!("{} {} delta {mv} mV exceeds the ±{} mV bound", rm_volt::rail_name(rail as u8), limit.label(), rm_volt::LIMIT_DELTA_BOUND_UV / 1000);
                }
                // STATUS limits already include the current delta.
                if let Some(s) = status.get(rail) {
                    let evaluated = match limit {
                        RailLimit::Vmin => s.vmin_limit_uv,
                        RailLimit::Rel => s.rel_limit_uv,
                        RailLimit::AltRel => s.alt_rel_limit_uv,
                        RailLimit::Ov => s.ov_limit_uv,
                    };
                    let resulting = i64::from(evaluated) - i64::from(current[rail][i]) + i64::from(uv);
                    if resulting > i64::from(volt.device_max_uv()) {
                        bail!(
                            "{} {} delta {mv} mV would put the limit at {} mV, above the voltage device maximum of {} mV",
                            rm_volt::rail_name(rail as u8),
                            limit.label(),
                            resulting / 1000,
                            volt.device_max_uv() / 1000
                        );
                    }
                }
                deltas[i] = uv;
            }
        }
        if wanted == current {
            return Ok(());
        }
        debug!("applying rail limit deltas {wanted:?} µV (was {current:?})");
        volt.set_limit_deltas(handle, &wanted)
            .context("Could not apply the rail limit deltas")?;
        Ok(())
    }

    /// The populated limit clients, named from data where possible: a
    /// voltage limit is matched against the rail's policy limits (VMIN / REL /
    /// ALT/OP / OV), a frequency limit is named by its domain. IDs known to be
    /// floors (the PERF-CF controller minimums, per Loong0x00's A/B on R610)
    /// are flagged so the GUI does not report them as what bounds the boost.
    fn rm_perf_limits(&self) -> Vec<PerfLimitEntry> {
        let (Some(handle), Some(perf)) = (self.driver_handle.as_ref(), self.rm_perf.as_ref()) else {
            return Vec::new();
        };
        let limits = match perf.read(handle) {
            Ok(limits) => limits,
            Err(err) => {
                warn!("could not read performance limits: {err:#}");
                return Vec::new();
            }
        };
        let rails = self
            .rm_volt
            .as_ref()
            .and_then(|v| v.rails_status(handle).ok())
            .unwrap_or_default();
        let domain_name = |mask: u32| -> Option<String> {
            RmDomainKind::from_api_domain(mask)
                .map(|k| k.to_string())
                .or_else(|| {
                    self.rm_clock_domains
                        .as_deref()?
                        .iter()
                        .find(|d| d.api_domain == mask)
                        .map(RmClockDomain::name)
                })
        };
        const CONTROLLER_MINIMUMS: [u32; 3] = [0xd0, 0xd1, 0xd2];
        limits
            .into_iter()
            .map(|l| {
                let (name, domain, limit_mhz, limit_mv) = match l.kind {
                    PerfLimitKind::Frequency { khz, domain_mask } => {
                        let domain = domain_name(domain_mask);
                        let what = if CONTROLLER_MINIMUMS.contains(&l.id) {
                            "controller minimum"
                        } else {
                            "frequency limit"
                        };
                        (
                            format!("{} {what} {:#04x}", domain.as_deref().unwrap_or("clock"), l.id),
                            domain,
                            Some(khz / 1000),
                            None,
                        )
                    }
                    PerfLimitKind::Voltage { uv, rail } => {
                        let rail_name = rm_volt::rail_name(rail);
                        let which = rails.iter().find(|r| r.index == rail).and_then(|r| {
                            [
                                (r.vmin_limit_uv, "VMIN"),
                                (r.rel_limit_uv, "REL"),
                                (r.alt_rel_limit_uv, "ALT/OP"),
                                (r.ov_limit_uv, "OV"),
                            ]
                            .into_iter()
                            .find(|(v, _)| *v == uv)
                            .map(|(_, n)| n)
                        });
                        (
                            match which {
                                Some(which) => format!("{rail_name} {which} limit"),
                                None => format!("{rail_name} voltage limit {:#04x}", l.id),
                            },
                            l.result_khz.map(|_| RmDomainKind::Gpc.to_string()),
                            None,
                            Some(uv / 1000),
                        )
                    }
                    PerfLimitKind::Other { type_code, value } => (
                        format!("limit {:#04x} (type {type_code}, value {value})", l.id),
                        None,
                        None,
                        None,
                    ),
                };
                PerfLimitEntry {
                    id: l.id,
                    name,
                    domain,
                    limit_mhz,
                    limit_mv,
                    result_mhz: l.result_khz.map(|k| k / 1000),
                    is_minimum: CONTROLLER_MINIMUMS.contains(&l.id),
                }
            })
            .collect()
    }

    /// Voltage sensors for the stats map: sensed rails from the ADCs and the
    /// arbitrated rail targets. Millivolts, like `voltage.gpu`.
    fn rm_voltage_sensors(&self) -> HashMap<String, u64> {
        let mut out = HashMap::new();
        let (Some(handle), Some(volt)) = (self.driver_handle.as_ref(), self.rm_volt.as_ref()) else {
            return out;
        };
        let (nvvdd, msvdd) = volt.sensed_mv(handle);
        if let Some(mv) = msvdd {
            out.insert("MSVDD".to_owned(), u64::from(mv));
        }
        if let Some(mv) = nvvdd {
            out.insert("NVVDD (ADC)".to_owned(), u64::from(mv));
        }
        if let Ok(status) = volt.rails_status(handle) {
            for s in status {
                out.insert(
                    format!("{} target", rm_volt::rail_name(s.index)),
                    u64::from(s.target_uv / 1000),
                );
            }
        }
        out
    }

    /// Apply the configured propagation ratio; `None` means the factory
    /// relation, like every other clock setting.
    fn apply_rm_propagation(&self, clocks: &ClocksConfiguration) -> anyhow::Result<()> {
        let (Some(handle), Some(rel)) = (self.driver_handle.as_ref(), self.rm_prop.as_ref()) else {
            if clocks.gpc_xbar_ratio_milli.is_some() {
                warn!(
                    "GPC→XBAR propagation ratio in the config was not applied: the relation is not \
                     available on this driver"
                );
            }
            return Ok(());
        };
        let wanted = match clocks.gpc_xbar_ratio_milli {
            Some(milli) => rm_prop::ratio_milli_to_raw(milli)?,
            None => rel.factory_raw,
        };
        let current = rel.read_raw(handle)?;
        if current == wanted {
            return Ok(());
        }
        debug!(
            "applying GPC→XBAR propagation ratio {:.4} (was {:.4})",
            rm_prop::raw_to_ratio(wanted),
            rm_prop::raw_to_ratio(current)
        );
        rel.set_raw(handle, wanted)
            .context("Could not apply the GPC→XBAR propagation ratio")
    }

    fn rm_domain_table(&self, block: &ControlBlock) -> Vec<NvidiaRmClockDomain> {
        let Some((handle, domains)) = self.rm() else {
            return Vec::new();
        };
        domains
            .iter()
            .map(|d| NvidiaRmClockDomain {
                index: d.index,
                name: d.name(),
                api_domain: d.api_domain,
                measured_khz: handle.clk_measure_khz(d.api_domain).ok(),
                offset_khz: if d.controllable {
                    block.freq_offset_khz(d.index)
                } else {
                    0
                },
                offset_range_mhz: d.offset_range_mhz,
                controllable: d.controllable,
            })
            .collect()
    }

    /// Apply the configured RM offsets. Missing values mean stock (0) — the
    /// same semantics as every other clock setting in LACT. Writes only when
    /// the block would actually change, since config is reapplied on a timer.
    fn apply_rm_offsets(&self, clocks: &ClocksConfiguration) -> anyhow::Result<()> {
        let Some((handle, _)) = self.rm() else {
            if clocks.xbar_clock_offset.is_some()
                || clocks.sys_clock_offset.is_some()
                || clocks.video_clock_offset.is_some()
                || clocks.msvdd_offset.is_some()
                || clocks.nvvdd_offset.is_some()
            {
                // Non-fatal, like voltage boost: after a driver update the
                // branch gate disables the RM controls, and a stale offset in
                // the config must not stop fan, thermal and power settings
                // from being applied. The Advanced page shows the RM status.
                warn!(
                    "RM clock domain offsets in the config were not applied: the ClockClient \
                     interface is not available on this driver"
                );
            }
            return Ok(());
        };

        let current = handle.clk_domains_get_control()?;
        let mut wanted = current;
        for (kind, mhz) in [
            (RmDomainKind::Xbar, clocks.xbar_clock_offset),
            (RmDomainKind::Sys, clocks.sys_clock_offset),
            (RmDomainKind::Video, clocks.video_clock_offset),
        ] {
            let mhz = mhz.unwrap_or(0);
            let Some(domain) = self.rm_domain(kind) else {
                if mhz != 0 {
                    bail!("{kind} offset is not adjustable on this GPU");
                }
                continue;
            };
            let range = i32::try_from(domain.offset_range_mhz).unwrap_or(0);
            if mhz.abs() > range {
                bail!("{kind} offset {mhz} MHz exceeds the driver range of ±{range} MHz");
            }
            wanted.set_freq_offset_khz(domain.index, mhz * 1000);
        }
        // Each domain's voltage demand goes to the rail slot its INFO entry
        // names; a slot the domain does not use is ignored by the driver.
        for (kind, mv, name) in [
            (RmDomainKind::Gpc, clocks.nvvdd_offset, "NVVDD"),
            (RmDomainKind::Xbar, clocks.msvdd_offset, "MSVDD"),
            (RmDomainKind::Sys, clocks.sys_voltage_offset, "SYS voltage"),
            (RmDomainKind::Video, clocks.video_voltage_offset, "video voltage"),
        ] {
            let mv = mv.unwrap_or(0);
            match self.rm_domain(kind).and_then(|d| Some((d, d.rail_index()?))) {
                Some((domain, rail)) => {
                    if mv.abs() > 50 {
                        bail!("{name} offset {mv} mV exceeds the ±50 mV bound");
                    }
                    wanted.set_rail_offset_uv(domain.index, rail, mv * 1000);
                }
                None if mv != 0 => {
                    bail!("{name} offset requires an adjustable {kind} domain with a known rail")
                }
                None => {}
            }
        }

        if wanted.freq_offset_khz(0) == current.freq_offset_khz(0)
            && (0..8u8).all(|i| {
                wanted.freq_offset_khz(i) == current.freq_offset_khz(i)
                    && (0..4).all(|r| wanted.rail_offset_uv(i, r) == current.rail_offset_uv(i, r))
            })
        {
            return Ok(());
        }

        debug!(
            "applying RM offsets: xbar {:?} sys {:?} video {:?} MHz; nvvdd {:?} msvdd {:?} sys {:?} video {:?} mV",
            clocks.xbar_clock_offset,
            clocks.sys_clock_offset,
            clocks.video_clock_offset,
            clocks.nvvdd_offset,
            clocks.msvdd_offset,
            clocks.sys_voltage_offset,
            clocks.video_voltage_offset
        );
        handle
            .clk_domains_set_control(&wanted)
            .context("Could not apply RM clock domain offsets")
    }

    fn device(&self) -> Device<'_> {
        self.nvml
            .device_by_pci_bus_id(self.common.pci_slot_name.as_str())
            .expect("Can no longer get device")
    }

    fn get_nvidia_thermal_info(&self) -> NvidiaThermalInfo {
        NvidiaThermalInfo {
            target_temp: self.get_target_temp(),
            target_temp_default: self.initial_target_temp,
        }
    }

    fn get_target_temp(&self) -> Option<FanInfo> {
        let device = self.device();
        let current = device
            .temperature_threshold(TemperatureThreshold::AcousticCurr)
            .ok()?;
        let min = device
            .temperature_threshold(TemperatureThreshold::AcousticMin)
            .ok()?;
        let max = device
            .temperature_threshold(TemperatureThreshold::AcousticMax)
            .ok()?;

        Some(FanInfo {
            current,
            allowed_range: Some((min, max)),
        })
    }

    async fn start_curve_fan_control_task(
        &self,
        curve: FanCurve,
        settings: FanControlSettings,
    ) -> anyhow::Result<()> {
        // Stop existing task to re-apply new curve
        self.stop_fan_control().await?;

        let device = self.device();
        device
            .temperature(TemperatureSensor::Gpu)
            .context("Could not read temperature")?;

        let fan_count = device.num_fans().context("Could not read fan count")?;
        if fan_count == 0 {
            return Err(anyhow!("Device has no fans"));
        }

        let mut notify_guard = self
            .fan_control_handle
            .try_borrow_mut()
            .map_err(|err| anyhow!("Lock error: {err}"))?;

        let notify = Rc::new(Notify::new());
        let task_notify = notify.clone();

        let nvml = self.nvml.clone();
        let pci_slot_id = self.common.pci_slot_name.clone();
        debug!("spawning new fan control task");

        let handle = tokio::task::spawn_local(async move {
            let mut device = nvml
                .device_by_pci_bus_id(pci_slot_id.as_str())
                .expect("Can no longer get device");

            let mut last_pwm = (None, Instant::now());
            let mut last_temp = 0;

            let interval = Duration::from_millis(settings.interval_ms);
            let spindown_delay = Duration::from_millis(settings.spindown_delay_ms.unwrap_or(0));
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let change_threshold = settings.change_threshold.unwrap_or(0) as i32;
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
            let auto_threshold = settings.auto_threshold.unwrap_or(0) as i32;

            let mut manual_mode = true;

            loop {
                select! {
                    () = sleep(interval) => (),
                    () = task_notify.notified() => break,
                }

                #[allow(clippy::cast_possible_wrap)]
                let current_temp = device
                    .temperature(TemperatureSensor::Gpu)
                    .expect("Could not read temperature") as i32;

                if (last_temp - current_temp).abs() < change_threshold {
                    trace!(
                        "temperature changed from {last_temp}°C to {current_temp}°C, which is less than the {change_threshold}°C threshold, skipping speed adjustment"
                    );
                    continue;
                }

                if current_temp < auto_threshold {
                    if manual_mode {
                        trace!("temperature below auto threshold, setting fan policy to auto");
                        for fan in 0..fan_count {
                            if let Err(err) = device.set_default_fan_speed(fan) {
                                error!(
                                    "could not set fan speed to auto: {err}, disabling fan control"
                                );
                                break;
                            }
                        }

                        manual_mode = false;
                    } else {
                        trace!("temperature below auto threshold, skipping control");
                    }
                    continue;
                }

                let target_pwm = curve.pwm_at_temp(Temperature {
                    #[allow(clippy::cast_precision_loss)]
                    current: Some(current_temp as f32),
                    crit: None,
                    crit_hyst: None,
                });
                let now = Instant::now();

                if let (Some(previous_pwm), previous_timestamp) = last_pwm {
                    let diff = now - previous_timestamp;
                    if target_pwm < previous_pwm && diff < spindown_delay {
                        trace!(
                            "delaying fan spindown ({}ms left)",
                            spindown_delay.checked_sub(diff).unwrap().as_millis()
                        );
                        continue;
                    }
                }

                last_pwm = (Some(target_pwm), now);
                last_temp = current_temp;

                trace!("fan control tick: setting pwm to {target_pwm}");

                for fan in 0..fan_count {
                    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                    if let Err(err) =
                        device.set_fan_speed(fan, (f64::from(target_pwm) / 2.55) as u32)
                    {
                        error!("could not set fan speed: {err}, disabling fan control");
                        break;
                    }
                }
                manual_mode = true;
            }
            debug!("exited fan control task");
        });

        *notify_guard = Some((notify, handle));

        debug!(
            "started fan control with interval {}ms",
            settings.interval_ms
        );

        Ok(())
    }

    async fn stop_fan_control(&self) -> anyhow::Result<()> {
        let mut fail_on_error = false;

        let maybe_notify = self
            .fan_control_handle
            .try_borrow_mut()
            .map_err(|err| anyhow!("Lock error: {err}"))?
            .take();
        if let Some((notify, handle)) = maybe_notify {
            notify.notify_one();
            handle.await?;
            fail_on_error = true;
        }

        let mut device = self.device();
        let fan_count = device.num_fans().context("Could not get fan count")?;
        for i in 0..fan_count {
            if let Err(err) = device
                .set_default_fan_speed(i)
                .context("Could not reset fan control to default")
            {
                if fail_on_error {
                    return Err(err);
                }
                error!("{err:#?}");
            }
        }

        Ok(())
    }

    fn try_get_power_states(&self) -> anyhow::Result<PowerStates> {
        let device = self.device();

        let supported_states = device
            .supported_performance_states()
            .context("Could not get supported pstates")?;

        let mut power_states = PowerStates::default();

        for pstate in supported_states {
            let (gpu_min, gpu_max) = device
                .min_max_clock_of_pstate(Clock::Graphics, pstate)
                .context("Could not read GPU pstates")?;

            power_states.core.push(PowerState {
                enabled: true,
                min_value: Some(u64::from(gpu_min)),
                value: u64::from(gpu_max),
                id: Some(PowerLevelId::Index(
                    pstate
                        .as_c()
                        .try_into()
                        .expect("Power state always fits in u8"),
                )),
            });

            let (mem_min, mem_max) = device
                .min_max_clock_of_pstate(Clock::Memory, pstate)
                .context("Could not read memory pstates")?;

            power_states.vram.push(PowerState {
                enabled: true,
                min_value: Some(u64::from(mem_min)),
                value: u64::from(mem_max),
                id: Some(PowerLevelId::Index(
                    pstate
                        .as_c()
                        .try_into()
                        .expect("Power state always fits in u8"),
                )),
            });
        }

        Ok(power_states)
    }

    fn get_vf_curve(&self) -> anyhow::Result<Vec<NvidiaVfPoint>> {
        let (nvapi, handle) = self.nvapi.as_ref().context("NvAPI not available")?;

        let info;
        let status;
        let control;

        unsafe {
            info = nvapi.clock_client_clk_vf_points_get_info(*handle)?;
            status = nvapi.clock_client_clk_vf_points_get_status(*handle, info.vf_points_mask)?;
            control = nvapi.clock_client_clk_vf_points_get_control(*handle, info.vf_points_mask)?;
        }

        Ok(build_vf_curve(&info, &status, &control))
    }

    fn apply_vf_curve(&self, curve: &IndexMap<u8, NvidiaCurvePoint>) -> anyhow::Result<()> {
        let (nvapi, handle) = self.nvapi.as_ref().context("NvAPI not available")?;

        debug!("applying curve with {} points", curve.len());

        let offset_info = self
            .device()
            .clock_offset(Clock::Graphics, PerformanceState::Zero)
            .context("Could not get offset info")?;

        let info;
        let status;
        let control;

        unsafe {
            info = nvapi.clock_client_clk_vf_points_get_info(*handle)?;
            status = nvapi.clock_client_clk_vf_points_get_status(*handle, info.vf_points_mask)?;
            control = nvapi.clock_client_clk_vf_points_get_control(*handle, info.vf_points_mask)?;
        }

        let control = build_curve_control(
            curve,
            &info,
            &status,
            control,
            (
                offset_info.min_clock_offset_mhz,
                offset_info.max_clock_offset_mhz,
            ),
        )?;

        unsafe {
            nvapi.clock_client_clk_vf_points_set_control(*handle, control)?;
        }

        self.vf_curve_written.set(true);

        Ok(())
    }

    fn reset_vf_curve(&self) -> anyhow::Result<()> {
        let (nvapi, handle) = self.nvapi.as_ref().context("NvAPI not available")?;

        let info = unsafe { nvapi.clock_client_clk_vf_points_get_info(*handle)? };
        let mut curve_control =
            unsafe { nvapi.clock_client_clk_vf_points_get_control(*handle, info.vf_points_mask)? };

        for i in 0..point_count_from_mask(info.vf_points_mask) {
            let point_info = info.vf_points[i];
            if vf_curve_point_is_editable(point_info) {
                curve_control.vf_points[i].data.prog.freq_offset_khz = 0;
            }
        }

        unsafe {
            nvapi.clock_client_clk_vf_points_set_control(*handle, curve_control)?;
        }

        Ok(())
    }

    fn get_voltage_boost(&self) -> anyhow::Result<NvidiaVoltageBoost> {
        let (nvapi, handle) = self.nvapi.as_ref().context("NvAPI not available")?;

        let current = unsafe { nvapi.client_volt_rails_get_control(*handle)? };

        Ok(NvidiaVoltageBoost {
            current: current.into(),
            min: *VOLTAGE_BOOST_RANGE.start(),
            max: *VOLTAGE_BOOST_RANGE.end(),
        })
    }

    fn apply_voltage_boost(&self, percent: i32) -> anyhow::Result<()> {
        let (nvapi, handle) = self.nvapi.as_ref().context("NvAPI not available")?;

        if !VOLTAGE_BOOST_RANGE.contains(&percent) {
            bail!("Configured voltage boost {percent}% is outside of the allowed range");
        }
        let percent = u8::try_from(percent).expect("Validated value fits into u8");

        debug!("applying voltage boost {percent}%");

        unsafe {
            nvapi.client_volt_rails_set_control(*handle, percent)?;
        }
        self.voltage_boost_written.set(true);

        let applied = unsafe { nvapi.client_volt_rails_get_control(*handle) }
            .context("Could not verify voltage boost")?;
        ensure!(
            applied == percent,
            "Voltage boost was not applied: requested {percent}%, driver reports {applied}%"
        );

        Ok(())
    }

    fn reset_voltage_boost(&self) -> anyhow::Result<()> {
        let (nvapi, handle) = self.nvapi.as_ref().context("NvAPI not available")?;

        unsafe {
            nvapi.client_volt_rails_set_control(*handle, 0)?;
        }
        self.voltage_boost_written.set(false);

        Ok(())
    }

    fn reset_target_temp(&self) -> anyhow::Result<()> {
        if let Some(initial) = self.initial_target_temp {
            let device = self.device();

            let current = device.temperature_threshold(TemperatureThreshold::AcousticCurr)?;

            if current != initial {
                debug!("resetting target temperature to {initial}");
                device.set_temperature_threshold(
                    TemperatureThreshold::AcousticCurr,
                    initial.cast_signed(),
                )?;
            }
        } else {
            debug!("no initial target temperature was read, skipping reset");
        }

        Ok(())
    }
}

fn vf_curve_point_is_editable(point: ClockClientClkVfPointInfoV1) -> bool {
    point.b_voltage_based == 1 && point.type_ == CLOCK_CLIENT_CLK_VF_POINT_TYPE_PROG
}

/// The base curve for a point. Changes with GPU state(load,temps, e.tc), so it may only be used for display and validation
fn vf_curve_point_base_freq_khz(
    status: &ClockClientClkVfPointsStatusV3,
    control: &ClockClientClkVfPointsControlV1,
    i: usize,
) -> i32 {
    if status.b_vf_tuple_base_supported == 0 {
        // Cards without the base tuple (Turing) only expose the resulting curve
        status.vf_points[i].freq_khz.cast_signed()
            - unsafe { control.vf_points[i].data.prog.freq_offset_khz }
    } else {
        status.vf_points[i].vf_tuple_base.freq_khz.cast_signed()
    }
}

fn build_vf_curve(
    info: &ClockClientClkVfPointsInfoV1,
    status: &ClockClientClkVfPointsStatusV3,
    control: &ClockClientClkVfPointsControlV1,
) -> Vec<NvidiaVfPoint> {
    let point_count = point_count_from_mask(info.vf_points_mask);
    let mut curve = Vec::with_capacity(point_count);

    for i in 0..point_count {
        // Only report configurable and voltage-based points
        if !vf_curve_point_is_editable(info.vf_points[i]) {
            continue;
        }

        let point = status.vf_points[i];
        let offset_khz = unsafe { control.vf_points[i].data.prog.freq_offset_khz };

        let base_freq_khz = vf_curve_point_base_freq_khz(status, control, i);
        let base_voltage_uv = if status.b_vf_tuple_base_supported == 0 {
            point.voltage_uv
        } else {
            point.vf_tuple_base.voltage_uv
        };

        curve.push(NvidiaVfPoint {
            index: u8::try_from(i).expect("max 255 points"),
            freq: point.freq_khz / 1000,
            voltage: point.voltage_uv / 1000,
            base_freq: base_freq_khz.max(0).cast_unsigned() / 1000,
            base_voltage: base_voltage_uv / 1000,
            freq_offset: offset_khz / 1000,
        });
    }

    curve
}

fn build_curve_control(
    curve: &IndexMap<u8, NvidiaCurvePoint>,
    info: &ClockClientClkVfPointsInfoV1,
    status: &ClockClientClkVfPointsStatusV3,
    mut control: ClockClientClkVfPointsControlV1,
    (min_offset_mhz, max_offset_mhz): (i32, i32),
) -> anyhow::Result<ClockClientClkVfPointsControlV1> {
    let point_count = point_count_from_mask(info.vf_points_mask);

    for (index, configured_point) in curve {
        let i = usize::from(*index);

        if i >= point_count || !vf_curve_point_is_editable(info.vf_points[i]) {
            bail!("Point {i} is not configurable on this device");
        }

        if let Some(configured_mv) = configured_point.voltage {
            let current_mv = status.vf_points[i].voltage_uv / 1000;
            ensure!(
                configured_mv == current_mv,
                "Voltage is immutable - point {i} is at {current_mv}mV but was configured as {configured_mv}mV"
            );
        }

        let offset_mhz = configured_point.clockspeed_offset;
        let base_freq_mhz = vf_curve_point_base_freq_khz(status, &control, i) / 1000;

        let point_min_offset_mhz = cmp::min(min_offset_mhz, -base_freq_mhz);
        ensure!(
            (point_min_offset_mhz..=max_offset_mhz).contains(&offset_mhz),
            "Configured offset {offset_mhz}MHz for point {i} is outside of the allowed range {point_min_offset_mhz}..={max_offset_mhz}"
        );
        let offset_mhz = offset_mhz.max(1 - base_freq_mhz);

        trace!("writing offset {offset_mhz}MHz to point {i}");

        control.vf_points[i].data.prog.freq_offset_khz = offset_mhz * 1000;
    }

    Ok(control)
}

fn point_count_from_mask(mask: [u32; 8]) -> usize {
    let count: usize = mask.iter().map(|i| i.count_ones() as usize).sum();
    assert!(u8::try_from(count).is_ok());
    count
}

fn average_fan_value<E>(
    num_fans: u32,
    mut get_value: impl FnMut(u32) -> Result<u32, E>,
) -> Option<u32> {
    let mut sum: u32 = 0;
    let mut count: u32 = 0;

    for idx in 0..num_fans {
        if let Ok(value) = get_value(idx) {
            sum = sum.saturating_add(value);
            count += 1;
        }
    }

    (count > 0).then(|| sum / count)
}

fn apply_power_mizer_mode(
    device: &mut Device<'_>,
    configured_mode: Option<PowerMizerMode>,
) -> anyhow::Result<()> {
    let mode_info = match device.power_mizer_mode() {
        Ok(info) => info,
        Err(
            NvmlError::NotSupported
            | NvmlError::FunctionNotFound
            | NvmlError::FailedToLoadSymbol(_),
        ) if configured_mode.is_none() => return Ok(()),
        Err(err) => return Err(err).context("Could not get PowerMizer mode"),
    };
    let mode = configured_mode.unwrap_or(PowerMizerMode::Auto);
    let supported = supported_power_mizer_modes(mode_info.supported);

    if !supported.contains(&mode) {
        bail!("PowerMizer mode {mode:?} is not supported by this GPU");
    }

    if mode_info.current == mode {
        return Ok(());
    }

    debug!("setting PowerMizer mode to {mode:?}");
    device
        .set_power_mizer_mode(mode)
        .context("Could not set PowerMizer mode")?;

    Ok(())
}

fn supported_power_mizer_modes(modes: PowerMizerModes) -> Vec<PowerMizerMode> {
    [
        (PowerMizerMode::Auto, PowerMizerModes::AUTO),
        (PowerMizerMode::Adaptive, PowerMizerModes::ADAPTIVE),
        (
            PowerMizerMode::PreferMaximumPerformance,
            PowerMizerModes::PREFER_MAXIMUM_PERFORMANCE,
        ),
        (
            PowerMizerMode::PreferConsistentPerformance,
            PowerMizerModes::PREFER_CONSISTENT_PERFORMANCE,
        ),
    ]
    .into_iter()
    .filter_map(|(mode, flag)| modes.contains(flag).then_some(mode))
    .collect()
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn apply_power_cap(device: &mut Device<'_>, power_cap: Option<f64>) -> anyhow::Result<()> {
    if let Some(cap) = power_cap {
        let cap = (cap * 1000.0) as u32;

        let current_cap = device
            .power_management_limit()
            .context("Could not get current cap")?;

        if current_cap != cap {
            debug!("setting power cap to {cap}");
            device
                .set_power_management_limit(cap)
                .context("Could not set power cap")?;
        }
    } else {
        let current_cap = device.power_management_limit();
        let default_cap = device.power_management_limit_default();

        if let (Ok(current_cap), Ok(default_cap)) = (current_cap, default_cap)
            && current_cap != default_cap
        {
            debug!("resetting power cap to {default_cap}");
            device
                .set_power_management_limit(default_cap)
                .context("Could not reset power cap")?;
        }
    }

    Ok(())
}

impl GpuController for NvidiaGpuController {
    fn controller_type(&self) -> &'static str {
        "nvidia"
    }

    fn controller_info(&self) -> &CommonControllerInfo {
        &self.common
    }

    fn device_type(&self) -> DeviceType {
        // No clue what happens on Tegra chips
        DeviceType::Dedicated
    }

    fn friendly_name(&self) -> Option<String> {
        self.device()
            .name()
            .ok()
            .or_else(|| self.common.pci_info.device_pci_info.model.clone())
    }

    fn get_info(
        &self,
        unique_vendor: bool,
        include_api_info: bool,
    ) -> LocalBoxFuture<'_, DeviceInfo> {
        Box::pin(async move {
            let api_info = if include_api_info {
                self.get_api_info(unique_vendor).await
            } else {
                DeviceApiInfo::default()
            };

            let device = self.device();
            let driver_handle = self.driver_handle.as_ref();

            DeviceInfo {
                pci_info: Some(self.common.pci_info.clone()),
                api_info,
                driver: format!(
                    "nvidia {}",
                    self.nvml.sys_driver_version().unwrap_or_default()
                ), // NVML should always be "nvidia"
                vbios_version: device
                    .vbios_version()
                    .map_err(|err| error!("could not get VBIOS version: {err}"))
                    .ok(),
                link_info: LinkInfo {
                    current_width: device.current_pcie_link_width().map(|v| v.to_string()).ok(),
                    current_speed: device
                        .pcie_link_speed()
                        .map(|v| {
                            let mut output = format!("{} GT/s", v / 1000);
                            if let Ok(link_gen) = device.current_pcie_link_gen() {
                                let _ = write!(output, " Gen {link_gen}");
                            }
                            output
                        })
                        .ok(),
                    max_width: device.max_pcie_link_width().map(|v| v.to_string()).ok(),
                    max_speed: device
                        .max_pcie_link_speed()
                        .ok()
                        .and_then(|v| v.as_integer())
                        .map(|v| {
                            let mut output = format!("{} GT/s", v / 1000);
                            if let Ok(link_gen) = device.max_pcie_link_gen() {
                                let _ = write!(output, " Gen {link_gen}");
                            }
                            output
                        }),
                },
                drm_info: Some(DrmInfo {
                    device_name: device.name().ok(),
                    pci_revision_id: None,
                    family_name: device.architecture().map(|arch| arch.to_string()).ok(),
                    family_id: None,
                    asic_name: None,
                    chip_class: device.architecture().map(|arch| arch.to_string()).ok(),
                    compute_units: None,
                    streaming_multiprocessors: driver_handle
                        .and_then(|handle| handle.get_sm_count().ok()),
                    cuda_cores: device.num_cores().ok(),
                    vram_type: driver_handle
                        .and_then(|handle| handle.get_ram_type().ok())
                        .map(str::to_owned),
                    vram_clock_ratio: 1.0,
                    isa: None,
                    vram_vendor: driver_handle
                        .and_then(|handle| handle.get_ram_vendor().ok())
                        .map(str::to_owned),
                    vram_bit_width: driver_handle.and_then(|handle| handle.get_bus_width().ok()),
                    vram_max_bw: None,
                    cache_info: driver_handle
                        .and_then(|handle| handle.get_l2_cache_size().ok())
                        .map(|size| CacheInfo::Nvidia { l2: size }),
                    rop_info: driver_handle
                        .as_ref()
                        .and_then(|handle| handle.get_rop_info().ok()),
                    memory_info: device
                        .bar1_memory_info()
                        .map(|bar_info| DrmMemoryInfo {
                            cpu_accessible_used: bar_info.used,
                            cpu_accessible_total: bar_info.total,
                            resizeable_bar: device
                                .memory_info()
                                .ok()
                                .map(|memory_info| bar_info.total >= memory_info.total),
                        })
                        .ok(),
                    amd_ip_info: vec![],
                    intel: IntelDrmInfo::default(),
                }),
                flags: vec![
                    DeviceFlag::ConfigurableFanControl,
                    DeviceFlag::AutoFanThreshold,
                ],
            }
        })
    }

    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::too_many_lines
    )]
    fn get_stats(&self, gpu_config: Option<&GpuConfig>) -> DeviceStats {
        let device = self.device();

        let mut temps = IndexMap::new();

        if let Ok(temp) = device.temperature(TemperatureSensor::Gpu) {
            let crit = device
                .temperature_threshold(TemperatureThreshold::Shutdown)
                .map(|value| value as f32)
                .ok();

            let value = Temperature {
                current: Some(temp as f32),
                crit,
                crit_hyst: None,
            };

            temps.insert(
                "GPU".to_owned(),
                TemperatureEntry {
                    value,
                    primary: true,
                    display_only: false,
                },
            );
        }

        let mut extra_clocks = [
            ("SM", device.clock_info(Clock::SM)),
            ("Video", device.clock_info(Clock::Video)),
        ]
        .into_iter()
        .filter_map(|(name, value)| Some((name.to_owned(), u64::from(value.ok()?))))
        .collect::<IndexMap<String, u64>>();
        // RM-measured domains NVML does not report; the graphs window plots
        // every entry of this map, so this is what makes XBAR graphable.
        for (name, kind) in [("XBAR", RmDomainKind::Xbar), ("SYS", RmDomainKind::Sys)] {
            if let Some(mhz) = self.rm_measure_mhz(kind) {
                extra_clocks.insert(name.to_owned(), mhz);
            }
        }

        let mut voltage = None;

        if let Some((nvapi, handle)) = self.nvapi.as_ref() {
            let arch = device.architecture().ok();

            unsafe {
                if let Some(mask) = self.nvapi_therm_channel_mask
                    && let Ok(thermals) = nvapi.therm_channel_get_status(*handle, mask)
                {
                    if let Some(hotspot) = nvapi.read_hotspot(&thermals, *handle, arch.as_ref()) {
                        temps.insert(
                            "GPU Hotspot".to_owned(),
                            TemperatureEntry {
                                value: Temperature {
                                    current: Some(hotspot as f32),
                                    crit: None,
                                    crit_hyst: None,
                                },
                                primary: true,
                                display_only: true,
                            },
                        );
                    }

                    let vram_type = self
                        .driver_handle
                        .as_ref()
                        .and_then(|driver| driver.get_ram_type().ok());

                    if let Some(vram) = thermals.vram(vram_type) {
                        temps.insert(
                            "VRAM".to_owned(),
                            TemperatureEntry {
                                value: Temperature {
                                    current: Some(vram as f32),
                                    crit: None,
                                    crit_hyst: None,
                                },
                                primary: true,
                                display_only: true,
                            },
                        );
                    }

                    match nvapi.read_vram_temps(*handle, vram_type) {
                        Ok(sensors) => {
                            for (label, value) in sensors {
                                temps.insert(
                                    format!("VRAM Chip {label}"),
                                    TemperatureEntry {
                                        value: Temperature {
                                            current: Some(value as f32),
                                            crit: None,
                                            crit_hyst: None,
                                        },
                                        primary: false,
                                        display_only: true,
                                    },
                                );
                            }
                        }
                        Err(err) => {
                            warn!("could not read VRAM sensors: {err:#}");
                        }
                    }
                }

                if let Ok(value) = nvapi.client_volt_rails_get_status(*handle) {
                    voltage = Some(u64::from(value) / 1000);
                }

                if let Ok(clocks) = nvapi.get_all_clocks(*handle) {
                    for domain in NvGpuClockDomainId::EXTRA {
                        if let Some(info) = clocks.get_domain(domain) {
                            extra_clocks
                                .insert(domain.to_string(), u64::from(info.frequency) / 1000);
                        }
                    }
                }
            }
        }

        let fan_settings = gpu_config.and_then(|config| config.fan_control_settings.as_ref());

        let num_fans = device.num_fans().unwrap_or(0);

        let (pwm_current, speed_current) = if num_fans == 0 {
            (None, None)
        } else {
            let pwm_current = average_fan_value(num_fans, |idx| device.fan_speed(idx))
                .map(|avg_speed| (f64::from(avg_speed) * 2.55) as u8);

            let speed_current = average_fan_value(num_fans, |idx| device.fan_speed_rpm(idx));

            (pwm_current, speed_current)
        };

        let vram = device
            .memory_info()
            .map(|info| VramStats {
                total: Some(info.total),
                used: Some(info.used),
                gtt_total_usable: None,
                gtt_used: None,
            })
            .unwrap_or_default();

        let active_pstate = device
            .performance_state()
            .map(|pstate| {
                PowerLevelId::Index(
                    pstate
                        .as_c()
                        .try_into()
                        .expect("Power state always fits in u8"),
                )
            })
            .ok();

        let fan_range = device.min_max_fan_speed().ok();
        let power_mizer_info = device.power_mizer_mode().ok();
        let power_constraints = device.power_management_limit_constraints().ok();

        DeviceStats {
            temps,
            fan: FanStats {
                control_enabled: gpu_config.is_some_and(|config| config.fan_control_enabled),
                control_mode: fan_settings.map(|settings| settings.mode),
                static_speed: fan_settings.map(|settings| settings.static_speed),
                curve: fan_settings.map(|settings| settings.curve.0.clone()),
                spindown_delay_ms: fan_settings.and_then(|settings| settings.spindown_delay_ms),
                change_threshold: fan_settings.and_then(|settings| settings.change_threshold),
                auto_threshold: fan_settings.and_then(|settings| settings.auto_threshold),
                temperature_key: None,
                speed_current,
                speed_max: None,
                speed_min: None,
                pwm_current,
                pwm_max: fan_range.map(|(_, max)| (f64::from(max) * 2.55).round() as u32),
                pwm_min: fan_range.map(|(min, _)| (f64::from(min) * 2.55).round() as u32),
                temperature_range: None,
                pmfw_info: PmfwInfo::default(),
            },
            nvidia_thermal_info: self.get_nvidia_thermal_info(),
            active_power_mizer_mode: power_mizer_info.as_ref().map(|info| info.current),
            supported_power_mizer_modes: power_mizer_info
                .map(|info| supported_power_mizer_modes(info.supported)),
            power: PowerStats {
                average: None,
                current: device.power_usage().map(|mw| f64::from(mw) / 1000.0).ok(),
                cap_current: device
                    .power_management_limit()
                    .map(|mw| f64::from(mw) / 1000.0)
                    .ok(),
                cap_max: power_constraints
                    .as_ref()
                    .map(|constraints| f64::from(constraints.max_limit) / 1000.0),
                cap_min: power_constraints
                    .as_ref()
                    .map(|constraints| f64::from(constraints.min_limit) / 1000.0),
                cap_default: device
                    .power_management_limit_default()
                    .map(|mw| f64::from(mw) / 1000.0)
                    .ok(),
                sensors: HashMap::new(),
            },
            busy_percent: device
                .utilization_rates()
                .map(|utilization| u8::try_from(utilization.gpu).expect("Invalid percentage"))
                .ok(),
            vram,
            clockspeed: ClockspeedStats {
                gpu_clockspeed: device.clock_info(Clock::Graphics).map(Into::into).ok(),
                vram_clockspeed: device.clock_info(Clock::Memory).map(Into::into).ok(),
                target_gpu_clockspeed: None,
                xbar_clockspeed: self.rm_measure_mhz(RmDomainKind::Xbar),
                sys_clockspeed: self.rm_measure_mhz(RmDomainKind::Sys),
                video_clockspeed: self.rm_measure_mhz(RmDomainKind::Video),
                sensors: extra_clocks,
            },
            throttle_info: device.current_throttle_reasons().ok().map(|reasons| {
                reasons
                    .iter()
                    .filter(|reason| *reason != ThrottleReasons::GPU_IDLE)
                    .map(|reason| {
                        let mut name = String::new();
                        bitflags::parser::to_writer(&reason, &mut name).unwrap();
                        (name, vec![])
                    })
                    .collect()
            }),
            voltage: VoltageStats {
                gpu: voltage,
                sensors: self.rm_voltage_sensors(),
            },
            perf_limits: self.rm_perf_limits(),
            performance_level: None,
            active_power_states: active_pstate.map(|active_pstate| ActivePowerStates {
                core: Some(active_pstate),
                memory: Some(active_pstate),
                pcie: None,
            }),
        }
    }

    #[allow(clippy::cast_possible_wrap)]
    fn get_clocks_info(&self, _gpu_config: Option<&GpuConfig>) -> anyhow::Result<ClocksInfo> {
        let device = self.device();

        let mut gpu_offsets = IndexMap::new();
        let mut mem_offsets = IndexMap::new();
        let mut gpu_clock_range = None;
        let mut vram_clock_range = None;

        let supported_pstates = device.supported_performance_states()?;

        let mut clock_types = [
            (Clock::Graphics, &mut gpu_offsets, &mut gpu_clock_range),
            (Clock::Memory, &mut mem_offsets, &mut vram_clock_range),
        ];

        for pstate in supported_pstates.iter().rev() {
            for (clock_type, offsets, clock_range) in &mut clock_types {
                if let Ok(offset) = device.clock_offset(*clock_type, *pstate) {
                    let mut offset = NvidiaClockOffset {
                        current: offset.clock_offset_mhz,
                        min: offset.min_clock_offset_mhz,
                        max: offset.max_clock_offset_mhz,
                    };

                    // On some driver versions, the applied offset values are not reported.
                    // In these scenarios we must store them manually for reporting.
                    if offset.current == 0
                        && let Some(applied_offsets) =
                            self.last_applied_offsets.borrow().get(clock_type)
                        && let Some(applied_offset) = applied_offsets.get(pstate)
                    {
                        offset.current = *applied_offset;
                    }

                    offsets.insert(pstate.as_c(), offset);
                }

                // Find min and max clockspeeds from any pstate
                if let Ok((pstate_min, pstate_max)) =
                    device.min_max_clock_of_pstate(*clock_type, *pstate)
                {
                    match clock_range {
                        Some((current_min, current_max)) => {
                            *current_min = cmp::min(*current_min, pstate_min);
                            *current_max = cmp::max(*current_max, pstate_max);
                        }
                        None => {
                            **clock_range = Some((pstate_min, pstate_max));
                        }
                    }
                }
            }
        }

        let gpu_vf_curve = self
            .get_vf_curve()
            .inspect_err(|err| warn!("could not get VF curve: {err:#}"))
            .unwrap_or_default();

        let voltage_boost = self
            .get_voltage_boost()
            .inspect_err(|err| warn!("could not get voltage boost: {err:#}"))
            .ok();

        let rm_block = self.rm().and_then(|(handle, _)| {
            handle
                .clk_domains_get_control()
                .inspect_err(|err| warn!("could not read RM clock control block: {err:#}"))
                .ok()
        });

        let table = NvidiaClocksTable {
            gpu_offsets,
            mem_offsets,
            gpu_locked_clocks: *self.last_applied_gpu_locked_clocks.borrow(),
            vram_locked_clocks: *self.last_applied_vram_locked_clocks.borrow(),
            gpu_clock_range,
            vram_clock_range,
            gpu_vf_curve,
            voltage_boost,
            xbar_offset: rm_block
                .as_ref()
                .and_then(|b| self.rm_freq_offset(b, RmDomainKind::Xbar)),
            sys_offset: rm_block
                .as_ref()
                .and_then(|b| self.rm_freq_offset(b, RmDomainKind::Sys)),
            video_offset: rm_block
                .as_ref()
                .and_then(|b| self.rm_freq_offset(b, RmDomainKind::Video)),
            msvdd_offset: rm_block
                .as_ref()
                .and_then(|b| self.rm_rail_offset(b, RmDomainKind::Xbar)),
            nvvdd_offset: rm_block
                .as_ref()
                .and_then(|b| self.rm_rail_offset(b, RmDomainKind::Gpc)),
            sys_voltage_offset: rm_block
                .as_ref()
                .and_then(|b| self.rm_rail_offset(b, RmDomainKind::Sys)),
            video_voltage_offset: rm_block
                .as_ref()
                .and_then(|b| self.rm_rail_offset(b, RmDomainKind::Video)),
            gpc_xbar_ratio: self.rm_propagation_ratio(),
            voltage_rails: self.rm_voltage_rails(),
            rm_clock_domains: rm_block
                .as_ref()
                .map(|b| self.rm_domain_table(b))
                .unwrap_or_default(),
        };

        Ok(ClocksInfo {
            table: Some(ClocksTable::Nvidia(table)),
            ..Default::default()
        })
    }

    fn get_power_states(&self, _gpu_config: Option<&GpuConfig>) -> PowerStates {
        self.try_get_power_states().unwrap_or_else(|err| {
            warn!("could not get pstates info: {err:#}");
            PowerStates::default()
        })
    }

    fn get_power_profile_modes(&self) -> anyhow::Result<PowerProfileModesTable> {
        Err(anyhow!("Not supported on Nvidia"))
    }

    fn vbios_dump(&self) -> anyhow::Result<Vec<u8>> {
        Err(anyhow!("Not supported on Nvidia"))
    }

    #[allow(clippy::cast_possible_wrap, clippy::cast_sign_loss)]
    fn apply_config<'a>(&'a self, config: &'a GpuConfig) -> LocalBoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async {
            let mut device = self.device();

            apply_power_cap(&mut device, config.power_cap)?;
            apply_power_mizer_mode(&mut device, config.power_mizer_mode)?;

            self.reset_clocks()?;

            let clocks = &config.clocks_configuration;

            match (clocks.min_core_clock, clocks.max_core_clock) {
                (Some(min), Some(max)) => {
                    debug!("applying GPU locked clocks: {min}..{max}");
                    device
                        .set_gpu_locked_clocks(GpuLockedClocksSetting::Numeric {
                            min_clock_mhz: min as u32,
                            max_clock_mhz: max as u32,
                        })
                        .context("Could not apply GPU locked clocks")?;
                    self.last_applied_gpu_locked_clocks
                        .replace(Some((min as u32, max as u32)));
                }
                (None, None) => (),
                _ => bail!("Min and max GPU clock must be set together"),
            }

            match (clocks.min_memory_clock, clocks.max_memory_clock) {
                (Some(min), Some(max)) => {
                    debug!("applying VRAM locked clocks: {min}..{max}");
                    device
                        .set_mem_locked_clocks(min as u32, max as u32)
                        .context("Could not apply VRAM locked clocks")?;
                    self.last_applied_vram_locked_clocks
                        .replace(Some((min as u32, max as u32)));
                }
                (None, None) => (),
                _ => bail!("Min and max VRAM clock must be set together"),
            }

            for (pstate, offset) in &clocks.gpu_clock_offsets {
                let pstate = PerformanceState::try_from(*pstate)
                    .map_err(|_| anyhow!("Invalid pstate '{pstate}'"))?;
                debug!("applying offset {offset} for GPU pstate {pstate:?}");
                device
                    .set_clock_offset(Clock::Graphics, pstate, *offset)
                    .with_context(|| {
                        format!("Could not set clock offset {offset} for GPU pstate {pstate:?}")
                    })?;

                self.last_applied_offsets
                    .borrow_mut()
                    .entry(Clock::Graphics)
                    .or_default()
                    .insert(pstate, *offset);
            }

            for (pstate, offset) in &clocks.mem_clock_offsets {
                let pstate = PerformanceState::try_from(*pstate)
                    .map_err(|_| anyhow!("Invalid pstate '{pstate}'"))?;
                debug!("applying offset {offset} for VRAM pstate {pstate:?}");
                device
                    .set_clock_offset(Clock::Memory, pstate, *offset)
                    .with_context(|| {
                        format!("Could not set clock offset {offset} for VRAM pstate {pstate:?}")
                    })?;

                self.last_applied_offsets
                    .borrow_mut()
                    .entry(Clock::Memory)
                    .or_default()
                    .insert(pstate, *offset);
            }

            if !clocks.nvidia_gpu_vf_curve.is_empty() {
                self.apply_vf_curve(&clocks.nvidia_gpu_vf_curve)
                    .context("Could not apply VF curve")?;
            }

            if let Some(percent) = clocks.voltage_boost
                && let Err(err) = self.apply_voltage_boost(percent)
            {
                warn!("could not apply voltage boost: {err:#}");

                if self.voltage_boost_written.get()
                    && let Err(err) = self.reset_voltage_boost()
                {
                    warn!("could not reset voltage boost: {err:#}");
                }
            }

            self.apply_rm_offsets(clocks)?;
            self.apply_rm_propagation(clocks)?;
            self.apply_rm_rail_limits(clocks)?;

            if config.fan_control_enabled {
                let settings = config
                    .fan_control_settings
                    .as_ref()
                    .context("Fan control enabled with no settings")?;
                match settings.mode {
                    FanControlMode::Static => {
                        self.stop_fan_control()
                            .await
                            .context("Could not reset fan control")?;

                        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
                        let speed = (settings.static_speed * 100.0) as u32;

                        let fan_count = device.num_fans().context("Could not get fan count")?;
                        for fan in 0..fan_count {
                            device
                                .set_fan_speed(fan, speed)
                                .context("Could not reset fan speed to default")?;
                        }
                    }

                    FanControlMode::Curve => {
                        let (min_speed, max_speed) = device
                            .min_max_fan_speed()
                            .context("Could not get fan speed range")?;

                        for point in settings.curve.0.values() {
                            #[allow(clippy::cast_possible_truncation)]
                            if !(min_speed..=max_speed).contains(&((*point * 100.0) as u32)) {
                                bail!(
                                    "Fan speed {}% outside of the allowed range {min_speed}% to {max_speed}%",
                                    point * 100.0
                                );
                            }
                        }

                        self.start_curve_fan_control_task(settings.curve.clone(), settings.clone())
                            .await?;
                    }
                }
            } else {
                self.stop_fan_control()
                    .await
                    .context("Could not reset fan control")?;
            }

            if let Some(target_temp_info) = self.get_target_temp()
                && let Some((min, max)) = target_temp_info.allowed_range
            {
                if let Some(target_temp) = config.nvidia_thermal_options.target_temperature {
                    let target_temp = target_temp.clamp(min, max);

                    if target_temp_info.current != target_temp {
                        debug!("setting target temperature to {target_temp}");
                        if let Err(err) = device.set_temperature_threshold(
                            TemperatureThreshold::AcousticCurr,
                            target_temp.cast_signed(),
                        ) {
                            warn!("Could not set target temperature: {err:#}");
                        }
                    }
                } else if let Err(err) = self.reset_target_temp() {
                    warn!("could not reset target temperature: {err:#}");
                }
            }

            Ok(())
        })
    }

    fn reset_clocks(&self) -> anyhow::Result<()> {
        let mut device = self.device();

        if let Ok(supported_pstates) = device.supported_performance_states() {
            for pstate in supported_pstates {
                for clock_type in [Clock::Graphics, Clock::Memory] {
                    if let Ok(current_offset) = device.clock_offset(clock_type, pstate)
                        && (current_offset.clock_offset_mhz != 0
                            || self
                                .last_applied_offsets
                                .borrow()
                                .get(&clock_type)
                                .and_then(|applied_offsets| applied_offsets.get(&pstate))
                                .is_some_and(|offset| *offset != 0))
                    {
                        debug!("resetting clock offset for {clock_type:?} pstate {pstate:?}");
                        if let Err(err) = device.set_clock_offset(clock_type, pstate, 0) {
                            warn!("could not reset {clock_type:?} pstate {pstate:?}: {err:#}");
                        }
                    }

                    if let Some(applied_offsets) =
                        self.last_applied_offsets.borrow_mut().get_mut(&clock_type)
                    {
                        applied_offsets.remove(&pstate);
                    }
                }
            }
        }

        if self.last_applied_gpu_locked_clocks.borrow().is_some() {
            device
                .reset_gpu_locked_clocks()
                .context("Could not reset locked GPU clocks")?;
            self.last_applied_gpu_locked_clocks.take();
        }

        if self.last_applied_vram_locked_clocks.borrow().is_some() {
            device
                .reset_mem_locked_clocks()
                .context("Could not reset locked GPU clocks")?;
            self.last_applied_vram_locked_clocks.take();
        }

        if self.vf_curve_written.get() {
            self.reset_vf_curve().context("Could not reset VF curve")?;
        }

        if self.voltage_boost_written.get() {
            self.reset_voltage_boost()
                .context("Could not reset voltage boost")?;
        }

        if let Some((handle, _)) = self.rm()
            && handle
                .clk_domains_reset()
                .context("Could not reset RM clock domain offsets")?
        {
            debug!("reset RM clock domain offsets to stock");
        }

        if let (Some(handle), Some(rel)) = (self.driver_handle.as_ref(), self.rm_prop.as_ref())
            && rel.read_raw(handle)? != rel.factory_raw
        {
            rel.set_raw(handle, rel.factory_raw)
                .context("Could not reset the GPC→XBAR propagation ratio")?;
            debug!("reset GPC→XBAR propagation ratio to factory");
        }

        if let (Some(handle), Some(volt)) = (self.driver_handle.as_ref(), self.rm_volt.as_ref())
            && volt
                .set_limit_deltas(handle, volt.start_deltas_uv())
                .context("Could not reset the rail limit deltas")?
        {
            debug!("reset rail limit deltas to the values found at start");
        }

        Ok(())
    }

    fn cleanup(&self) -> LocalBoxFuture<'_, ()> {
        async {
            if let Some((fan_notify, fan_handle)) = self.fan_control_handle.take() {
                debug!("sending stop notification to old fan control task");
                fan_notify.notify_one();
                fan_handle.await.unwrap();
                debug!("finished controller cleanup");
            }
        }
        .boxed_local()
    }

    fn process_list(&self) -> anyhow::Result<ProcessList> {
        fn map_process(
            process: &nvml_wrapper::struct_wrappers::device::ProcessInfo,
            process_type: ProcessType,
        ) -> ProcessInfo {
            #[allow(clippy::cast_possible_wrap)]
            let (name, args) = resolve_process_name((process.pid as i32).into())
                .unwrap_or_else(|_| ("<Unknown>".to_owned(), String::new()));

            ProcessInfo {
                name,
                args,
                memory_used: match process.used_gpu_memory {
                    UsedGpuMemory::Used(size) => size,
                    UsedGpuMemory::Unavailable => 0,
                },
                types: vec![process_type],
                util: SUPPORTED_UTIL_TYPES.iter().map(|util| (*util, 0)).collect(),
            }
        }

        let device = self.device();

        let mut processes = BTreeMap::new();

        for process in device
            .running_graphics_processes()
            .context("Could not get graphics processes")?
        {
            processes.insert(process.pid, map_process(&process, ProcessType::Graphics));
        }

        for process in device
            .running_compute_processes()
            .context("Could not get compute processes")?
        {
            match processes.entry(process.pid) {
                Entry::Vacant(entry) => {
                    entry.insert(map_process(&process, ProcessType::Compute));
                }
                Entry::Occupied(mut entry) => {
                    entry.get_mut().types.push(ProcessType::Compute);
                }
            }
        }

        match device.process_utilization_stats(self.last_util_timestamp.get()) {
            Ok(stats) => {
                if let Some(stat) = stats.first() {
                    self.last_util_timestamp.set(Some(stat.timestamp));
                }

                for stat in stats {
                    if let Some(info) = processes.get_mut(&stat.pid) {
                        info.util
                            .insert(ProcessUtilizationType::Graphics, stat.sm_util);
                        info.util
                            .insert(ProcessUtilizationType::Memory, stat.mem_util);
                        info.util
                            .insert(ProcessUtilizationType::Encode, stat.enc_util);
                        info.util
                            .insert(ProcessUtilizationType::Decode, stat.dec_util);
                    }
                }
            }
            Err(NvmlError::NotFound) => (),
            Err(err) => {
                error!("could not get process util stats: {err}");
            }
        }
        Ok(ProcessList {
            processes,
            supported_util_types: SUPPORTED_UTIL_TYPES.iter().copied().collect(),
        })
    }

    #[cfg(feature = "display-info")]
    fn populate_displays_info(&self, info: &mut lact_schema::DisplaysInfo) -> anyhow::Result<()> {
        use lact_schema::DisplayConnector;
        use std::os::fd::AsRawFd as _;

        if let Some(handle) = &self.driver_handle {
            let drm_file = self
                .common
                .open_drm_render()
                .context("Could not open DRM file")?;

            for (key, display_info) in &mut info.displays {
                match driver::connector_id_to_display_id(
                    display_info.connector_id,
                    drm_file.as_raw_fd(),
                ) {
                    Ok(display_id) => {
                        if let DisplayConnector::DisplayPort {
                            lanes, bandwidth, ..
                        } = &mut display_info.connector_type
                        {
                            match handle.get_dp_link_config(display_id) {
                                Ok(params) => {
                                    *lanes = Some(params.laneCount.try_into()?);
                                    *bandwidth = Some(if params.linkBW != 0 {
                                        crate::server::display::dp1_rate_to_bandwidth(params.linkBW)
                                    } else {
                                        crate::server::display::dp2_rate_to_bandwidth(
                                            params.dp2LinkBW,
                                        )
                                    });
                                }
                                Err(err) => {
                                    warn!("could not fetch DP info for display {key}: {err:#}");
                                }
                            }
                        }
                    }
                    Err(err) => {
                        warn!(
                            "could not resolve display '{key}' into the driver display id: {err:#}"
                        );
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CLOCK_CLIENT_CLK_VF_POINT_TYPE_PROG, ClockClientClkVfPointInfoV1,
        ClockClientClkVfPointsControlV1, ClockClientClkVfPointsInfoV1,
        ClockClientClkVfPointsStatusV3, build_curve_control, build_vf_curve,
    };
    use indexmap::IndexMap;
    use lact_schema::config::NvidiaCurvePoint;

    const OFFSET_RANGE: (i32, i32) = (-1000, 1000);

    #[derive(Clone, Copy)]
    struct TestPoint {
        voltage_mv: u32,
        base_freq_mhz: u32,
        offset_mhz: i32,
        /// Difference between the materialized frequency and `base + offset`
        correction_mhz: i32,
    }

    impl TestPoint {
        fn new(voltage_mv: u32, base_freq_mhz: u32, offset_mhz: i32) -> Self {
            Self {
                voltage_mv,
                base_freq_mhz,
                offset_mhz,
                correction_mhz: 0,
            }
        }

        fn with_correction(mut self, correction_mhz: i32) -> Self {
            self.correction_mhz = correction_mhz;
            self
        }
    }

    fn tables(
        points: &[TestPoint],
        base_tuple_supported: bool,
    ) -> (
        ClockClientClkVfPointsInfoV1,
        ClockClientClkVfPointsStatusV3,
        ClockClientClkVfPointsControlV1,
    ) {
        let mut info = ClockClientClkVfPointsInfoV1::default();
        let mut status = ClockClientClkVfPointsStatusV3::default();
        let mut control = ClockClientClkVfPointsControlV1::default();

        status.b_vf_tuple_base_supported = u8::from(base_tuple_supported);

        for (i, point) in points.iter().enumerate() {
            info.vf_points_mask[i / 32] |= 1 << (i % 32);

            info.vf_points[i] = ClockClientClkVfPointInfoV1 {
                type_: CLOCK_CLIENT_CLK_VF_POINT_TYPE_PROG,
                b_voltage_based: 1,
                rsvd: [0; 16],
            };

            let materialized_mhz =
                point.base_freq_mhz.cast_signed() + point.offset_mhz + point.correction_mhz;

            status.vf_points[i].voltage_uv = point.voltage_mv * 1000;
            status.vf_points[i].freq_khz = (materialized_mhz * 1000).cast_unsigned();
            status.vf_points[i].vf_tuple_base.voltage_uv = point.voltage_mv * 1000;
            status.vf_points[i].vf_tuple_base.freq_khz = point.base_freq_mhz * 1000;

            control.vf_points[i].data.prog.freq_offset_khz = point.offset_mhz * 1000;
        }

        (info, status, control)
    }

    fn written_offsets(control: &ClockClientClkVfPointsControlV1, count: usize) -> Vec<i32> {
        (0..count)
            .map(|i| unsafe { control.vf_points[i].data.prog.freq_offset_khz })
            .collect()
    }

    fn offset_curve(points: &[(u8, i32)]) -> IndexMap<u8, NvidiaCurvePoint> {
        points
            .iter()
            .map(|(index, offset)| {
                (
                    *index,
                    NvidiaCurvePoint {
                        clockspeed_offset: *offset,
                        voltage: None,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn reports_offset_and_base_from_the_tables() {
        let points = [
            TestPoint::new(450, 270, 90),
            TestPoint::new(1195, 2880, -60).with_correction(15),
        ];
        let (info, status, control) = tables(&points, true);

        let curve = build_vf_curve(&info, &status, &control);

        assert_eq!(2, curve.len());

        assert_eq!(0, curve[0].index);
        assert_eq!(450, curve[0].voltage);
        assert_eq!(270, curve[0].base_freq);
        assert_eq!(90, curve[0].freq_offset);
        assert_eq!(360, curve[0].freq);

        assert_eq!(2880, curve[1].base_freq);
        assert_eq!(-60, curve[1].freq_offset);
        assert_eq!(2835, curve[1].freq);
    }

    #[test]
    fn derives_base_from_the_offset_without_the_base_tuple() {
        let points = [TestPoint::new(450, 270, 90), TestPoint::new(600, 1000, -45)];
        let (info, status, control) = tables(&points, false);

        let curve = build_vf_curve(&info, &status, &control);

        assert_eq!(270, curve[0].base_freq);
        assert_eq!(90, curve[0].freq_offset);
        assert_eq!(1000, curve[1].base_freq);
        assert_eq!(-45, curve[1].freq_offset);
    }

    #[test]
    fn skips_non_editable_points() {
        let points = [TestPoint::new(450, 270, 90), TestPoint::new(600, 1000, 0)];
        let (mut info, status, control) = tables(&points, true);
        info.vf_points[0].b_voltage_based = 0;

        let curve = build_vf_curve(&info, &status, &control);

        assert_eq!(1, curve.len());
        assert_eq!(1, curve[0].index);
    }

    #[test]
    fn keeps_the_offsets_of_unconfigured_points() {
        let points = [TestPoint::new(450, 270, 90), TestPoint::new(600, 1000, 45)];
        let (info, status, control) = tables(&points, true);

        let result = build_curve_control(
            &offset_curve(&[(0, -15)]),
            &info,
            &status,
            control,
            OFFSET_RANGE,
        )
        .expect("apply failed");

        assert_eq!(vec![-15_000, 45_000], written_offsets(&result, 2));
    }

    #[test]
    fn rejects_an_offset_outside_of_the_range() {
        let points = [TestPoint::new(450, 270, 0)];
        let (info, status, control) = tables(&points, true);

        let err = build_curve_control(
            &offset_curve(&[(0, 1500)]),
            &info,
            &status,
            control,
            OFFSET_RANGE,
        )
        .map(|_| ())
        .expect_err("out of range offset was accepted");
        assert!(
            err.to_string().contains("outside of the allowed range"),
            "{err}"
        );
    }

    #[test]
    fn clamps_an_offset_that_would_reduce_the_frequency_to_zero() {
        let points = [TestPoint::new(450, 270, 0)];
        let (info, status, control) = tables(&points, true);

        // Below the reported minimum and equal to `-base`, so clamp to a 1 MHz frequency
        let result = build_curve_control(
            &offset_curve(&[(0, -270)]),
            &info,
            &status,
            control,
            (-100, 1000),
        )
        .expect("apply failed");
        assert_eq!(vec![-269_000], written_offsets(&result, 1));
    }

    #[test]
    fn rejects_points_that_do_not_exist_or_do_not_match() {
        let points = [TestPoint::new(450, 270, 0)];

        let invalid_curves = [
            IndexMap::from([(
                0,
                NvidiaCurvePoint {
                    clockspeed_offset: 90,
                    // Stale voltage, the point layout changed under the configuration
                    voltage: Some(500),
                },
            )]),
            offset_curve(&[(7, 45)]),
            // The point arrays hold 255 entries, so this index cannot be used to index them
            offset_curve(&[(u8::MAX, 30)]),
        ];

        for curve in invalid_curves {
            let (info, status, control) = tables(&points, true);

            build_curve_control(&curve, &info, &status, control, OFFSET_RANGE)
                .map(|_| ())
                .expect_err("an invalid point was accepted");
        }
    }
}
