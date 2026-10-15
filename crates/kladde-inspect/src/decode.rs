//! Decoding the sequence by the file's own descriptor table, front to back,
//! to see which values its bytes belong to (`spec/tooling.md#level-3`).
//!
//! The walk knows the encodings of the descriptor kinds and nothing about
//! the application: it attributes every byte to the fields, variants and
//! types it lies in, and keeps the largest *leaves* — texts, opaque values,
//! and sequences or arrays of plain elements, which hold a value's bulk.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use kladde_schema::{Primitive, TypeDescriptor, TypeRef, TypeTable};

/// What the walk found.
pub struct Decoded {
    /// Bytes by what they lie in: `type T`, `field T.f`, `variant E::V`,
    /// each counted once however deeply it nests in itself.
    pub groups: Vec<Group>,
    /// The largest leaves, largest first.
    pub largest: Vec<Leaf>,
}

/// One group of [`Decoded::groups`].
pub struct Group {
    pub key: String,
    pub count: u64,
    pub bytes: u64,
}

/// One leaf of [`Decoded::largest`].
pub struct Leaf {
    pub path: String,
    pub ty: String,
    pub start: u64,
    pub len: u64,
    pub preview: String,
}

/// A step of the path from the root to the value being decoded.
#[derive(Clone, Copy)]
enum Step<'a> {
    Field(&'a str),
    Index(u64),
    Variant(&'a str),
    Key(u64),
    Value(u64),
}

struct Walker<'a> {
    table: &'a TypeTable,
    bytes: &'a [u8],
    at: usize,
    path: Vec<Step<'a>>,
    /// Group ids by key, the groups, and how often each is open on the path.
    ids: HashMap<String, usize>,
    groups: Vec<Group>,
    open: Vec<u32>,
    /// Per type and form: the group of the type, and whether it is a leaf.
    type_group: Vec<Option<usize>>,
    leaf: Vec<bool>,
    /// Per struct type, the groups of its fields; per enum type, of its
    /// variants.
    member_groups: Vec<Vec<usize>>,
    top: BinaryHeap<Reverse<(u64, u64, usize)>>,
    leaves: Vec<Option<Leaf>>,
    keep: usize,
}

/// Walks `bytes` as one value of the table's root type, keeping the `keep`
/// largest leaves.
pub fn walk(table: &TypeTable, bytes: &[u8], keep: usize) -> Result<Decoded, String> {
    let n = table.descriptors().len();
    let mut w = Walker {
        table,
        bytes,
        at: 0,
        path: Vec::new(),
        ids: HashMap::new(),
        groups: Vec::new(),
        open: Vec::new(),
        type_group: vec![None; n],
        leaf: vec![false; n],
        member_groups: vec![Vec::new(); n],
        top: BinaryHeap::new(),
        leaves: Vec::new(),
        keep,
    };
    for i in 0..n {
        let name = type_name(table, i, 0);
        w.type_group[i] = match table.get(TypeRef(i)) {
            TypeDescriptor::Struct { .. }
            | TypeDescriptor::Enum { .. }
            | TypeDescriptor::Opaque { .. }
            | TypeDescriptor::Text => Some(w.group(format!("type {name}"))),
            _ => None,
        };
        w.leaf[i] = match table.get(TypeRef(i)) {
            TypeDescriptor::Text | TypeDescriptor::Opaque { .. } => true,
            TypeDescriptor::Sequence(e) | TypeDescriptor::Array { element: e, .. } => {
                matches!(table.get(*e), TypeDescriptor::Primitive(_))
                    || table.fixed_size(*e).is_some()
            }
            _ => false,
        };
        w.member_groups[i] = match table.get(TypeRef(i)) {
            TypeDescriptor::Struct { name, fields } => fields
                .iter()
                .map(|f| w.group(format!("field {name}.{}", f.name)))
                .collect(),
            TypeDescriptor::Enum { name, variants, .. } => variants
                .iter()
                .map(|v| w.group(format!("variant {name}::{}", v.name)))
                .collect(),
            _ => Vec::new(),
        };
    }
    w.value(0, false)?;
    if w.at != bytes.len() {
        return Err(format!(
            "the root value ends at byte {} of {}",
            w.at,
            bytes.len()
        ));
    }
    let mut largest: Vec<Leaf> = w
        .top
        .into_sorted_vec()
        .into_iter()
        .filter_map(|Reverse((_, _, i))| w.leaves[i].take())
        .collect();
    largest.sort_by_key(|l| Reverse(l.len));
    let mut groups = w.groups;
    groups.retain(|g| g.count > 0);
    groups.sort_by_key(|g| Reverse(g.bytes));
    Ok(Decoded { groups, largest })
}

