# Kladde — Project Assessment (v1 prototype)

A point-in-time review of where the project stands after the v1 milestone (working
end-to-end against an **in-memory mock** allocator), and what lies ahead. Written after a
full read of `spec.md`, `later.md`, and every crate. Ordered most-important-first.

---

## 1. Executive summary

**The central architectural bet has paid off.** The hardest, riskiest idea in the design —
record every mutation as a small fixed set of *type-agnostic* byte-level microoperations
(`Alloc`/`Free`/`Write`/`Copy`/`Resize`), so that flushing is pure replay that never calls
back into any data type — is fully implemented and works through arbitrarily deep nesting
(`PersistableHashMap<PersistableString, Contact{ PersistableString, PersistableVec<enum> }>`
round-trips through flush + reload). This de-risks the part most likely to have forced a
redesign. Everything else in the vision is comparatively conventional engineering.

**But "v1 works" is a narrow claim.** Everything runs against an in-memory mock. The three
things that make this a *durable persistence library* rather than an elaborate in-memory
data structure — **a real file backend, compaction, and crash consistency** — are entirely
undesigned-in-code. The gap between here and a shippable product is larger than the gap
already crossed.

**The single biggest unaddressed risk is schema evolution.** The on-disk layout is a
fixed-offset format keyed on *declaration order*: a struct's field offsets are the running
sum of earlier fields' sizes, and an enum's discriminant is its position in the source. This
means **adding a field, reordering fields, or reordering enum variants silently changes the
byte layout and makes every existing file unreadable**, with no versioning or migration path.
Orthogonally-persistent systems live or die on schema migration; today there is no story at
all. This must shape the on-disk format *before* the first real-file release — possibly
before much more is built.

**Two cheap, high-value things can and should be done now, against the mock**, to validate
invariants that are currently only asserted by hand: (a) a **crash-prefix replay test** (truncate
the microop journal at every prefix, assert `load()` still yields a valid state) directly
exercises the write-ahead-ordering discipline the whole crash-consistency story rests on;
(b) a **reachability-based leak detector** validates the "every `Persistable` impl doesn't
leak" convention that is otherwise trusted, not checked. Neither needs a real file.

---

## 2. Expectations met vs. not met

### Met (and genuinely well)

- **Type-agnostic microop journal + replay-only flush.** Implemented, tested end-to-end. No
  per-type `Op`, no callback into data types at flush. This is the keystone.
- **Read-speed parity by construction.** Reads hit real in-memory `Vec`/`HashMap`/`String`;
  the backend is write-mostly and only read at load/open. The design keeps the promise that
  reads never touch the file.
- **Ergonomics close to `std`.** `push`/`insert`/`remove`/`get_mut` through a `Guard`; a
  derive macro covering structs *and* enums (unit, tuple, and named-field variants), with
  static field offsets. The `Location { anchor, offset }` threading is a clean solution to
  "where does a deeply-nested leaf write."
- **Dropped the `T: Clone` requirement** that an earlier op-log design implied.
- **Type-level leak safety.** `String` deliberately doesn't implement `Persistable` (a
  compile-fail doctest enforces it); `PersistableString`/`PersistableBlob<T>` give owning types the
  cached `pointer` field they need. The `store(&mut self)` fix closed a real
  self-consistency bug (a `from_iter`/`from`-built value learning its pointer on first store).
- **Test discipline.** Regression tests carry rationale and pin specific past bugs.

### Not met yet — all deferred, all documented

- **No real file backend.** No `open`/`create`, no file header, no root pointer on disk, no
  serialization boundary. `Kladde::load` reconstructs from the same in-process backend.
- **No allocator / no compaction.** The mock is `HashMap<index, Box<[u8]>>`. The spec's
  elaborate index-keyed registry (+ `BTreeMap<position, index>` for relocating embedded
  pointers) is unbuilt and, critically, **unexercised** (see §3).
