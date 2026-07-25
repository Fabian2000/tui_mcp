//! Helpers for tuning the JSON Schemas that `schemars` derives for tool inputs.

/// `schemars` transform that collapses a derived `oneOf` of `const` string
/// variants into a compact `{"type":"string","enum":[…]}`.
///
/// `schemars` only emits the `oneOf`/`const` shape for a unit enum so it can
/// attach a per-variant `description` (taken from each variant's `///` doc
/// comment). Applied via `#[schemars(transform = schema::flatten_enum)]`, this
/// keeps those comments for source readers / rustdoc but drops them from the
/// emitted schema in favour of the flat, widely-supported `enum` form — a
/// smaller payload and better client tooling. The type-level description and
/// any sibling keywords are left untouched.
///
/// No-op unless the schema is exactly an all-`const` `oneOf`, so it is safe to
/// attach to any enum and degrades gracefully if `schemars` changes its output.
pub fn flatten_enum(schema: &mut schemars::Schema) {
    // Technique recommended by the schemars maintainer (a custom Transform that
    // reads the derived oneOf's `const` values):
    // https://github.com/GREsau/schemars/issues/34#issuecomment-2910357754
    // We diverge on purpose: that example appends each variant's description to
    // the parent and panics on unexpected variants; we drop the descriptions
    // (smaller schema) and no-op on anything that isn't an all-`const` oneOf.
    let Some(obj) = schema.as_object_mut() else {
        return;
    };
    let Some(one_of) = obj.get("oneOf").and_then(|v| v.as_array()) else {
        return;
    };
    let values: Vec<serde_json::Value> = one_of
        .iter()
        .filter_map(|v| v.as_object()?.get("const").cloned())
        .collect();
    // Only rewrite when every branch was a plain `const` (the shape we expect).
    if values.is_empty() || values.len() != one_of.len() {
        return;
    }
    obj.remove("oneOf");
    obj.insert("type".to_string(), serde_json::json!("string"));
    obj.insert("enum".to_string(), serde_json::Value::Array(values));
}
