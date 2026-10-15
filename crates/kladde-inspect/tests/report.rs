//! `kladde-inspect` run on a file that kladde-store writes.

use std::process::Command;

use kladde_schema::{Field, Primitive, TypeDescriptor, TypeRef, TypeTable};
use kladde_store::{FileStorage, Store, WriteBackend};

/// `struct Doc { title: Text, image: Seq<u8> }`.
fn schema() -> TypeTable {
    TypeTable::new(vec![
        TypeDescriptor::Struct {
            name: "Doc".into(),
            fields: vec![
                Field {
                    name: "title".into(),
                    ty: TypeRef(1),
                },
                Field {
                    name: "image".into(),
                    ty: TypeRef(2),
                },
            ],
        },
        TypeDescriptor::Text,
        TypeDescriptor::Sequence(TypeRef(3)),
        TypeDescriptor::Primitive(Primitive::U8),
    ])
}

#[test]
fn reports_a_file_with_its_largest_value_and_its_consolidated_size() {
    let dir = std::env::temp_dir().join(format!("kladde-inspect-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("doc.kladde");
    let table = schema();
    let mut value = b"hello\xff".to_vec();
    kladde_varint::encode(50_000, &mut value);
    value.extend((0..50_000u32).map(|i| (i * 7919 % 251) as u8));
    {
        let store = Store::create(
            Box::new(FileStorage::create(&path).unwrap()),
            Default::default(),
        )
        .unwrap();
        store.set_schema(table.encode());
        store.set_root_fingerprint(*table.fingerprint().as_bytes());
        store.splice(0, 0, &value).unwrap();
        store.flush().unwrap();
        // An edit the journal still holds, which the report replays.
        store.write(0, b"j").unwrap();
    }

    let out = Command::new(env!("CARGO_BIN_EXE_kladde-inspect"))
        .args(["--top", "3", "--map", "--types"])
        .arg(&path)
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(out.status.success(), "{text}");
    assert!(text.contains("root type    Doc"), "{text}");
    assert!(text.contains("(the header's)"), "{text}");
    assert!(text.contains("journal      1 transaction"), "{text}");
    assert!(text.contains("A fully consolidated copy"), "{text}");
    assert!(text.contains("root.image: Seq<u8>"), "{text}");
    assert!(text.contains("\"jello\""), "{text}");
    assert!(
        text.contains("struct Doc { title: Text, image: Seq<u8> }"),
        "{text}"
    );
    assert!(text.contains("Map of the file"), "{text}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn refuses_what_is_not_a_kladde_file() {
    let path = std::env::temp_dir().join(format!("kladde-inspect-not-{}", std::process::id()));
    std::fs::write(&path, vec![b'x'; 8192]).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_kladde-inspect"))
        .arg(&path)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a kladde file"));
    std::fs::remove_file(&path).unwrap();
}
