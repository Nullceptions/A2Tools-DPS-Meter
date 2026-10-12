//! Plugin decoders: a plugin's manifest names a frame type and the fields it
//! wants, and this reads exactly those, as numbers, from frames of that type.
//! A plugin never sees the frame itself.
//!
//! ```json
//! { "name": "soul-gained", "opcode": "0D 90",
//!   "fields": [ { "type": "skip:1" },
//!               { "name": "pet", "type": "u32" },
//!               { "name": "amount", "type": "u32" } ] }
//! ```
//!
//! Field types: `u8`, `u16`, `u32`, `u64`, `i32` (little-endian), `varint`,
//! `skip:N`, and `list` (a `count` of `u8`, `u16`, `u32` or `varint`, then that
//! many entries of the list's own `fields`). Numbers only: there is no string
//! type, so no name can be read out of a frame.
//!
//! `"self": "<field>"` (plugin API 2) names a top-level number field that holds
//! an entity id: the meter passes the event on only when that entity is the
//! player's own character, so a plugin can tell "I did this" without ever
//! seeing anyone else's id.

use serde::Deserialize;
use serde_json::{Map, Value};

/// Most fields one level of a decoder may have.
const MAX_FIELDS: usize = 32;
/// Most entries a list may have.
const MAX_LIST: u64 = 4096;
/// How deep lists may nest.
const MAX_DEPTH: u8 = 2;

#[derive(Debug, Clone, Deserialize)]
pub struct DecoderSpec {
    pub name: String,
    /// Two bytes in hex, as frames are logged: `"0D 90"`.
    pub opcode: String,
    pub fields: Vec<FieldSpec>,
    /// A field that must be the player's own entity id (see the module docs).
    #[serde(default, rename = "self")]
    pub self_field: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FieldSpec {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(rename = "type")]
    pub kind: String,
    /// For `list`: the count's type.
    #[serde(default)]
    pub count: Option<String>,
    /// For `list`: each entry's fields.
    #[serde(default)]
    pub fields: Option<Vec<FieldSpec>>,
}

/// A decoder checked and ready to run.
#[derive(Debug, Clone)]
pub struct Decoder {
    pub name: String,
    pub opcode: [u8; 2],
    fields: Vec<FieldSpec>,
    /// The field that must be the player's own entity id, if any.
    pub self_field: Option<String>,
}

impl Decoder {
    /// Check a manifest's decoder: a sane name, a two-byte opcode, and fields
    /// of known types within the limits.
    pub fn compile(spec: &DecoderSpec) -> Result<Self, String> {
        if spec.name.is_empty()
            || spec.name.len() > 40
            || !spec.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(format!("decoder name {:?}: lowercase letters, digits and '-' only", spec.name));
        }
        let bytes: Vec<u8> = spec
            .opcode
            .split_whitespace()
            .map(|b| u8::from_str_radix(b, 16))
            .collect::<Result<_, _>>()
            .map_err(|_| format!("decoder {}: opcode {:?} is not hex bytes", spec.name, spec.opcode))?;
        let opcode: [u8; 2] = bytes
            .try_into()
            .map_err(|_| format!("decoder {}: opcode must be two bytes", spec.name))?;
        check_fields(&spec.fields, 0).map_err(|e| format!("decoder {}: {e}", spec.name))?;
        if let Some(field) = &spec.self_field {
            let ok = spec.fields.iter().any(|f| {
                f.name.as_deref() == Some(field.as_str()) && matches!(f.kind.as_str(), "u16" | "u32" | "u64" | "varint")
            });
            if !ok {
                return Err(format!("decoder {}: \"self\" must name one of its number fields", spec.name));
            }
        }
        Ok(Self { name: spec.name.clone(), opcode, fields: spec.fields.clone(), self_field: spec.self_field.clone() })
    }

    /// Decode a frame's body (the bytes after its type) into the fields.
    pub fn decode(&self, body: &[u8]) -> Option<Value> {
        let mut pos = 0;
        decode_fields(&self.fields, body, &mut pos).map(Value::Object)
    }
}

fn check_fields(fields: &[FieldSpec], depth: u8) -> Result<(), String> {
    if fields.len() > MAX_FIELDS {
        return Err(format!("more than {MAX_FIELDS} fields"));
    }
    for field in fields {
        let kind = field.kind.as_str();
        match kind {
            "u8" | "u16" | "u32" | "u64" | "i32" | "varint" => {
                if field.name.as_deref().is_none_or(str::is_empty) {
                    return Err(format!("a {kind} field needs a name"));
                }
            }
            "list" => {
                if depth >= MAX_DEPTH {
                    return Err("lists nest too deep".into());
                }
                if field.name.as_deref().is_none_or(str::is_empty) {
                    return Err("a list needs a name".into());
                }
                match field.count.as_deref() {
                    Some("u8" | "u16" | "u32" | "varint") => {}
                    _ => return Err("a list's count must be u8, u16, u32 or varint".into()),
                }
                check_fields(field.fields.as_deref().unwrap_or(&[]), depth + 1)?;
            }
            skip if skip.starts_with("skip:") => {
                let n: usize = skip[5..].parse().map_err(|_| format!("bad {skip}"))?;
                if n == 0 || n > 4096 {
                    return Err(format!("bad {skip}"));
                }
            }
            other => return Err(format!("unknown field type {other:?}")),
        }
    }
    Ok(())
}

