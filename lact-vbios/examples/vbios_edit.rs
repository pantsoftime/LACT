//! Writes an edited copy of a ROM: `cargo run -p lact-vbios --example vbios_edit -- IN OUT field=value...`
//! Fields: board_power_max_mw, board_power_default_mw, board_power_min_mw, nvvdd_rel_uv,
//! msvdd_rel_uv, dram_max_table_mhz.

use lact_vbios::edit::{EditField, Rail, edit};

fn main() {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let [input, output, changes @ ..] = &args[..] else {
        eprintln!("usage: vbios_edit IN OUT field=value...");
        std::process::exit(2);
    };
    let changes = changes
        .iter()
        .map(|change| {
            let (name, value) = change.split_once('=').expect("field=value");
            let field = match name {
                "board_power_max_mw" => EditField::BoardPowerMax,
                "board_power_default_mw" => EditField::BoardPowerDefault,
                "board_power_min_mw" => EditField::BoardPowerMin,
                "nvvdd_rel_uv" => EditField::RelVoltageMax(Rail::Nvvdd),
                "msvdd_rel_uv" => EditField::RelVoltageMax(Rail::Msvdd),
                "dram_max_table_mhz" => EditField::DramOffsetMax,
                other => panic!("unknown field {other}"),
            };
            (field, value.parse().expect("integer value"))
        })
        .collect::<Vec<_>>();
    let data = std::fs::read(input).expect("could not read the input");
    match edit(&data, &changes) {
        Ok(result) => {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(output)
                .expect("could not create the output (it must not exist)");
            std::io::Write::write_all(&mut file, &result.data).expect("could not write the output");
            println!("{}", serde_json::to_string_pretty(&result.summary).unwrap());
        }
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    }
}
