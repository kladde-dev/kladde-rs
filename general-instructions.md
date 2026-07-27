# General instructions for implementation work

- Update the document `spec.md` whenever you realize that a slightly different implementation path is better than what is stated in the document, or when implementation work reveals some inconsistency, gap, or ambiguity in the document.
- Run `cargo fmt` and `cargo doc` before any git commit that commits rust code; fix any warnings reported by `cargo doc`.

## Doc comments

Write the doc comment of every **public** item (modules, types, traits, methods, functions, fields, …) for a *user* of the library, not for someone trying to understand its internals:

- Focus on **how to use** the item and **what guarantees it does or does not provide** — especially anything non-obvious: runtime complexity, crash-safety, panics, laziness/allocation behavior, invariants the caller must uphold. Do not describe how the item is implemented.
- Include a **short code example** demonstrating a typical use, wherever one can be written against the public API (e.g. via `kladde::Kladde` / a public constructor). Omit the example only when no runnable example is possible from the item's own crate (e.g. a low-level trait method whose only implementors live in a downstream crate).
- **Never contrast against previous behavior.** Before a first release there is no "previously"/"now"/"used to" — describe only what the item does today, as if it had always done so. Don't reference prior implementations, migrations, or removed APIs in public doc comments.
- Follow Rust doc conventions: start with a **short, usually single-line summary sentence**, then a blank line, then the fuller explanation and any examples.

Doc comments of **private** items (private modules, fields, helpers) may freely discuss internals, rationale, ordering subtleties, and the "why" behind a design — that's the right place for it.
