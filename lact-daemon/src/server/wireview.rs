//! Thermal Grizzly `WireView` Pro II over its USB CDC-ACM port (this fork).
//!
//! A port of the `wv2ctl` tooling: the device speaks a tiny byte protocol on
//! an STM32 virtual COM port (0483:5740) at 115200 baud. Command bytes come
//! from `LibreHardwareMonitor`'s `WireViewPro2.cs` and emaspa's wireview-linux;
//! the points those references miss (found with wv2ctl on 2026-09-12):
//!
//! - the config CRC is CRC-16/CCITT-FALSE over bytes 2..96, kept by the firmware;
//! - opening the port makes the firmware push its welcome string unprompted
//!   ~50 ms later, so the line is drained before the first exchange;
//! - `NVM_LOAD` copies flash into the live config verbatim, even an older
//!   struct version left by earlier firmware (which v5 otherwise ignores and
//!   boots with defaults), so the live config is put back whenever flash is
//!   read that way.
//!
//! Only config struct version 2 (96 bytes, firmware v5) is handled. Every
//! write backs the previous config up under `/var/lib/lact/wireview`, reads
//! the result back and, unless "live", stores it to flash and reads the flash
//! copy back too. The port is opened on demand and released after a few
//! seconds without requests, so `wv2ctl` can still be used while the LACT
//! page is not being looked at.

use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use lact_schema::{
    WireViewConfig, WireViewConfigState, WireViewFlashInfo, WireViewInfo, WireViewNvmOp,
    WireViewReadings, WireViewStatus, WireViewWriteResult,
    wireview::{
        CONFIG_SIZE, CONFIG_VERSION, crc16, crc_ok, decode_config, encode_config, hex, rd_i16,
        rd_u16, rd_u32, unhex,
    },
};
use nix::{
    fcntl::{FcntlArg, OFlag},
    sys::termios::{self, BaudRate, ControlFlags, FlushArg, SetArg, SpecialCharacterIndices},
};
use tracing::{debug, info, warn};

const BY_ID_DIR: &str = "/dev/serial/by-id";
const PORT_PREFIX: &str = "usb-STMicroelectronics_STM32_Virtual_ComPort_";
const WELCOME: &[u8] = b"Thermal Grizzly WireView Pro II";
const VENDOR_ID: u8 = 0xEF;
const PRODUCT_ID: u8 = 0x05;

const CMD_WELCOME: u8 = 0x00;
const CMD_READ_VENDOR: u8 = 0x01;
const CMD_READ_UID: u8 = 0x02;
const CMD_READ_SENSORS: u8 = 0x04;
const CMD_READ_CONFIG: u8 = 0x05;
const CMD_WRITE_CONFIG: u8 = 0x06;
const CMD_SCREEN: u8 = 0x0C;
const CMD_READ_BUILD: u8 = 0x0D;
const CMD_CLEAR_FAULTS: u8 = 0x0E;
const CMD_NVM: u8 = 0xF2;
const NVM_MAGIC: [u8; 4] = [0x55, 0xAA, 0x55, 0xAA];
const NVM_LOAD: u8 = 1;
const NVM_STORE: u8 = 2;
const NVM_RESET: u8 = 3;

pub const SCREENS: [(&str, u8); 8] = [
    ("main", 0xE0),
    ("simple", 0xE1),
    ("current", 0xE2),
    ("temp", 0xE3),
    ("status", 0xE4),
    ("same", 0xEF),
    ("pause", 0xF0),
    ("resume", 0xF1),
];

const SENSORS_SIZE: usize = 100;
const BUILD_SIZE: usize = 68;
const UID_SIZE: usize = 12;
const WRITE_CHUNK: usize = 62;

const BACKUP_DIR: &str = "/var/lib/lact/wireview";
/// The port is released after this long without a request.
pub const IDLE_CLOSE: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------- config state

fn config_state(raw: &[u8]) -> anyhow::Result<WireViewConfigState> {
    Ok(WireViewConfigState {
        raw: hex(raw),
        config: decode_config(raw).map_err(|e| anyhow!(e))?,
        crc_ok: crc_ok(raw),
    })
}

