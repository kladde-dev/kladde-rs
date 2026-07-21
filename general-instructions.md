# General instructions for implementation work

- Update the document `spec.md` whenever you realize that a slightly different implementation path is better than what is stated in the document, or when implementation work reveals some inconsistency, gap, or ambiguity in the document.
- Run `cargo fmt` and `cargo doc` before any git commit that commits rust code; fix any warnings reported by `cargo doc`.
- When writing doc comments, document from the point of view of a _user_. Don't focus on how the documented item is implemented, focus instead on what the documented item does, how it can be used, and potential pitfalls.
