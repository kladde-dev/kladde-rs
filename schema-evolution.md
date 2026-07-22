# Schema Evolution

I'm currently deliberating the following ideas for schema evolution. Help me understand the requirements, typical solution strategies, trade-offs of possible solutions.

## Goals and typical approaches

Claude: define in more detail the goals for schema evolution and fill in the text here. What exactly are the scenarios you are concerned about in regards to the "schema evolution" issue you identifiec in `assessment.md`? There are probably several different concerns and several different levels of support that a data format can offer (I can think of a rang from just "detecting an incompatible schema" over "being able to convert an old schema to a new one" to "being able to load an old schema without even noticing minor differences and writing it back in the same old format so that an old app can read it again"). What other options are there, what are the terms generally used in this field for the concerns one wants to address. Briefly summarize their typical solutions.

## Current ideas for schema evolution

Complete the lists of advantages and disadvantages of each idea below and add any other relevant comments or questions inline. Also try to come up with better ideas than the ones I wrote up in this document.

### Idea 1: fully self-describing schemas

A `kladde` file stores an array of serialized type descriptions at a pre-defined memory address. Each type description is essentially a binary serialized representation of something similar to the following:

```rust
enum Type {
    Primitive(PrimitiveType),
    Struct{ name: String, fields: Vec<(String, Index)> },
    Enum{ name: String, variants: Vec<EnumVariant> }, // with `enum EnumVariant { Tuple{name: ..., fields: ...}, Struct{...} }`
    Manual{ name: String, crate_name: String, version: (u32, u32, u32), type_parameters: Vec<Index> },
}
```

Here, `Index` is an integer index into the array of serialized type description. The first type in this array has to be the root type stored in the `kladde` file.

When opening a file, kladde first parses the types list into a `Vec<Type>`. It then constructs the data, which calls `Persistable::load` for the root type and all its nested types as already implemented, except that `Persistable::load` gets a new argument of type `&Type` and is made fallible.

**Advantages:**

- Fully self describing and even human readable (debuggable).
- Type-level backward compatibility: if somewhere deep in the nested stored data structure a type from a library is used and that library releases a new verison, the library may choose to be backward-compatible, i.e., still support the old type as well. Since `load` gets the `&Type` argument, it can check against a list of types it chooses to support.
- Little runtime overhead for storing because type descriptions can probably a `const` trait item of `Persistable` (check if the derive macro can create them at compile time).
- Relatively space efficient: we store type information only once _per type_, not per instance (this is different to, e.g., JSON). But for small files or for settings files that basically contain a large nested struct with no repeated data types, the impact on file size might still be substantial.

**Disadvantages:**

- `load` should probably check the type description of every loaded instance of every type. These checks can be expensive because we're potentially comparing complex nested data structures with lots of strings, and we have to look up indices.
- I'm not sure if we can allow updates to type implementations to change the `INLINE_SIZE` because this would change the logic of types further up the hierarchy (i.e., if `ForeignType` from `foreign_crate` changes its `INLINE_SIZE` in a new version of `foreign_crate`, then a `PersistentVec<ForeignType>` has to change its logic). Unfortunately, we can't simply include the `INLINE_SIZE` in the type description because, for example for `Struct` types, the `INLINE_SIZE` depends on the `INLINE_SIZE` of the fields, which might depend on their versions.
- For `Struct` and `Enum`, we're essentially implementing structural typing. Two struct types with the same name and same fields will serialize to the same description even if they're different rust types. For derived types, I'm not sure if this is an issue: we need the type description only to check if the data representation in the file matches what the program's `Persistable` implementation of the type does, and for derived types, that's the case if the type is structurally identical. Note that the serialized type description does not affect which rust type will be used to actually handle the data structure. That's handled as implemented now, via rust's normal type system.
- Names of `Manual` can clash. The `(crate_name, name)` identification is not versatile (assumes globally known crate names, ...).
- Some unclear cases: consider the type `struct Calculation { formula: PersistedString }` with a derived `Persistable` implementation. Its type description is `Type::Struct{ ... }`. Now assume that we want to cache the result of the calculation, but we don't want to store the result in the file (we'll redo the calculation when we open the file). Thus, we now have `struct Calculation { formula: PersistedString, cached_result: i32 }`. We don't want to derive `Persistable` for it because that would also persist the cached result. So we manually implement `Persistable` as if the field `cached_result` wasn't there. Should this type's description now be `Type::Manual` or should it be the same `Type::Struct` as we had before because the representaiton in the file remained unchanged?

### Idea 2: merkelize Idea 1

Think of the list of serialized data types as a vertices of a graph, where any `Index` in the definition of `Type` above defines a vertex of the graph. Merkelize that graph: calculate a reproducible hash of each `Type` entry in the list. If a `Type` contains an `Index`, replace the `Index` with the hash of the referenced data structure.

**Main practicaly difference to Idea 1:** I think this moves all updates, regardless of how deep in the tree of nested data types, to the application layer. If we include some type from a foreign crate somewhere nested in our type, and we update that foreign crate to a version with a non-backward-compatible change of the type, then all upstream hashes including the hash of the root data type change. For `kladde` files that are supposed to be used only by a single authoritative app, this might work: when application authors prepare a new release, they can update all dependencies and record the new root hash. Then they keep a table of app versions and root hashes, and they implement support for all previously supported root hashes (Claude: how would one do this ergonomically?). However, when people start to fork the app, the space of possible root hashes grows probably combinatorically.

**Note:** in this approach, the `version` field of `Type::Manual` should probably only contain the leading version number so that changes in the minor or patch version number are indeed recognized as backward compatible.

**Advantages:**

- Even more space efficient. Hashes are short, and we probably only need to store the root hash.
- Changes in the `INLINE_SIZE` of a type are now allowed. If some leaf type of the system changes its `INLINE_SIZE`, its definition or the version of its crate must have changed somehow, so its hash changes. That change propagates up the merkelized data structure to the root hash, so everybody upstream knows that their memory layout might change.

**Disadvantages:**

- No cyclic dependencies allowed (i.e., no linked list) because only DAGs can be merkelized. Is this true? Is there a way out? Or do we have that limitation anyway somehow in the system?
- Types that contain other (non-primitive) types can no longer detect whether a changed hash is due to their own update or an update of the contained types. Generic types can't use the hash at all.

### Other ideas

Claude: fill in

## Conclusions

Claude: fill in your conclusions here.
