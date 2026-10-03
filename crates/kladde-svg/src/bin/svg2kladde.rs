//! Converts an SVG file to a kladde file.
//!
//! ```text
//! svg2kladde <in.svg> <out.kladde>
//! ```
//!
//! Prints the two files' sizes. The kladde file is closed, so it holds its
//! live pages only.

use std::process::ExitCode;

use kladde::Kladde;
use kladde_svg::Document;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output] = args.as_slice() else {
        eprintln!("usage: svg2kladde <in.svg> <out.kladde>");
        return ExitCode::FAILURE;
    };
    let text = match std::fs::read_to_string(input) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("{input}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let doc = match kladde_svg::parse(&text) {
        Ok(doc) => doc,
        Err(e) => {
            eprintln!("{input}: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = Kladde::<Document>::create(output, doc).and_then(|drawing| drawing.close());
    if let Err(e) = result {
        eprintln!("{output}: {e}");
        return ExitCode::FAILURE;
    }
    let size = std::fs::metadata(output).map(|m| m.len()).unwrap_or(0);
    println!("{input}: {} bytes", text.len());
    println!("{output}: {size} bytes");
    ExitCode::SUCCESS
}
