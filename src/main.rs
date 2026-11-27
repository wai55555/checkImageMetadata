use fast_meta::{extract_to_writer_with_diagnostics, ScanOptions, StealthPolicy};
use std::io::{self, Write};
use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut strict = false;
    let mut stealth_policy = StealthPolicy::Auto;
    let mut image_path = None;
    let mut arguments = args.iter().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--strict" => strict = true,
            "--stealth" => {
                let Some(value) = arguments
                    .next()
                    .and_then(|value| StealthPolicy::parse(value))
                else {
                    eprintln!(
                        "Usage: {} [--strict] [--stealth <auto|always|never>] <image_file>\n  auto: 生成内容を担うレコードが1件も無いときだけ Stealth PNG Info を調べる",
                        args[0]
                    );
                    std::process::exit(1);
                };
                stealth_policy = value;
            }
            _ if argument.starts_with("--") || image_path.is_some() => {
                eprintln!(
                    "Usage: {} [--strict] [--stealth <auto|always|never>] <image_file>",
                    args[0]
                );
                std::process::exit(1);
            }
            _ => image_path = Some(argument.as_str()),
        }
    }
    let Some(image_path) = image_path else {
        eprintln!(
            "Usage: {} [--strict] [--stealth <auto|always|never>] <image_file>",
            args[0]
        );
        std::process::exit(1);
    };
    let stdout = io::stdout();
    let mut writer = io::BufWriter::new(stdout.lock());
    // REQ-DSP-02: 診断文（案内文言）は stderr へ出す。見出し行とレコードは stdout のまま。
    let stderr = io::stderr();
    let mut diagnostics = io::BufWriter::new(stderr.lock());
    let options = ScanOptions {
        strict,
        stealth: stealth_policy,
    };
    let result = extract_to_writer_with_diagnostics(
        Path::new(image_path),
        &options,
        &mut writer,
        &mut diagnostics,
    );
    let flush_result = writer
        .flush()
        .and_then(|()| diagnostics.flush());
    match result {
        Err(e) => {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        }
        Ok(outcome) if outcome.format.is_none() => {
            eprintln!("Unsupported file format");
        }
        Ok(_) => {}
    }
    if let Err(e) = flush_result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
