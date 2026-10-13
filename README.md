<img src="https://kladde-dev.github.io/static/logo.svg" alt="kladde logo" width="80" align="right">

# kladde-rs

The Rust implementation of [kladde](https://kladde-dev.github.io/): durable data structures that you mutate in memory, and it's on disk.

**Status: a proof of concept, to test the specification; not for production use.**
It was written almost entirely by an AI coding agent from the [specification and design documents](https://kladde-dev.github.io/), and the file format it writes is not frozen.

```rust
use kladde::Kladde;
use kladde::Durable;
use kladde_types::{DurableString, DurableVec};

#[derive(Durable)]
struct Notes {
    title: DurableString,
    lines: DurableVec<DurableString>,
}

let mut notes = Kladde::new(Notes {
    title: DurableString::from("today"),
    lines: DurableVec::new(),
});

notes.guard().lines_mut().push(DurableString::from("wrote a doc"))?;
```

The `push` updates the in-memory vector and appends the change to the journal before it returns.
There is no save call.
`Kladde::new` keeps its file in memory; `Kladde::create` and `Kladde::open` do the same on a real file.

The file holds the root value's encoding as one byte sequence, kept on copy-on-write pages as a chain of spans; the `push` above splices the new string and the vector's count into that sequence, and a flush later restates only the few spans where the splice cut the chain.

## The crates

| crate | role |
| --- | --- |
| `kladde` | the application-facing entry point: creating and opening files, the root value, transactions and batches |
| `kladde-types` | the built-in containers: vector, map, string, blob |
| `kladde-derive` | the `#[derive(Durable)]` macro, reached through `kladde` |
| `kladde-persist` | the serialization layer: `Durable`, `Bounded`, `Guard`, `Region`, `Fixed`, cuts, and the schema binding |
| `kladde-schema` | type descriptors, their canonical encoding, and fingerprints |
| `kladde-store` | the storage layer: pages, the span table, the journal, the flush, and consolidation |
| `kladde-varint` | LEB128 varints |
| `kladde-svg` | an SVG drawing as a durable value, and `svg-bench`, a drawing editor's edits measured flush by flush |
| `kladde-bench` | `text-bench`, typing into one text measured flush by flush; with `svg-bench`, the numbers behind the [evaluation](https://kladde-dev.github.io/evaluation/) |

```sh
cargo test --workspace
cargo run --release -p kladde-bench --bin text-bench -- <output directory>
cargo run --release -p kladde-svg --bin svg-bench -- <drawing.svg> <output directory>
```

The branch `address-table` keeps the previous design, of [allocations and an address table](https://kladde-dev.github.io/superseded/allocations/), which most of the evaluation measured.

## Documentation and tutorial

To use kladde-rs in an application, start with the **[tutorial](https://kladde-dev.github.io/rust/tutorial/)**.
The [kladde-rs section](https://kladde-dev.github.io/rust/) of the documentation covers the rest of this implementation.
It builds on the language-independent [specification](https://kladde-dev.github.io/spec/) and [reference algorithms](https://kladde-dev.github.io/impl/).

[`implementation-notes.md`](implementation-notes.md) records where this implementation departs from the documentation, and what it chose where the documentation leaves a question open.
[`later.md`](later.md) lists deferred work and ideas, and [`journal-semantics3.md`](journal-semantics3.md) is a working note from an earlier iteration of the design.

## License

Available under your choice of the [MIT](LICENSE-MIT), [Apache 2.0](LICENSE-APACHE), or [Boost Software License 1.0](LICENSE-BOOST) (SPDX: `MIT OR Apache-2.0 OR BSL-1.0`).

Unless you explicitly state otherwise, any contribution you intentionally submit for inclusion in this repository shall be licensed as above, without any additional terms or conditions.
