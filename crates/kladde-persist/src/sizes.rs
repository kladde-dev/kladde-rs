//! Constant arithmetic on encoding sizes, for the `SLOTTED_SIZE` and
//! `PACKED_SIZE` of composite types: `None` -- no such size -- absorbs
//! everything it is combined with.

/// The size of fields laid out back to back: the sum of `sizes`, or `None`
/// if any of them is `None`.
///
/// ```
/// use kladde_persist::sum_sizes;
///
/// assert_eq!(sum_sizes(&[Some(1), Some(4)]), Some(5));
/// assert_eq!(sum_sizes(&[Some(1), None]), None);
/// assert_eq!(sum_sizes(&[]), Some(0));
/// ```
pub const fn sum_sizes(sizes: &[Option<usize>]) -> Option<usize> {
    let mut total = 0;
    let mut i = 0;
    while i < sizes.len() {
        match sizes[i] {
            Some(size) => total += size,
            None => return None,
        }
        i += 1;
    }
    Some(total)
}

/// The slot of an enum: its discriminant's `width` plus its largest variant,
/// whose fields' slotted sizes add up to `variants[i]`, or `None` if any
/// variant has no fixed encoding.
///
/// ```
/// use kladde_persist::enum_slotted_size;
///
/// assert_eq!(enum_slotted_size(1, &[Some(0), Some(8), Some(4)]), Some(9));
/// assert_eq!(enum_slotted_size(1, &[Some(0), None]), None);
/// ```
pub const fn enum_slotted_size(width: usize, variants: &[Option<usize>]) -> Option<usize> {
    let mut largest = 0;
    let mut i = 0;
    while i < variants.len() {
        match variants[i] {
            Some(size) if size > largest => largest = size,
            Some(_) => {}
            None => return None,
        }
        i += 1;
    }
    Some(width + largest)
}

/// The packed size of an enum, if every value's is the same: each variant's
/// discriminant as a varint, plus its fields' packed sizes `variants[i]`.
///
/// ```
/// use kladde_persist::enum_packed_size;
///
/// assert_eq!(enum_packed_size(&[0, 1], &[Some(4), Some(4)]), Some(5));
/// assert_eq!(enum_packed_size(&[0, 1], &[Some(0), Some(4)]), None);
/// assert_eq!(enum_packed_size(&[0, 300], &[Some(4), Some(4)]), None);
/// ```
pub const fn enum_packed_size(discriminants: &[u64], variants: &[Option<usize>]) -> Option<usize> {
    let mut common = None;
    let mut i = 0;
    while i < variants.len() {
        let size = match variants[i] {
            Some(size) => varint_len(discriminants[i]) + size,
            None => return None,
        };
        match common {
            Some(c) if c != size => return None,
            _ => common = Some(size),
        }
        i += 1;
    }
    common
}

/// Appends the unsigned LEB128 varint of `value` to `out`: how a packed
/// place writes an integer, a pointer, or an enum's discriminant.
///
/// ```
/// let mut out = Vec::new();
/// kladde_persist::write_varint(300, &mut out);
/// assert_eq!(out, [0xac, 0x02]);
/// ```
pub fn write_varint(value: u64, out: &mut Vec<u8>) {
    kladde_varint::encode(value, out);
}

/// How many bytes the LEB128 varint of `value` takes.
///
/// ```
/// assert_eq!(kladde_persist::varint_len(127), 1);
/// assert_eq!(kladde_persist::varint_len(128), 2);
/// ```
pub const fn varint_len(value: u64) -> usize {
    let mut len = 1;
    let mut rest = value >> 7;
    while rest != 0 {
        len += 1;
        rest >>= 7;
    }
    len
}
