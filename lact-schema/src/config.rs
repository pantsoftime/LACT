use amdgpu_sysfs::gpu_handle::{PerformanceLevel, PowerLevelKind};
use indexmap::IndexMap;
use nvml_wrapper::enums::device::PowerMizerMode;
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;

use crate::{
    FanControlMode, FanCurveMap, NvidiaThermalOptions, PmfwOptions, ProfileRule, default_fan_curve,
    request::{ClockspeedType, SetClocksCommand},
};

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct Profile {
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub gpus: IndexMap<String, GpuConfig>,
    pub rule: Option<ProfileRule>,
    #[serde(default, skip_serializing_if = "ProfileHooks::is_empty")]
    pub hooks: ProfileHooks,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct ProfileHooks {
    pub activated: Option<String>,
    pub deactivated: Option<String>,
}

impl ProfileHooks {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct GpuConfig {
    #[serde(default)]
    pub fan_control_enabled: bool,
    pub fan_control_settings: Option<FanControlSettings>,
    #[serde(default, skip_serializing_if = "PmfwOptions::is_empty")]
    pub pmfw_options: PmfwOptions,
    #[serde(default, skip_serializing_if = "NvidiaThermalOptions::is_empty")]
    pub nvidia_thermal_options: NvidiaThermalOptions,
    pub power_mizer_mode: Option<PowerMizerMode>,
    pub power_cap: Option<f64>,
    pub performance_level: Option<PerformanceLevel>,
    #[serde(default, flatten)]
    pub clocks_configuration: ClocksConfiguration,
    pub power_profile_mode_index: Option<u16>,
    /// Outer vector is for power profile components, inner vector is for the heuristics within a component
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom_power_profile_mode_hueristics: Vec<Vec<Option<i32>>>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub power_states: IndexMap<PowerLevelKind, Vec<u8>>,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ClocksConfiguration {
    pub min_core_clock: Option<i32>,
    pub min_memory_clock: Option<i32>,
    pub min_voltage: Option<i32>,
    pub max_core_clock: Option<i32>,
    pub max_memory_clock: Option<i32>,
    pub max_voltage: Option<i32>,
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "int_map::deserialize"
    )]
    pub gpu_clock_offsets: IndexMap<u32, i32>,
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "int_map::deserialize"
    )]
    pub mem_clock_offsets: IndexMap<u32, i32>,
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "int_map::deserialize"
    )]
    pub gpu_vf_curve: IndexMap<u8, CurvePoint>,
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "int_map::deserialize"
    )]
    pub mem_vf_curve: IndexMap<u8, CurvePoint>,
    #[serde(
        default,
        skip_serializing_if = "IndexMap::is_empty",
        deserialize_with = "int_map::deserialize"
    )]
    pub nvidia_gpu_vf_curve: IndexMap<u8, NvidiaCurvePoint>,
    pub voltage_offset: Option<i32>,
    pub voltage_boost: Option<i32>,
    /// NVIDIA-only: frequency offsets (MHz) for the RM ClockClient domains that
    /// NVML does not expose. Applied through the private control block; the
    /// daemon refuses them on driver branches it has not verified.
    pub xbar_clock_offset: Option<i32>,
    pub sys_clock_offset: Option<i32>,
    pub video_clock_offset: Option<i32>,
    /// NVIDIA-only: MSVDD voltage demand offset of the XBAR domain, millivolts.
    pub msvdd_offset: Option<i32>,
    /// NVIDIA-only: NVVDD voltage demand offset of the GPC (core) domain, millivolts.
    pub nvvdd_offset: Option<i32>,
    /// NVIDIA-only: voltage demand offsets of the SYS and video domains, millivolts.
    pub sys_voltage_offset: Option<i32>,
    pub video_voltage_offset: Option<i32>,
    /// NVIDIA-only: GPC→XBAR clock propagation ratio × 1000 (900 = 0.900).
    /// `None` = the factory relation.
    pub gpc_xbar_ratio_milli: Option<i32>,
    /// NVIDIA-only: voltage-policy limit deltas per rail, millivolts, written
    /// as absolute values into the rail control object (`None` = the value
    /// the daemon found at start, i.e. the firmware default unless a delta
    /// was left applied across a daemon restart).
    pub nvvdd_vmin_delta_mv: Option<i32>,
    pub nvvdd_rel_delta_mv: Option<i32>,
    pub nvvdd_alt_rel_delta_mv: Option<i32>,
    pub nvvdd_ov_delta_mv: Option<i32>,
    pub msvdd_vmin_delta_mv: Option<i32>,
    pub msvdd_rel_delta_mv: Option<i32>,
    pub msvdd_alt_rel_delta_mv: Option<i32>,
    pub msvdd_ov_delta_mv: Option<i32>,
}

