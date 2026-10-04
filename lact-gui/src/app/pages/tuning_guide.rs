//! The tuning guide window, opened from the Advanced page.
//!
//! The per-card tooltips on the Advanced page answer "what is this control?".
//! They cannot answer the questions that span controls: which knob is worth
//! turning first, what a safe starting point looks like, how to tell a good
//! setting from one that only looks good, and which measurement mistakes
//! produce confident wrong answers. That is what this window holds.
//!
//! Content mirrors `docs/TUNING_GUIDE.md`; keep the two in step. Every number
//! is from the reference card (RTX 5090 / GB202, 800 W Matrix vBIOS, driver
//! 615.71.09) and is a data point, not a recommendation — the window says so
//! before it says anything else, because validated maxima are silicon-specific
//! while the failure modes and the validation method are not.

use gtk::prelude::*;

const PAD: i32 = 12;

enum Block {
    Para(&'static str),
    /// Called out in the warning style — the things that cost you a card or a
    /// week of wrong conclusions.
    Note(&'static str),
    Bullets(&'static [&'static str]),
    Table(&'static [&'static str], &'static [&'static [&'static str]]),
}

struct Section {
    title: &'static str,
    blocks: &'static [Block],
}

const SECTIONS: &[Section] = &[
    Section {
        title: "Before anything else",
        blocks: &[
            Block::Note(
                "Every number in this guide came from one card: an RTX 5090 (GB202), 800 W Matrix vBIOS, driver 615.71.09. Silicon varies. The validated maxima are NOT transferable — treat them as the shape of the problem, not as settings to copy. The failure modes and the validation method are transferable, and they are the part worth reading.",
            ),
            Block::Para(
                "A setting that never crashes can still compute wrong results. On the reference card, XBAR +340 and +380 MHz passed every check the card offers — clocks read back correctly, no Xid, no driver message, no artifacts, no crash — and produced bit-different output from a deterministic workload. +450 MHz hard-locked the machine.",
            ),
            Block::Para(
                "There is a window between \"works\" and \"crashes\" where the card quietly returns wrong answers. Stability testing that looks for crashes and artifacts cannot see it. If you tune the fabric clocks (XBAR, SYS, video) you need a correctness check, not a benchmark.",
            ),
        ],
    },
    Section {
        title: "What each control is actually worth",
        blocks: &[
            Block::Para(
                "Measured on the reference card. This ranking is the main reason to read this window — it is not what tuning folklore suggests.",
            ),
            Block::Table(
                &["control", "measured gain", "verdict"],
                &[
                    &[
                        "Memory offset",
                        "+9.8% bandwidth (+3000 to +6000)",
                        "The only clean win. Tune this first.",
                    ],
                    &[
                        "Core offset",
                        "~+2% prefill (+110 to +200)",
                        "Worth doing, modest.",
                    ],
                    &[
                        "XBAR offset",
                        "+0.42% decode (at +270)",
                        "Tiny, and the riskiest control on the page.",
                    ],
                    &[
                        "SYS offset",
                        "+0.02% (nothing), measured twice",
                        "Buys nothing on these workloads.",
                    ],
                    &[
                        "MSVDD clock ratio",
                        "nothing at normal offsets",
                        "Does not bind. Leave at factory.",
                    ],
                    &[
                        "V/F flatten @1050 mV",
                        "-29 W prefill, -14 W decode",
                        "A power lever, not a performance one.",
                    ],
                    &[
                        "MSVDD rail offset",
                        "negative: +20 mV cost 31 MHz of XBAR",
                        "Not free headroom.",
                    ],
                ],
            ),
            Block::Para(
                "Tuning advice that puts the fabric clocks first takes the largest risk for the smallest gain. Memory is where the performance is.",
            ),
        ],
    },
    Section {
        title: "Known-good starting configs",
        blocks: &[
            Block::Table(
                &["", "core", "boost", "mem", "XBAR", "SYS", "power cap"],
                &[
                    &["Everyday", "+150", "50", "+6000", "+270", "0", "660 W"],
                    &["Peak", "+175", "100", "+6000", "+270", "0", "660 W"],
                ],
            ),
            Block::Para(
                "Start below these and work up, validating each step. The everyday config sits deliberately under the validated peak: the last 25 MHz of core is worth about 0.5% and costs the margin that absorbs a hot day or a different workload.",
            ),
        ],
    },
    Section {
        title: "Validated limits per domain",
        blocks: &[
            Block::Table(
                &["domain", "validated good", "known bad", "failure mode"],
                &[
                    &[
                        "Memory",
                        "+6000 (the driver's max)",
                        "none found in range",
                        "bandwidth linear at a constant 93.3% of theoretical",
                    ],
                    &[
                        "Core",
                        "+175 soaked, +200 short gate",
                        "+250",
                        "cuBLAS INTERNAL_ERROR, Xid 109 x2, Xid 154 -> reset, reboot",
                    ],
                    &[
                        "XBAR",
                        "+270 / +275 (+300 clean earlier)",
                        "+340, +380, +450",
                        "silent corruption at +340/+380; hard lock at +450",
                    ],
                    &[
                        "SYS",
                        "0, +200, +400 all pass",
                        "not searched",
                        "no ceiling found; buys nothing so never pushed",
                    ],
                ],
            ),
            Block::Note(
                "Never set a finite maximum memory clock (nvidia-smi --lock-memory-clocks). Upstream issue #1266: a non-binding maximum collapses XBAR by about 36% and SYS to around 1500 MHz, costing 15-32% performance, while the reported GPU clock goes up. Use offsets, not locked clocks.",
            ),
            Block::Para(
                "The memory ceiling is administrative, not physical. The driver's reported range is metadata, not enforcement — the RM plane accepts an out-of-range write and stores it verbatim. What binds is a hard clamp on the resulting frequency, enforced below both control planes in firmware and sourced from a vBIOS table. The two control planes do not stack.",
            ),
        ],
    },
    Section {
        title: "Validating a setting",
        blocks: &[
            Block::Para("Validate by correctness, not by the absence of crashes."),
            Block::Bullets(&[
                "Build a stock baseline: two runs at stock, compared against each other. They must match bit-for-bit before the baseline means anything.",
                "Change one control at a time. Two at once and you have learned nothing about either.",
                "Run the correctness check: a fixed-seed workload producing a bit-exact digest, compared with the stock baseline. MATCH or SILENT CORRUPTION.",
                "Watch for Xids — necessary, not sufficient. Corruption arrives without one.",
                "Soak it. Minutes is not a soak. Passing a 2-minute gate and failing after an hour is the normal case.",
            ]),
            Block::Para(
                "The Tests section further down the Advanced page runs this tooling directly: correctness check, baseline rebuild, steady load, torch bench, FurMark and the driver check.",
            ),
            Block::Note(
                "Prove your detector fires. A digest that never changes is not a check — an early version of the reference harness was hashing an all-NaN tensor and would have reported MATCH forever. Break something deliberately and confirm the check notices.",
            ),
        ],
    },
    Section {
        title: "Measurement traps",
        blocks: &[
            Block::Para(
                "Every one of these produced a confident wrong conclusion before it was understood.",
            ),
            Block::Bullets(&[
                "A single run resolves about 1%. Anything smaller needs interleaved ABBA rounds (~0.15%). Do not call a sub-1% single-run difference a result.",
                "Thermal drift inside a session is real. Across four consecutive runs GPC sagged 3265 -> 3231 MHz and VRAM went 52 -> 56 C, independent of the setting being tested. Always re-run the reference last.",
                "Cross-session throughput is not comparable. Host load swings it 7-10%; a desktop app's own GPU usage cost about 10% while merely visible. Compare back-to-back runs in one session.",
                "A power-capped comparison inverts. At the cap the GPU trades voltage for clocks instead of drawing more power, so the usual relationships reverse.",
                "A result at idle power is not evidence. An early harness measured a 3.6% effect on a control that could not possibly have mattered, because it was timing idle clocks.",
                "Use a positive control. Make sure something observable differs that you know should differ, or a null result cannot distinguish \"the change did nothing\" from \"the change never took effect\".",
            ]),
        ],
    },
    Section {
        title: "Why the memory timings table is read-only",
        blocks: &[
            Block::Para(
                "Not a missing feature. Two independent routes were tested to exhaustion on the reference card, and both are closed.",
            ),
            Block::Bullets(&[
                "Runtime register writes are blocked by hardware. A masked WRITE_32 to the FBPA timing block returns NV_OK with per-op status OK, and the value never lands: the readback 0 ms later is the original word, and stays so across 100 reads over 5 seconds, with no Xid. The registers are protected by a privilege-level mask — only signed falcon firmware can write them. The RM/GSP path reports the RPC, not the PRI acknowledgement, which is why it reports success.",
                "The vBIOS timing table is not consumed from a host-supplied image. A hash-consistent image with a loosened tRP was staged to GSP through the vBIOS-override path and the FBPA still trained to the stock word — while the clock offset-range table from the same image applied on the same boot. The firmware parses the directory but takes memory timings from the real flash.",
            ]),
            Block::Para(
                "So on GB202, GDDR7 timings are reachable from neither a privileged MMIO write nor a host-supplied vBIOS table. Claims that memory timings are tunable on this generation should be treated as unproven until someone shows a changed FBPA word.",
            ),
        ],
    },
    Section {
        title: "Recovery",
        blocks: &[
            Block::Bullets(&[
                "Offsets do not survive a reboot. A reboot is always a valid recovery path.",
                "LACT's confirm-or-revert timer covers the RM offsets like any other clock setting: an unconfirmed setting reverts on its own.",
                "The boot guard breaks the loop where a profile that hangs the machine is re-applied at the next boot. If it engages, the GPU comes up stock and you get Resume saved profile or Keep fallback until reboot.",
                "A hard lock needs a power cycle, and the boot guard is what stops it recurring.",
                "LACT persists every configuration you apply, including ones you were only testing. Snapshot a known-good profile before a tuning session.",
            ]),
            Block::Note(
                "Connector margin is the tightest safety number measured. At 646 W board power, pin 6 of the 12V-2x6 carried 10.17 A — above the commonly cited 9.5 A per-contact rating and about 3% under a 10.5 A wire-OCP trip. Pin 6 was hottest in 98% of samples. If you run high power caps, reseat the connector.",
            ),
        ],
    },
];

fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .margin_top(PAD)
        .css_classes(["heading", "accent"])
        .build()
}

fn body(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(92)
        .build()
}

fn note(text: &str) -> gtk::Widget {
    let label = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(88)
        .css_classes(["warning"])
        .build();
    let frame = gtk::Frame::builder().css_classes(["view"]).build();
    let holder = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    holder.set_margin_top(6);
    holder.set_margin_bottom(6);
    holder.set_margin_start(8);
    holder.set_margin_end(8);
    holder.append(
        &gtk::Image::builder()
            .icon_name("dialog-warning-symbolic")
            .valign(gtk::Align::Start)
            .css_classes(["warning"])
            .build(),
    );
    holder.append(&label);
    frame.set_child(Some(&holder));
    frame.upcast()
}

fn bullets(items: &[&str]) -> gtk::Widget {
    let list = gtk::Box::new(gtk::Orientation::Vertical, 4);
    for item in items {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(
            &gtk::Label::builder()
                .label("\u{2022}")
                .valign(gtk::Align::Start)
                .css_classes(["dim-label"])
                .build(),
        );
        row.append(
            &gtk::Label::builder()
                .label(*item)
                .xalign(0.0)
                .wrap(true)
                .max_width_chars(88)
                .hexpand(true)
                .build(),
        );
        list.append(&row);
    }
    list.upcast()
}

fn table(headers: &[&str], rows: &[&[&str]]) -> gtk::Widget {
    let grid = gtk::Grid::builder()
        .row_spacing(4)
        .column_spacing(16)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    for (col, head) in headers.iter().enumerate() {
        grid.attach(
            &gtk::Label::builder()
                .label(*head)
                .xalign(0.0)
                .css_classes(["heading"])
                .build(),
            col as i32,
            0,
            1,
            1,
        );
    }
    for (r, row) in rows.iter().enumerate() {
        for (col, cell) in row.iter().enumerate() {
            grid.attach(
                &gtk::Label::builder()
                    .label(*cell)
                    .xalign(0.0)
                    .wrap(true)
                    .max_width_chars(38)
                    .build(),
                col as i32,
                r as i32 + 1,
                1,
                1,
            );
        }
    }
    let frame = gtk::Frame::builder().css_classes(["view"]).build();
    grid.set_margin_start(8);
    grid.set_margin_end(8);
    grid.set_margin_top(8);
    grid.set_margin_bottom(8);
    frame.set_child(Some(&grid));
    frame.upcast()
}

/// Build and show the guide. `parent` makes it transient for the main window
/// so it stacks and closes with it; `None` is fine (it is then a plain
/// top-level), which keeps this callable from anywhere on the page.
pub fn present(parent: Option<&gtk::Window>) {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 6);
    content.set_margin_top(PAD);
    content.set_margin_bottom(PAD);
    content.set_margin_start(PAD);
    content.set_margin_end(PAD);

    content.append(&body(
        "What each control on the Advanced page is measurably worth, how it fails, and how to tell a good setting from one that only looks good. Each card's own tooltip explains that control; this window covers what spans them.",
    ));

    for section in SECTIONS {
        content.append(&heading(section.title));
        for block in section.blocks {
            match block {
                Block::Para(text) => content.append(&body(text)),
                Block::Note(text) => content.append(&note(text)),
                Block::Bullets(items) => content.append(&bullets(items)),
                Block::Table(headers, rows) => content.append(&table(headers, rows)),
            }
        }
    }

    content.append(
        &gtk::Label::builder()
            .label("The full written version, with the run log and what is still untested, is docs/TUNING_GUIDE.md in this fork.")
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(92)
            .margin_top(PAD)
            .css_classes(["caption", "dim-label"])
            .build(),
    );

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&content)
        .build();

    let window = gtk::Window::builder()
        .title("Tuning guide")
        .default_width(860)
        .default_height(760)
        .child(&scrolled)
        .build();
    window.set_transient_for(parent);
    window.present();
}