#[allow(clippy::cast_precision_loss)]
fn decode_sensors(raw: &[u8]) -> WireViewReadings {
    let temp = |off: usize| {
        let t = rd_i16(raw, off);
        (-400..=2000).contains(&t).then(|| f32::from(t) / 10.0)
    };
    let mut pin_v = [0.0f32; 6];
    let mut pin_a = [0.0f32; 6];
    let mut pin_w = [0.0f32; 6];
    for i in 0..6 {
        let base = 12 + 12 * i;
        pin_v[i] = f32::from(rd_i16(raw, base)) / 1000.0;
        pin_a[i] = rd_u32(raw, base + 4) as f32 / 1000.0;
        pin_w[i] = rd_u32(raw, base + 8) as f32 / 1000.0;
    }
    WireViewReadings {
        temps: [temp(0), temp(2), temp(4), temp(6)],
        vdd: f32::from(rd_u16(raw, 8)) / 1000.0,
        fan: raw[10],
        pin_v,
        pin_a,
        pin_w,
        power: rd_u32(raw, 84) as f32 / 1000.0,
        current: rd_u32(raw, 88) as f32 / 1000.0,
        avg_v: f32::from(rd_u16(raw, 92)) / 1000.0,
        psu_cap: match raw[94] {
            0 => "600W",
            1 => "450W",
            2 => "300W",
            3 => "150W",
            _ => "?",
        }
        .to_owned(),
        fault_status: rd_u16(raw, 96),
        fault_log: rd_u16(raw, 98),
    }
}

// ---------------------------------------------------------------- serial port

/// The first STM32 virtual COM port under `/dev/serial/by-id`, if any.
pub fn find_port() -> Option<PathBuf> {
    let mut ports: Vec<PathBuf> = fs::read_dir(BY_ID_DIR)
        .ok()?
        .filter_map(Result::ok)
        .filter(|e| e.file_name().to_string_lossy().starts_with(PORT_PREFIX))
        .map(|e| e.path())
        .collect();
    ports.sort();
    if ports.len() > 1 {
        warn!("several STM32 virtual COM ports found; using {}", ports[0].display());
    }
    ports.into_iter().next()
}

struct Port {
    file: File,
}

impl Port {
    fn open(path: &Path) -> anyhow::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NOCTTY | nix::libc::O_NONBLOCK)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        // Exclusive, like pyserial's exclusive=True: a second opener gets EBUSY.
        // SAFETY: TIOCEXCL takes no argument and only affects this open fd.
        if unsafe { nix::libc::ioctl(file.as_raw_fd(), nix::libc::TIOCEXCL) } != 0 {
            bail!("{}: cannot take the port exclusively (is wv2ctl using it?)", path.display());
        }
        let mut t = termios::tcgetattr(&file).context("tcgetattr")?;
        termios::cfmakeraw(&mut t);
        termios::cfsetspeed(&mut t, BaudRate::B115200).context("cfsetspeed")?;
        t.control_flags |= ControlFlags::CLOCAL | ControlFlags::CREAD;
        t.control_chars[SpecialCharacterIndices::VMIN as usize] = 0;
        t.control_chars[SpecialCharacterIndices::VTIME as usize] = 1; // 100 ms per read
        termios::tcsetattr(&file, SetArg::TCSANOW, &t).context("tcsetattr")?;
        nix::fcntl::fcntl(&file, FcntlArg::F_SETFL(OFlag::empty())).context("clearing O_NONBLOCK")?;
        Ok(Self { file })
    }

    fn flush_input(&self) {
        let _ = termios::tcflush(&self.file, FlushArg::TCIFLUSH);
    }

    /// Discard input until the line has been quiet for `quiet` (at most `limit`).
    fn drain(&mut self, quiet: Duration, limit: Duration) {
        let start = Instant::now();
        let mut deadline = start + quiet;
        let mut buf = [0u8; 256];
        while Instant::now() < deadline.min(start + limit) {
            match self.file.read(&mut buf) {
                Ok(n) if n > 0 => deadline = Instant::now() + quiet,
                _ => {}
            }
        }
    }

    fn write_all(&mut self, data: &[u8]) -> anyhow::Result<()> {
        self.file.write_all(data).context("serial write")?;
        self.file.flush().ok();
        Ok(())
    }

    /// Send a command and collect `size` reply bytes (fewer on timeout).
    fn request(&mut self, cmd: u8, payload: &[u8], size: usize, timeout: Duration) -> anyhow::Result<Vec<u8>> {
        self.flush_input();
        let mut msg = Vec::with_capacity(1 + payload.len());
        msg.push(cmd);
        msg.extend_from_slice(payload);
        self.write_all(&msg)?;
        let mut buf = Vec::with_capacity(size);
        let deadline = Instant::now() + timeout;
        let mut chunk = [0u8; 256];
        while buf.len() < size && Instant::now() < deadline {
            let want = (size - buf.len()).min(chunk.len());
            match self.file.read(&mut chunk[..want]) {
                Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(anyhow!("serial read: {e}")),
            }
        }
        Ok(buf)
    }

    fn send(&mut self, data: &[u8], settle: Duration) -> anyhow::Result<()> {
        self.write_all(data)?;
        std::thread::sleep(settle);
        Ok(())
    }
}

