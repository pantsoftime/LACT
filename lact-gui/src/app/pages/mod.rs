pub mod adv_voltage_page;
pub mod crash_page;
pub mod displays_page;
pub mod info_page;
pub mod oc_page;
pub mod software_page;
pub mod thermals_page;
/// This fork: the tuning guide window opened from the Advanced page
pub mod tuning_guide;
/// This fork: the VBIOS page (NVIDIA)
pub mod vbios_page;
pub mod wireview_page;

use lact_schema::{DeviceInfo, DeviceStats};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageId {
    Info,
    Oc,
    Thermals,
    Software,
    Displays,
    /// This fork: the Advanced tuning page
    AdvVoltage,
    /// This fork: the WireView Pro II page
    WireView,
    /// This fork: the VBIOS page
    Vbios,
    Crash,
}

impl PageId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info_page",
            Self::Oc => "oc_page",
            Self::Thermals => "thermals_page",
            Self::Software => "software_page",
            Self::Displays => "displays_page",
            Self::AdvVoltage => "adv_voltage_page",
            Self::WireView => "wireview_page",
            Self::Vbios => "vbios_page",
            Self::Crash => "crash_page",
        }
    }
}

#[derive(Debug, Clone)]
pub enum PageUpdate {
    Info(Arc<DeviceInfo>),
    Stats(Arc<DeviceStats>),
}
