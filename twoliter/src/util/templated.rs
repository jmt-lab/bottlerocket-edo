use handlebars::Handlebars;
use std::path::{Path, PathBuf};
use toml::Value;

use serde::{Serialize, de::DeserializeOwned};
use snafu::{ResultExt, Snafu};

pub fn load_templated<T, I, C>(tables: I, context: &C) -> Result<T, TemplateError>
where
    T: DeserializeOwned,
    I: IntoIterator<Item = toml::Table>,
    C: Serialize,
{
    // 1. Parse and deep-merge every toml into one document.
    let mut merged = Value::Table(Default::default());
    let mut any = false;
    let ctx_table = toml::Value::try_from(context).context(SerializeSnafu)?;

    for table in tables {
        any = true;
        deep_merge(&mut merged, Value::Table(table.clone()));
    }
    if !any {
        return EmptySnafu {}.fail();
    }

    // 2. Resolve cross-references to a fixpoint.
    let mut hb = Handlebars::new();
    hb.set_strict_mode(true); // error on a reference to a key that does not exist
    hb.register_escape_fn(handlebars::no_escape); // config values, not html

    const MAX_PASSES: usize = 16;
    let mut passes = 0;
    loop {
        // Snapshot the current document as the render context.
        let mut cycle_ctx = ctx_table.clone();
        deep_merge(&mut cycle_ctx, merged.clone());
        let ctx = serde_json::to_value(&cycle_ctx).unwrap();
        let mut changed = false;
        render_tree(&mut merged, &ctx, &hb, &mut changed)?;
        if !changed {
            break;
        }
        passes += 1;
        if passes >= MAX_PASSES {
            return UnresolvedSnafu { pass: MAX_PASSES }.fail();
        }
    }

    // 3. Deserialize the fully-resolved coument
    merged.try_into().context(DeserializeSnafu)
}

fn deep_merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Table(b), Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        (slot, other) => *slot = other,
    }
}

/// Walk the tree; render every string leaf that contains `{{`. Sets `changed` if
/// any leaf's value differs after rendering (drives the fixpoint loop).
fn render_tree(
    node: &mut Value,
    ctx: &serde_json::Value,
    hb: &Handlebars,
    changed: &mut bool,
) -> Result<(), TemplateError> {
    match node {
        Value::Table(t) => {
            for (_, v) in t.iter_mut() {
                render_tree(v, ctx, hb, changed)?;
            }
        }
        Value::Array(a) => {
            for v in a.iter_mut() {
                render_tree(v, ctx, hb, changed)?;
            }
        }
        Value::String(s) => {
            if !s.contains("{{") {
                return Ok(());
            }
            let pure_ref = is_pure_template(s);
            let rendered = hb.render_template(s, ctx).context(RenderSnafu)?;
            if &rendered != s {
                *changed = true;
                // If the whole value was a single `{{ref}}`, coerce the rendered
                // string back into a typed scalar so `port = "{{net.port}}"` can
                // deserialize into a u16 rather than staying a String.
                *node = if pure_ref {
                    coerce_scalar(rendered)
                } else {
                    Value::String(rendered)
                };
            }
        }
        _ => {}
    }
    Ok(())
}

/// True if the entire trimmed string is exactly one `{{ ... }}` expression.
fn is_pure_template(s: &str) -> bool {
    let t = s.trim();
    t.starts_with("{{") && t.ends_with("}}") && t.len() > 4 && !t[2..t.len() - 2].contains("{{")
}

/// Best-effort typed coercion for a fully-substituted value.
fn coerce_scalar(s: String) -> Value {
    let t = s.trim();
    if let Ok(b) = t.parse::<bool>() {
        return Value::Boolean(b);
    }
    if let Ok(i) = t.parse::<i64>() {
        return Value::Integer(i);
    }
    if let Ok(f) = t.parse::<f64>() {
        return Value::Float(f);
    }
    Value::String(s)
}

#[derive(Debug, Snafu)]
pub enum TemplateError {
    #[snafu(display("failed to read {path:?}: {source}"))]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[snafu(display("failed to parse file at {path:?}: {source}"))]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[snafu(display("failed to render template: {source}"))]
    Render { source: handlebars::RenderError },
    #[snafu(display("unresolved references after {pass} passes (cycle or dangling {{{{ref}}}})"))]
    Unresolved { pass: usize },
    #[snafu(display("final deserialization error: {source}"))]
    Deserialize { source: toml::de::Error },
    #[snafu(display("failed to serialize context: {source}"))]
    Serialize { source: toml::ser::Error },
    #[snafu(display("no toml files provided"))]
    Empty,
}
