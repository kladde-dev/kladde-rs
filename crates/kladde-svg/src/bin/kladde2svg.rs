//! Converts a kladde file written by `svg2kladde` back to SVG.
//!
//! ```text
//! kladde2svg [--indent] <in.kladde> <out.svg>
//! ```
//!
//! Writes the canonical form, or with `--indent` one element per line.

use std::process::ExitCode;

use kladde::Kladde;
use kladde_svg::Document;

fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let indent = args.iter().any(|a| a == "--indent");
    args.retain(|a| a != "--indent");
    let [input, output] = args.as_slice() else {
        eprintln!("usage: kladde2svg [--indent] <in.kladde> <out.svg>");
        return ExitCode::FAILURE;
    };
    let drawing = match Kladde::<Document>::open(input) {
        Ok(drawing) => drawing,
        Err(e) => {
            eprintln!("{input}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let text = if indent {
        kladde_svg::write_indented(drawing.get())
    } else {
        kladde_svg::write(drawing.get())
    };
    if let Err(e) = std::fs::write(output, text) {
        eprintln!("{output}: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
