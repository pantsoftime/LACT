//! Codec for the Thermal Grizzly WireView Pro II configuration struct
//! (version 2, 96 bytes, firmware v5), shared by the daemon and the GUI so
//! backup files made by either — or by the `wv2ctl` tooling — can be read.

use crate::WireViewConfig;

pub const CONFIG_SIZE: usize = 96;
pub const CONFIG_VERSION: u8 = 2;

/// Fault-mask bit names, in bit order.
pub const FAULT_BITS: [&str; 6] = ["otp_chip", "otp_ts", "ocp", "wire_ocp", "opp", "imbalance"];
/// Screen names, in id order.
pub const SCREENS: [&str; 5] = ["main", "simple", "current", "temp", "status"];

// ---------------------------------------------------------------- config codec

pub fn rd_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

pub fn rd_i16(b: &[u8], off: usize) -> i16 {
    i16::from_le_bytes([b[off], b[off + 1]])
}

pub fn rd_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
}

/// CRC-16/CCITT-FALSE, which the firmware keeps over config bytes 2..96.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

pub fn crc_ok(raw: &[u8]) -> bool {
    raw.len() == CONFIG_SIZE && rd_u16(raw, 0) == crc16(&raw[2..])
}

/// Field offsets of the version-2 struct (ARM-native alignment, pads explicit).
pub fn decode_config(raw: &[u8]) -> Result<WireViewConfig, String> {
    if raw.len() != CONFIG_SIZE {
        return Err(format!("config is {} bytes, expected {CONFIG_SIZE}", raw.len()));
    }
    let name = raw[3..35]
        .split(|b| *b == 0)
        .next()
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .unwrap_or_default();
    Ok(WireViewConfig {
        version: raw[2],
        name,
        fan_mode: raw[36],
        fan_source: raw[37],
        fan_duty_min: raw[38],
        fan_duty_max: raw[39],
        fan_temp_min: rd_i16(raw, 40),
        fan_temp_max: rd_i16(raw, 42),
        backlight: raw[44],
        fault_display: rd_u16(raw, 46),
        fault_buzzer: rd_u16(raw, 48),
        fault_soft_off: rd_u16(raw, 50),
        fault_hard_off: rd_u16(raw, 52),
        ts_fault: rd_i16(raw, 54),
        ocp: raw[56],
        wire_ocp: raw[57],
        opp: rd_u16(raw, 58),
        imbalance: raw[60],
        imbalance_min_load: raw[61],
        shutdown_wait: raw[62],
        log_interval: raw[63],
        avg: raw[64],
        default_screen: raw[68],
        current_scale: raw[69],
        power_scale: raw[70],
        rotation: raw[71],
        timeout_mode: raw[72],
        cycle_screens: raw[73],
        cycle_time: raw[74],
        timeout: raw[75],
        color_primary: rd_u32(raw, 76),
        color_secondary: rd_u32(raw, 80),
        color_highlight: rd_u32(raw, 84),
        color_background: rd_u32(raw, 88),
        background: raw[92],
        fan_bitmap: raw[93],
        invert: raw[94],
    })
}

/// Encode with a fresh CRC. The name must be ASCII and at most 31 bytes.
pub fn encode_config(c: &WireViewConfig) -> Result<[u8; CONFIG_SIZE], String> {
    if c.version != CONFIG_VERSION {
        return Err(format!("config struct version {} is not supported (expected {CONFIG_VERSION})", c.version));
    }
    if !c.name.is_ascii() || c.name.len() > 31 || c.name.bytes().any(|b| b == 0) {
        return Err("device name must be at most 31 ASCII characters".to_owned());
    }
    let mut raw = [0u8; CONFIG_SIZE];
    raw[2] = c.version;
    raw[3..3 + c.name.len()].copy_from_slice(c.name.as_bytes());
    raw[36] = c.fan_mode;
    raw[37] = c.fan_source;
    raw[38] = c.fan_duty_min;
    raw[39] = c.fan_duty_max;
    raw[40..42].copy_from_slice(&c.fan_temp_min.to_le_bytes());
    raw[42..44].copy_from_slice(&c.fan_temp_max.to_le_bytes());
    raw[44] = c.backlight;
    raw[46..48].copy_from_slice(&c.fault_display.to_le_bytes());
    raw[48..50].copy_from_slice(&c.fault_buzzer.to_le_bytes());
    raw[50..52].copy_from_slice(&c.fault_soft_off.to_le_bytes());
    raw[52..54].copy_from_slice(&c.fault_hard_off.to_le_bytes());
    raw[54..56].copy_from_slice(&c.ts_fault.to_le_bytes());
    raw[56] = c.ocp;
    raw[57] = c.wire_ocp;
    raw[58..60].copy_from_slice(&c.opp.to_le_bytes());
    raw[60] = c.imbalance;
    raw[61] = c.imbalance_min_load;
    raw[62] = c.shutdown_wait;
    raw[63] = c.log_interval;
    raw[64] = c.avg;
    raw[68] = c.default_screen;
    raw[69] = c.current_scale;
    raw[70] = c.power_scale;
    raw[71] = c.rotation;
    raw[72] = c.timeout_mode;
    raw[73] = c.cycle_screens;
    raw[74] = c.cycle_time;
    raw[75] = c.timeout;
    raw[76..80].copy_from_slice(&c.color_primary.to_le_bytes());
    raw[80..84].copy_from_slice(&c.color_secondary.to_le_bytes());
    raw[84..88].copy_from_slice(&c.color_highlight.to_le_bytes());
    raw[88..92].copy_from_slice(&c.color_background.to_le_bytes());
    raw[92] = c.background;
    raw[93] = c.fan_bitmap;
    raw[94] = c.invert;
    let crc = crc16(&raw[2..]);
    raw[0..2].copy_from_slice(&crc.to_le_bytes());
    Ok(raw)
}

pub fn hex(data: &[u8]) -> String {
    use std::fmt::Write;
    data.iter().fold(String::with_capacity(data.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

pub fn unhex(text: &str) -> Result<Vec<u8>, String> {
    if !text.len().is_multiple_of(2) {
        return Err("odd-length hex".to_owned());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|e| format!("bad hex: {e}")))
        .collect()
}

