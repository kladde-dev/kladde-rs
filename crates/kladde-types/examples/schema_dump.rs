//! Dumps a type's descriptor table and 128-bit schema fingerprint.
//!
//! Run with `cargo run -p kladde-types --example schema_dump`. Handy for
//! eyeballing what `#[derive(Durable)]` produces and for reading off
//! golden-vector fingerprints.

use kladde::{Durable, TypeDescriptor};
use kladde_types::{DurableString, DurableVec};

#[derive(kladde::Durable)]
enum PhoneKind {
    Mobile,
    Work,
    Home,
}

#[derive(kladde::Durable)]
struct Contact {
    name: DurableString,
    kind: PhoneKind,
    numbers: DurableVec<DurableString>,
}

fn main() {
    dump::<Contact>("Contact");
}

fn dump<T: Durable + 'static>(label: &str) {
    let table = T::schema();
    println!("{label}: fingerprint = {}", T::fingerprint());
    println!("  descriptors ({}):", table.descriptors().len());
    for (index, descriptor) in table.descriptors().iter().enumerate() {
        println!("    [{index}] {}", describe_one(descriptor));
    }
    println!("  canonical encoding: {} bytes", table.encode().len());
}

fn describe_one(descriptor: &TypeDescriptor) -> String {
    match descriptor {
        TypeDescriptor::Primitive(primitive) => {
            format!("Primitive({primitive:?}, code={})", primitive.code())
        }
        TypeDescriptor::Struct { name, fields } => {
            let fields: Vec<String> = fields
                .iter()
                .map(|f| format!("{}: #{}", f.name, f.ty.0))
                .collect();
            format!("Struct {name} {{ {} }}", fields.join(", "))
        }
        TypeDescriptor::Enum { name, variants, .. } => {
            let variants: Vec<String> = variants
                .iter()
                .map(|v| format!("{}={}", v.name, v.discriminant))
                .collect();
            format!("Enum {name} {{ {} }}", variants.join(", "))
        }
        TypeDescriptor::Opaque {
            library_name,
            type_name,
            parameters,
            ..
        } => {
            let params: Vec<String> = parameters.iter().map(|p| format!("#{}", p.0)).collect();
            format!("Opaque {library_name}::{type_name}<{}>", params.join(", "))
        }
        TypeDescriptor::Array { element, len } => format!("Array(#{}, {len})", element.0),
        TypeDescriptor::Sequence(element) => format!("Sequence(#{})", element.0),
        TypeDescriptor::Text => "Text".to_string(),
        TypeDescriptor::Fixed(target) => format!("Fixed(#{})", target.0),
    }
}
