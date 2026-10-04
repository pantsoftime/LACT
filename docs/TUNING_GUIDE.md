# Tuning guide (Advanced page)

What each control on the **Advanced** page actually does, what it is measurably
worth, how it fails, and how to tell a good setting from one that only *looks*
good. [ADVANCED_PAGE.md](ADVANCED_PAGE.md) covers the mechanism and the private
RM interfaces; this page is the tuning reference.

> **Every number here came from one card.** An ASUS ROG Astral RTX 5090
> (GB202), 800 W Matrix/XOC vBIOS `98.02.2E.80.C9`, driver 615.71.09, on Linux.
> Silicon varies: the *validated maxima below are not transferable*. Treat them
> as the shape of the problem and the order of magnitude, not as settings to
> copy. The failure modes and the validation method **are** transferable, and
> they are the part worth reading.

---

## 1. The one thing to take away

**A setting that never crashes can still compute wrong results.**

On the reference card, XBAR +340 and +380 MHz passed every check the card
offers — clocks read back correctly, no Xid, no driver message, no artifacts,
no crash — and produced **bit-different output** from a deterministic workload.
+450 MHz hard-locked the machine. There is a window between "works" and
"crashes" where the card quietly returns wrong answers.

Stability testing that looks for crashes and artifacts cannot see this. If you
tune the fabric clocks (XBAR, SYS, video) you need a **correctness** check: run
a fixed-seed workload, hash the output, and compare it bit-for-bit against the
same workload at stock. See [§7](#7-validating-a-setting).

This matters most for compute and AI workloads, where a silent bit flip
propagates into results you keep. For gaming it is less visible — which is
exactly why it goes unnoticed.

---

## 2. What each control is actually worth

Measured on the reference card. This table is the main reason to read this
page: the ranking is **not** what tuning folklore suggests.

| control | measured gain | cost | verdict |
|---|---|---|---|
| **Memory offset** | **+9.8%** bandwidth (+3000 → +6000) | ~46 W | **The only clean win.** Tune this first. |
| **Core offset** | ~+2% prefill (+110 → +200) | power-capped | Worth doing, modest. |
| **XBAR offset** | **+0.42%** decode (at +270) | — | Real but tiny; the riskiest control on the page. |
| **SYS offset** | **+0.02%** (≈ nothing), twice measured | — | Buys nothing on these workloads. |
| **MSVDD clock ratio** | nothing at normal offsets | — | A ceiling that does not bind. Leave at factory. |
| **V/F flatten @ 1050 mV** | throughput inconclusive | **−29 W** prefill, −14 W decode | A *power* lever, not a performance one. |
| **MSVDD rail offset** | negative — see [§5](#5-voltage-rails) | — | Not free headroom. |

Memory bandwidth scaled linearly across the whole adjustable range at a
constant **93.3% of theoretical**, with no fall-off:

| memory offset | memory clock | GB/s | % of theoretical |
|---|---|---|---|
| +3000 | 15,287 | 1,826.0 | 93.3% |
| +4000 | 15,785 | 1,886.1 | 93.4% |
| +5000 | 16,285 | 1,945.7 | 93.3% |
| +6000 | 16,785 | 2,005.1 | 93.3% |

No "it went flat" point exists inside the range. If your bandwidth *does* fall
away from the theoretical line as you raise the clock, that is GDDR7 link
retries — the link is past its clean limit and losing throughput silently,
while still passing every correctness test. Watch the percentage, not just the
GB/s.

---

## 3. Known-good starting configs

Two configs validated on the reference card. **Start below these and work up**,
validating each step.

| | core | boost | mem | XBAR | SYS | power cap |
|---|---|---|---|---|---|---|
| **Everyday** | +150 | 50 | +6000 | +270 | 0 | 660 W |
| **Peak** | +175 | 100 | +6000 | +270 | 0 | 660 W |

The everyday config sits deliberately below the validated peak for stability
margin. That is a reasonable habit: the last 25 MHz of core is worth ~0.5% and
costs you the margin that absorbs a hot day or a different workload.

---

## 4. Clock domains

### Memory offset

The one control with a real payoff. On the reference card every step from
+3000 to +6000 passed all gates including `memtest_vulkan`, and no failure was
found anywhere in the adjustable range.

**The ceiling is administrative, not physical.** The driver's reported range
(±6000 NVML, ±3000 MHz on the RM plane) is *metadata, not enforcement* — the RM
plane accepts an out-of-range write and stores it verbatim. What actually binds
is a **hard clamp on the resulting memory frequency**, enforced below both
control planes in GSP/firmware, sourced from a table in the vBIOS. Ask for
+3500 or +6000 or anything larger and you land on the same clock (~16.78 GHz
measured on the Matrix tables).

So the card never showed a memory limit of its own. Raising it requires a vBIOS
carrying a higher bound, not a software trick — and the two control planes do
**not** stack (each reaches the same final clock; applying the second adds
nothing).

> **Never set a finite maximum memory clock**
> (`nvidia-smi --lock-memory-clocks=a,b`). Upstream issue #1266: a non-binding
> maximum collapses XBAR by ~36% and SYS to ~1500 MHz, costing 15–32%
> performance — while the *reported* GPU clock goes up. Use offsets, not locked
> clocks.

### Core offset

Validated to **+175** soaked on the reference card; +200 passed a short gate.
**+250 crashes**, and not gently: cuBLAS `INTERNAL_ERROR`, two Xid 109s, then
Xid 154 PF FLR → "GPU Recovery Action: Reset" → reboot required.

Worth roughly +1% prefill per +50 MHz, measured while pinned at the power cap.
Note that a power-capped comparison behaves differently: at the cap the GPU
trades voltage for clocks rather than drawing more power.

The page writes the core offset to **pstate 0 only** and clears other entries,
because on this driver NVML's per-pstate offset is a single global register —
a stray zero written for another pstate silently cancels the value.

### XBAR offset

The GPU's internal crossbar/interconnect clock — *not* memory speed. It governs
data movement between the core, caches and memory controllers.

| setting | result |
|---|---|
| +270 / +275 | harness-validated (+300 clean in earlier testing) |
| **+340** | **silent corruption** |
| **+380** | **silent corruption** — wrong matmul/attention results, no Xid, no crash |
| **+450** | hard lock |

Worth **+0.42%** on decode. Weigh that against the failure mode: this is the
control that corrupts quietly. The card's slider spans the full driver range
(±1000 MHz) deliberately — there is no guard switch, because a number is not a
guard. **The correctness check is the guard.**

### SYS offset

Measured at 0, +200 and +400: all pass, no ceiling found, and it bought
**+0.02%** — nothing, across two separate measurement sessions. The apparent
1% loss seen in a first pass was thermal drift, caught by re-running the
reference last (see [§8](#8-measurement-traps)).

It was never searched to failure because there is no reason to. If you have a
front-end-bound workload (heavy draw-call traffic, many tiny kernels) it might
show something; on compute workloads it does not.

### Video offset

The media engine clock (NVENC/NVDEC). Irrelevant to gaming and compute; only
worth touching if you are encoding/decoding heavily. Untested for throughput
on the reference card.

### MSVDD clock ratio

Exposed as a card, and frequently misunderstood. It is the **GPC→XBAR
propagation ratio** (factory 0.90 on GB202, accepted range 0.80–1.20).

**It is a propagation constraint, not `XBAR = GPC × ratio`.** You will see the
multiply-the-core-clock formula repeated in tuning discussions; it does not
describe what this control does. On the reference card it is a ceiling that
**does not bind at normal offsets**, so changing it buys nothing. Leave it at
factory unless you are investigating the propagation topology itself.

---

## 5. Voltage rails

### MSVDD rail offset

**Not free headroom.** On the reference card, +20 mV on the XBAR-domain rail
*lowered* XBAR by about 31 MHz on its own, did not extend the ceiling, and did
not prevent the lockup at +450. It is bounded to ±50 mV and labelled for that
reason.

The intuition "more voltage on the fabric rail → more fabric clock" does not
hold here. Measure before believing it on your card.

Note that **−50 mV on MSVDD REL is the driver's default** on GB202, which is
why the MSVDD REL limit sits 50 mV below NVVDD's. That is stock behaviour, not
something that was done to your card.

### NVVDD / per-domain demand

Per-domain voltage demand offsets target each domain's own rail slot, read from
the INFO entry's rail mask. The NVVDD offset is the **GPC domain's** demand
(rail 0). A rail-0 offset on the *XBAR* domain — an earlier version of this
control — never did anything, which the rail mask explains.

### Voltage boost

Sets the rail ceilings rather than a voltage directly:

| boost | NVVDD | MSVDD |
|---|---|---|
| 100 | 1075 mV | 1025 mV |
| 50 | 1065 mV | 1015 mV |

A 10 mV span. What it buys beyond that has not been measured.

### Rail voltage limits and OCP — leave these alone

The rail limit deltas (VMIN / REL / ALT-OP / OV) and the rail current limits
(OCP) are exposed and functional, but they are **not performance controls**:

- The driver's **evaluated MAX is the tightest of REL, ALT/OP and OV**. Raising
  REL alone does nothing while ALT or OV is tighter.
- Neither current limit binds at a 620–660 W board cap, so raising them buys
  nothing. *Lowering* one is a way to cap a single rail deliberately — on the
  reference card, dropping NVVDD to 100 A throttled the core to 150 W board
  power within a second.

Treat raising these as out of scope for tuning. They exist for investigation.

---

## 6. Power, thermals and safety

### Power limit

One of the largest real-world levers, because most tuning on this card is
power-bound before it is clock-bound. The reference card runs a 660 W user
ceiling; prefill runs pinned at the cap, decode sits around 540 W uncapped.

The card's power card also supports caps *below* the vBIOS minimum through the
client power-policy route (down to 30 W). Inside the vBIOS range the cap goes
through NVML; below it through the RM route; a reset always through NVML.

> **Connector margin is the tightest safety number measured.** At 646 W board
> power, pin 6 of the 12V-2x6 carried **10.17 A** — above the commonly cited
> 9.5 A per-contact rating and about 3% under a 10.5 A wire-OCP trip. Pin 6 was
> the hottest in 98% of samples. If you run high power caps, reseat the
> connector and consider per-pin monitoring. 12 V sagging 12.07 → 11.92 V under
> load is normal and healthy.

### V/F curve editing

Two distinct things, and they stack:

- **Core (GPC) curve** — upstream's editor writes the *client* layer: the NVML
  core offset applied as the same value to all 127 points. The Advanced page's
  editor writes the separate *regional* layer, all zeros by default.
- **Fabric curves (XBAR, SYS, video)** — per-point offsets, range −1000…+300 MHz
  per point.

The practical use of the fabric editor is **not** chasing clocks: it is holding
a fabric clock *down* over the specific voltage region where the correctness
harness finds errors, while keeping the global offset that passes everywhere
else. That is a targeted fix, and it is the right tool for it.

Flattening the core curve at 1050 mV on the reference card cost 15 MHz of GPC
and saved **29 W** on prefill, 14 W on decode, with throughput inconclusive.
Treat flattening as a power/thermal lever.

> Dropping the V/F curve config key does **not** revert the card — the points
> stay written. Clear the curve explicitly.

### Thermal inputs (VFE) — understand the risk

These simulate a temperature to a sensor. The firmware's VFE adds voltage
margin as the card warms; feeding it a *lower* temperature than reality removes
that margin, which is the lever these controls sell. A value low enough **can
blind the card's own protection.**

The daemon has guards — value floored at 20 °C, refused while LACT's fan
control is active, all simulations cleared when the config is reapplied, and a
watchdog that clears every simulation if any independent sensor reaches 95 °C.
Guards are not a reason to be casual: you are overriding a safety input.

Note the effect is large in both directions — pinning the GPU sensor to 60 °C
dropped the idle boost clock from 3277 MHz to about 1600 MHz.

### Fan control

Not an electrical tuning control, but on a card that can pull 600+ W it is
often the difference between sustained and throttled clocks. Note that LACT's
own fan curve and the thermal-input simulation are mutually exclusive by
design — the curve would follow the simulated reading.

---

## 7. Validating a setting

Validate by **correctness**, not by the absence of crashes.

The minimum useful loop for a frequency offset:

1. **Build a stock baseline.** Two runs at stock, compared against each other —
   they must match bit-for-bit before the baseline means anything.
2. **Apply one change.** One control at a time. Two at once and you have
   learned nothing about either.
3. **Run the correctness check** — a fixed-seed workload producing a bit-exact
   digest, compared with the stock baseline. `MATCH` or `SILENT CORRUPTION`.
4. **Watch for Xids** — necessary, not sufficient. Corruption arrives without
   one.
5. **Soak it.** Minutes is not a soak. A setting that passes a 2-minute gate
   and fails after an hour is the normal case, not an edge case.

The Advanced page's **Tests** section runs this tooling directly (correctness
check, baseline rebuild, steady load, torch bench, FurMark, driver check).

**Prove your detector fires.** A digest that never changes is not a check — an
early version of the reference harness was hashing an all-NaN tensor and would
have reported `MATCH` forever. Deliberately break something and confirm the
check notices.

---

## 8. Measurement traps

Every one of these produced a wrong conclusion before it was understood.

- **A single run resolves about 1%.** Anything smaller needs interleaved ABBA
  rounds (~0.15%). Do not call a sub-1% single-run difference a result.
- **Thermal drift inside a session is real.** Across four consecutive runs GPC
  sagged 3265 → 3242 → 3239 → 3231 MHz and VRAM went 52 → 56 °C, *independent
  of the setting being tested*. **Always re-run the reference last** — that
  control is what proved SYS costs nothing.
- **Cross-session throughput is not comparable.** Host load swings it 7–10%; a
  desktop app's own GPU usage cost ~10% while merely visible. Compare
  back-to-back runs only, in one session.
- **A power-capped comparison inverts.** At the cap the GPU trades voltage for
  clocks instead of drawing more power, so the usual relationships reverse.
- **A result at idle power is not evidence.** An early harness "measured" a
  3.6% effect on a control that could not possibly have been affected, because
  it was timing 0.5–1.4 s of work at 21–49 W on idle clocks.
- **Use a positive control.** When comparing two configurations, make sure
  something observable differs that you *know* should differ. Otherwise a null
  result cannot distinguish "the change did nothing" from "the change never
  took effect."

---

## 9. Memory timings — read-only, and why

The page shows the live GDDR7 timing words (CONFIG0/CONFIG1 at FBPA base
+0x290/+0x294, decoded as tRC/tRFC/tRAS/tRP and tCL/tWL/tRCD) and refuses the
decode unless tRC = tRAS + tRP holds. They are **read-only, and this is not a
missing feature** — two independent routes were tested to exhaustion on the
reference card:

1. **Runtime register writes are blocked by hardware.** A masked `WRITE_32` to
   the FBPA timing block through `EXEC_REG_OPS` returns `NV_OK` with per-op
   status OK — and the value never lands. The readback 0 ms later is the
   original word, and stays so across 100 reads over 5 seconds, with no
   transition and no Xid. This is *not* the firmware re-asserting the value
   (the pattern reported on Ada, where a write takes and is then overwritten);
   the write never arrives. The FBPA timing registers are protected by a
   privilege-level mask — only the falcons (signed firmware) can write them.
   The RM/GSP path reports the RPC, not the PRI acknowledgement, which is why
   it returns success regardless.

2. **The vBIOS timing table is not consumed from a host-supplied image.** The
   GDDR7 tweak table is object `0x10` of the vBIOS object directory. A
   hash-consistent image with a loosened tRP (28 → 30, tRC held at tRAS + tRP)
   was staged to GSP through the vBIOS-override path and **the FBPA still
   trained to the stock word.** The image was definitely consumed — the clock
   offset-range table (object `0x03`) from the *same* image applied on the same
   boot — so the firmware parses the directory but does not take memory timings
   from it. They are read during training from the real flash.

So on GB202, GDDR7 timings are reachable from **neither** a privileged MMIO
write **nor** a host-supplied vBIOS table. Claims that memory timings are
tunable on this generation should be treated as unproven until someone shows a
changed FBPA word.

---

## 10. Recovery

- **Offsets do not survive a reboot.** A reboot is always a valid recovery path.
- **LACT's confirm-or-revert timer** covers the RM offsets like any other clock
  setting — an unconfirmed setting reverts on its own.
- **The boot guard** breaks the loop where a profile that hangs the machine is
  reapplied at the next boot. If it engages, the GPU comes up stock and the GUI
  offers *Resume saved profile* or *Keep fallback until reboot*.
- **A hard lock** (XBAR far past its limit) needs a power cycle, and the boot
  guard is what stops it recurring.
- Note that **LACT persists every configuration you apply**, including ones you
  were only testing. Snapshot a known-good profile before a tuning session.

---

## 11. What is still untested

Honest gaps on the reference card, so nobody mistakes silence for a clean bill:

1. **Graphics/gaming stability.** Every gate above is compute. XBAR +270 and
   core +175 have never been validated in a game, and freezes that only appear
   under light or irregular load are a reported pattern.
2. **Light-load stability** of the mid V/F points (800–950 mV), never stressed
   specifically.
3. **Multi-hour soak** of the everyday config; the longest so far is minutes.
4. **The core bracket between +200 and +250**, where the real edge is.
5. **XBAR retune** at higher memory and core clocks.
6. **Voltage boost 0 vs 100** — never measured beyond the 10 mV ceiling.
