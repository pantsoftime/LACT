# Advanced page (NVIDIA RM ClockClient controls)

This fork adds an **Advanced** page to the LACT GUI: a one-stop overclocking
dashboard for NVIDIA GPUs, laid out after mVolt+. It exposes the clock domains
NVML does not reach — **XBAR**, **SYS** and **video** frequency offsets — and
the **MSVDD** / **NVVDD** voltage-rail offsets on the XBAR domain, next to the
NVML-backed controls LACT already has (core and memory offsets, power limit,
voltage boost, locked core clock).

Developed and verified on an RTX 5090 (GB202) with drivers **610.57.04** and **615.71.09** (identical private layout and domain map).

## How it works

The new controls go through NVIDIA's private RM "ClockClient" interface
(`NV2080_CTRL_CMD_CLK_CLK_DOMAINS_GET_INFO / GET_CONTROL / SET_CONTROL`,
`CLK_MEASURE_FREQ`) over the same `/dev/nvidiactl` RM path the daemon already
uses for other queries. The control-block layout is **not in the public
headers** and is **driver-branch specific**. The daemon therefore:

- only enables the interface on driver branches it has been verified against
  (R610 and R615 today), and refuses otherwise with a warning in the log. RM offsets
  left in the config from a verified branch are then skipped with a warning
  instead of failing the profile, so fan, thermal and power settings still
  apply after a driver update;
- checks the `GET_INFO` / `GET_CONTROL` entry layout before touching anything;
- resolves domains by their `apiDomain` selector rather than by index — the
  index-to-domain map is **not** an identity (on GB202: index 2 is memory,
  index 3 is SYS);
- reads back every write and treats a mismatch as an error, never trusting
  the `SET_CONTROL` status alone.

Offsets live in LACT's normal config, are applied at daemon start, and are
covered by LACT's confirm-or-revert timer and reset-to-default like any other
clock setting. They do not survive a reboot on their own.

## What the measurements showed (RTX 5090, driver 610)

- XBAR frequency offsets behave as advertised: a +300 MHz request measured
  +300 MHz, verified via `CLK_MEASURE_FREQ`.
- **A setting that never crashes can still compute wrong results.** At
  +380 MHz XBAR the card passed every clock, readback and Xid check and
  produced bit-different results from a deterministic torch workload. At
  +450 MHz it hard-locked. The page's XBAR guard (validated maximum +300 MHz,
  above it only with an explicit switch) exists because of this.
- **MSVDD offset is not free headroom.** +20 mV on the XBAR-domain rail
  *lowered* XBAR by ~31 MHz on its own and did not extend the ceiling or
  prevent the lockup. It is exposed, bounded to ±50 mV, and labelled.
- The NVVDD (rail 0) offset is accepted by the driver but produced no
  measurable effect; it is exposed as experimental.
- NVML's per-pstate clock offset is a single global register on this driver:
  a value written for pstate 0 reads back on every pstate. The page's core and
  memory cards therefore write pstate 0 only and clear other entries, because
  a stray zero for another pstate silently cancels the value.

## Validation

Frequency offsets should be validated by **correctness**, not by the absence
of crashes. The tooling used for that (a fixed-seed torch harness producing
bit-exact digests, plus the RM probe scripts) lives outside this repository.

## Layout and telemetry

Cards flow two or three to a row. A card only writes its value on Apply if
you touched it, so values set on the Overclocking page are never overridden
by leaving a card alone. Telemetry tiles open LACT's graphs window on their
series (the daemon publishes the RM-measured XBAR and SYS clocks into the
sensors map, so they can be graphed and exported like any other stat); the
menu on each tile offers Show / Remove. Controls with no known Linux
mechanism yet (voltage ranges, MSVDD clock ratio, extended voltage) are
present but insensitive, with the reason in their tooltip.

## Installing this fork

```sh
cargo build --release
sudo sh install_fork.sh      # installs under /usr/local and enables lactd
```

## Voltage rails, sensed voltages and the propagation ratio (2026-09-11)

Three more private RM objects were verified read-only on R610 and R615 and
are now used by the daemon (all fail closed on any layout mismatch):

