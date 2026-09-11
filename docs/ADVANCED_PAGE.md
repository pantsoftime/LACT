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
evaluated limits; +0x18 / +0x1c move no limit and are never written. The
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
