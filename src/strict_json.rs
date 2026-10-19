//! JSON objects reject duplicate keys at every nesting level.
//! serde_json::Value last-wins; this visitor sees each key as the parser yields it.

use std::fmt;

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StrictErr {
    Duplicate,
    Json,
}

pub fn from_slice(bytes: &[u8]) -> Result<Value, StrictErr> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    match DenyDup.deserialize(&mut de) {
        Ok(v) => match de.end() {
            Ok(()) => Ok(v),
            Err(_) => Err(StrictErr::Json),
        },
        Err(e) => {
            if e.to_string().contains("duplicate_field") {
                Err(StrictErr::Duplicate)
            } else {
                Err(StrictErr::Json)
            }
        }
    }
}

struct DenyDup;

impl<'de> DeserializeSeed<'de> for DenyDup {
    type Value = Value;
    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(DenyDupVisitor)
    }
}

struct DenyDupVisitor;

impl<'de> Visitor<'de> for DenyDupVisitor {
    type Value = Value;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "any JSON value")
    }
    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Ok(v.into())
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }
    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }
    fn visit_none<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_some<D: Deserializer<'de>>(self, d: D) -> Result<Value, D::Error> {
        DenyDup.deserialize(d)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut out = Vec::new();
        while let Some(v) = seq.next_element_seed(DenyDup)? {
            out.push(v);
        }
        Ok(Value::Array(out))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut obj = serde_json::Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if obj.contains_key(&key) {
                return Err(de::Error::custom("duplicate_field"));
            }
            let v = map.next_value_seed(DenyDup)?;
            obj.insert(key, v);
        }
        Ok(Value::Object(obj))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_key_same_or_conflict_is_error() {
        assert_eq!(
            from_slice(br#"{"a":1,"a":1}"#),
            Err(StrictErr::Duplicate)
        );
        assert_eq!(
            from_slice(br#"{"a":1,"a":2}"#),
            Err(StrictErr::Duplicate)
        );
        assert_eq!(
            from_slice(br#"{"body":"x","public":true,"body":"x"}"#),
            Err(StrictErr::Duplicate)
        );
        assert_eq!(
            from_slice(br#"{"public":true,"body":"x","public":true}"#),
            Err(StrictErr::Duplicate)
        );
    }

    #[test]
    fn unique_keys_ok() {
        let v = from_slice(br#"{"body":"x","public":true}"#).unwrap();
        assert_eq!(v["body"], "x");
        assert_eq!(v["public"], true);
    }
}