// ---------------------------------------------------------------- the device

pub struct WireView {
    port: Port,
    info: WireViewInfo,
}

impl WireView {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let mut port = Port::open(path)?;
        port.drain(Duration::from_millis(150), Duration::from_secs(1));
        let welcome = port.request(CMD_WELCOME, &[], WELCOME.len() + 1, Duration::from_secs(1))?;
        if welcome.iter().take_while(|b| **b != 0).copied().collect::<Vec<_>>() != WELCOME {
            bail!("{}: no WireView Pro II welcome response", path.display());
        }
        port.drain(Duration::from_millis(150), Duration::from_secs(1));
        let vendor = port.request(CMD_READ_VENDOR, &[], 3, Duration::from_secs(1))?;
        if vendor.len() != 3 || vendor[0] != VENDOR_ID || vendor[1] != PRODUCT_ID {
            bail!("{}: unexpected vendor data {}", path.display(), hex(&vendor));
        }
        let firmware = vendor[2];
        let uid = port.request(CMD_READ_UID, &[], UID_SIZE, Duration::from_secs(1))?;
        let build = port.request(CMD_READ_BUILD, &[], BUILD_SIZE, Duration::from_secs(1))?;
        let cstr = |b: &[u8]| {
            String::from_utf8_lossy(b.split(|c| *c == 0).next().unwrap_or(&[])).into_owned()
        };
        let (product, build) = if build.len() == BUILD_SIZE {
            (cstr(&build[3..35]), cstr(&build[35..67]))
        } else {
            (String::new(), String::new())
        };
        let mut this = Self {
            port,
            info: WireViewInfo {
                port: path.display().to_string(),
                product,
                firmware,
                build,
                uid: hex(&uid).to_uppercase(),
                config_version: 0,
            },
        };
        this.info.config_version = this.read_config_raw()?[2];
        debug!(
            "WireView Pro II on {}: firmware v{} ({}), config struct v{}",
            this.info.port, this.info.firmware, this.info.build, this.info.config_version
        );
        Ok(this)
    }

    pub fn info(&self) -> WireViewInfo {
        self.info.clone()
    }

    pub fn read_sensors(&mut self) -> anyhow::Result<WireViewReadings> {
        for _ in 0..3 {
            let raw = self.port.request(CMD_READ_SENSORS, &[], SENSORS_SIZE, Duration::from_secs(1))?;
            // No framing or CRC on the wire: reject frames with non-zero pad
            // bytes or a fan duty over 100.
            if raw.len() == SENSORS_SIZE && raw[10] <= 100 && raw[11] == 0 && raw[95] == 0 {
                return Ok(decode_sensors(&raw));
            }
        }
        bail!("no valid sensor frame from the device")
    }

    /// The live config, any struct version (used after NVM_LOAD).
    fn read_config_any(&mut self) -> anyhow::Result<Vec<u8>> {
        let raw = self.port.request(CMD_READ_CONFIG, &[], CONFIG_SIZE, Duration::from_secs(1))?;
        if raw.len() != CONFIG_SIZE {
            bail!("short config read ({} of {CONFIG_SIZE} bytes)", raw.len());
        }
        Ok(raw)
    }

    pub fn read_config_raw(&mut self) -> anyhow::Result<Vec<u8>> {
        let raw = self.read_config_any()?;
        if raw[2] != CONFIG_VERSION {
            bail!("config struct version {} is not supported (expected {CONFIG_VERSION})", raw[2]);
        }
        Ok(raw)
    }

    pub fn read_config(&mut self) -> anyhow::Result<WireViewConfigState> {
        config_state(&self.read_config_raw()?)
    }

    fn write_config_raw(&mut self, raw: &[u8]) -> anyhow::Result<()> {
        self.port.flush_input();
        for (i, chunk) in raw.chunks(WRITE_CHUNK).enumerate() {
            let mut msg = vec![CMD_WRITE_CONFIG, (i * WRITE_CHUNK) as u8];
            msg.extend_from_slice(chunk);
            self.port.send(&msg, Duration::from_millis(20))?;
        }
        std::thread::sleep(Duration::from_millis(200));
        // Redraw with the new settings, as the official app does.
        self.screen("same")
    }

    fn nvm(&mut self, op: u8) -> anyhow::Result<()> {
        let mut msg = vec![CMD_NVM];
        msg.extend_from_slice(&NVM_MAGIC);
        msg.push(op);
        self.port.send(&msg, Duration::from_millis(500))
    }

    pub fn clear_faults(&mut self) -> anyhow::Result<()> {
        // The two 16-bit masks are AND-ed into fault status and fault log; zeros clear every bit.
        self.port.send(&[CMD_CLEAR_FAULTS, 0, 0, 0, 0], Duration::from_millis(50))
    }

    pub fn screen(&mut self, name: &str) -> anyhow::Result<()> {
        let code = SCREENS
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, c)| *c)
            .with_context(|| format!("unknown screen {name:?}"))?;
        self.port.send(&[CMD_SCREEN, code], Duration::from_millis(50))
    }

    fn write_backup(&mut self, raw: &[u8]) -> anyhow::Result<PathBuf> {
        fs::create_dir_all(BACKUP_DIR).with_context(|| format!("creating {BACKUP_DIR}"))?;
        let stamp = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S").to_string();
        let path = Path::new(BACKUP_DIR).join(format!("config-{stamp}.json"));
        let doc = serde_json::json!({
            "device": "WireView Pro II",
            "uid": self.info.uid,
            "firmware": self.info.firmware,
            "config_version": raw[2],
            "saved_at": jiff::Zoned::now().strftime("%Y-%m-%dT%H:%M:%S").to_string(),
            "raw": hex(raw),
            "config": decode_config(raw).ok(),
        });
        fs::write(&path, serde_json::to_string_pretty(&doc)? + "\n")
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    /// `NVM_LOAD`, returning (previous live config, config loaded from flash).
    /// The load goes straight into the live config whatever its struct
    /// version, so the previous live config is written back unless `keep`
    /// is set and the loaded one is a valid version-2 config.
    fn load_flash(&mut self, keep: bool) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
        let live = self.read_config_raw()?;
        self.write_backup(&live)?;
        self.nvm(NVM_LOAD)?;
        let loaded = self.read_config_any()?;
        if keep && loaded[2] == CONFIG_VERSION && crc_ok(&loaded) {
            return Ok((live, loaded));
        }
        self.write_config_raw(&live)?;
        if self.read_config_any()? != live {
            bail!("could not put the live config back after reading flash; restore the newest backup in {BACKUP_DIR}");
        }
        Ok((live, loaded))
    }

    fn verify_store(&mut self, expected: &[u8]) -> anyhow::Result<()> {
        self.nvm(NVM_STORE)?;
        let (_, stored) = self.load_flash(false)?;
        if stored != expected {
            bail!(
                "config read back from flash differs from what was written (flash struct version {})",
                stored[2]
            );
        }
        Ok(())
    }

    /// Back up, write, verify the read-back and, unless `live`, store to
    /// flash and verify that too. `expected_raw` guards against editing a
    /// config that changed under the caller.
    pub fn set_config(
        &mut self,
        config: &WireViewConfig,
        expected_raw: Option<&[u8]>,
        live: bool,
    ) -> anyhow::Result<WireViewWriteResult> {
        let old = self.read_config_raw()?;
        if let Some(expected) = expected_raw
            && expected != old.as_slice()
        {
            bail!("the config on the device changed since it was loaded; reload and redo the edits");
        }
        if !crc_ok(&old) {
            bail!("device config CRC does not match this struct layout; refusing to write");
        }
        let new = encode_config(config).map_err(|e| anyhow!(e))?;
        if new[..] == old[..] {
            return Ok(WireViewWriteResult {
                state: config_state(&old)?,
                backup: None,
            });
        }
        let backup = self.write_backup(&old)?;
        self.write_config_raw(&new)?;
        let readback = self.read_config_raw()?;
        if readback[2..] != new[2..] {
            bail!("device read-back does not match what was sent");
        }
        if !live {
            self.verify_store(&new)?;
        }
        info!(
            "WireView config written{} (backup {})",
            if live { " (live only)" } else { " and saved to flash" },
            backup.display()
        );
        Ok(WireViewWriteResult {
            state: config_state(&readback)?,
            backup: Some(backup.display().to_string()),
        })
    }

    pub fn nvm_op(&mut self, op: WireViewNvmOp) -> anyhow::Result<WireViewWriteResult> {
        match op {
            WireViewNvmOp::Save => {
                let live = self.read_config_raw()?;
                self.verify_store(&live)?;
                info!("WireView live config saved to flash and verified");
                Ok(WireViewWriteResult {
                    state: config_state(&live)?,
                    backup: None,
                })
            }
            WireViewNvmOp::Revert => {
                let (before, loaded) = self.load_flash(true)?;
                if loaded[2] != CONFIG_VERSION || !crc_ok(&loaded) {
                    bail!(
                        "flash holds an incompatible config (struct version {}, likely saved by older firmware); live config left unchanged — Save replaces it",
                        loaded[2]
                    );
                }
                let backup = self.write_backup(&before)?;
                Ok(WireViewWriteResult {
                    state: config_state(&loaded)?,
                    backup: Some(backup.display().to_string()),
                })
            }
            WireViewNvmOp::FactoryReset => {
                let before = self.read_config_raw()?;
                let backup = self.write_backup(&before)?;
                self.nvm(NVM_RESET)?;
                let now = self.read_config_raw()?;
                info!("WireView live config reset to firmware defaults (not saved)");
                Ok(WireViewWriteResult {
                    state: config_state(&now)?,
                    backup: Some(backup.display().to_string()),
                })
            }
        }
    }

    pub fn flash_info(&mut self) -> anyhow::Result<WireViewFlashInfo> {
        let (_, stored) = self.load_flash(false)?;
        let version = stored[2];
        let size = match version {
            0 => 72,
            1 => 74,
            CONFIG_VERSION => CONFIG_SIZE,
            _ => 0,
        };
        let crc_ok = size > 0 && rd_u16(&stored, 0) == crc16(&stored[2..size]);
        Ok(WireViewFlashInfo {
            version,
            crc_ok,
            raw: hex(&stored),
            config: (version == CONFIG_VERSION).then(|| decode_config(&stored).ok()).flatten(),
        })
    }
}

