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
