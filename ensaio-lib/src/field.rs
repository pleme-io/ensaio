use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Key(String),
    Index(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldPath {
    text: String,
    segments: Vec<Segment>,
}

impl FieldPath {
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        if text.is_empty() {
            return Err("path is empty".into());
        }
        let mut segments = Vec::new();
        for part in text.split('.') {
            let (key, mut rest) = part.split_at(part.find('[').unwrap_or(part.len()));
            if key.is_empty() && rest.is_empty() {
                return Err(format!("path `{text}` has an empty segment"));
            }
            if !key.is_empty() {
                segments.push(Segment::Key(key.to_string()));
            } else if segments.is_empty() {
                return Err(format!(
                    "path `{text}` starts with an index; it must start with a field"
                ));
            }
            while !rest.is_empty() {
                let inner = rest
                    .strip_prefix('[')
                    .and_then(|r| r.split_once(']'))
                    .ok_or_else(|| format!("path `{text}`: `{rest}` is not an index `[n]`"))?;
                let n = inner
                    .0
                    .parse::<usize>()
                    .map_err(|_| format!("path `{text}`: index `[{}]` is not a number", inner.0))?;
                segments.push(Segment::Index(n));
                rest = inner.1;
            }
        }
        Ok(Self {
            text: text.to_string(),
            segments,
        })
    }

    #[must_use]
    pub fn resolve<'v>(&self, root: &'v Value) -> Option<&'v Value> {
        self.segments.iter().try_fold(root, |v, s| match (s, v) {
            (Segment::Key(k), Value::Object(m)) => m.get(k),
            (Segment::Key(k), Value::Array(a)) => k.parse::<usize>().ok().and_then(|i| a.get(i)),
            (Segment::Index(i), Value::Array(a)) => a.get(*i),
            _ => None,
        })
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Scalar {
    Bool(bool),
    Number(f64),
    Text(String),
}

impl Scalar {
    fn from_json(v: &Value) -> Result<Self, String> {
        match v {
            Value::Bool(b) => Ok(Self::Bool(*b)),
            Value::Number(n) => n
                .as_f64()
                .map(Self::Number)
                .ok_or_else(|| format!("{n} is not a finite number")),
            Value::String(s) if s == "true" => Ok(Self::Bool(true)),
            Value::String(s) if s == "false" => Ok(Self::Bool(false)),
            Value::String(s) => Ok(Self::Text(s.clone())),
            Value::Null | Value::Array(_) | Value::Object(_) => {
                Err(format!("{v} is not a string, number or boolean"))
            }
        }
    }

    fn to_json(&self) -> Value {
        match self {
            Self::Bool(b) => Value::Bool(*b),
            Self::Number(n) => serde_json::Number::from_f64(*n).map_or(Value::Null, Value::Number),
            Self::Text(s) => Value::String(s.clone()),
        }
    }

    #[must_use]
    pub fn matches(&self, observed: &Value) -> bool {
        match (self, observed) {
            (Self::Bool(w), Value::Bool(o)) => w == o,
            (Self::Number(w), Value::Number(o)) => {
                o.as_f64().is_some_and(|o| (o - w).abs() < f64::EPSILON)
            }
            (Self::Text(w), Value::String(o)) => w == o,
            _ => false,
        }
    }
}

impl fmt::Display for Scalar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_json())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FieldTest {
    Equals(Scalar),
    OneOf(Vec<Scalar>),
    Range { min: Option<f64>, max: Option<f64> },
    Present,
    Absent,
}

impl fmt::Display for FieldTest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Equals(s) => write!(f, "= {s}"),
            Self::OneOf(v) => write!(
                f,
                "one of [{}]",
                v.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::Range {
                min: Some(lo),
                max: Some(hi),
            } => write!(f, "in [{lo}, {hi}]"),
            Self::Range {
                min: Some(lo),
                max: None,
            } => write!(f, ">= {lo}"),
            Self::Range {
                min: None,
                max: Some(hi),
            } => write!(f, "<= {hi}"),
            Self::Range {
                min: None,
                max: None,
            } => f.write_str("a number"),
            Self::Present => f.write_str("present"),
            Self::Absent => f.write_str("absent"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "FieldPredicateWire", into = "FieldPredicateWire")]
pub struct FieldPredicate {
    pub path: FieldPath,
    pub test: FieldTest,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldCheck {
    pub predicate: FieldPredicate,
    pub observed: Option<Value>,
    pub held: bool,
}

impl FieldCheck {
    #[must_use]
    pub fn observed_text(&self) -> String {
        self.observed
            .as_ref()
            .map_or_else(|| "absent".into(), ToString::to_string)
    }
}

impl FieldPredicate {
    #[must_use]
    pub fn check(&self, root: Option<&Value>) -> FieldCheck {
        let observed = root
            .and_then(|r| self.path.resolve(r))
            .filter(|v| !v.is_null())
            .cloned();
        let held = match (&self.test, &observed) {
            (FieldTest::Absent, o) => o.is_none(),
            (FieldTest::Present, o) => o.is_some(),
            (_, None) => false,
            (FieldTest::Equals(s), Some(o)) => s.matches(o),
            (FieldTest::OneOf(v), Some(o)) => v.iter().any(|s| s.matches(o)),
            (FieldTest::Range { min, max }, Some(o)) => o
                .as_f64()
                .is_some_and(|n| min.is_none_or(|m| n >= m) && max.is_none_or(|m| n <= m)),
        };
        FieldCheck {
            predicate: self.clone(),
            observed,
            held,
        }
    }
}

impl fmt::Display for FieldPredicate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.path, self.test)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FieldPredicateWire {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "any_scalar")]
    pub equals: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "any_scalars")]
    pub one_of: Option<Vec<Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "any_scalar")]
    pub present: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "any_scalar")]
    pub absent: Option<Value>,
}

