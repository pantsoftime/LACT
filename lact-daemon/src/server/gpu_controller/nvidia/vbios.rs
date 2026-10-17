//! vBIOS reading for NVIDIA GPUs: the PCI expansion ROM, and the state of a vBIOS override
//! handed to the GSP firmware by a patched kernel module.
//!
//! Two override patches exist, both configured through module parameters:
//! - the `linux_mvolt` patch: `NVreg_RegistryDwords=RmLinuxVbiosOverride=<mode>` with the
//!   image at `nvidia/<driver version>/vbios_override.bin` in the firmware directory;
//! - the per-GPU patch (nvidia-linux-vbios-vix-loader):
//!   `NVreg_RegistryDwordsPerDevice="pci=<bdf>;RmLinuxVbiosOverride=2;..."` and
//!   `NVreg_RegistryBinaryFilePerDevice="pci=<bdf>;RmLinuxVbiosOverrideImage=<path>"`, the
//!   path relative to `nvidia/` in the firmware directory.
//!
//! Everything here is read-only.

use anyhow::{Context, bail};
use lact_schema::vbios::{VbiosInfo, VbiosOverrideScope, VbiosOverrideStatus};
use std::{
    fs,
    path::Path,
    time::{Duration, SystemTime},
};
use tracing::warn;

const OVERRIDE_KEY: &str = "RmLinuxVbiosOverride";
const OVERRIDE_DISABLE_KEY: &str = "RmLinuxVbiosOverrideDisable";
const OVERRIDE_IMAGE_KEY: &str = "RmLinuxVbiosOverrideImage";
const GLOBAL_IMAGE_NAME: &str = "vbios_override.bin";

const FIRMWARE_DIR: &str = "/lib/firmware";
const DRIVER_PARAMS: &str = "/proc/driver/nvidia/params";
const KALLSYMS: &str = "/proc/kallsyms";
const MODULE_VERSION: &str = "/sys/module/nvidia/version";
/// Override images are at most a few MiB; anything larger is not one.
const MAX_IMAGE_SIZE: u64 = 16 * 1024 * 1024;

/// Reads the PCI expansion ROM (legacy + EFI images) through sysfs. Needs root.
pub fn read_pci_rom(device_path: &Path) -> anyhow::Result<Vec<u8>> {
    let rom_path = device_path.join("rom");
    // Writing 1 enables reads of the ROM BAR; it is disabled again afterwards either way.
    fs::write(&rom_path, "1")
        .with_context(|| format!("could not enable {}", rom_path.display()))?;
    let result = fs::read(&rom_path);
    if let Err(err) = fs::write(&rom_path, "0") {
        warn!("could not disable {}: {err}", rom_path.display());
    }
    let data = result.with_context(|| format!("could not read {}", rom_path.display()))?;
    if data.is_empty() {
        bail!("the PCI expansion ROM is empty");
    }
    Ok(data)
}

pub fn vbios_info(device_path: &Path, pci_slot_name: &str) -> VbiosInfo {
    let mut info = VbiosInfo::default();

    match read_pci_rom(device_path).and_then(|rom| lact_vbios::parse(&rom).map_err(Into::into)) {
        Ok(report) => info.flash = Some(report),
        Err(err) => info.flash_error = Some(format!("{err:#}")),
    }

    let status = override_status(pci_slot_name);
    if status.requested
        && let Some(path) = &status.image_path
    {
        match read_image(path) {
            Ok(report) => info.override_image = Some(report),
            Err(err) => info.override_image_error = Some(format!("{err:#}")),
        }
    }
    info.override_status = Some(status);
    info
}

fn read_image(path: &Path) -> anyhow::Result<lact_vbios::VbiosReport> {
    let len = fs::metadata(path)
        .with_context(|| format!("could not read {}", path.display()))?
        .len();
    if len > MAX_IMAGE_SIZE {
        bail!("{} is too large to be a vBIOS image", path.display());
    }
    let data = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    Ok(lact_vbios::parse(&data)?)
}

