# kladde-rs

The Rust implementation of [kladde](https://kladde-dev.github.io/): durable data structures that you mutate in memory, and it's on disk.

**Status: a proof of concept, to test the specification; not for production use.**
It was written almost entirely by an AI coding agent from the [specification and design documents](https://kladde-dev.github.io/), and the file format it writes is not frozen.

```rust
use kladde::Kladde;
use kladde::Persistable;
use kladde_types::{PersistableString, PersistableVec};

#[derive(Persistable)]
struct Notes {
    title: PersistableString,
    lines: PersistableVec<PersistableString>,
}

let mut notes = Kladde::new(Notes {
    title: PersistableString::from("today"),
    lines: PersistableVec::new(),
});

notes.guard().lines_mut().push(PersistableString::from("wrote a doc"))?;
```

The `push` updates the in-memory vector and appends the change to the journal before it returns.
There is no save call.
`Kladde::new` keeps its file in memory; `Kladde::create` and `Kladde::open` do the same on a real file.

## The crates

| crate | role |
| --- | --- |
| `kladde` | the application-facing entry point: creating and opening files, the root value, transactions and batches |
| `kladde-types` | the built-in containers: vector, hash map, string, blob |
| `kladde-derive` | the `#[derive(Persistable)]` macro, reached through `kladde` |
| `kladde-persist` | the serialization layer: `Persistable`, `Guard`, `Location`, and the schema binding |
| `kladde-schema` | type descriptors, their canonical encoding, and fingerprints |
| `kladde-store` | the storage layer: pages, the address table, the journal, the flush, and consolidation |
| `kladde-varint` | LEB128 varints |
| `kladde-bench` | realistic workloads on real files, measured flush by flush: the numbers behind the [evaluation](https://kladde-dev.github.io/evaluation/) |

```sh
cargo test --workspace
cargo run --release -p kladde-bench -- <output directory> [scenario ...]
```

## Documentation

The [kladde-rs section](https://kladde-dev.github.io/rust/) of the documentation covers this implementation, with a tutorial for application authors.
It builds on the language-independent [specification](https://kladde-dev.github.io/spec/) and [reference algorithms](https://kladde-dev.github.io/impl/).

[`implementation-notes.md`](implementation-notes.md) records where this implementation departs from the documentation, what the documentation leaves unclear, and what went wrong along the way.
[`general-instructions.md`](general-instructions.md) holds the agent's standing instructions, and [`later.md`](later.md) and [`journal-semantics3.md`](journal-semantics3.md) are working notes from earlier iterations of the design.

## License

Available under your choice of the [MIT](LICENSE-MIT), [Apache 2.0](LICENSE-APACHE), or [Boost Software License 1.0](LICENSE-BOOST) (SPDX: `MIT OR Apache-2.0 OR BSL-1.0`).

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this repository shall be licensed as above, without any additional terms or conditions.
