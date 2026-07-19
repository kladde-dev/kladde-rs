# Questions Before I Implement v1

Goal: a mock `Allocator` in `alloc`; fully working `traits`, `derive`, `frontend`; a `types` crate with `Vec<T>` and `HashMap<K, V>`; unit tests throughout; an `example` crate demonstrating usage, including `#[derive]`d custom `struct`/`enum` types.

Please answer inline (same pattern as `README.md`), commit, and I'll take it from there without further check-ins. For anything left unanswered or that turns out to be underspecified once I'm actually writing code, I'll make the call myself, document *why* in the commit that makes the decision, and keep moving rather than stopping again — flag now if you'd rather I stop and ask when that happens instead.

Sounds good to me.

## 1. How far does "mock" extend — does `frontend` touch a real file at all in v1?

The task names a mock *allocator*, but "fully working `frontend`" is more ambitious than that phrase alone resolves. Two readings:

- **(a)** Only `alloc` is mocked (an in-memory arena standing in for the real file-backed heap). `frontend` still does real file I/O: opens an actual OS file, writes real journal entries with the length+checksum framing described in the README, etc. — it's just that what those bytes eventually get compacted *into* (the snapshot, via `Allocator`) is backed by memory instead of a real heap-management scheme.
- **(b)** The whole persistence path is mocked for v1: `frontend` exercises the real flushing pipeline, type registry, and API shape, but the "file" is the same in-memory arena `alloc` already mocks — no real OS file, no byte-level journal framing, no crash-consistency machinery (shadow paging, `fsync` ordering) yet, since none of that can be meaningfully exercised without a real `Allocator` anyway.

**I'd recommend (b)**, with one addition (see question 6) so the demo app can still show data surviving a restart. Building real crash-consistent file I/O against a foundation (`Allocator`) that's explicitly a throwaway mock seems like it would mostly be wasted work — get the trait boundaries, the flushing pipeline, the type registry, and the derive macro right first, then build the real file-backed `Allocator` and the real crash-consistent I/O around it together, since those two are what actually need to co-design (see the README's "Pointers and Memory Management" section on why the registry's shape is still open).

I agree with your recommendation.

## 2. Should `Op` stay a lifetime-GAT, or should v1 simplify to owned `Op`s?

`sketch.rs` currently has `Persistable::Op<'a>` as a lifetime-parameterized GAT specifically so `Vec::push` can record a *reference* to the just-pushed value instead of cloning it. That's real complexity — it propagates through `Journal::record`'s signature, and the derive macro has to generate correct GAT impls for every derived type, not just plain associated types.

**I'd recommend simplifying to owned `Op`s for v1** (`type Op: Serialize + DeserializeOwned;`, no lifetime), accepting that operations like `push` will need a `T: Clone` bound and pay a clone. This is a real capability regression (a `Persistable` type that isn't `Clone` couldn't be pushed into a backed `Vec` without a different formulation), but it removes a substantial amount of macro-generation risk from the first end-to-end pass. The lifetime-GAT version can come back once the simpler system works and it's clear where the clone costs actually hurt.

If you'd rather keep the lifetime-GAT version from the start, say so and I will — just flagging that it's the single biggest complexity lever in this plan.

Yes, go without the simpler owned `Op`s first, but mention in the readme / in `spec.md` that we'll eventually want to turn it into a lifetime-GAT and remove any unnecessary `clone`s.

## 3. Serialization format for `Op` and snapshot data

~~The README says "a fast and compact binary `serde` format" without naming one. Candidates: `bincode`, `postcard`, `rmp-serde` (MessagePack), `ciborium` (CBOR).~~ **Superseded:** `bincode` is no longer maintained (RUSTSEC-2025-0141 — the team ceased development after a doxxing/harassment incident) and is off the table. Its own advisory lists `wincode`, `postcard`, `bitcode`, and `rkyv` as replacements.