pub fn override_status(pci_slot_name: &str) -> VbiosOverrideStatus {
    let params = fs::read_to_string(DRIVER_PARAMS).unwrap_or_default();
    let module_supports_override = fs::read_to_string(KALLSYMS)
        .ok()
        .map(|symbols| kallsyms_has_override(&symbols));
    let driver_version = fs::read_to_string(MODULE_VERSION)
        .ok()
        .map(|version| version.trim().to_owned());

    let mut status = parse_override_params(&params, pci_slot_name, driver_version.as_deref());
    status.module_supports_override = module_supports_override;
    status.image_modified_since_boot = status
        .image_path
        .as_deref()
        .and_then(modified_since_boot);
    status
}

fn kallsyms_has_override(symbols: &str) -> bool {
    symbols
        .lines()
        .any(|line| line.ends_with("[nvidia]") && line.contains("VbiosOverride"))
}

fn modified_since_boot(path: &Path) -> Option<bool> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    let stat = fs::read_to_string("/proc/stat").ok()?;
    let boot_time = stat
        .lines()
        .find_map(|line| line.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    Some(modified > SystemTime::UNIX_EPOCH + Duration::from_secs(boot_time))
}

/// Value of a `Name: "value"` line of `/proc/driver/nvidia/params`.
fn param<'a>(params: &'a str, name: &str) -> Option<&'a str> {
    params.lines().find_map(|line| {
        let value = line.strip_prefix(name)?.strip_prefix(':')?.trim();
        Some(value.trim_matches('"'))
    })
}

fn parse_dword(value: &str) -> Option<u32> {
    match value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        Some(hex) => u32::from_str_radix(hex, 16).ok(),
        None => value.parse().ok(),
    }
}

/// Parses the PCI address forms the driver accepts (`bus:slot`, `domain:bus:slot`,
/// `domain:bus:slot.func`, all hex) into (domain, bus, slot, function).
fn parse_bdf(value: &str) -> Option<(u32, u32, u32, u32)> {
    let (address, function) = match value.split_once('.') {
        Some((address, function)) => (address, u32::from_str_radix(function, 16).ok()?),
        None => (value, 0),
    };
    let parts = address
        .split(':')
        .map(|part| u32::from_str_radix(part, 16).ok())
        .collect::<Option<Vec<_>>>()?;
    match parts[..] {
        [bus, slot] => Some((0, bus, slot, function)),
        [domain, bus, slot] => Some((domain, bus, slot, function)),
        _ => None,
    }
}

/// `name=value` entries of a per-device parameter that apply to `slot`: each `pci=<bdf>`
/// entry selects the device the following entries apply to.
fn per_device_entries<'a>(value: &'a str, slot: &str) -> Vec<(&'a str, &'a str)> {
    let target = parse_bdf(slot);
    let mut selected = false;
    let mut entries = Vec::new();
    for entry in value
        .split(';')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
    {
        let Some((name, value)) = entry.split_once('=') else {
            continue;
        };
        if name == "pci" {
            selected = target.is_some() && parse_bdf(value) == target;
        } else if selected {
            entries.push((name, value));
        }
    }
    entries
}

fn find<'a>(entries: &[(&str, &'a str)], key: &str) -> Option<&'a str> {
    entries
        .iter()
        .find(|(name, _)| *name == key)
        .map(|(_, value)| *value)
}

