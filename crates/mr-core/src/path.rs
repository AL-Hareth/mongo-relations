//! Dotted-path helpers for BSON documents.

use bson::{Bson, Document};

/// Get the value at a dotted path. Does not traverse arrays.
pub fn get_path<'a>(doc: &'a Document, path: &str) -> Option<&'a Bson> {
    let mut cur = doc;
    let mut segs = path.split('.').peekable();
    while let Some(seg) = segs.next() {
        let v = cur.get(seg)?;
        if segs.peek().is_none() {
            return Some(v);
        }
        cur = v.as_document()?;
    }
    None
}

/// Set the value at a dotted path, creating intermediate documents.
/// Non-document intermediates are replaced.
pub fn set_path(doc: &mut Document, path: &str, value: Bson) {
    match path.split_once('.') {
        None => {
            doc.insert(path, value);
        }
        Some((head, rest)) => {
            if !matches!(doc.get(head), Some(Bson::Document(_))) {
                doc.insert(head, Document::new());
            }
            if let Some(Bson::Document(inner)) = doc.get_mut(head) {
                set_path(inner, rest, value);
            }
        }
    }
}

/// Remove the value at a dotted path.
pub fn remove_path(doc: &mut Document, path: &str) -> Option<Bson> {
    match path.split_once('.') {
        None => doc.remove(path),
        Some((head, rest)) => match doc.get_mut(head) {
            Some(Bson::Document(inner)) => remove_path(inner, rest),
            _ => None,
        },
    }
}

/// Key values stored at `path`: a scalar yields one key, an array yields its
/// elements; `null`/missing yield nothing.
pub fn key_values(doc: &Document, path: &str) -> Vec<Bson> {
    match get_path(doc, path) {
        None | Some(Bson::Null) => vec![],
        Some(Bson::Array(a)) => a
            .iter()
            .filter(|v| !matches!(v, Bson::Null))
            .cloned()
            .collect(),
        Some(v) => vec![v.clone()],
    }
}

/// Is `a` equal to `b` or a dotted prefix of it (or vice versa)?
/// `"a.b"` overlaps `"a"`, `"a.b.c"` and `"a.b"`, but not `"a.bc"`.
pub fn paths_overlap(a: &str, b: &str) -> bool {
    fn prefix(p: &str, of: &str) -> bool {
        of.len() > p.len() && of.starts_with(p) && of.as_bytes()[p.len()] == b'.'
    }
    a == b || prefix(a, b) || prefix(b, a)
}

/// A hashable, totally ordered representation of a key value, used to group
/// and deduplicate join keys. Uses the canonical Extended JSON encoding so
/// that e.g. `Int32(1)` and `Int64(1)` stay distinct like in BSON.
pub fn key_repr(v: &Bson) -> String {
    match v {
        Bson::ObjectId(o) => o.to_hex(),
        Bson::String(s) => format!("s:{s}"),
        other => other.clone().into_canonical_extjson().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bson::doc;

    #[test]
    fn get_set_remove() {
        let mut d = doc! { "a": { "b": 1 } };
        assert_eq!(get_path(&d, "a.b"), Some(&Bson::Int32(1)));
        set_path(&mut d, "a.c.d", Bson::Int32(2));
        assert_eq!(get_path(&d, "a.c.d"), Some(&Bson::Int32(2)));
        assert_eq!(remove_path(&mut d, "a.b"), Some(Bson::Int32(1)));
        assert_eq!(get_path(&d, "a.b"), None);
    }

    #[test]
    fn overlap() {
        assert!(paths_overlap("a", "a.b"));
        assert!(paths_overlap("a.b", "a"));
        assert!(paths_overlap("a.b", "a.b"));
        assert!(!paths_overlap("a.b", "a.bc"));
        assert!(!paths_overlap("ab", "a"));
    }

    #[test]
    fn keys() {
        let d = doc! { "x": [1, null, 2], "y": null, "z": "k" };
        assert_eq!(key_values(&d, "x").len(), 2);
        assert!(key_values(&d, "y").is_empty());
        assert!(key_values(&d, "missing").is_empty());
        assert_eq!(key_values(&d, "z"), vec![Bson::String("k".into())]);
    }
}