impl<'a> Walker<'a> {
    fn group(&mut self, key: String) -> usize {
        if let Some(&i) = self.ids.get(&key) {
            return i;
        }
        self.groups.push(Group {
            key: key.clone(),
            count: 0,
            bytes: 0,
        });
        self.open.push(0);
        self.ids.insert(key, self.groups.len() - 1);
        self.groups.len() - 1
    }

    fn err(&self, what: &str) -> String {
        format!("at byte {} ({}): {what}", self.at, self.path_string())
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let bytes = self.bytes;
        let out = bytes
            .get(self.at..self.at.saturating_add(n))
            .ok_or_else(|| self.err("the sequence ends inside a value"))?;
        self.at += n;
        Ok(out)
    }

    fn varint(&mut self) -> Result<u64, String> {
        let rest = &self.bytes[self.at.min(self.bytes.len())..];
        let (v, r) = kladde_varint::decode(rest).map_err(|_| self.err("a truncated varint"))?;
        self.at += rest.len() - r.len();
        Ok(v)
    }

    /// Opens the group `g` around a value starting at `start`, runs `f`,
    /// and counts the value's bytes to the group unless it is already open.
    fn within(
        &mut self,
        g: Option<usize>,
        f: impl FnOnce(&mut Self) -> Result<(), String>,
    ) -> Result<(), String> {
        let Some(g) = g else { return f(self) };
        let start = self.at;
        self.open[g] += 1;
        let r = f(self);
        self.open[g] -= 1;
        self.groups[g].count += 1;
        if self.open[g] == 0 {
            self.groups[g].bytes += (self.at - start) as u64;
        }
        r
    }

    fn value(&mut self, ty: usize, form: bool) -> Result<(), String> {
        let start = self.at;
        let g = self.type_group[ty];
        self.within(g, |w| w.inner(ty, form))?;
        if self.leaf[ty] {
            self.offer(ty, start);
        }
        Ok(())
    }

    fn inner(&mut self, ty: usize, form: bool) -> Result<(), String> {
        let table = self.table;
        match table.get(TypeRef(ty)) {
            TypeDescriptor::Primitive(p) if form => {
                self.take(p.fixed_form_size() as usize)?;
            }
            TypeDescriptor::Primitive(p) => match p.fixed_size() {
                Some(n) => {
                    self.take(n as usize)?;
                }
                None if *p == Primitive::Char => {
                    let n = match *self.take(1)?.first().unwrap() {
                        0x00..=0x7F => 0,
                        0xC0..=0xDF => 1,
                        0xE0..=0xEF => 2,
                        _ => 3,
                    };
                    self.take(n)?;
                }
                None => {
                    self.varint()?;
                }
            },
            TypeDescriptor::Struct { fields, .. } => {
                for (i, f) in fields.iter().enumerate() {
                    let g = self.member_groups[ty][i];
                    self.path.push(Step::Field(&f.name));
                    self.within(Some(g), |w| w.value(f.ty.0, form))?;
                    self.path.pop();
                }
            }
            TypeDescriptor::Enum {
                discriminant_width,
                variants,
                ..
            } => {
                let start = self.at;
                let discriminant = |w: &mut Self| {
                    if form {
                        let mut wide = [0u8; 8];
                        let n = *discriminant_width as usize;
                        wide[..n].copy_from_slice(w.take(n)?);
                        Ok(u64::from_le_bytes(wide))
                    } else {
                        w.varint()
                    }
                };
                let d = discriminant(self)?;
                let (i, v) = variants
                    .iter()
                    .enumerate()
                    .find(|(_, v)| v.discriminant == d)
                    .ok_or_else(|| self.err(&format!("discriminant {d} names no variant")))?;
                // The variant's group holds its discriminant too.
                self.at = start;
                let g = self.member_groups[ty][i];
                self.path.push(Step::Variant(&v.name));
                self.within(Some(g), |w| {
                    discriminant(w)?;
                    w.value(v.payload.0, form)
                })?;
                self.path.pop();
                if form {
                    let size = table
                        .fixed_form_size(TypeRef(ty))
                        .ok_or_else(|| self.err("an enum without a fixed form in a Fixed"))?;
                    let end = start + size as usize;
                    let pad = end
                        .checked_sub(self.at)
                        .ok_or_else(|| self.err("a variant overruns its fixed form"))?;
                    self.take(pad)?;
                }
            }
            TypeDescriptor::Opaque { fixed_size, .. } => {
                let n = match fixed_size {
                    Some(n) => *n,
                    None => self.varint()?,
                };
                self.take(n as usize)?;
            }
            TypeDescriptor::Array { element, len } => self.elements(element.0, *len, form)?,
            TypeDescriptor::Sequence(element) if !form => {
                let n = self.varint()?;
                self.elements(element.0, n, false)?;
            }
            TypeDescriptor::Map { key, value } if !form => {
                let n = self.varint()?;
                for i in 0..n {
                    self.path.push(Step::Key(i));
                    self.value(key.0, false)?;
                    self.path.pop();
                    self.path.push(Step::Value(i));
                    self.value(value.0, false)?;
                    self.path.pop();
                }
            }
            TypeDescriptor::Text if !form => {
                let rest = &self.bytes[self.at..];
                let end = rest
                    .iter()
                    .position(|&b| b == 0xFF)
                    .ok_or_else(|| self.err("a text without its end"))?;
                self.at += end + 1;
            }
            TypeDescriptor::Fixed(t) => self.value(t.0, true)?,
            _ => return Err(self.err("a type without a fixed form inside a Fixed")),
        }
        Ok(())
    }