- **No crash consistency.** Shadow-paging/`fsync` is design-only. The journal isn't even
  framed/checksummed yet (it's an in-memory `Vec<Microop>`).
- **No flush optimization pass** (naive in-order replay).
- **No Drop-based freeing.** Replacing or dropping an "owning" value currently orphans its
  content allocation. Harmless in-memory (dies with the process); a real leak against a file.
- **Rope / real text** — promised in the intro; `PersistableString` is a byte-`Vec` placeholder
  with O(n) edits.

---

## 3. Architectural review

### What's solid

- The trait layer (`Persistable`/`Guard`/`Allocator`/`Backend`) is small, composes to
  arbitrary nesting, and cleanly separates "what a value's bytes are" (`INLINE_SIZE` +
  `store`/`load`) from "where they go" (`Location`) from "how bytes are stored" (`Allocator`).
- The enum derive reusing the struct field-offset logic per-variant is a tidy, low-complexity
  win.
- `&self`-with-interior-mutability on `Backend` (via `RefCell`/`Cell`) is the right shape to
  leave room for a future locking/MVCC backend without changing call sites.

### Poor / premature decisions worth correcting *now* (cheap now, expensive later)

1. **The `u32` ceiling is real, and the aliases meant to hide it are dead code.** Pointers,
   targets, offsets, and lengths are all hardcoded `NonZeroU32`/`u32` on the hot path (4 GiB
   file/allocation ceiling, ~4 billion live allocations). The `Position`/`Target`/`Size` type
   aliases in `kladde-traits` — introduced, per the spec, so "widening to `u64` only touches
   one definition" — **are not used anywhere.** So the claimed benefit is fictional; widening
   later would touch every pointer/offset site and the on-disk format. **Decide deliberately:**
   either thread the aliases through for real (so `u64` stays a one-line change), or commit to
   `u32` and document the ceiling as a format guarantee. Don't leave it implicit.

2. **The compaction mechanism — the trickiest unbuilt part — has an unexpressed coupling.**
   The spec's plan for relocating a block that contains *embedded* pointers relies on the
   allocator knowing each pointer's *position* (where its bytes live) so it can keep the
   `BTreeMap<position, index>` current as blocks move. But nothing in today's trait surface
   reports a pointer's position to the allocator: `write_header` just writes bytes. When the
   real allocator is built, `store`/`write_header` will have to *also* tell the allocator "a
   pointer for index I now lives at (anchor, offset)". This is a new obligation on every
   owning type's `store`, not just an allocator-internal detail — and the mock has let it hide.
   **This is the least-validated load-bearing assumption in the whole design.** Prototype the
   real allocator (or at least the position-registry + relocate path) against the existing
   containers *soon*, before more types are written to the current trait shape.

3. **Fallibility boundary is undecided and the API is ossifying around "infallible."**
   Every method panics on the unexpected (`expect`/`unwrap` throughout); nothing returns
   `Result`. For a real file, I/O errors are unavoidable. The saving grace is the journal
   model: mutations only append in memory, so keeping `push`/`insert`/`set` infallible is
   *defensible* **if** the fallible surface is confined to `flush`/`open`/`close`. Make that
   split explicit and intended now — retrofitting `Result` onto mutation methods later is a
   pervasive breaking change.

### Contained risks (inherent to the approach, acceptable, worth naming)

- **Offset arithmetic is unsafe-by-convention.** Once `RawPointer` type-erases `T`, container
  code is doing manual byte math with no compiler check — the same fragility as `std`'s
  internal `unsafe`, minus the keyword. It's confined to a handful of library types and the
  derive macro, and well-tested, but the correctness burden is real and concentrated there.
- **`INLINE_SIZE: usize` but offsets are `u32`** — a wide struct could overflow the `u32`
  offset cast. Tied to the width decision above.

---

## 4. Pivotal decisions ahead

In rough priority order:

1. **On-disk format & schema evolution** *(the big one)*. Fixed offsets + declaration-order
   discriminants mean any struct/enum change is a breaking format change. Options to weigh:
   explicit per-type schema descriptors written to the file; field/variant identity that
   survives reordering; a versioning + migration mechanism (the spec's deferred
   semantic-versioning slot is a start but only *detects* incompatibility, doesn't *resolve*
   it). This decision constrains the layout, so make it before the format is frozen.

2. **In-memory copy vs. file-as-arena** (already flagged in `later.md`'s "major redesigns").
   Today: a full in-memory `Vec`/`HashMap` *and* the on-disk snapshot — great read speed, but
   ~2× memory transiently at open and a duplicated steady state. The alternative (read
   directly from an in-memory mirror of the file, used as an arena) saves memory but
   complicates journaling and forces read paths to borrow the backend. This is a genuine fork
   in the road and worth prototyping before committing further.

3. **Real allocator + compaction + the position registry** (see §3.2). Validating this may
   change the trait surface, so it gates a lot.

4. **Crash-consistency implementation**: shadow-paging + `fsync` ordering, journal framing/
   checksums, and the `StartAtomic`/`EndAtomic` fallback for mutations whose microop sequence
   can't be made prefix-safe. The ordering *discipline* is testable now (§5).

5. **Rope for text.** `PersistableString`'s byte-`Vec` won't serve the document-editor use case
   the intro gestures at. A chunked/rope representation is a substantial design in itself.

---

## 5. Obstacles / limitations foreseen for a full release

- **Schema migration** (§4.1) — the defining challenge for this class of system; currently absent.
- **Write amplification on growth.** `PersistableVec::push` resizes the whole content
  allocation each time (O(n) copy per push → O(n²) to build); `PersistableString` edits
  byte-by-byte. Both are documented as awaiting chunked representations, but they're real
  cliffs the moment collections get large.
- **Unbounded on-disk growth under churn.** `PersistableHashMap` tombstones are only reclaimed
  by a compaction pass that doesn't exist; sustained insert/remove grows the file without
  bound. The `later.md` free-list middle ground is a cheap partial mitigation.
- **4 GiB ceiling** (§3.1) until the width question is settled.
- **Single-writer only.** Fine as scoped, but concurrent/multi-process access is a large
  future lift; the single-owner pointer design leans the right way for it (MVCC), which is a
  point in the architecture's favor.
- **Everything panics.** Until the fallibility boundary is drawn (§3.3), any real-world I/O
  error is a process abort.

---

## 6. Opportunities (some not yet in `later.md`)

- **Test the crash-consistency discipline now, for free.** The "any journal prefix replays to
  a valid state" invariant — the foundation of the whole crash story — can be tested today
  against the mock: truncate the `Vec<Microop>` journal at every prefix, replay, assert
  `load()` yields a valid (possibly stale) state. This turns a hand-waved invariant into a
  machine-checked one *before* the real backend depends on it. High value, low cost.
  (`later.md` sketches this under WAL discipline; it's buildable immediately, not "later.")
- **Build the leak detector now.** Walk allocations reachable from the root via
  `Persistable`'s own size/offset structure, compare to the allocator's live set. Validates
  the trusted-not-checked no-leak convention, and would immediately surface the drop/replace
  leaks from the missing `Drop` impls. Also buildable against the mock.
- **A spy `Backend`** that records the exact microop sequence a mutation emits would make the
  ordering discipline regression-testable per mutation shape — a natural complement to the
  prefix test, and trivial given the trait.
- **`kladde-alloc` as a standalone product.** The type-agnostic relocatable persistent heap
  is reusable well beyond this library; the `spec.md` Automerge comparison already hints at
  this. Worth keeping the crate boundary clean with that option in mind.
- **Ergonomic guard sugar** (`set_<field>` on struct guards; enum-guard pattern matching) —
  in `later.md`; low-risk polish that markedly improves the application-author experience and
  is worth doing before an API-stabilizing release.
- **Variable-width discriminants/integers** (in `later.md`) pair naturally with the
  schema-evolution work — a 1-byte discriminant for small enums both saves space and is a
  good moment to introduce variant *identity* rather than positional discriminants.

---

## 7. Recommended near-term sequence

1. **Decide the width question** (§3.1) — trivial code change, but it's a format guarantee, so
   settle it before writing bytes to a real file.
2. **Add the crash-prefix replay test and the leak detector** (§6) — cheap, high-confidence
   validation of two currently-unchecked invariants, entirely against the existing mock.
3. **Prototype the real allocator's relocate/position-registry path** against today's
   containers (§3.2) — this is the assumption most likely to force a trait change, so learn it
   early.
4. **Sketch the on-disk format with schema evolution in mind** (§4.1) before committing to the
   first file format — even a lightweight per-type schema descriptor + version gate changes
   what the layout should look like.
5. Only then build out the real file backend, crash consistency, and compaction.

The foundation is genuinely strong and the hardest conceptual risk is retired. The work ahead
is mostly "conventional but exacting systems engineering" — with the sharp exception of schema
evolution, which is a design problem, not just an implementation one, and deserves attention
sooner than its current absence from the roadmap suggests.