impl ClocksConfiguration {
    pub fn rail_limit_delta(&self, rail: u8, limit: crate::RailLimit) -> Option<i32> {
        use crate::RailLimit::*;
        match (rail, limit) {
            (0, Vmin) => self.nvvdd_vmin_delta_mv,
            (0, Rel) => self.nvvdd_rel_delta_mv,
            (0, AltRel) => self.nvvdd_alt_rel_delta_mv,
            (0, Ov) => self.nvvdd_ov_delta_mv,
            (1, Vmin) => self.msvdd_vmin_delta_mv,
            (1, Rel) => self.msvdd_rel_delta_mv,
            (1, AltRel) => self.msvdd_alt_rel_delta_mv,
            (1, Ov) => self.msvdd_ov_delta_mv,
            _ => None,
        }
    }

    pub fn set_rail_limit_delta(&mut self, rail: u8, limit: crate::RailLimit, value: Option<i32>) {
        use crate::RailLimit::*;
        let slot = match (rail, limit) {
            (0, Vmin) => &mut self.nvvdd_vmin_delta_mv,
            (0, Rel) => &mut self.nvvdd_rel_delta_mv,
            (0, AltRel) => &mut self.nvvdd_alt_rel_delta_mv,
            (0, Ov) => &mut self.nvvdd_ov_delta_mv,
            (1, Vmin) => &mut self.msvdd_vmin_delta_mv,
            (1, Rel) => &mut self.msvdd_rel_delta_mv,
            (1, AltRel) => &mut self.msvdd_alt_rel_delta_mv,
            (1, Ov) => &mut self.msvdd_ov_delta_mv,
            _ => return,
        };
        *slot = value;
    }

    pub fn any_rail_limit_delta(&self) -> bool {
        (0..2u8).any(|rail| crate::RailLimit::ALL.iter().any(|l| self.rail_limit_delta(rail, *l).is_some()))
    }
}

impl ClocksConfiguration {
    pub fn apply_clocks_command(&mut self, command: &SetClocksCommand) {
        let value = command.value;
        match command.r#type {
            ClockspeedType::MaxCoreClock => self.max_core_clock = value,
            ClockspeedType::MaxMemoryClock => self.max_memory_clock = value,
            ClockspeedType::MaxVoltage => self.max_voltage = value,
            ClockspeedType::MinCoreClock => self.min_core_clock = value,
            ClockspeedType::MinMemoryClock => self.min_memory_clock = value,
            ClockspeedType::MinVoltage => self.min_voltage = value,
            ClockspeedType::VoltageOffset => self.voltage_offset = value,
            ClockspeedType::VoltageBoost => self.voltage_boost = value,
            ClockspeedType::GpuClockOffset(pstate) => match value {
                Some(value) => {
                    self.gpu_clock_offsets.insert(pstate, value);
                }
                None => {
                    self.gpu_clock_offsets.shift_remove(&pstate);
                }
            },
            ClockspeedType::MemClockOffset(pstate) => match value {
                Some(value) => {
                    self.mem_clock_offsets.insert(pstate, value);
                }
                None => {
                    self.mem_clock_offsets.shift_remove(&pstate);
                }
            },
            ClockspeedType::GpuVfCurveClock(point) => {
                self.gpu_vf_curve.entry(point).or_default().clockspeed = value;
            }
            ClockspeedType::GpuVfCurveVoltage(point) => {
                self.gpu_vf_curve.entry(point).or_default().voltage = value;
            }
            ClockspeedType::MemVfCurveClock(point) => {
                self.mem_vf_curve.entry(point).or_default().clockspeed = value;
            }
            ClockspeedType::MemVfCurveVoltage(point) => {
                self.mem_vf_curve.entry(point).or_default().voltage = value;
            }
            ClockspeedType::XbarClockOffset => self.xbar_clock_offset = value,
            ClockspeedType::SysClockOffset => self.sys_clock_offset = value,
            ClockspeedType::VideoClockOffset => self.video_clock_offset = value,
            ClockspeedType::MsvddOffset => self.msvdd_offset = value,
            ClockspeedType::NvvddOffset => self.nvvdd_offset = value,
            ClockspeedType::SysVoltageOffset => self.sys_voltage_offset = value,
            ClockspeedType::VideoVoltageOffset => self.video_voltage_offset = value,
            ClockspeedType::GpcXbarRatioMilli => self.gpc_xbar_ratio_milli = value,
            ClockspeedType::RailLimitDelta(rail, limit) => self.set_rail_limit_delta(rail, limit, value),
            ClockspeedType::Reset => {
                *self = ClocksConfiguration::default();
            }
        }
    }
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CurvePoint {
    pub voltage: Option<i32>,
    pub clockspeed: Option<i32>,
}

