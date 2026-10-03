//! SVG drawings as durable data structures: a realistic workload for kladde.
//!
//! [`model`] defines one `#[derive(Persistable)]` type per kind of SVG object,
//! with every decimal stored as a [`Number`]. [`parse()`] reads SVG text into
//! a [`Document`], and [`write()`] writes one back as SVG text. Anything the
//! model does not type is kept verbatim, so every SVG document converts.
//!
//! The *canonical form* of an SVG document is what [`canon`] returns:
//! `write(parse(text))`. Converting a document to kladde and back is
//! lossless exactly when it reproduces the canonical form byte for byte.
//!
//! ```
//! use kladde::Kladde;
//!
//! let text = r#"<svg xmlns="http://www.w3.org/2000/svg"><rect width="10" height="5" fill="red"/></svg>"#;
//! let canonical = kladde_svg::canon(text)?;
//!
//! let drawing = Kladde::new(kladde_svg::parse(text)?);
//! assert_eq!(kladde_svg::write(drawing.get()), canonical);
//! # Ok::<(), kladde_svg::ParseError>(())
//! ```

pub mod copy;
pub mod model;
pub mod parse;
pub mod size;
pub mod write;

pub use copy::DeepCopy;
pub use model::Document;
pub use parse::{parse, parse_with_stats, ParseError, ParseStats};
pub use write::{write, write_indented};

/// The type every decimal of the model is stored as: `f32`, the precision the
/// SVG specification requires, or `f64` with the crate's `f64` feature.
#[cfg(not(feature = "f64"))]
pub type Number = f32;

/// The type every decimal of the model is stored as: `f32`, the precision the
/// SVG specification requires, or `f64` with the crate's `f64` feature.
#[cfg(feature = "f64")]
pub type Number = f64;

/// The canonical form of an SVG document: `write(parse(text))`.
///
/// It is idempotent, `canon(canon(text)) == canon(text)`, and it is what a
/// round trip through kladde must reproduce exactly.
///
/// ```
/// let canonical = kladde_svg::canon(r#"<svg xmlns="http://www.w3.org/2000/svg"> <circle r="2.50"/> </svg>"#)?;
/// assert_eq!(canonical, r#"<svg xmlns="http://www.w3.org/2000/svg"><circle r="2.5"/></svg>"#);
/// assert_eq!(kladde_svg::canon(&canonical)?, canonical);
/// # Ok::<(), kladde_svg::ParseError>(())
/// ```
pub fn canon(text: &str) -> Result<String, ParseError> {
    Ok(write(&parse(text)?))
}
