//! Constant arithmetic on encoding sizes, for the `FIXED_SIZE` and
//! `FIXED_FORM_SIZE` of composite types. For `FIXED_SIZE`, `None` -- no such
//! size -- absorbs everything it is combined with.

use crate::codec::varint_len;

/// The sum of `sizes`, or `None` if any is `None`: a struct's or a tuple's
/// size from its fields'.
///
/// ```
/// use kladde_persist::sum_sizes;
///
/// assert_eq!(sum_sizes(&[Some(4), Some(1)]), Some(5));
/// assert_eq!(sum_sizes(&[Some(4), None]), None);
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

/// An enum's fixed size from its variants': the discriminant's varint and
/// the variant's fields, if every variant comes to the same.
///
/// ```
/// use kladde_persist::enum_fixed_size;
///
/// assert_eq!(enum_fixed_size(&[0, 1], &[Some(4), Some(4)]), Some(5));
/// assert_eq!(enum_fixed_size(&[0, 1], &[Some(4), Some(0)]), None);
/// assert_eq!(enum_fixed_size(&[0, 200], &[Some(0), Some(0)]), None);
/// ```
pub const fn enum_fixed_size(discriminants: &[u64], variants: &[Option<usize>]) -> Option<usize> {
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

/// An enum's fixed form size from its variants' fixed forms: its
/// discriminant at `width` bytes, and its largest variant.
///
/// ```
/// use kladde_persist::enum_fixed_form_size;
///
/// assert_eq!(enum_fixed_form_size(2, &[4, 0]), 6);
/// ```
pub const fn enum_fixed_form_size(width: usize, variants: &[usize]) -> usize {
    let mut largest = 0;
    let mut i = 0;
    while i < variants.len() {
        if variants[i] > largest {
            largest = variants[i];
        }
        i += 1;
    }
    width + largest
}

/// The smallest of 1, 2, 4 and 8 bytes that holds `largest`, the width of an
/// enum's discriminant in its fixed form unless its `repr` says otherwise.
///
/// ```
/// assert_eq!(kladde_persist::discriminant_width(255), 1);
/// assert_eq!(kladde_persist::discriminant_width(256), 2);
/// ```
pub const fn discriminant_width(largest: u64) -> usize {
    if largest <= u8::MAX as u64 {
        1
    } else if largest <= u16::MAX as u64 {
        2
    } else if largest <= u32::MAX as u64 {
        4
    } else {
        8
    }
}