- **`VOLT_RAILS`** INFO / STATUS / CONTROL: per-rail arbitrated target and the
  policy limits (VMIN, REL, ALT/OP, OV and the evaluated MAX). The status
  record layout was mapped field by field from the NvAPI library's own wrapper.
  Shown on the "NVVDD voltage limits" / "MSVDD voltage limits" cards and as
  `NVVDD target` / `MSVDD target` voltage sensors. The rail control object's
  REL delta is displayed (−50 mV on MSVDD is the driver default on GB202);
  its setter is located but deliberately not wired until the remaining fields
  are identified.
- **`CLK_ADC_DEVICES`** INFO / STATUS: the on-chip ADCs. ADCs whose INFO
  record carries a single GPC bit are attributed to NVVDD, the remaining one
  to MSVDD — the attribution was confirmed by lifting the XBAR voltage demand
  under load and watching that ADC follow the MSVDD target. Exposed as the
  `MSVDD` and `NVVDD (ADC)` voltage sensors (graphable) and on the rail cards.
- **`CLK_PROP_TOPS` / `CLK_CLK_PROP_TOP_RELS`**: the clock-propagation
  topology and its relations. The daemon locates the bidirectional GPC→XBAR
  ratio relation of the *active* topology by its properties, never by index,
  and offers it as the "MSVDD clock ratio" card (config
  `gpc_xbar_ratio_milli`, e.g. 950 = 0.950; `None` = factory). Writes follow
  the same discipline as the clock-domain block: fresh read, single-field
  change, exact readback, restore on mismatch; `reset` returns the factory
  ratio. Accepted range 0.80–1.20. The ratio is a propagation constraint,
  not `XBAR = GPC × ratio`.

Per-domain voltage demand offsets now target each domain's own rail slot, read
from the INFO entry's rail mask: the NVVDD offset is the GPC domain's demand
(rail 0), and SYS / video demand cards were added next to XBAR's. A rail-0
offset on the XBAR domain — the previous "NVVDD" control — never did anything,
which the rail mask explains.

## Rail limit deltas (2026-09-11, tranche 2)

The rail control record (`VOLT_RAILS` GET_CONTROL `0x2080b213` / SET_CONTROL
`0x2080f214`) was mapped with single-field writes and exact restores: +0x08
REL, +0x0c ALT/OP, +0x10 OV, +0x14 VMIN, all µV deltas to the driver's
evaluated limits; +0x18 / +0x1c move no limit and are never written.
The record's first word on rail 0 is NvAPI's voltage-boost percent (what
the Voltage boost card writes, 0–100), so the probe checks only the type
byte; it once refused the whole feature after the boost was set to 100 %. The
"NVVDD voltage limits" / "MSVDD voltage limits" cards now edit these four
deltas per rail (config `nvvdd_*_delta_mv`, `msvdd_*_delta_mv`; `None` =
the values found at daemon start, which are the firmware defaults unless a
delta was left applied across a daemon restart — the driver keeps no other
record, so reset before restarting the daemon if you want the defaults
re-captured). Bounds: ±250 mV per delta, and a raised limit is refused if it
would exceed the voltage device's reported maximum (1280 mV on the reference
card's XOC vBIOS). The driver's evaluated MAX — the tightest of REL, ALT/OP
and OV — is what binds; raising REL alone does nothing while ALT or OV is
tighter, exactly as mVolt+'s guide warns. −50 mV on MSVDD REL is the driver
default on GB202, which is why the MSVDD REL limit sits 50 mV under NVVDD's.

Layout note: telemetry values and card captions now request fixed widths so
changing digits no longer re-lays out the page.

## Boost limits (2026-09-11)

`PERF_LIMITS_GET_STATUS_V2` (`0x2080a079`) lists the clock arbiter's limit
clients. The daemon reads all 256 IDs (two requests of 128; 256 at once is
rejected) and publishes the populated ones as `perf_limits` in the stats.
Record layout, decoded on GB202 / R615: +0x4 type (2 = frequency, 6 =
voltage-policy), +0xc frequency kHz, +0x10 `apiDomain` mask, +0x20 limit
code (bit 8 = MSVDD), +0x34 voltage µV, +0x13c valid, +0x144 resulting kHz.
Voltage limits are named by matching their voltage to the rail policy limits
(so "NVVDD REL limit 1055 mV → 3217 MHz" is data, not a table); frequency
limits are named by domain and ID; the PERF-CF controller minimums (0xd0–0xd2,
per Loong0x00) are flagged as floors. The page's "Boost limits" panel shows
the tightest core maximum among the non-floor clients as the reason the core
sits where it does, and lists the rest. Which client the driver actually
selects is not flagged by this object; the power-policy client was not
observed in testing (a 300 W cap did not engage within the test window).

