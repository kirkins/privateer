use super::Budget;
use anyhow::{Context, Result, bail, ensure};
use saphyr_parser::{Event, Parser, ScalarStyle, StrInput, Tag};
use serde_json::{Map, Number, Value};
use std::collections::BTreeMap;
use std::rc::Rc;

enum Node {
    Scalar(Value),
    Sequence(Vec<Rc<Node>>),
    Mapping(Vec<(Rc<Node>, Rc<Node>)>),
}

struct Builder<'a> {
    parser: Parser<'a, StrInput<'a>>,
    anchors: BTreeMap<usize, Rc<Node>>,
    budget: Budget,
}

pub(super) fn parse(bytes: &[u8], budget: &mut Budget) -> Result<Value> {
    let mut builder = Builder {
        parser: Parser::new_from_str(std::str::from_utf8(bytes)?),
        anchors: BTreeMap::new(),
        budget: Budget::new(),
    };
    ensure!(
        builder.next()? == Event::StreamStart,
        "expected YAML stream"
    );
    let start = builder.next()?;
    ensure!(
        matches!(start, Event::DocumentStart(_)),
        "argument file must contain one object"
    );
    let first = builder.next()?;
    let root = builder.node(first, 0)?;
    ensure!(
        builder.next()? == Event::DocumentEnd,
        "expected document end"
    );
    ensure!(
        builder.next()? == Event::StreamEnd,
        "more than one document"
    );
    root.expand(budget, 0)
}

impl<'a> Builder<'a> {
    fn next(&mut self) -> Result<Event<'a>> {
        Ok(self
            .parser
            .next()
            .context("unexpected end of YAML stream")??
            .0)
    }

    fn node(&mut self, event: Event<'a>, depth: usize) -> Result<Rc<Node>> {
        self.budget.node(depth)?;
        let (anchor, node) = match event {
            Event::Alias(id) => {
                return self
                    .anchors
                    .get(&id)
                    .cloned()
                    .context("recursive alias nesting is not supported");
            }
            Event::Scalar(text, style, anchor, tag) => {
                self.budget.scalar(text.len())?;
                (anchor, Node::Scalar(scalar(&text, style, tag.as_deref())?))
            }
            Event::SequenceStart(anchor, tag) => {
                let kind = tag_kind(tag.as_deref())?;
                ensure!(
                    kind.is_none_or(|kind| kind == "seq"),
                    "invalid sequence tag"
                );
                let mut values = Vec::new();
                loop {
                    let event = self.next()?;
                    if event == Event::SequenceEnd {
                        break;
                    }
                    values.push(self.node(event, depth + 1)?);
                }
                (anchor, Node::Sequence(values))
            }
            Event::MappingStart(anchor, tag) => {
                let kind = tag_kind(tag.as_deref())?;
                ensure!(kind.is_none_or(|kind| kind == "map"), "invalid mapping tag");
                let mut values = Vec::new();
                loop {
                    let event = self.next()?;
                    if event == Event::MappingEnd {
                        break;
                    }
                    let key = self.node(event, depth + 1)?;
                    let event = self.next()?;
                    let value = self.node(event, depth + 1)?;
                    values.push((key, value));
                }
                (anchor, Node::Mapping(values))
            }
            _ => bail!("expected YAML value"),
        };
        let node = Rc::new(node);
        if anchor != 0 {
            self.anchors.insert(anchor, node.clone());
        }
        Ok(node)
    }
}

impl Node {
    fn expand(&self, budget: &mut Budget, depth: usize) -> Result<Value> {
        budget.node(depth)?;
        match self {
            Self::Scalar(value) => {
                budget.scalar(match value {
                    Value::String(text) => text.len(),
                    Value::Null | Value::Bool(_) => 1,
                    _ => 8,
                })?;
                Ok(value.clone())
            }
            Self::Sequence(values) => values
                .iter()
                .map(|value| value.expand(budget, depth + 1))
                .collect::<Result<Vec<_>>>()
                .map(Value::Array),
            Self::Mapping(values) => {
                let mut object = Map::new();
                for (key, value) in values {
                    let Value::String(key) = key.expand(budget, depth + 1)? else {
                        bail!("expected a string object key");
                    };
                    ensure!(!object.contains_key(&key), "duplicate object key");
                    object.insert(key, value.expand(budget, depth + 1)?);
                }
                Ok(Value::Object(object))
            }
        }
    }
}