// ---------------------------------------------------------------- manager

struct State {
    dev: Option<WireView>,
    last_used: Instant,
    /// Identity read the last time the port was opened, so detection polls
    /// while the page is not visible do not have to open it again.
    known: Option<WireViewInfo>,
}

/// Owns the (lazily opened) device; every call runs on the blocking pool so
/// the serial waits never stall the daemon's event loop.
#[derive(Clone)]
pub struct WireViewManager {
    state: Arc<Mutex<State>>,
}

impl Default for WireViewManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WireViewManager {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State {
                dev: None,
                last_used: Instant::now(),
                known: None,
            })),
        }
    }

    /// Release the port if nothing has used it for `IDLE_CLOSE`.
    pub fn close_if_idle(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.dev.is_some() && st.last_used.elapsed() > IDLE_CLOSE {
            debug!("releasing the WireView port after {IDLE_CLOSE:?} idle");
            st.dev = None;
        }
    }

    async fn with<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut WireView) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        let state = self.state.clone();
        tokio::task::spawn_blocking(move || {
            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
            if st.dev.is_none() {
                let path = find_port().context("no WireView Pro II found (no STM32 virtual COM port under /dev/serial/by-id)")?;
                let dev = WireView::open(&path)?;
                if st.known.is_none() {
                    let i = dev.info();
                    info!("WireView Pro II on {}: firmware v{} ({})", i.port, i.firmware, i.build);
                }
                st.known = Some(dev.info());
                st.dev = Some(dev);
            }
            st.last_used = Instant::now();
            let result = f(st.dev.as_mut().expect("device just opened"));
            if let Err(err) = &result {
                // Any failure could be a dropped port; reopen on the next call.
                debug!("WireView call failed, releasing the port: {err:#}");
                st.dev = None;
            }
            result
        })
        .await
        .context("WireView task")?
    }

    pub async fn info(&self) -> anyhow::Result<Option<WireViewInfo>> {
        let Some(path) = find_port() else {
            self.state.lock().unwrap_or_else(|e| e.into_inner()).known = None;
            return Ok(None);
        };
        // Answer from the cached identity when the same port was already
        // opened once; opening it just to say "still there" would take the
        // port away from the CLI every few seconds.
        let known = self.state.lock().unwrap_or_else(|e| e.into_inner()).known.clone();
        if let Some(info) = known
            && info.port == path.display().to_string()
        {
            return Ok(Some(info));
        }
        self.with(|dev| Ok(dev.info())).await.map(Some)
    }

    pub async fn status(&self) -> anyhow::Result<WireViewStatus> {
        self.with(|dev| {
            Ok(WireViewStatus {
                readings: dev.read_sensors()?,
                state: dev.read_config()?,
            })
        })
        .await
    }

    pub async fn set_config(
        &self,
        config: WireViewConfig,
        expected_raw: Option<String>,
        live: bool,
    ) -> anyhow::Result<WireViewWriteResult> {
        let expected = expected_raw.map(|h| unhex(&h)).transpose().map_err(|e| anyhow!(e))?;
        self.with(move |dev| dev.set_config(&config, expected.as_deref(), live))
            .await
    }

    pub async fn nvm(&self, op: WireViewNvmOp) -> anyhow::Result<WireViewWriteResult> {
        self.with(move |dev| dev.nvm_op(op)).await
    }

    pub async fn flash(&self) -> anyhow::Result<WireViewFlashInfo> {
        self.with(|dev| dev.flash_info()).await
    }

    pub async fn clear_faults(&self) -> anyhow::Result<()> {
        self.with(|dev| dev.clear_faults()).await
    }

    pub async fn screen(&self, name: String) -> anyhow::Result<()> {
        self.with(move |dev| dev.screen(&name)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_round_trips_and_crc_matches_the_tooling() {
        let mut c = WireViewConfig {
            version: 2,
            name: "WireView".to_owned(),
            ocp: 70,
            wire_ocp: 105,
            opp: 800,
            ts_fault: 800,
            color_primary: 0xFFFF8800,
            ..Default::default()
        };
        c.fan_temp_max = 600;
        let raw = encode_config(&c).unwrap();
        assert!(crc_ok(&raw));
        assert_eq!(decode_config(&raw).unwrap(), c);
        assert_eq!(raw[56], 70);
        assert_eq!(rd_u16(&raw, 58), 800);
    }

    /// Read-only exchange with a plugged-in device: `cargo test -p lact-daemon wireview -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs a WireView Pro II on a free port"]
    fn talks_to_the_real_device() {
        let path = find_port().expect("no STM32 virtual COM port");
        let mut dev = WireView::open(&path).unwrap();
        println!("{:#?}", dev.info());
        let readings = dev.read_sensors().unwrap();
        println!("{readings:#?}");
        let state = dev.read_config().unwrap();
        println!("{state:#?}");
        assert!(state.crc_ok, "config CRC must match the struct layout");
        // Writing the identical config is a no-op: nothing is sent.
        let same = dev.set_config(&state.config, Some(&unhex(&state.raw).unwrap()), true).unwrap();
        assert!(same.backup.is_none());
        assert_eq!(same.state, state);
    }

    #[test]
    fn crc16_ccitt_false_check_value() {
        assert_eq!(crc16(b"123456789"), 0x29B1);
    }

    #[test]
    fn sensor_frame_decodes() {
        let mut raw = [0u8; SENSORS_SIZE];
        raw[0..2].copy_from_slice(&(415i16).to_le_bytes());
        raw[2..4].copy_from_slice(&(-9999i16).to_le_bytes());
        raw[10] = 42;
        raw[12..14].copy_from_slice(&(12050i16).to_le_bytes());
        raw[16..20].copy_from_slice(&9540u32.to_le_bytes());
        raw[84..88].copy_from_slice(&633_000u32.to_le_bytes());
        raw[96..98].copy_from_slice(&0b100u16.to_le_bytes());
        let r = decode_sensors(&raw);
        assert_eq!(r.temps[0], Some(41.5));
        assert_eq!(r.temps[1], None);
        assert_eq!(r.fan, 42);
        assert!((r.pin_v[0] - 12.05).abs() < 1e-4);
        assert!((r.pin_a[0] - 9.54).abs() < 1e-4);
        assert!((r.power - 633.0).abs() < 1e-3);
        assert_eq!(r.fault_status, 4);
    }
}
