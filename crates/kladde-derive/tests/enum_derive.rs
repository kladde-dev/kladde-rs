mod support;

use kladde_derive::Persistable;
use kladde_traits::Persistable as _;
use support::MockBackend;

#[derive(Persistable, serde::Serialize, serde::Deserialize, Debug, PartialEq)]
enum PhoneNumber {
    Mobile(String),
    Landline(String),
}

#[test]
fn set_replaces_the_whole_value() {
    let backend = MockBackend::default();
    let location = backend.root_location(PhoneNumber::INLINE_SIZE);
    let mut phone = PhoneNumber::Mobile("555-0100".to_string());

    let mut guard = phone.guard(&backend, location);
    guard.set(PhoneNumber::Landline("555-0199".to_string()));

    assert_eq!(*guard, PhoneNumber::Landline("555-0199".to_string()));
    assert_eq!(phone, PhoneNumber::Landline("555-0199".to_string()));

    let reloaded = PhoneNumber::load(&backend, location);
    assert_eq!(reloaded, PhoneNumber::Landline("555-0199".to_string()));
}

#[test]
fn deref_allows_matching_on_the_current_variant_read_only() {
    let backend = MockBackend::default();
    let location = backend.root_location(PhoneNumber::INLINE_SIZE);
    let mut phone = PhoneNumber::Mobile("555-0100".to_string());
    let guard = phone.guard(&backend, location);

    // Fine-grained mutation within a variant isn't supported in v1 (see
    // spec.md's Future Work), but read-only inspection via Deref is.
    let is_mobile = matches!(&*guard, PhoneNumber::Mobile(_));
    assert!(is_mobile);
}