fn tag_kind(tag: Option<&Tag>) -> Result<Option<&str>> {
    let Some(tag) = tag else {
        return Ok(None);
    };
    ensure!(
        !tag.handle.starts_with('!') && !(tag.handle.is_empty() && tag.suffix.starts_with('!')),
        "custom constructors are not supported"
    );
    let kind = if tag.is_yaml_core_schema() {
        Some(tag.suffix.as_str())
    } else if tag.handle.is_empty() {
        tag.suffix.strip_prefix("tag:yaml.org,2002:")
    } else {
        None
    };
    Ok(kind.filter(|kind| {
        matches!(
            *kind,
            "str" | "bool" | "null" | "int" | "float" | "seq" | "map"
        )
    }))
}

fn scalar(text: &str, style: ScalarStyle, tag: Option<&Tag>) -> Result<Value> {
    let kind = tag_kind(tag)?;
    if kind == Some("str") || (kind.is_none() && (tag.is_some() || style != ScalarStyle::Plain)) {
        return Ok(Value::String(text.to_owned()));
    }
    let null = matches!(text, "" | "~" | "null" | "Null" | "NULL");
    let boolean = match text {
        "true" | "True" | "TRUE" => Some(true),
        "false" | "False" | "FALSE" => Some(false),
        _ => None,
    };
    match kind {
        Some("null") => {
            ensure!(null, "invalid null");
            Ok(Value::Null)
        }
        Some("bool") => Ok(Value::Bool(boolean.context("invalid boolean")?)),
        Some("int") => integer(text)?.context("invalid integer"),
        Some("float") => float(text)?.context("invalid float"),
        Some(_) => bail!("invalid scalar tag"),
        None if null => Ok(Value::Null),
        None => {
            if let Some(boolean) = boolean {
                return Ok(Value::Bool(boolean));
            }
            if let Some(integer) = integer(text)? {
                return Ok(integer);
            }
            Ok(float(text)?.unwrap_or_else(|| Value::String(text.to_owned())))
        }
    }
}

fn integer(text: &str) -> Result<Option<Value>> {
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let (digits, radix) = if let Some(digits) = unsigned.strip_prefix("0x") {
        (digits, 16)
    } else if let Some(digits) = unsigned.strip_prefix("0o") {
        (digits, 8)
    } else if let Some(digits) = unsigned.strip_prefix("0b") {
        (digits, 2)
    } else {
        (unsigned, 10)
    };
    if digits.is_empty() || !digits.chars().all(|ch| ch.is_digit(radix)) {
        return Ok(None);
    }
    let number = if text.starts_with('-') {
        i64::from_str_radix(&format!("-{digits}"), radix).map(Number::from)
    } else {
        u64::from_str_radix(digits, radix).map(Number::from)
    }
    .context("integer is outside the JSON number range")?;
    Ok(Some(Value::Number(number)))
}

fn float(text: &str) -> Result<Option<Value>> {
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    ensure!(
        !matches!(
            unsigned,
            ".inf" | ".Inf" | ".INF" | ".nan" | ".NaN" | ".NAN"
        ),
        "number must be finite and representable in JSON"
    );
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|ch| ch.is_ascii_digit());
    let mantissa = if let Some((mantissa, exponent)) = unsigned.split_once(['e', 'E']) {
        if !digits(exponent.strip_prefix(['+', '-']).unwrap_or(exponent)) {
            return Ok(None);
        }
        mantissa
    } else {
        unsigned
    };
    let numeric = if let Some((whole, fraction)) = mantissa.split_once('.') {
        (whole.is_empty() || digits(whole))
            && (fraction.is_empty() || digits(fraction))
            && !(whole.is_empty() && fraction.is_empty())
    } else {
        digits(mantissa)
    };
    if !numeric {
        return Ok(None);
    }
    let number = text
        .parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .context("number must be finite and representable in JSON")?;
    Ok(Some(Value::Number(number)))
}
