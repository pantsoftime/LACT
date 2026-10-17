//! vBIOS information: the card's flashed option ROM, a vBIOS override loaded through the
//! driver, and the decoded tables of either.

pub use lact_vbios::{
    ClockOffsetRange, ImageCompleteness, Integrity, PowerPolicyRole, PowerPolicyRow, PowerUnit,
    RailLimit, RailLimitRole, RailVoltageLimits, VbiosFamily, VbiosReport, edit, parse,
    same_pci_images, sha256_hex,
};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use std::path::PathBuf;

#[skip_serializing_none]
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct VbiosInfo {
    /// The PCI expansion ROM read from sysfs: the flashed legacy/EFI images only, so it
    /// identifies the flashed vBIOS but carries no data tables.
    pub flash: Option<VbiosReport>,
    pub flash_error: Option<String>,
    pub override_status: Option<VbiosOverrideStatus>,
    /// The override image file, when an override is configured for this GPU.
    pub override_image: Option<VbiosReport>,
    pub override_image_error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VbiosOverrideScope {
    /// `NVreg_RegistryDwords=RmLinuxVbiosOverride=<mode>`, image at
    /// `nvidia/<driver version>/vbios_override.bin` (the linux_mvolt patch).
    Global,
    /// `NVreg_RegistryDwordsPerDevice` + `NVreg_RegistryBinaryFilePerDevice`
    /// (the per-GPU patch used by nvidia-linux-vbios-vix-loader).
    PerDevice,
}

#[skip_serializing_none]
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct VbiosOverrideStatus {
    /// The running nvidia module carries vBIOS override code (`None` if unknown).
    pub module_supports_override: Option<bool>,
    /// The module parameters request an override for this GPU.
    pub requested: bool,
    pub scope: Option<VbiosOverrideScope>,
    pub mode: Option<u32>,
    pub disabled: bool,
    pub image_path: Option<PathBuf>,
    /// The image file changed after boot, so it may not be the image that was loaded.
    pub image_modified_since_boot: Option<bool>,
}

impl VbiosOverrideStatus {
    /// An override is requested, not disabled, and the module can honour it. Whether the
    /// firmware accepted the image is not visible from here.
    pub fn active(&self) -> bool {
        self.requested && !self.disabled && self.module_supports_override != Some(false)
    }
}