    fn elements(&mut self, element: usize, n: u64, form: bool) -> Result<(), String> {
        // Plain elements are skipped by their size, not one by one.
        let size = if form {
            self.table.fixed_form_size(TypeRef(element))
        } else {
            self.table.fixed_size(TypeRef(element))
        };
        if let (Some(size), None) = (size, self.type_group[element]) {
            let total = size
                .checked_mul(n)
                .ok_or_else(|| self.err("an array too large"))?;
            self.take(total as usize)?;
            return Ok(());
        }
        for i in 0..n {
            self.path.push(Step::Index(i));
            self.value(element, form)?;
            self.path.pop();
        }
        Ok(())
    }

    /// Keeps the leaf of type `ty` from `start` to here if it is among the
    /// largest so far.
    fn offer(&mut self, ty: usize, start: usize) {
        let len = (self.at - start) as u64;
        if self.keep == 0 {
            return;
        }
        if self.top.len() == self.keep {
            let Reverse((min, _, _)) = *self.top.peek().unwrap();
            if len <= min {
                return;
            }
            let Reverse((_, _, i)) = self.top.pop().unwrap();
            self.leaves[i] = None;
        }
        let leaf = Leaf {
            path: self.path_string(),
            ty: type_name(self.table, ty, 0),
            start: start as u64,
            len,
            preview: preview(self.table, ty, &self.bytes[start..self.at]),
        };
        self.leaves.push(Some(leaf));
        self.top
            .push(Reverse((len, start as u64, self.leaves.len() - 1)));
    }

    fn path_string(&self) -> String {
        let mut s = String::from("root");
        for step in &self.path {
            match step {
                Step::Field(f) => {
                    s.push('.');
                    s.push_str(f);
                }
                Step::Index(i) => s.push_str(&format!("[{i}]")),
                Step::Variant(v) => s.push_str(&format!("({v})")),
                Step::Key(i) => s.push_str(&format!("{{key {i}}}")),
                Step::Value(i) => s.push_str(&format!("{{value {i}}}")),
            }
        }
        s
    }
}

