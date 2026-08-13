//! Recursive "upsert" merge of a user TOML overlay onto a defaults
//! table. Nested tables merge; every leaf (scalars, arrays) from the
//! overlay overrides the defaults.
//!
//! Ported verbatim from
//! `old-twoliter/tools/pubsys/src/aws/ami/register/merge_toml.rs`.
use toml::Value;

/// A recursive merge from `overlay` into `base`. Tables merge
/// key-by-key; non-table values from `overlay` replace whatever's in
/// `base` at that key.
pub(crate) trait Merge {
    fn merge(&mut self, overlay: Self);
}

impl Merge for Value {
    fn merge(&mut self, overlay: Value) {
        match (self, overlay) {
            (Value::Table(base_table), Value::Table(overlay_table)) => {
                for (key, overlay_val) in overlay_table {
                    match base_table.get_mut(&key) {
                        Some(base_val) => base_val.merge(overlay_val),
                        None => {
                            base_table.insert(key, overlay_val);
                        }
                    }
                }
            }
            // Non-table overlay values replace the base outright.
            (base, overlay) => *base = overlay,
        }
    }
}

impl Merge for toml::Table {
    fn merge(&mut self, overlay: toml::Table) {
        for (key, overlay_val) in overlay {
            match self.get_mut(&key) {
                Some(Value::Table(inner_base)) if overlay_val.is_table() => {
                    // Recurse table-in-table.
                    if let Value::Table(inner_overlay) = overlay_val {
                        let mut base_v = Value::Table(std::mem::take(inner_base));
                        base_v.merge(Value::Table(inner_overlay));
                        if let Value::Table(merged) = base_v {
                            *inner_base = merged;
                        }
                    }
                }
                Some(base_val) => *base_val = overlay_val,
                None => {
                    self.insert(key, overlay_val);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Merge;

    /// Overlay scalar overrides the base scalar at the same key.
    #[test]
    fn scalar_override() {
        let mut base: toml::Table = toml::from_str(r#"name = "old""#).unwrap();
        let overlay: toml::Table = toml::from_str(r#"name = "new""#).unwrap();
        base.merge(overlay);
        assert_eq!(base.get("name").and_then(|v| v.as_str()), Some("new"));
    }

    /// Nested tables merge key-by-key rather than replacing the
    /// whole table.
    #[test]
    fn nested_table_merges_by_key() {
        let mut base: toml::Table = toml::from_str(
            r#"
[block-device-mappings."/dev/xvda".ebs]
volume-type = "gp2"
volume-size = 4
"#,
        )
        .unwrap();
        let overlay: toml::Table = toml::from_str(
            r#"
[block-device-mappings."/dev/xvda".ebs]
volume-size = 8
"#,
        )
        .unwrap();
        base.merge(overlay);
        let rendered = toml::to_string(&base).unwrap();
        // volume-type kept, volume-size overridden.
        assert!(rendered.contains(r#"volume-type = "gp2""#));
        assert!(rendered.contains("volume-size = 8"));
    }

    /// Overlay-only keys extend the base.
    #[test]
    fn overlay_only_keys_added() {
        let mut base: toml::Table = toml::from_str(r#"a = 1"#).unwrap();
        let overlay: toml::Table = toml::from_str(r#"b = 2"#).unwrap();
        base.merge(overlay);
        assert_eq!(base.get("a").and_then(|v| v.as_integer()), Some(1));
        assert_eq!(base.get("b").and_then(|v| v.as_integer()), Some(2));
    }
}
