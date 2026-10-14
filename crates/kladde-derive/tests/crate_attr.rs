//! `#[kladde(crate = "...")]` roots the generated paths elsewhere than at
//! `::kladde`: here at `kladde_persist`, as a library built on it without the
//! facade would.

use kladde_derive::Durable;
use kladde_persist::{Durable as _, Region};
use kladde_store::{MemoryStorage, Store, WriteBackend};

#[derive(Durable, Debug, PartialEq)]
#[kladde(crate = "kladde_persist")]
struct Sample {
    count: u32,
    flag: bool,
}

#[derive(Durable, Debug, PartialEq)]
#[kladde(crate = "kladde_persist")]
enum Choice {
    One(u8),
    Two,
}

#[test]
fn generated_code_names_the_crate_it_is_told() {
    let store = Store::create(Box::new(MemoryStorage::new()), Default::default()).unwrap();
    let mut sample = Sample {
        count: 1,
        flag: false,
    };
    store.splice(0, 0, &sample.to_bytes()).unwrap();
    sample
        .mutate(Region::root(&store))
        .count_mut()
        .set(500)
        .unwrap();
    store.flush().unwrap();
    assert_eq!(
        Sample::from_bytes(&store.read_all().unwrap()).unwrap(),
        sample
    );
    assert_eq!(Choice::Two.to_bytes(), [1]);
    assert_eq!(Choice::One(9).to_bytes(), [0, 9]);
}