fn any_scalar(_: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    let mut s = schemars::schema::SchemaObject::default();
    s.extensions.insert(
        "x-kubernetes-preserve-unknown-fields".to_owned(),
        Value::Bool(true),
    );
    s.into()
}

fn any_scalars(g: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
    schemars::schema::SchemaObject {
        instance_type: Some(schemars::schema::InstanceType::Array.into()),
        array: Some(Box::new(schemars::schema::ArrayValidation {
            items: Some(any_scalar(g).into()),
            ..Default::default()
        })),
        ..Default::default()
    }
    .into()
}

fn flag(path: &str, key: &str, v: &Value) -> Result<(), String> {
    match Scalar::from_json(v) {
        Ok(Scalar::Bool(true)) => Ok(()),
        _ => Err(format!(
            "predicate on `{path}`: :{key} takes true (state the other case with :{})",
            if key == "present" {
                "absent"
            } else {
                "present"
            }
        )),
    }
}

impl TryFrom<FieldPredicateWire> for FieldPredicate {
    type Error = String;

    fn try_from(w: FieldPredicateWire) -> Result<Self, String> {
        let path = FieldPath::parse(&w.path)?;
        let p = &w.path;
        let mut tests: Vec<FieldTest> = Vec::new();
        if let Some(v) = &w.equals {
            tests.push(FieldTest::Equals(
                Scalar::from_json(v).map_err(|e| format!("predicate on `{p}`: :equals {e}"))?,
            ));
        }
        if let Some(vs) = &w.one_of {
            if vs.is_empty() {
                return Err(format!(
                    "predicate on `{p}`: :one-of is empty, so nothing could match"
                ));
            }
            let set = vs
                .iter()
                .map(Scalar::from_json)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("predicate on `{p}`: :one-of {e}"))?;
            tests.push(FieldTest::OneOf(set));
        }
        if w.min.is_some() || w.max.is_some() {
            if let (Some(lo), Some(hi)) = (w.min, w.max)
                && lo > hi
            {
                return Err(format!("predicate on `{p}`: :min {lo} is above :max {hi}"));
            }
            tests.push(FieldTest::Range {
                min: w.min,
                max: w.max,
            });
        }
        if let Some(v) = &w.present {
            flag(p, "present", v)?;
            tests.push(FieldTest::Present);
        }
        if let Some(v) = &w.absent {
            flag(p, "absent", v)?;
            tests.push(FieldTest::Absent);
        }
        match tests.len() {
            1 => Ok(Self {
                path,
                test: tests.remove(0),
            }),
            0 => Err(format!(
                "predicate on `{p}` tests nothing; give one of :equals, :one-of, :min/:max, :present, :absent"
            )),
            _ => Err(format!(
                "predicate on `{p}` gives {} tests; one predicate is one test (write two predicates)",
                tests.len()
            )),
        }
    }
}

impl From<FieldPredicate> for FieldPredicateWire {
    fn from(p: FieldPredicate) -> Self {
        let mut w = Self {
            path: p.path.text,
            ..Self::default()
        };
        match p.test {
            FieldTest::Equals(s) => w.equals = Some(s.to_json()),
            FieldTest::OneOf(v) => w.one_of = Some(v.iter().map(Scalar::to_json).collect()),
            FieldTest::Range { min, max } => {
                w.min = min;
                w.max = max;
            }
            FieldTest::Present => w.present = Some(Value::Bool(true)),
            FieldTest::Absent => w.absent = Some(Value::Bool(true)),
        }
        w
    }
}

impl JsonSchema for FieldPredicate {
    fn schema_name() -> String {
        "FieldPredicate".to_owned()
    }

    fn json_schema(generator: &mut schemars::r#gen::SchemaGenerator) -> schemars::schema::Schema {
        FieldPredicateWire::json_schema(generator)
    }
}

pub(crate) fn one_or_many_strings<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<String>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        Many(Vec<String>),
        One(String),
    }
    Ok(match Option::<OneOrMany>::deserialize(d)? {
        None => None,
        Some(OneOrMany::Many(v)) => Some(v),
        Some(OneOrMany::One(s)) => Some(vec![s]),
    })
}

pub(crate) fn one_or_many<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<Option<Vec<FieldPredicate>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        Many(Vec<FieldPredicateWire>),
        One(FieldPredicateWire),
    }
    let wires = match Option::<OneOrMany>::deserialize(d)? {
        None => return Ok(None),
        Some(OneOrMany::Many(v)) => v,
        Some(OneOrMany::One(w)) => vec![w],
    };
    wires
        .into_iter()
        .map(FieldPredicate::try_from)
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
        .map_err(serde::de::Error::custom)
}