Update 2026-09-14: the driver ships no name strings for the Blackwell
clients above NVML's table (the GSP firmware and every driver library were
searched; `PERF_LIMITS_GET_INFO` variants refuse every request size). Their
structure is visible in the data instead: two triples of (P-state limit,
GPC limit, XBAR limit), the same shape as the named thermal-policy set.
0x10f–0x111 are shown as "Power cap controller" (0x110/0x111 tracked a 400 W
cap); 0x112–0x114 as "Power policy 2" (0x114 followed the MSVDD current
limit). Type-1/3 rows are shown as "P-state limit (level N)"; the level's
meaning is not decoded.

## Tests section and the XBAR guard (2026-09-11)

The XBAR card no longer has a validated-maximum switch: the slider spans the
driver's range (±1000 MHz) and the hover note carries what the harness found
on the reference card (+250 daily, +300 clean, +340 and +380 silent
corruption with no crash or Xid, +450 hard lock). The guard is the
correctness check itself.

A "Tests" section runs the tooling repo's scripts from the page. The tooling
directory is `$LACT_ADV_TOOLS_DIR`, defaulting to
`~/claude_workspace/linux_mvolt`, and the torch venv comes from the tooling's
own `linuxvolt.json`. Buttons:

- **Correctness check** — harness run at the applied setting, compared bit
  for bit with `stock_a.json` (about 2.5 minutes). The verdict (MATCH or
  SILENT CORRUPTION) is shown in the status line.
- **Rebuild baseline** — two stock runs plus the probe reference; refuses
  unless every RM offset is 0 and 4 GiB of VRAM is free.
- **Steady load (2 min)** — the duty-cycled load the clock probes use.
- **Driver check** — the read-only half of the post-driver-update checklist.

Each run is a shell pipeline in its own process group (Stop ends all of it),
writing to `<tools>/gui_tests/<name>-<timestamp>.log`, which the page tails
once a second into the output pane. Buttons are disabled while a run is
active and greyed out entirely if the tooling directory is not found.

## Rail current limits (OCP) and rail currents (2026-09-11)

The RM `PWR_POLICIES` objects (INFO `0x2080a618`, CONTROL `0x2080a61a`, SET
`0x2080e61b`, STATUS `0x2080a619`) hold the board TGP and, on this card, one
current-limit policy per voltage rail — what mVolt+ calls OCP. INFO entry
word 0 is the boardobj header `{type, chIdx, limitUnit, n}` (limitUnit 0 =
mW, 1 = mA), followed by limitMin / limitRated / limitMax; CONTROL (policy
mask at +0x10, entries at `0x14 + i*0xc4`, type byte at +0, limit at +4)
is what the setter takes; STATUS (mask at +4, block i at `0xa0 + i*0x1730`) carries the
arbitrated limit at +0 and **the live channel reading at +4**.

The daemon locates the two rail policies by their properties — milliamp
unit, a finite rated limit under an unlimited maximum — and takes the larger
rated limit as NVVDD (480 A on the reference card, 180 A on MSVDD). Before
enabling anything it checks that the TGP entry (index from INFO header byte
+0x98) reads exactly NVML's power cap; any mismatch disables the feature.

What the reference card showed (steady load at the 620 W cap): NVVDD
307–373 A at 1.01 V, MSVDD about 72 A at 0.96 V. Lowering the NVVDD limit
to 100 A throttled the core within a second through the PMU's GPC limit
client (board power 150 W, NVML throttle reason "SW power cap"); the
type-0x11 twin policy on the same channel behaves identically. Lowering the
MSVDD limit to 50 A did the same (613 → 250 W, core 1300–1600 MHz, XBAR
pinned at 2497 MHz by `PWR_POLICY_XBAR`), while 100 A did nothing because
the reading was already under it. Neither limit binds at the 620 W TGP, so
raising them buys nothing until the power limit is raised toward the XOC
ceiling; lowering one is a way to cap that rail on its own.

