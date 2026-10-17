//! Prints the parsed report of a ROM file as JSON: `cargo run -p lact-vbios --example vbios_report -- <file>`

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: vbios_report <rom file>");
    let data = std::fs::read(&path).expect("could not read the ROM file");
    match lact_vbios::parse(&data) {
        Ok(report) => println!("{}", serde_json::to_string_pretty(&report).unwrap()),
        Err(err) => {
            eprintln!("{path}: {err}");
            std::process::exit(1);
        }
    }
}