#[skip_serializing_none]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct NvidiaCurvePoint {
    pub clockspeed_offset: i32,
    pub voltage: Option<u32>,
}

mod int_map {
    use indexmap::IndexMap;
    use serde::{Deserialize, Deserializer, de::Error};
    use serde_json::Value;
    use std::hash::Hash;
    use std::str::FromStr;

    pub fn deserialize<'a, D, K, V>(deserializer: D) -> Result<IndexMap<K, V>, D::Error>
    where
        D: Deserializer<'a>,
        K: Deserialize<'a> + Hash + Eq + TryFrom<i64> + FromStr,
        V: Deserialize<'a>,
    {
        let map: IndexMap<Value, V> = IndexMap::deserialize(deserializer)?;

        map.into_iter()
            .map(|(key, value)| {
                let parsed_key = match &key {
                    Value::Number(number) => number.as_i64().and_then(|val| K::try_from(val).ok()),
                    Value::String(s) => s.parse::<K>().ok(),
                    _ => None,
                };
                let key =
                    parsed_key.ok_or_else(|| D::Error::custom(format!("Invalid key {key}")))?;

                Ok((key, value))
            })
            .collect()
    }
}

impl GpuConfig {
    pub fn is_core_clocks_used(&self) -> bool {
        self.clocks_configuration != ClocksConfiguration::default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FanCurve(pub FanCurveMap);

impl Default for FanCurve {
    fn default() -> Self {
        Self(default_fan_curve())
    }
}

#[skip_serializing_none]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FanControlSettings {
    #[serde(default)]
    pub mode: FanControlMode,
    #[serde(default = "default_fan_static_speed")]
    pub static_speed: f32,
    pub temperature_key: String,
    pub interval_ms: u64,
    pub curve: FanCurve,
    pub spindown_delay_ms: Option<u64>,
    pub change_threshold: Option<u64>,
    pub auto_threshold: Option<u64>,
}

impl Default for FanControlSettings {
    fn default() -> Self {
        Self {
            mode: FanControlMode::default(),
            static_speed: default_fan_static_speed(),
            temperature_key: "edge".to_owned(),
            interval_ms: 500,
            curve: FanCurve(default_fan_curve()),
            spindown_delay_ms: None,
            change_threshold: None,
            auto_threshold: None,
        }
    }
}

pub fn default_fan_static_speed() -> f32 {
    0.5
}

#[cfg(test)]
mod tests {
    use super::GpuConfig;

    #[test]
    fn deserialize_config_json() {
        let data = r#"{"fan_control_enabled":false,"fan_control_settings":{"mode":"curve","static_speed":0.5938412,"temperature_key":"edge","interval_ms":500,"curve":{"40":0.3,"50":0.35,"60":0.5,"70":0.75,"80":1.0},"spindown_delay_ms":1000,"change_threshold":2},"power_cap":318.0,"gpu_clock_offsets":{"0":-64}}"#;
        let config: GpuConfig = serde_json::from_str(data).unwrap();
        assert_eq!(
            -64,
            *config
                .clocks_configuration
                .gpu_clock_offsets
                .get(&0)
                .unwrap()
        );
    }
}
