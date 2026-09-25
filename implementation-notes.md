# Implementation notes

Where this implementation deviates from kladde-docs, what the documentation leaves unclear, what could be done better, and what went wrong along the way.

This file takes over the role that `general-instructions.md` gives to `spec.md`.
The design lives in kladde-docs, so findings from implementing it are recorded here first, and proposed for the documents from here.
Paths like `spec/journal.md` are relative to kladde-docs' `content/`.

## Storage

- **`FileStorage` grows the file with `set_len`, not with `fallocate`.**
  The new region stays unallocated until it is written, so a full disk shows up as a failed write or `fsync` during a flush, which poisons the store, rather than as a failed growth before the flush starts.
  `impl/` says nothing about preallocation, and `spec/durability.md#headroom` asks only that failure be clean, which poisoning provides.
- **`MemoryStorage` models a power cut as any subset of the writes since the last sync, each write whole.**
  It does not model a write torn within a page.
  The page CRCs are meant to catch those, but no test exercises them yet.