/// A short name for the type at `i`: its own name for structs, enums and
/// opaque types, and the shape of the others.
pub fn type_name(table: &TypeTable, i: usize, depth: usize) -> String {
    if depth > 4 {
        return "…".into();
    }
    let sub = |t: &TypeRef| type_name(table, t.0, depth + 1);
    match table.get(TypeRef(i)) {
        TypeDescriptor::Primitive(p) => format!("{p:?}").to_lowercase(),
        TypeDescriptor::Struct { name, .. } | TypeDescriptor::Enum { name, .. } => name.clone(),
        TypeDescriptor::Opaque {
            library_name,
            type_name,
            ..
        } => format!("{library_name}::{type_name}"),
        TypeDescriptor::Array { element, len } => format!("[{}; {len}]", sub(element)),
        TypeDescriptor::Sequence(e) => format!("Seq<{}>", sub(e)),
        TypeDescriptor::Map { key, value } => format!("Map<{}, {}>", sub(key), sub(value)),
        TypeDescriptor::Text => "Text".into(),
        TypeDescriptor::Fixed(t) => format!("Fixed<{}>", sub(t)),
    }
}

/// The start of a leaf's content, for a reader to recognize it by: text as
/// text, anything else as hex.
fn preview(table: &TypeTable, ty: usize, bytes: &[u8]) -> String {
    const N: usize = 48;
    if let TypeDescriptor::Text = table.get(TypeRef(ty)) {
        let text = String::from_utf8_lossy(&bytes[..bytes.len() - 1]);
        let mut s: String = text.chars().take(N).collect();
        if text.chars().nth(N).is_some() {
            s.push('…');
        }
        return format!("{s:?}");
    }
    let mut s: String = bytes
        .iter()
        .take(N / 2)
        .map(|b| format!("{b:02x}"))
        .collect();
    if bytes.len() > N / 2 {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use kladde_schema::{Field, Variant};

    /// `struct Tree { label: Text, kind: Kind, children: Seq<Tree> }` with
    /// `enum Kind { Leaf(Unit), Blob(Seq<u8>) }`.
    fn tree() -> TypeTable {
        let field = |name: &str, ty| Field {
            name: name.into(),
            ty: TypeRef(ty),
        };
        TypeTable::new(vec![
            TypeDescriptor::Struct {
                name: "Tree".into(),
                fields: vec![field("label", 1), field("kind", 2), field("children", 3)],
            },
            TypeDescriptor::Text,
            TypeDescriptor::Enum {
                name: "Kind".into(),
                discriminant_width: 1,
                variants: vec![
                    Variant {
                        discriminant: 0,
                        name: "Leaf".into(),
                        payload: TypeRef(4),
                    },
                    Variant {
                        discriminant: 1,
                        name: "Blob".into(),
                        payload: TypeRef(5),
                    },
                ],
            },
            TypeDescriptor::Sequence(TypeRef(0)),
            TypeDescriptor::Struct {
                name: "Unit".into(),
                fields: vec![],
            },
            TypeDescriptor::Sequence(TypeRef(6)),
            TypeDescriptor::Primitive(Primitive::U8),
        ])
    }

    #[test]
    fn bytes_go_to_their_groups_once_and_the_largest_leaf_is_found() {
        // Tree "r", a leaf, with one child: Tree "c", a blob of 300 bytes,
        // without children.
        let mut seq = b"r\xff\x00\x01c\xff\x01".to_vec();
        kladde_varint::encode(300, &mut seq);
        seq.extend([7u8; 300]);
        seq.push(0);
        let d = walk(&tree(), &seq, 2).unwrap();
        let group = |k: &str| d.groups.iter().find(|g| g.key == k).unwrap();
        // Nested trees count once, as the outermost one.
        assert_eq!(group("type Tree").bytes, seq.len() as u64);
        assert_eq!(group("type Tree").count, 2);
        assert_eq!(group("variant Kind::Blob").bytes, 1 + 2 + 300);
        assert_eq!(group("field Tree.label").bytes, 4);
        let top = &d.largest[0];
        assert_eq!(top.path, "root.children[0].kind(Blob)");
        assert_eq!(top.ty, "Seq<u8>");
        assert_eq!(top.len, 302);
        // Of two leaves of the same size, the later one is kept.
        assert_eq!(d.largest[1].preview, "\"c\"");
    }

    #[test]
    fn a_sequence_that_does_not_decode_says_where() {
        let e = walk(&tree(), b"r\xff\x07", 1).err().unwrap();
        assert!(e.contains("discriminant 7"), "{e}");
        assert!(walk(&tree(), b"r\xff\x00\x00extra", 1).is_err());
    }
}
