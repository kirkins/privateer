//! Human-authored command arguments. Machine protocols continue to use JSON.
use anyhow::{Context, Result, bail, ensure};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use std::fmt;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

mod yaml;

const MAX_BYTES: usize = 65_536;
const MAX_NODES: usize = 10_000;
const MAX_DEPTH: usize = 64;
const MAX_SCALAR_BYTES: usize = 1_048_576;

pub(crate) fn read_file(path: &Path) -> Result<Value> {
    let yaml = match path.extension().and_then(|ext| ext.to_str()) {
        Some("yaml" | "yml") => true,
        Some("json") => false,
        _ => bail!("--file requires a .yaml, .yml or .json extension"),
    };
    // Nonblocking open lets us reject a pipe/device without waiting for its
    // writer. The limit applies to actual bytes read, even if the file grows.
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .context("cannot open argument file")?;
    ensure!(
        file.metadata()?.is_file(),
        "argument file must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_BYTES, "argument file exceeds 64 KiB");
    let mut budget = Budget::new();
    let value = if yaml {
        yaml::parse(&bytes, &mut budget)
            .map_err(|error| anyhow::anyhow!("invalid YAML argument file: {error}"))?
    } else {
        let mut parser = serde_json::Deserializer::from_slice(&bytes);
        let value = Node {
            budget: &mut budget,
            depth: 0,
        }
        .deserialize(&mut parser)
        .map_err(|error| anyhow::anyhow!("invalid JSON argument file: {error}"))?;
        parser.end()?;
        value
    };
    ensure!(value.is_object(), "argument file must contain one object");
    // YAML block strings can need more bytes when JSON-escaped. Bound the
    // normalized arguments too, before opening a home or dispatching work.
    ensure!(
        value.to_string().len() <= MAX_BYTES,
        "normalized arguments exceed 64 KiB"
    );
    Ok(value)
}

struct Node<'a> {
    budget: &'a mut Budget,
    depth: usize,
}

struct Budget {
    nodes: usize,
    scalar_bytes: usize,
}

impl Budget {
    fn new() -> Self {
        Self {
            nodes: MAX_NODES,
            scalar_bytes: MAX_SCALAR_BYTES,
        }
    }

    fn node(&mut self, depth: usize) -> Result<()> {
        ensure!(depth < MAX_DEPTH, "argument nesting exceeds 64 levels");
        self.nodes = self
            .nodes
            .checked_sub(1)
            .context("arguments exceed 10000 nodes")?;
        Ok(())
    }

    fn scalar(&mut self, bytes: usize) -> Result<()> {
        self.scalar_bytes = self
            .scalar_bytes
            .checked_sub(bytes)
            .context("expanded scalar/key data exceeds 1 MiB")?;
        Ok(())
    }
}

impl Node<'_> {
    fn charge<E: de::Error>(&mut self) -> std::result::Result<(), E> {
        self.budget.node(self.depth).map_err(E::custom)
    }
}

impl<'de> DeserializeSeed<'de> for Node<'_> {
    type Value = Value;

    fn deserialize<D: de::Deserializer<'de>>(
        mut self,
        parser: D,
    ) -> std::result::Result<Value, D::Error> {
        self.charge()?;
        parser.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Node<'_> {
    type Value = Value;

    fn expecting(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("JSON-compatible data without custom constructors")
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> std::result::Result<Value, E> {
        self.budget.scalar(1).map_err(E::custom)?;
        Ok(Value::Bool(value))
    }
    fn visit_i64<E: de::Error>(self, value: i64) -> std::result::Result<Value, E> {
        self.budget.scalar(8).map_err(E::custom)?;
        Ok(Value::Number(value.into()))
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> std::result::Result<Value, E> {
        self.budget.scalar(8).map_err(E::custom)?;
        Ok(Value::Number(value.into()))
    }
    fn visit_f64<E: de::Error>(self, value: f64) -> std::result::Result<Value, E> {
        self.budget.scalar(8).map_err(E::custom)?;
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| E::custom("number must be finite and representable in JSON"))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Value, E> {
        self.budget.scalar(value.len()).map_err(E::custom)?;
        Ok(Value::String(value.to_owned()))
    }
    fn visit_unit<E: de::Error>(self) -> std::result::Result<Value, E> {
        self.budget.scalar(1).map_err(E::custom)?;
        Ok(Value::Null)
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(value) = seq.next_element_seed(Node {
            budget: self.budget,
            depth: self.depth + 1,
        })? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        let mut values = Map::new();
        while let Some(key) = map.next_key_seed(Key(Node {
            budget: self.budget,
            depth: self.depth + 1,
        }))? {
            if values.contains_key(&key) {
                return Err(de::Error::custom("duplicate object key"));
            }
            let value = map.next_value_seed(Node {
                budget: self.budget,
                depth: self.depth + 1,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}

struct Key<'a>(Node<'a>);

impl<'de> DeserializeSeed<'de> for Key<'_> {
    type Value = String;

    fn deserialize<D: de::Deserializer<'de>>(
        mut self,
        parser: D,
    ) -> std::result::Result<String, D::Error> {
        self.0.charge()?;
        parser.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Key<'_> {
    type Value = String;

    fn expecting(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("a string object key")
    }
    fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<String, E> {
        self.0.budget.scalar(value.len()).map_err(E::custom)?;
        Ok(value.to_owned())
    }
}