fn parse_override_params(
    params: &str,
    pci_slot_name: &str,
    driver_version: Option<&str>,
) -> VbiosOverrideStatus {
    let mut status = VbiosOverrideStatus::default();
    let firmware_dir = Path::new(FIRMWARE_DIR).join("nvidia");

    let per_device_dwords = per_device_entries(
        param(params, "RegistryDwordsPerDevice").unwrap_or_default(),
        pci_slot_name,
    );
    let per_device_files = per_device_entries(
        param(params, "RegistryBinaryFilePerDevice").unwrap_or_default(),
        pci_slot_name,
    );
    let global_dwords = param(params, "RegistryDwords")
        .unwrap_or_default()
        .split(';')
        .filter_map(|entry| entry.trim().split_once('='))
        .collect::<Vec<_>>();

    if let Some(mode) = find(&per_device_dwords, OVERRIDE_KEY).and_then(parse_dword) {
        status.scope = Some(VbiosOverrideScope::PerDevice);
        status.mode = Some(mode);
        status.image_path =
            find(&per_device_files, OVERRIDE_IMAGE_KEY).map(|path| firmware_dir.join(path));
        status.disabled = find(&per_device_dwords, OVERRIDE_DISABLE_KEY)
            .and_then(parse_dword)
            .is_some_and(|value| value != 0);
    } else if let Some(mode) = find(&global_dwords, OVERRIDE_KEY).and_then(parse_dword) {
        status.scope = Some(VbiosOverrideScope::Global);
        status.mode = Some(mode);
        status.image_path =
            driver_version.map(|version| firmware_dir.join(version).join(GLOBAL_IMAGE_NAME));
        status.disabled = find(&global_dwords, OVERRIDE_DISABLE_KEY)
            .and_then(parse_dword)
            .is_some_and(|value| value != 0);
    }
    status.requested = status.mode.is_some_and(|mode| mode != 0);
    status
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const SLOT: &str = "0000:01:00.0";

    #[test]
    fn no_override() {
        let params = "RegistryDwords: \"\"\nRegistryDwordsPerDevice: \"\"\nRegistryBinaryFilePerDevice: \"\"\n";
        assert_eq!(
            parse_override_params(params, SLOT, Some("615.78.08")),
            VbiosOverrideStatus::default()
        );
    }

    #[test]
    fn global_override() {
        let params = "EnableMSI: 1\nRegistryDwords: \"RmLinuxVbiosOverride=2\"\nRegistryDwordsPerDevice: \"\"\n";
        let status = parse_override_params(params, SLOT, Some("615.78.08"));
        assert!(status.requested);
        assert_eq!(status.scope, Some(VbiosOverrideScope::Global));
        assert_eq!(status.mode, Some(2));
        assert_eq!(
            status.image_path.as_deref(),
            Some(Path::new(
                "/lib/firmware/nvidia/615.78.08/vbios_override.bin"
            ))
        );
    }

    #[test]
    fn per_device_override() {
        let params = concat!(
            "RegistryDwordsPerDevice: \"pci=0000:02:00.0;RmLinuxVbiosOverride=2;pci=01:00;RmLinuxVbiosOverride=0x2;RmLinuxVbiosOverrideSize=1961983\"\n",
            "RegistryBinaryFilePerDevice: \"pci=0000:01:00.0;RmLinuxVbiosOverrideImage=615.78.08/vbios-override/card0.rom\"\n",
        );
        let status = parse_override_params(params, SLOT, None);
        assert!(status.active());
        assert_eq!(status.scope, Some(VbiosOverrideScope::PerDevice));
        assert_eq!(
            status.image_path.as_deref(),
            Some(Path::new(
                "/lib/firmware/nvidia/615.78.08/vbios-override/card0.rom"
            ))
        );

        let other = parse_override_params(params, "0000:03:00.0", None);
        assert!(!other.requested);
    }

    #[test]
    fn disabled_override() {
        let params = "RegistryDwords: \"RmLinuxVbiosOverride=2;RmLinuxVbiosOverrideDisable=1\"\n";
        let status = parse_override_params(params, SLOT, Some("615.78.08"));
        assert!(status.requested);
        assert!(status.disabled);
        assert!(!status.active());
    }

    #[test]
    fn kallsyms() {
        assert!(kallsyms_has_override(
            "0000000000000000 t kgspVbiosOverrideStage\t[nvidia]"
        ));
        assert!(!kallsyms_has_override(
            "0000000000000000 t kgspInitRm\t[nvidia]\n"
        ));
    }

    #[test]
    fn bdf_forms() {
        assert_eq!(parse_bdf("0000:01:00.0"), Some((0, 1, 0, 0)));
        assert_eq!(parse_bdf("01:00"), Some((0, 1, 0, 0)));
        assert_eq!(parse_bdf("0001:0a:00.1"), Some((1, 10, 0, 1)));
        assert_eq!(parse_bdf("garbage"), None);
    }
}