**Recommendation: `postcard`.** It's serde-based (no friction with the `Op: Serialize + DeserializeOwned` bounds already assumed throughout), and it's the most mature/best-maintained of the four (stable wire format since 1.0, 60+ contributors, 7,000+ dependents) — worth weighing heavily given the whole reason for this reconsideration is a dependency losing its maintainer. `bitcode` is faster but has notably thinner maintenance; `wincode` is very new (~2 months old) and its lower-level, direct-memory-write API would actually be more integration work than postcard's, not less; `rkyv`'s zero-copy archive model doesn't fit a serde-based `Op`/journal design without restructuring the still-open `UniquePointer` (de)serialization question (#4) — worth keeping in mind for the snapshot section later, where zero-copy reads actually fit the design's own goals, but not for this.

Use `postcard`.

## 4. How does `UniquePointer` actually get (de)serialized?

This is the one piece of the design I don't think has a settled answer anywhere yet, and it's load-bearing. A `UniquePointer<T>`'s in-memory `index` is meaningless outside the process that assigned it — but `UniquePointer`s can appear *embedded* inside other serialized values (e.g., inside a `HashMap`'s serialized bucket array). So serializing a container that holds pointers can't just serialize the `index` field via `#[derive(Serialize)]`; it needs to go through `Allocator` to resolve `index` → the real on-disk `position` at serialization time (and the reverse on load).

Concretely, I'm planning to give `UniquePointer` custom `Serialize`/`Deserialize` impls that require a `Backend`/`Allocator` in scope — via serde's stateful-deserialization support (`DeserializeSeed`) for the read side, and a thread-local or explicitly-threaded allocator reference for the write side, since plain `serde::Serialize` has no way to pass extra context through. **Flag if you had something else in mind** — this is the piece most likely to need a design decision I can't fully make from the README alone, since it's genuinely underspecified there.

No, I want something else. Don't make `UniquePointer` serializable. Define an additional type that can be constructed from a `UniquePointer` and a `Backend` or `Allocator`, and that resolves the pointer and is serializable.
That additional type probably has a lifetime bound to the allocator to prevent allocator operations that would invalidate the resolved pointer while it is alive.

## 5. `frontend`'s API shape: one root value per file, or multiple named roots?

Not specified anywhere yet. Two shapes:

- **(a)** A file holds exactly one root `Persistable` value (the application's whole state is one struct/enum you design). Opening looks like `Kladde::<MyState>::open(path) -> Kladde<MyState>`, `Kladde::create(path, MyState::default())`, etc.
- **(b)** A file holds multiple independently-named top-level slots (closer to how `sled` exposes multiple named trees), so you could open a `Vec<Note>` under one name and a `HashMap<UserId, Profile>` under another in the same file without wrapping both in one outer struct.

**I'd recommend (a)** for v1 — simpler API, and it doesn't foreclose (b) later (a single root struct with named fields already gets you most of what (b) offers). If you want (b) from the start, let me know.

Yes, go with (a). But remember that, internally, the file always also stores some header / meta data whose management ideally uses as much of the same machinery that management of the payload uses as possible. For example, the list of type hashes is part of the metadata.

## 6. Should the mock allocator's contents survive a process restart?

Given question 1's answer (b), nothing persists across runs by default in v1 — closing and reopening the `example` app would show an empty/default state every time, which makes "durable persistence" hard to actually *see* in the demo. I'd like to add one pragmatic thing: have the mock `Allocator` optionally dump its in-memory arena to a plain file on close and reload it on open (via the same serde format from question 3, not the real Kladde wire format) — purely so the `example` app can demonstrate "data survives restart" honestly, without pretending the mock is the real file format. **OK to do this, or would you rather the demo just run within a single process (e.g., a test or a REPL-style loop) and not claim cross-run persistence at all in v1?**

No, don't bother about this. Don't really think about what you're about to implement as a true "v1" yet. It's more a first prototype that should work but will probably reveal some poor design choices along the way. For that purpose, an ephemeral mock allocator is good enough and easier to test than something that writes to disk.

## 7. Derive macro: how do struct fields that aren't themselves nested containers get handled?

The `Point` example in the README shows scalar fields (`x: i32`) getting a direct setter (`set_x`) that records an op and mutates in place, while nested `Persistable` fields (`y: BackedString`) get a `_mut()` accessor returning a nested `Guard`. For the derive macro to generate the right code per field, it needs to know which case each field is in. Two ways to do that:

- **(a)** Give primitive types (`i32`, `bool`, `String`, etc.) their own blanket/generated `Persistable` impls in `types`, so *every* field is treated uniformly by the macro (always generate a `_mut()` accessor returning a nested guard, even for `x: i32` — no special-cased direct setters). Simpler macro, more uniform API, but `guard.x_mut().set(5)` instead of `guard.set_x(5)` for scalar fields.
- **(b)** Keep the two-path model from the README's example (direct setters for "leaf" types, nested guards for `Persistable` types) — nicer resulting API, but the macro needs some way to distinguish the two cases per field, which isn't trivial in a proc macro without extra machinery (there's no stable specialization to lean on).

**I'd recommend (a)** for v1 — it's a meaningfully simpler macro to get right first, and the API difference is minor (`guard.x_mut().set(5)` vs `guard.set_x(5)`). Let me know if the nicer API in (b) is worth the extra macro complexity to you.

Choose (a).

## 8. Enum derive scope for v1

Also flagged as an open question in the README. For a `#[derive(Persistable)]` enum, how much granularity of mutation is in scope?

- **(a)** Coarse: the only mutation is "replace the whole enum value with a new variant" (one `Op` per assignment, no way to mutate a field *within* the current variant without replacing the whole thing).
- **(b)** Fine-grained: also expose per-field mutation within whichever variant is currently active (closer to what the earlier `OptionGuard::take` discussion sketched — tracking that a mutation only changes the enum's tag vs. changes a payload field without touching the tag).

**I'd recommend (a)** for v1 — it's a much smaller surface, and it's still enough to demonstrate `#[derive]` working on enums in the example app. (b) is a natural v1.1 once (a) is solid.

Agreed, but mark convenience methods to mutate fields of struct enum variants or some way to `match` on a `MyEnumMut<'_, B>` as future task. 

## 9. Crate names — bare or prefixed, and is there a facade crate?

The README's Workspace Layout uses bare names (`traits`, `types`, `alloc`, `derive`, `frontend`). `alloc` in particular collides in spirit with the (currently unstable-only-relevant) `alloc` crate in `std`, and bare names like `types` would be unpublishable on crates.io as-is. For a local workspace this doesn't matter functionally, but it affects how `Cargo.toml` dependency names and imports read everywhere.

**I'd recommend prefixing** — `kladde-traits`, `kladde-types`, `kladde-alloc`, `kladde-derive`, `kladde-frontend` — and adding a small facade crate, **`kladde`**, that re-exports the pieces application code actually needs (the way `serde`+`serde_derive` or `tokio`'s feature-gated re-exports work), so the `example` crate (and any real application) depends on one crate instead of five. Let me know if you'd rather keep bare names or skip the facade.

Yes, prefix the names. But don't create an additional facade. Regarding the facad: either follow your recommendation, or rename `kladde-frontend` to `kladde` and use that as the facade, since that's probably what application authors will use (and the name "frontend" is a bit weird anyway because it contains parts of the implementation of what we are now calling a `Backend`, if I understand the plan correctly). Use your own best judgment which of these two options are 

## 10. `Vec`/`HashMap` naming — shadow `std`'s names, or use distinct names?

`sketch.rs` names the backed vector type bare `Vec` (shadowing `std::vec::Vec` when both are in scope). Keeping that gives a "drop-in replacement" feel; renaming (e.g. `PersistedVec`/`PersistedHashMap`, or a short prefix) avoids the shadowing footgun (accidental wrong-`Vec` usage, needing `std::vec::Vec` qualification whenever you want the real one, IDE-autocomplete confusion) at the cost of a less elegant name.

**I'd recommend distinct names** (open to specific naming, e.g. `PersistedVec`/`PersistedMap` or `PVec`/`PMap`) — the shadowing trick reads nicely in an isolated code sample but seems like a real ergonomics tax in actual application code that also uses `std` collections. Let me know your preference (including if you'd rather keep the shadowing).

Agreed.

## 11. `HashMap<K, V>` — do keys need to be `Persistable`, or just `Hash + Eq + (De)Serialize`?

I'm planning: **keys are `Hash + Eq + Serialize + DeserializeOwned`, not necessarily `Persistable`** (no mutable access to a key in place — the usual pattern is remove+reinsert, matching how most map APIs treat keys as immutable once inserted); **values are `Persistable`** (so `get_mut` can hand back a `Guard`). Flag if you want keys to be mutable in place too.

Agreed.

## 12. API completeness for `Vec<T>`/`HashMap<K, V>` in v1

Minimal CRUD (`push`/`get`/`get_mut`/`remove`/`len`/`new`, roughly what's already sketched) or closer to `std` parity (iterators, `entry()`, etc.)? **I'd default to minimal CRUD plus a read-only iterator (`iter()`, since it's cheap given the in-memory representation is a real `std::collections` value underneath) for v1**, deferring anything more std-parity-shaped (mutable iteration, `entry()`, etc.) unless you want it now.

Agreed.

## 13. Wire-format integer widths

`sketch.rs` currently uses `u32` for both `UniquePointer::index` and (in the README's earlier draft) file positions — a 4 GiB ceiling on file size / allocation count. Since this bakes into the wire format, it's the kind of thing that's cheap to fix now and expensive later. **OK to keep `u32` for v1 (mock allocator, no real file, ceiling doesn't bite yet), with a note to reconsider before the real `Allocator`/file format ships?** Or would you rather start with `u64` throughout even though it's not load-bearing until the real allocator exists?

Use 32-bit but:
- define type aliases so that changing this to either 64-bit or variable length later will be easier.
- consider using `NonZeroU32` for pointers and maybe also for sizes of memory regions. We can probably assume that real pointers are never zero because the file will most likely start with a few magic bytes to identify the type. And I expect that we might have to introduce the concept of "not yet allocated pointers" (if an allocation operation hasn't been flushed yet), which can then be represented as a `None` variant of `Option<NonZeroU32>` without increasing memory footprint in lookup tables.

## 14. Error handling / code-quality bar for v1

Is this meant to be validate-the-architecture-quality code (reasonable `Result`/error types, but not exhaustively hardened) or closer to a rough proof-of-concept (`.unwrap()` where convenient, minimal error types)? **I'd default to the former** — real (if minimal) error enums via `thiserror` at crate boundaries, no silent panics on realistically-reachable error paths (file I/O, malformed data on load), but not chasing exhaustive edge-case coverage given this is explicitly a v1 built on a mock allocator.

Agreed.

## 15. What should the `example` app actually do?

Needs to exercise `Vec<T>`, `HashMap<K, V>`, and at least one `#[derive]`d custom `struct` and `enum`, nested inside each other. **I'm planning a small CLI contact book**: a `HashMap<String, Contact>` (name → contact) where `Contact` is a derived struct with a `Vec<PhoneNumber>` field and a derived enum (e.g. `PhoneNumber { Mobile(String), Landline(String) }`), with subcommands to add/list/edit/remove contacts, backed by a file on disk (subject to question 1/6's answers for what "backed by a file" actually means in v1). Let me know if you'd rather I build something else.

Agreed.

---

## Assumptions I'll proceed with unless you say otherwise (not blocking, just flagging)

- Latest stable Rust, 2021 edition, no particular MSRV target.
- `syn` + `quote` + `proc-macro2` for the derive macro (there isn't a real alternative).
- `HashMap<K, V>`'s in-memory representation uses `std::collections::HashMap`'s default hasher for v1 (swappable later if it matters).
- Plain `#[cfg(test)] mod tests` unit tests in each crate, plus a small integration test or two in `frontend`/`example`; no crash-consistency / fault-injection tests in v1, since the mock allocator has no real crash scenario to test against yet.
- No CI setup (GitHub Actions etc.) unless you want it.
- Exact method signatures on `Allocator` beyond `free` (i.e. `alloc`, resolving a pointer to its current target) are mine to design as I implement, following the shape already described in the README's "Pointers and Memory Management" section.