On the page: the "NVVDD current limit (OCP)" / "MSVDD current limit (OCP)"
cards (config `nvvdd_current_limit_a` / `msvdd_current_limit_a`, amps;
`None` = the value found at daemon start), accepted range 50 A … 2× the rated
limit (NV-Voltelle's rule; the driver itself accepts 5001 A). Writes read a
fresh preimage, change only the two limit words, require an exact readback
and restore the preimage on mismatch; reset returns the start values. The
"NVVDD rail" / "MSVDD rail" tiles show the policies' live readings and the
power they imply at the rail target, published as the `NVVDD` / `MSVDD`
current sensors (amps, graphable as "Current (…)") and the `NVVDD rail` /
`MSVDD rail` power sensors (watts).

Cost: the boost-limit sweep (two 84 KB requests, ~27 ms) and the 400 KB
power-policy STATUS (~6 ms) are the two expensive RM reads; the daemon
refreshes each at most every 0.9 s regardless of the GUI's stats polling
interval, so a 250 ms poll no longer multiplies them.

## WireView II page (2026-09-13)

A second fork-only page, listed in the sidebar only while a Thermal Grizzly
WireView Pro II is plugged in (an STM32 virtual COM port under
`/dev/serial/by-id`). It is a port of the `wv2gui` / `wv2ctl` tooling:

- **Daemon** (`lact-daemon/src/server/wireview.rs`): the serial protocol
  (115200 baud, welcome / vendor / UID / build / sensors / config / NVM /
  clear-faults / screen commands), the version-2 config codec with its
  CRC-16/CCITT-FALSE (shared with the GUI in `lact_schema::wireview`), and
  the write discipline: back up the previous config to
  `/var/lib/lact/wireview/`, write, read back, and unless "live" store to
  flash and read the flash copy back. `NVM_LOAD` is always followed by
  putting the live config back, because it copies flash verbatim even when
  flash holds an older struct. The port is opened on demand and released
  after 5 s without requests, so `wv2ctl` still works while the page is not
  being looked at (while it is, the CLI gets "device busy").
- **Requests**: `wireview_info` (`None` when absent), `wireview_status`
  (readings + live config), `wireview_set_config` (with the expected
  previous raw config, refused if the device's changed), `wireview_nvm`
  (save / revert / factory reset), `wireview_flash`, `wireview_clear_faults`,
  `wireview_screen`.
- **Page**: live panel (power and current bars scaled to the OPP / OCP
  limits, six per-pin bars scaled to the per-wire limit, amber at 80 % and
  red at 95 %; temperatures, fan, average voltage, PSU capability, active
  and logged faults with a Clear button) and the five settings tabs of the
  Qt tool (Protection with the fault-action matrix, Fan, Display, Theme,
  Device). Edited fields are highlighted; Apply live / Save to flash show
  the exact changes before writing; Backup / Restore use the same JSON as
  `wv2ctl`, so either tool can read the other's files. The page polls once
  a second only while visible.

## Ports from Panchovix/LACT (2026-09-14)

Three pieces of [Panchovix's fork](https://github.com/Panchovix/LACT)
(`feat/nvidia-lower-power-limit`), re-verified on R615 and adapted:

- **RM device identity by PCI** (`driver/device_id.rs`): the daemon used the
  Linux minor as the RM `deviceId`, which on multi-GPU hosts can pick the
  wrong card (his 5090 was minor 4, RM instance 5). The root-client queries
  `GET_ATTACHED_IDS` / `GET_PCI_INFO` / `GET_ID_INFO_V2` now resolve the
  device and subdevice instances from the PCI slot before anything is
  allocated. Harmless on a single-GPU machine, correct on any other.
- **Power caps below the vBIOS minimum** (`driver/power_limit.rs`): the
  client power-policy group (`0x2080a630` / `a632` / `e633`, ordinary client
  0xFE) accepts board power requests under the minimum NVML enforces, and the
  card holds them (his PRO 6000: 100 W on a 250 W minimum; 30 W settled near
  74 W). The Power limit card's range now starts at 30 W: inside the vBIOS
  range the cap still goes through NVML, below it through this route, and a
  reset to default always through NVML. It is enabled only when the RM
  bounds and current request agree byte for byte with NVML on a verified
  driver branch, writes change one word with whole-block readback and
  restore on failure, and the F8 client is never touched. It is a lower
  route only: the vBIOS maximum stands.
- **Clock domain V/F curves** (`nvidia/rm_vf.rs`, "Clock domain V/F curves"
  on this page): `CLK_VF_POINTS` GET_INFO / GET_STATUS, 127-point banks in
  one flat index space matched to domains in `CLK_DOMAINS` order (GPC, XBAR,
  memory without a curve, SYS, video, PWRCLK on GB202). Read-only: the
  driver takes writes to these points but gives no way to check them, and
  one domain drops a written point on the next read. Curves can be toggled
  in the legend (all on by default); hover reads the nearest point on both axes.

Also from his measurements: the previously unnamed domains with API bits
0x80000 and 0x200000 are PWRCLK and the legacy clock, both of which follow
XBAR through the propagation ratio; they are named in the domain table now.

## Boot guard (2026-09-15)

A tuned profile that hangs the machine is re-applied by the daemon at the
next boot, and with automatic switching on it comes back the moment the
game launches again. The boot guard breaks that loop the way Afterburner's
safe mode does on Windows, with a dirty flag on disk.

**Mechanism.** With the guard enabled the daemon writes
`/var/lib/lact/boot_guard/armed.json` (profile name, kernel boot id,
timestamp; written to a temp file, fsynced, renamed, directory fsynced)
*before* it applies settings — at startup, on a profile switch, on a config
reload, and on a manual Apply — and removes it on a clean shutdown (SIGTERM
from systemd) or a config reset. At startup a marker that is still there
means the previous session never shut down cleanly. The daemon then
*engages*: it records the trip in `engaged.json`, removes the marker, and
applies the **fallback** instead of the saved profile. The fallback is
*stock* by default — nothing is applied and, because a marker from the same
kernel boot means only the daemon died, the controllers are reset first so
nothing survives from before — or a named profile. Automatic profile
switching is not started while engaged, so a process rule cannot re-apply
the suspect profile. The trip survives daemon restarts until it is resolved.

**Resolving it.** The GUI shows a banner ("Boot guard engaged: the system
stopped uncleanly while profile 'bench' was active. The GPU is running stock
settings.") with a *Resume saved profile* button, and the Boot guard row on
this page has the same plus *Keep fallback until reboot*. Resume applies the
saved profile again (and re-arms), and restarts automatic switching if it
was on. Keep clears the notice but leaves the fallback in place; the next
boot is a normal one because nothing was armed. Any explicit action that
applies settings — switching a profile, applying from either page, a config
reset — also counts as resuming, since the user is taking over; passive
re-applies (a config-file reload, a GPU reload after suspend) keep the
fallback.

**Controls.** *Enabled* arms the marker; *Fallback* picks stock or a
profile; *Fallback at next start* is a one-shot that engages at the next
daemon start whether or not anything crashed (cleared once used), for
settings you already distrust. The status line says whether the marker is
on disk. If the state directory cannot be written the row says so and the
guard is inert rather than pretending.

**Login notice.** While engaged and unacknowledged the daemon keeps
`/run/motd.d/lact-boot-guard`, which `pam_motd` shows on tty and SSH logins.
Terminals opened from the desktop do not go through `pam_motd`, so
`install_fork.sh` also installs `res/boot-guard/lact-boot-guard.sh` into
`/etc/profile.d/`, the fish variant into `/etc/fish/conf.d/`, and sources
the sh snippet from `/etc/zsh/zshrc`; each prints the file in interactive
shells only. Resume or Keep removes it.

**What trips it that is not a GPU crash.** Any unclean stop: a power cut,
an unrelated kernel panic, holding the power button. That is accepted as
the conservative side; recovery is one click. A check of the previous
boot's kernel log for an Xid was left out on purpose — a hard hang often
logs nothing, so it could only make the guard less sensitive.

**Testing.** `sudo systemctl kill -s KILL lactd` exercises the same-boot
path (systemd restarts the daemon, which finds its own marker); a real
unclean shutdown — `echo c | sudo tee /proc/sysrq-trigger` with everything
saved — exercises the reboot path. A normal reboot must *not* trip it; that
is the case to check first. Unit tests cover the marker lifecycle,
the one-shot, a corrupt marker (an error, not a silent pass) and the config
round-trip.