fn take<'a>(body: &'a [u8], pos: &mut usize, n: usize) -> Option<&'a [u8]> {
    let end = pos.checked_add(n)?;
    let bytes = body.get(*pos..end)?;
    *pos = end;
    Some(bytes)
}

fn varint(body: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *take(body, pos, 1)?.first()?;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn number(kind: &str, body: &[u8], pos: &mut usize) -> Option<u64> {
    Some(match kind {
        "u8" => u64::from(take(body, pos, 1)?[0]),
        "u16" => u64::from(u16::from_le_bytes(take(body, pos, 2)?.try_into().ok()?)),
        "u32" => u64::from(u32::from_le_bytes(take(body, pos, 4)?.try_into().ok()?)),
        "u64" => u64::from_le_bytes(take(body, pos, 8)?.try_into().ok()?),
        "varint" => varint(body, pos)?,
        _ => return None,
    })
}

fn decode_fields(fields: &[FieldSpec], body: &[u8], pos: &mut usize) -> Option<Map<String, Value>> {
    let mut out = Map::new();
    for field in fields {
        let kind = field.kind.as_str();
        if let Some(n) = kind.strip_prefix("skip:") {
            take(body, pos, n.parse().ok()?)?;
            continue;
        }
        let name = field.name.clone().unwrap_or_default();
        match kind {
            "i32" => {
                let v = i32::from_le_bytes(take(body, pos, 4)?.try_into().ok()?);
                out.insert(name, Value::from(v));
            }
            "list" => {
                let count = number(field.count.as_deref()?, body, pos)?;
                if count > MAX_LIST {
                    return None;
                }
                let inner = field.fields.as_deref().unwrap_or(&[]);
                let mut items = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    items.push(Value::Object(decode_fields(inner, body, pos)?));
                }
                out.insert(name, Value::Array(items));
            }
            _ => {
                out.insert(name, Value::from(number(kind, body, pos)?));
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(json: &str) -> Decoder {
        Decoder::compile(&serde_json::from_str(json).unwrap()).unwrap()
    }

    #[test]
    fn reads_a_soul_gain() {
        let d = spec(r#"{"name":"soul-gained","opcode":"0D 90","fields":[
            {"type":"skip:1"},{"name":"pet","type":"u32"},{"name":"amount","type":"u32"}]}"#);
        assert_eq!(d.opcode, [0x0D, 0x90]);
        let body = [0x01, 0xF8, 0x03, 0, 0, 0x01, 0, 0, 0];
        assert_eq!(d.decode(&body).unwrap(), serde_json::json!({"pet": 1016, "amount": 1}));
    }

    #[test]
    fn reads_the_pet_list_with_its_two_lists() {
        let d = spec(r#"{"name":"pet-list","opcode":"00 90","fields":[
            {"type":"skip:4"},
            {"name":"levels","type":"list","count":"u8","fields":[
                {"name":"pet","type":"u32"},{"type":"skip:4"},{"name":"level","type":"u32"}]},
            {"name":"points","type":"list","count":"u8","fields":[
                {"name":"pet","type":"u32"},{"name":"points","type":"u32"}]}]}"#);
        let mut body = vec![0xFE, 0x04, 0, 0, 1];
        for v in [1097u32, 1097, 2] {
            body.extend(v.to_le_bytes());
        }
        body.push(2);
        for v in [1034u32, 28, 1016, 1] {
            body.extend(v.to_le_bytes());
        }
        body.extend([9, 9, 9]); // trailing data the decoder does not ask for
        assert_eq!(
            d.decode(&body).unwrap(),
            serde_json::json!({
                "levels": [{"pet": 1097, "level": 2}],
                "points": [{"pet": 1034, "points": 28}, {"pet": 1016, "points": 1}]
            })
        );
    }

    #[test]
    fn a_short_frame_decodes_to_nothing() {
        let d = spec(r#"{"name":"x","opcode":"0D 90","fields":[{"name":"a","type":"u32"}]}"#);
        assert!(d.decode(&[1, 2]).is_none());
    }

    #[test]
    fn bad_decoders_are_refused() {
        let bad = [
            r#"{"name":"x","opcode":"0D","fields":[]}"#,
            r#"{"name":"X!","opcode":"0D 90","fields":[]}"#,
            r#"{"name":"x","opcode":"0D 90","fields":[{"name":"s","type":"string"}]}"#,
            r#"{"name":"x","opcode":"0D 90","fields":[{"type":"u32"}]}"#,
            r#"{"name":"x","opcode":"0D 90","fields":[{"name":"l","type":"list","count":"u8","fields":[
                {"name":"m","type":"list","count":"u8","fields":[
                    {"name":"n","type":"list","count":"u8","fields":[]}]}]}]}"#,
        ];
        for json in bad {
            assert!(Decoder::compile(&serde_json::from_str(json).unwrap()).is_err(), "{json}");
        }
    }
}
