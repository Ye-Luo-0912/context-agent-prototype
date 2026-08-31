//! Bounded JSON-schema subset validation for tool arguments.
//!
//! The model may only invoke tools whose arguments match the immutable round
//! surface the model actually saw. This module compiles each `input_schema`
//! once per surface revision into a `SchemaProfile` and validates arguments
//! against it before approval or dispatch. Unsupported keywords fail
//! composition/admission instead of being silently ignored; a mismatch is a
//! typed no-dispatch result. Effect authority stays `HostToolPolicy`; the
//! schema only gates well-formedness.
//!
//! Supported subset (the repository's actual surface): `type`
//! (object/string/integer/number/boolean/array/null), `properties`,
//! `required`, `items`, `minItems`/`maxItems`, `minimum`/`maximum`,
//! `minLength`/`maxLength`, `enum` (primitive options), and
//! `additionalProperties` (boolean). `description` is an annotation and is
//! ignored. Every other keyword fails compilation.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// Maximum nesting depth of a compiled schema.
pub const MAX_SCHEMA_DEPTH: usize = 24;
/// Maximum number of profile nodes in one compiled schema.
pub const MAX_SCHEMA_NODES: usize = 4096;
/// Maximum nesting depth of accepted arguments.
pub const MAX_ARGUMENT_DEPTH: usize = 32;
/// Maximum number of JSON nodes in accepted arguments.
pub const MAX_ARGUMENT_NODES: usize = 8192;
/// Maximum serialized byte estimate for accepted arguments.
pub const MAX_ARGUMENT_BYTES: usize = 256 * 1024;

/// One immutable compiled tool-argument pattern. `compile` is fallible:
/// unsupported keywords, structural misuse and unbounded schemas refuse to
/// build rather than degrade the gate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SchemaProfile {
    root: BoundedNode,
}

/// A typed schema mismatch: where the value sits, what the schema required,
/// and what the value actually was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaViolation {
    /// JSON pointer into the argument value, e.g. `/tasks/2/limit`.
    pub pointer: String,
    /// Bounded description of the expected shape.
    pub expected: String,
    /// Bounded description of the observed shape.
    pub actual: String,
}

impl std::fmt::Display for SchemaViolation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "at {}: expected {}, found {}",
            self.pointer, self.expected, self.actual
        )
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
enum BoundedNode {
    Any,
    Null,
    Bool,
    Integer {
        minimum: Option<i64>,
        maximum: Option<i64>,
        enum_options: Option<Vec<Value>>,
    },
    Number {
        minimum: Option<f64>,
        maximum: Option<f64>,
        enum_options: Option<Vec<Value>>,
    },
    String {
        min_length: Option<usize>,
        max_length: Option<usize>,
        enum_options: Option<Vec<Value>>,
        /// Anchored match constraint, serialized verbatim. Compiled once at
        /// compile time for validity; validated per call against the value.
        pattern: Option<String>,
    },
    Enum {
        options: Vec<Value>,
    },
    Array {
        items: Box<BoundedNode>,
        min_items: usize,
        max_items: Option<usize>,
    },
    Object {
        properties: BTreeMap<String, BoundedNode>,
        required: BTreeSet<String>,
        allow_additional: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeType {
    Any,
    Null,
    Bool,
    Integer,
    Number,
    String,
    Array,
    Object,
}

impl SchemaProfile {
    /// Compile a tool `input_schema` into a bounded profile. The schema must
    /// be an object with `properties`; anything else, any unsupported
    /// keyword, or any structure exceeding the compile bounds fails closed.
    pub fn compile(schema: &Value) -> Result<Self, String> {
        let mut budget = BuildBudget::default();
        let root = BoundedNode::compile(schema, 0, &mut budget)?;
        // Every tool schema is an object whose keys are the argument names.
        let NodeType::Object = root.node_type() else {
            return Err("tool input_schema must be an object".into());
        };
        Ok(Self { root })
    }

    /// Validate one argument value against the profile. Returns the first
    /// mismatch; depth/node/size overflow is itself a mismatch.
    pub fn validate(&self, arguments: &Value) -> Result<(), SchemaViolation> {
        let mut budget = VerifyBudget::default();
        self.root.validate(arguments, "", &mut budget)
    }
}

#[derive(Default)]
struct BuildBudget {
    nodes: usize,
}

impl BuildBudget {
    fn charge(&mut self) -> Result<(), String> {
        self.nodes += 1;
        if self.nodes > MAX_SCHEMA_NODES {
            return Err(format!(
                "tool input_schema exceeds the {MAX_SCHEMA_NODES}-node compile bound"
            ));
        }
        Ok(())
    }
}

fn primitive_type(value: &Value) -> Result<NodeType, String> {
    match value.as_str() {
        Some("null") => Ok(NodeType::Null),
        Some("boolean") => Ok(NodeType::Bool),
        Some("integer") => Ok(NodeType::Integer),
        Some("number") => Ok(NodeType::Number),
        Some("string") => Ok(NodeType::String),
        Some("array") => Ok(NodeType::Array),
        Some("object") => Ok(NodeType::Object),
        Some(other) => Err(format!("unsupported schema type '{other}'")),
        None => Err("schema 'type' must be a string".into()),
    }
}

impl BoundedNode {
    fn compile(schema: &Value, depth: usize, budget: &mut BuildBudget) -> Result<Self, String> {
        if depth > MAX_SCHEMA_DEPTH {
            return Err(format!(
                "tool input_schema exceeds the {MAX_SCHEMA_DEPTH}-level compile depth bound"
            ));
        }
        let object = schema
            .as_object()
            .ok_or_else(|| "tool input_schema nodes must be objects".to_string())?;
        budget.charge()?;

        let mut declared: Option<NodeType> = None;
        let mut properties: BTreeMap<String, BoundedNode> = BTreeMap::new();
        let mut required: BTreeSet<String> = BTreeSet::new();
        let mut items: Option<BoundedNode> = None;
        let mut min_items: Option<usize> = None;
        let mut max_items: Option<usize> = None;
        let mut minimum: Option<f64> = None;
        let mut maximum: Option<f64> = None;
        let mut min_length: Option<usize> = None;
        let mut max_length: Option<usize> = None;
        let mut enum_options: Option<Vec<Value>> = None;
        let mut allow_additional: Option<bool> = None;
        let mut pattern: Option<String> = None;
        let mut saw_container_keyword = false;

        for (key, value) in object {
            match key.as_str() {
                "type" => declared = Some(primitive_type(value)?),
                "properties" => {
                    saw_container_keyword = true;
                    let map = value
                        .as_object()
                        .ok_or_else(|| "schema 'properties' must be an object".to_string())?;
                    for (name, nested) in map {
                        properties.insert(
                            name.clone(),
                            BoundedNode::compile(nested, depth + 1, budget)?,
                        );
                    }
                }
                "required" => {
                    saw_container_keyword = true;
                    let list = value
                        .as_array()
                        .ok_or_else(|| "schema 'required' must be an array".to_string())?;
                    for entry in list {
                        let name = entry.as_str().ok_or_else(|| {
                            "schema 'required' entries must be strings".to_string()
                        })?;
                        // `required` may name keys that `properties` leaves
                        // unconstrained (standard JSON Schema): the check is
                        // presence, not shape.
                        required.insert(name.to_string());
                    }
                }
                "items" => {
                    saw_container_keyword = true;
                    items = Some(BoundedNode::compile(value, depth + 1, budget)?);
                }
                "minItems" => {
                    saw_container_keyword = true;
                    min_items = Some(usize_from(value, "minItems")?);
                }
                "maxItems" => {
                    saw_container_keyword = true;
                    max_items = Some(usize_from(value, "maxItems")?);
                }
                "minimum" => minimum = Some(number_from(value, "minimum")?),
                "maximum" => maximum = Some(number_from(value, "maximum")?),
                "minLength" => min_length = Some(usize_from(value, "minLength")?),
                "maxLength" => max_length = Some(usize_from(value, "maxLength")?),
                "enum" => {
                    let options = value
                        .as_array()
                        .ok_or_else(|| "schema 'enum' must be an array".to_string())?;
                    let mut bounded: Vec<Value> = Vec::with_capacity(options.len());
                    for option in options {
                        if !is_primitive(option) {
                            return Err("schema 'enum' options must be primitive values".into());
                        }
                        if bounded.contains(option) {
                            return Err("schema 'enum' options must be unique".into());
                        }
                        bounded.push(option.clone());
                    }
                    enum_options = Some(bounded);
                }
                "pattern" => {
                    let text = value
                        .as_str()
                        .ok_or_else(|| "schema 'pattern' must be a string".to_string())?;
                    // Validate the regular expression once at compile time;
                    // an invalid pattern fails the tool's capability
                    // admission instead of failing shape checks mid-flight.
                    regex::Regex::new(text)
                        .map_err(|error| format!("schema 'pattern' is invalid: {error}"))?;
                    pattern = Some(text.to_string());
                }
                "additionalProperties" => {
                    let flag = value.as_bool().ok_or_else(|| {
                        "schema 'additionalProperties' must be a boolean".to_string()
                    })?;
                    allow_additional = Some(flag);
                }
                // Annotation; the model-facing compactor strips these but a
                // producer may still include them at this boundary.
                "description" => {}
                other => {
                    return Err(format!(
                        "unsupported JSON-schema keyword '{other}' in tool input_schema"
                    ));
                }
            }
        }

        if let (Some(min), Some(max)) = (min_items, max_items)
            && min > max
        {
            return Err("schema minItems exceeds maxItems".into());
        }
        if let (Some(min), Some(max)) = (min_length, max_length)
            && min > max
        {
            return Err("schema minLength exceeds maxLength".into());
        }
        if let (Some(min), Some(max)) = (minimum, maximum)
            && min > max
        {
            return Err("schema minimum exceeds maximum".into());
        }
        if enum_options
            .as_ref()
            .is_some_and(|_| declared.is_some_and(|ty| ty.is_container()))
        {
            return Err("schema 'enum' cannot constrain a container type".into());
        }
        if enum_options.as_ref().is_some_and(|options| {
            declared.is_some_and(|ty| options.iter().any(|option| !ty.matches_value(option)))
        }) {
            return Err("schema enum options must match the declared type".into());
        }
        if declared.is_some_and(|ty| !ty.is_container()) && saw_container_keyword {
            return Err("primitive schema carries container-only keywords".into());
        }
        if pattern.is_some() && !matches!(declared, None | Some(NodeType::String)) {
            return Err("schema 'pattern' applies only to strings".into());
        }

        let node = match declared.unwrap_or(NodeType::Any) {
            NodeType::Array => {
                let items = items.ok_or_else(|| "array schema requires 'items'".to_string())?;
                BoundedNode::Array {
                    items: Box::new(items),
                    min_items: min_items.unwrap_or(0),
                    max_items,
                }
            }
            NodeType::Object => BoundedNode::Object {
                properties,
                required,
                allow_additional: allow_additional.unwrap_or(true),
            },
            NodeType::Integer => {
                let minimum = minimum
                    .map(|value| {
                        to_i64(value).ok_or_else(|| {
                            "integer schema 'minimum' must be an integer".to_string()
                        })
                    })
                    .transpose()?;
                let maximum = maximum
                    .map(|value| {
                        to_i64(value).ok_or_else(|| {
                            "integer schema 'maximum' must be an integer".to_string()
                        })
                    })
                    .transpose()?;
                if let (Some(min), Some(max)) = (minimum, maximum)
                    && min > max
                {
                    return Err("schema minimum exceeds maximum".into());
                }
                BoundedNode::Integer {
                    minimum,
                    maximum,
                    enum_options,
                }
            }
            NodeType::Number => BoundedNode::Number {
                minimum,
                maximum,
                enum_options,
            },
            NodeType::String => BoundedNode::String {
                min_length,
                max_length,
                enum_options,
                pattern,
            },
            NodeType::Null => BoundedNode::Null,
            NodeType::Bool => BoundedNode::Bool,
            NodeType::Any => match enum_options {
                Some(options) => BoundedNode::Enum { options },
                None => BoundedNode::Any,
            },
        };
        Ok(node)
    }

    fn node_type(&self) -> NodeType {
        match self {
            BoundedNode::Any => NodeType::Any,
            BoundedNode::Null => NodeType::Null,
            BoundedNode::Bool => NodeType::Bool,
            BoundedNode::Integer { .. } => NodeType::Integer,
            BoundedNode::Number { .. } => NodeType::Number,
            BoundedNode::String { .. } => NodeType::String,
            BoundedNode::Enum { .. } => NodeType::Any,
            BoundedNode::Array { .. } => NodeType::Array,
            BoundedNode::Object { .. } => NodeType::Object,
        }
    }

    fn validate(
        &self,
        value: &Value,
        pointer: &str,
        budget: &mut VerifyBudget,
    ) -> Result<(), SchemaViolation> {
        budget.charge(pointer)?;
        match self {
            BoundedNode::Any => Ok(()),
            BoundedNode::Null => expect(value, pointer, NodeType::Null, budget),
            BoundedNode::Bool => expect(value, pointer, NodeType::Bool, budget),
            BoundedNode::Integer {
                minimum,
                maximum,
                enum_options,
            } => {
                let Some(number) = value.as_i64() else {
                    return Err(violation(pointer, "integer", value));
                };
                if let Some(min) = minimum
                    && number < *min
                {
                    return Err(violation_named(
                        pointer,
                        &format!("integer >= {min}"),
                        value,
                    ));
                }
                if let Some(max) = maximum
                    && number > *max
                {
                    return Err(violation_named(
                        pointer,
                        &format!("integer <= {max}"),
                        value,
                    ));
                }
                check_enum(enum_options.as_deref(), value, pointer)?;
                budget.node_size(value, pointer)?;
                Ok(())
            }
            BoundedNode::Number {
                minimum,
                maximum,
                enum_options,
            } => {
                let Some(number) = value.as_f64() else {
                    return Err(violation(pointer, "number", value));
                };
                if let Some(min) = minimum
                    && number < *min
                {
                    return Err(violation_named(pointer, &format!("number >= {min}"), value));
                }
                if let Some(max) = maximum
                    && number > *max
                {
                    return Err(violation_named(pointer, &format!("number <= {max}"), value));
                }
                check_enum(enum_options.as_deref(), value, pointer)?;
                budget.node_size(value, pointer)?;
                Ok(())
            }
            BoundedNode::String {
                min_length,
                max_length,
                enum_options,
                pattern,
            } => {
                let Some(text) = value.as_str() else {
                    return Err(violation(pointer, "string", value));
                };
                let length = text.chars().count();
                if let Some(min) = min_length
                    && length < *min
                {
                    return Err(violation_named(
                        pointer,
                        &format!("string of at least {min} characters"),
                        value,
                    ));
                }
                if let Some(max) = max_length
                    && length > *max
                {
                    return Err(violation_named(
                        pointer,
                        &format!("string of at most {max} characters"),
                        value,
                    ));
                }
                if let Some(pattern) = pattern {
                    // The compile step already proved this regular
                    // expression is valid, so this cannot fail here.
                    let matcher = regex::Regex::new(pattern)
                        .expect("schema pattern was validated at compile time");
                    if !matcher.is_match(text) {
                        return Err(violation_named(
                            pointer,
                            &format!("string matching \"/{pattern}/\""),
                            value,
                        ));
                    }
                }
                check_enum(enum_options.as_deref(), value, pointer)?;
                budget.node_size(value, pointer)?;
                Ok(())
            }
            BoundedNode::Enum { options } => {
                if !options.iter().any(|option| option == value) {
                    return Err(violation_named(
                        pointer,
                        &format!("one of [{}]", bounded_options(options)),
                        value,
                    ));
                }
                budget.node_size(value, pointer)?;
                Ok(())
            }
            BoundedNode::Array {
                items,
                min_items,
                max_items,
            } => {
                let Some(array) = value.as_array() else {
                    return Err(violation(pointer, "array", value));
                };
                budget.enter(pointer)?;
                budget.elements(array.len(), pointer)?;
                if array.len() < *min_items {
                    return Err(violation_named(
                        pointer,
                        &format!("array of at least {min_items} items"),
                        value,
                    ));
                }
                if let Some(max) = max_items
                    && array.len() > *max
                {
                    return Err(violation_named(
                        pointer,
                        &format!("array of at most {max} items"),
                        value,
                    ));
                }
                for (index, element) in array.iter().enumerate() {
                    let child = pointer.join_path(&index.to_string());
                    items.validate(element, &child, budget)?;
                }
                budget.leave();
                Ok(())
            }
            BoundedNode::Object {
                properties,
                required,
                allow_additional,
            } => {
                let Some(map) = value.as_object() else {
                    return Err(violation(pointer, "object", value));
                };
                budget.enter(pointer)?;
                budget.elements(map.len(), pointer)?;
                for name in required {
                    if !map.contains_key(name) {
                        return Err(violation_named(
                            pointer,
                            &format!("object with required field '{name}'"),
                            value,
                        ));
                    }
                }
                for (name, field) in map {
                    match properties.get(name) {
                        Some(node) => {
                            let child_pointer = pointer.join_path(name);
                            node.validate(field, &child_pointer, budget)?;
                        }
                        None if !*allow_additional => {
                            return Err(violation_named(
                                pointer,
                                "object without additional properties",
                                value,
                            ));
                        }
                        None => budget.scan_value(field, pointer)?,
                    }
                }
                budget.leave();
                Ok(())
            }
        }
    }
}

fn check_enum(
    options: Option<&[Value]>,
    value: &Value,
    pointer: &str,
) -> Result<(), SchemaViolation> {
    let Some(options) = options else {
        return Ok(());
    };
    if !options.iter().any(|option| option == value) {
        return Err(violation_named(
            pointer,
            &format!("one of [{}]", bounded_options(options)),
            value,
        ));
    }
    Ok(())
}

fn expect(
    value: &Value,
    pointer: &str,
    ty: NodeType,
    budget: &mut VerifyBudget,
) -> Result<(), SchemaViolation> {
    if ty.matches_value(value) {
        budget.node_size(value, pointer)?;
        Ok(())
    } else {
        Err(violation_named(pointer, type_name(ty), value))
    }
}

fn violation(pointer: &str, expected: &str, value: &Value) -> SchemaViolation {
    SchemaViolation {
        pointer: pointer.to_string(),
        expected: expected.to_string(),
        actual: bounded_value_shape(value),
    }
}

fn violation_named(pointer: &str, expected: &str, value: &Value) -> SchemaViolation {
    SchemaViolation {
        pointer: pointer.to_string(),
        expected: expected.to_string(),
        actual: bounded_value_shape(value),
    }
}

fn type_name(ty: NodeType) -> &'static str {
    match ty {
        NodeType::Any => "any value",
        NodeType::Null => "null",
        NodeType::Bool => "boolean",
        NodeType::Integer => "integer",
        NodeType::Number => "number",
        NodeType::String => "string",
        NodeType::Array => "array",
        NodeType::Object => "object",
    }
}

impl NodeType {
    fn is_container(self) -> bool {
        matches!(self, NodeType::Array | NodeType::Object)
    }

    fn matches_value(self, value: &Value) -> bool {
        match self {
            NodeType::Any => true,
            NodeType::Null => value.is_null(),
            NodeType::Bool => value.is_boolean(),
            NodeType::Integer => value.is_i64() || value.is_u64(),
            NodeType::Number => value.is_number(),
            NodeType::String => value.is_string(),
            NodeType::Array => value.is_array(),
            NodeType::Object => value.is_object(),
        }
    }
}

fn is_primitive(value: &Value) -> bool {
    value.is_null() || value.is_boolean() || value.is_number() || value.is_string()
}

fn bounded_value_shape(value: &Value) -> String {
    if let Some(items) = value.as_array() {
        format!("array of {} items", items.len())
    } else if let Some(map) = value.as_object() {
        format!("object with {} fields", map.len())
    } else if let Some(text) = value.as_str() {
        let length = text.chars().count();
        if length > 48 {
            let preview: String = text.chars().take(48).collect();
            format!("string \"{preview}...\" ({length} chars)")
        } else {
            format!("string \"{text}\"")
        }
    } else {
        match value {
            Value::Null => "null".into(),
            Value::Bool(flag) => format!("boolean {flag}"),
            Value::Number(number) => format!("number {number}"),
            _ => "value".into(),
        }
    }
}

fn bounded_options(options: &[Value]) -> String {
    let preview: Vec<String> = options.iter().take(6).map(bounded_value_shape).collect();
    if options.len() > 6 {
        preview.join(", ") + ", ..."
    } else {
        preview.join(", ")
    }
}

fn number_from(value: &Value, keyword: &str) -> Result<f64, String> {
    value
        .as_f64()
        .ok_or_else(|| format!("schema '{keyword}' must be a number"))
}

fn usize_from(value: &Value, keyword: &str) -> Result<usize, String> {
    let number = value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .ok_or_else(|| format!("schema '{keyword}' must be a non-negative integer"))?;
    usize::try_from(number).map_err(|_| format!("schema '{keyword}' exceeds usize"))
}

fn to_i64(value: f64) -> Option<i64> {
    let rounded = value.round();
    if (value - rounded).abs() < f64::EPSILON
        && rounded >= i64::MIN as f64
        && rounded <= i64::MAX as f64
    {
        Some(rounded as i64)
    } else {
        None
    }
}

#[derive(Default)]
struct VerifyBudget {
    nodes: usize,
    bytes: usize,
    depth: usize,
}

impl VerifyBudget {
    fn charge(&mut self, pointer: &str) -> Result<(), SchemaViolation> {
        self.nodes += 1;
        if self.nodes > MAX_ARGUMENT_NODES {
            return Err(SchemaViolation {
                pointer: pointer.to_string(),
                expected: format!("at most {MAX_ARGUMENT_NODES} JSON nodes"),
                actual: "argument exceeded the node bound".into(),
            });
        }
        Ok(())
    }

    fn enter(&mut self, pointer: &str) -> Result<(), SchemaViolation> {
        self.depth += 1;
        if self.depth > MAX_ARGUMENT_DEPTH {
            return Err(SchemaViolation {
                pointer: pointer.to_string(),
                expected: format!("at most {MAX_ARGUMENT_DEPTH} levels of nesting"),
                actual: "argument exceeded the depth bound".into(),
            });
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    fn elements(&mut self, count: usize, pointer: &str) -> Result<(), SchemaViolation> {
        if count > MAX_ARGUMENT_NODES {
            return Err(SchemaViolation {
                pointer: pointer.to_string(),
                expected: format!("at most {MAX_ARGUMENT_NODES} entries"),
                actual: format!("{count} entries"),
            });
        }
        Ok(())
    }

    fn node_size(&mut self, value: &Value, pointer: &str) -> Result<(), SchemaViolation> {
        self.bytes = self.bytes.saturating_add(value_byte_estimate(value));
        if self.bytes > MAX_ARGUMENT_BYTES {
            return Err(SchemaViolation {
                pointer: pointer.to_string(),
                expected: format!("at most {MAX_ARGUMENT_BYTES} argument bytes"),
                actual: "argument exceeded the byte bound".into(),
            });
        }
        Ok(())
    }

    /// Raw bounded scan of a value the schema does not constrain further
    /// (`additionalProperties` allowed): depth, node and byte limits still
    /// apply so an untyped argument cannot smuggle unbounded structure.
    fn scan_value(&mut self, value: &Value, pointer: &str) -> Result<(), SchemaViolation> {
        self.charge(pointer)?;
        match value {
            Value::Array(items) => {
                self.enter(pointer)?;
                self.elements(items.len(), pointer)?;
                for (index, item) in items.iter().enumerate() {
                    let child = pointer.join_path(&index.to_string());
                    self.scan_value(item, &child)?;
                }
                self.leave();
                Ok(())
            }
            Value::Object(map) => {
                self.enter(pointer)?;
                self.elements(map.len(), pointer)?;
                for (name, field) in map {
                    let child = pointer.join_path(name);
                    self.scan_value(field, &child)?;
                }
                self.leave();
                Ok(())
            }
            other => self.node_size(other, pointer),
        }
    }
}

fn value_byte_estimate(value: &Value) -> usize {
    match value {
        // Strings contribute their real length: the byte bound protects
        // against oversized payloads, so a long string must count in full.
        Value::String(text) => text.len().saturating_add(16),
        Value::Array(items) => items.len().saturating_mul(8).min(4096),
        Value::Object(map) => map.len().saturating_mul(16).min(4096),
        Value::Number(number) if number.is_f64() => 16,
        _ => 8,
    }
}

trait PointerExt {
    fn join_path(&self, field: &str) -> String;
}

impl PointerExt for str {
    /// Append a JSON-pointer path segment with the required `~0`/`~1`
    /// escaping for its key.
    fn join_path(&self, field: &str) -> String {
        let escaped = field.replace('~', "~0").replace('/', "~1");
        format!("{self}/{escaped}")
    }
}

/// Parse one tool-argument JSON document strictly: syntactically valid,
/// duplicate object keys rejected, and the document bounded by the same
/// depth/node/size limits used for validation. Returns the parsed value.
pub fn parse_arguments_strict(text: &str) -> Result<Value, String> {
    if text.len() > MAX_ARGUMENT_BYTES {
        return Err(format!(
            "tool arguments exceed the {MAX_ARGUMENT_BYTES}-byte bound"
        ));
    }
    let mut deserializer = serde_json::Deserializer::from_str(text);
    let seed = StrictDeserialize {
        budget: StrictBudget::default(),
    };
    let value = serde::de::DeserializeSeed::deserialize(seed, &mut deserializer)
        .map_err(|error| format!("tool arguments are not valid strict JSON: {error}"))?;
    deserializer
        .end()
        .map_err(|error| format!("tool arguments contain trailing content: {error}"))?;
    Ok(value)
}

/// Shared depth/node accounting for one strict parse. `Rc<Cell>` lets child
/// seeds update the same counters regardless of where they were spawned.
#[derive(Clone, Default)]
struct StrictBudget {
    depth: Rc<Cell<usize>>,
    nodes: Rc<Cell<usize>>,
}

impl StrictBudget {
    fn charge(&self) -> Result<(), String> {
        let nodes = self.nodes.get() + 1;
        self.nodes.set(nodes);
        if nodes > MAX_ARGUMENT_NODES {
            return Err(format!(
                "tool arguments exceed the {MAX_ARGUMENT_NODES}-node bound"
            ));
        }
        Ok(())
    }

    fn enter(&self) -> Result<(), String> {
        let depth = self.depth.get() + 1;
        self.depth.set(depth);
        if depth > MAX_ARGUMENT_DEPTH {
            return Err(format!(
                "tool arguments exceed the {MAX_ARGUMENT_DEPTH}-level depth bound"
            ));
        }
        Ok(())
    }

    fn leave(&self) {
        self.depth.set(self.depth.get().saturating_sub(1));
    }
}

struct StrictDeserialize {
    budget: StrictBudget,
}

impl<'de> serde::de::DeserializeSeed<'de> for StrictDeserialize {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor {
            budget: self.budget,
        })
    }
}

/// Visitor that rebuilds a `Value` with duplicate-key rejection and bounded
/// depth/node accounting.
struct StrictValueVisitor {
    budget: StrictBudget,
}

impl<'de> serde::de::Visitor<'de> for StrictValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a bounded strict JSON value")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.budget.charge().map_err(serde::de::Error::custom)?;
        Ok(Value::Bool(value))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.budget.charge().map_err(serde::de::Error::custom)?;
        Ok(Value::Number(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.budget.charge().map_err(serde::de::Error::custom)?;
        Ok(Value::Number(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.budget.charge().map_err(serde::de::Error::custom)?;
        Number::from_f64(value)
            .map(Value::Number)
            .ok_or_else(|| serde::de::Error::custom("non-finite number in tool arguments"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.budget.charge().map_err(serde::de::Error::custom)?;
        Ok(Value::String(value.to_string()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.budget.charge().map_err(serde::de::Error::custom)?;
        Ok(Value::String(value))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.budget.charge().map_err(serde::de::Error::custom)?;
        Ok(Value::Null)
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        self.budget.charge().map_err(serde::de::Error::custom)?;
        Ok(Value::Null)
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        self.budget.enter().map_err(serde::de::Error::custom)?;
        let mut items: Vec<Value> = Vec::new();
        while let Some(item) = sequence.next_element_seed(StrictDeserialize {
            budget: self.budget.clone(),
        })? {
            items.push(item);
        }
        self.budget.leave();
        Ok(Value::Array(items))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        self.budget.enter().map_err(serde::de::Error::custom)?;
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut fields: BTreeMap<String, Value> = BTreeMap::new();
        while let Some(key) = map.next_key::<String>()? {
            if !seen.insert(key.clone()) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate object key '{key}' in tool arguments"
                )));
            }
            let value = map.next_value_seed(StrictDeserialize {
                budget: self.budget.clone(),
            })?;
            fields.insert(key, value);
        }
        self.budget.leave();
        Ok(Value::Object(Map::from_iter(fields)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn compile(value: Value) -> SchemaProfile {
        SchemaProfile::compile(&value).expect("schema compiles")
    }

    fn valid(profile: &SchemaProfile, arguments: Value) {
        profile.validate(&arguments).expect("arguments validate");
    }

    fn invalid(profile: &SchemaProfile, arguments: Value) -> SchemaViolation {
        profile
            .validate(&arguments)
            .expect_err("arguments must fail validation")
    }

    #[test]
    fn object_required_and_additional_properties_are_enforced() {
        let profile = compile(json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
            "additionalProperties": false
        }));
        valid(&profile, json!({"path": "a"}));
        assert_eq!(
            invalid(&profile, json!({})).expected,
            "object with required field 'path'"
        );
        assert_eq!(
            invalid(&profile, json!({"path": "a", "extra": 1})).expected,
            "object without additional properties"
        );
    }

    #[test]
    fn missing_field_and_wrong_type_carry_pointers() {
        let profile = compile(json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "limit": {"type": "integer", "minimum": 1, "maximum": 50}
            },
            "required": ["path"]
        }));
        let violation = invalid(&profile, json!({"path": 7}));
        assert_eq!(violation.pointer, "/path");
        assert!(violation.expected.contains("string"), "{violation}");
        let violation = invalid(&profile, json!({"path": "x", "limit": 0}));
        assert!(violation.expected.contains("integer >= 1"), "{violation}");
    }

    #[test]
    fn arrays_items_and_bounds_are_enforced() {
        let profile = compile(json!({
            "type": "object",
            "properties": {
                "argv": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": 64}
            },
            "required": ["argv"]
        }));
        valid(&profile, json!({"argv": ["cargo", "test"]}));
        assert_eq!(
            invalid(&profile, json!({"argv": []})).expected,
            "array of at least 1 items"
        );
        assert_eq!(
            invalid(&profile, json!({"argv": ["x", 2]})).pointer,
            "/argv/1"
        );
        let long: Vec<Value> = (0..65).map(|i| json!(i.to_string())).collect();
        assert!(
            invalid(&profile, json!({"argv": long}))
                .expected
                .contains("at most 64")
        );
    }

    #[test]
    fn string_lengths_and_enum_are_enforced() {
        let profile = compile(json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "minLength": 36},
                "mode": {"type": "string", "enum": ["normal", "resume"]}
            },
            "required": ["mode"]
        }));
        valid(
            &profile,
            json!({"mode": "normal", "task_id": "a".repeat(36)}),
        );
        let violation = invalid(&profile, json!({"mode": "restart"}));
        assert!(violation.expected.starts_with("one of ["), "{violation}");
        let violation = invalid(&profile, json!({"mode": "normal", "task_id": "short"}));
        assert!(violation.expected.contains("at least 36"), "{violation}");
    }

    #[test]
    fn unsupported_keywords_fail_compilation() {
        for schema in [
            json!({"type": "object", "properties": {"x": {"type": "string"}}, "$ref": "#"}),
            json!({"type": "object", "properties": {"x": {"anyOf": [{"type": "string"}]}}}),
            json!({"type": "object", "properties": {"x": {"type": "string", "format": "uri"}}}),
            json!({"type": "object", "properties": {"x": {"type": "string", "patternProperties": {}}}}),
        ] {
            let error = SchemaProfile::compile(&schema).unwrap_err();
            assert!(error.contains("unsupported JSON-schema keyword"), "{error}");
        }
    }

    #[test]
    fn required_may_name_unconstrained_keys() {
        // Standard JSON Schema: `required` can demand keys that `properties`
        // does not describe. The validator enforces presence only.
        let profile = compile(json!({
            "type": "object",
            "required": ["op", "name"]
        }));
        valid(&profile, json!({"op": "load", "name": "x"}));
        assert!(
            invalid(&profile, json!({"op": "load"}))
                .expected
                .contains("required field 'name'"),
            "an unconstrained required key is still enforced for presence"
        );
    }

    #[test]
    fn enum_options_must_be_primitive_and_unique() {
        assert!(
            SchemaProfile::compile(&json!({
                "type": "object",
                "properties": {"x": {"enum": [{"a": 1}]}}
            }))
            .is_err()
        );
        assert!(
            SchemaProfile::compile(&json!({
                "type": "object",
                "properties": {"x": {"type": "string", "enum": ["a", "a"]}}
            }))
            .is_err()
        );
    }

    #[test]
    fn integer_accepts_only_json_integers() {
        let profile = compile(json!({
            "type": "object",
            "properties": {"n": {"type": "integer"}},
            "required": ["n"]
        }));
        valid(&profile, json!({"n": 3}));
        assert!(
            invalid(&profile, json!({"n": 3.0}))
                .expected
                .contains("integer"),
            "a float literal is not an integer"
        );
    }

    #[test]
    fn string_pattern_is_enforced_and_invalid_regex_fails_compile() {
        let profile = compile(json!({
            "type": "object",
            "properties": {"rev": {"type": "string", "pattern": "^[0-9a-f]{64}$"}},
            "required": ["rev"]
        }));
        valid(&profile, json!({"rev": "a".repeat(64)}));
        assert!(
            invalid(&profile, json!({"rev": "abc"}))
                .expected
                .contains("matching"),
            "pattern mismatch must be a typed violation"
        );
        assert!(
            SchemaProfile::compile(&json!({
                "type": "object",
                "properties": {"x": {"type": "string", "pattern": "("}}
            }))
            .is_err(),
            "an invalid regex must fail capability admission"
        );
    }

    #[test]
    fn primitive_schema_rejects_container_keywords() {
        assert!(
            SchemaProfile::compile(&json!({
                "type": "object",
                "properties": {"x": {"type": "string", "items": {"type": "string"}}}
            }))
            .is_err()
        );
    }

    #[test]
    fn depth_and_size_overflow_fail_closed() {
        let profile = compile(json!({
            "type": "object",
            "properties": {},
            "additionalProperties": true
        }));
        // Layers built by hand: `json!` macro expansion places the variable
        // by move, so an explicit loop is needed to reach depth 40.
        let mut deep = Value::Null;
        for _ in 0..40 {
            deep = Value::Object(Map::from_iter([("a".to_string(), deep)]));
        }
        let violation = invalid(
            &profile,
            Value::Object(Map::from_iter([("a".to_string(), deep)])),
        );
        assert_eq!(
            violation.actual, "argument exceeded the depth bound",
            "{violation}"
        );
        let huge = json!({"blob": "x".repeat(MAX_ARGUMENT_BYTES + 1)});
        let violation = invalid(&profile, huge);
        assert!(violation.expected.contains("byte"), "{violation}");
    }

    #[test]
    fn strict_parser_rejects_duplicate_keys() {
        let error = parse_arguments_strict(r#"{"a":1,"a":2}"#).unwrap_err();
        assert!(error.contains("duplicate object key 'a'"), "{error}");
        let error = parse_arguments_strict(r#"{"nested":{"x":1,"x":2}}"#).unwrap_err();
        assert!(error.contains("duplicate object key 'x'"), "{error}");
        let parsed = parse_arguments_strict(r#"{"a":1,"b":[1,2,3]}"#).unwrap();
        assert_eq!(parsed, json!({"a": 1, "b": [1, 2, 3]}));
        assert!(
            parse_arguments_strict(r#"{"a":1} trailing"#)
                .unwrap_err()
                .contains("trailing")
        );
    }

    #[test]
    fn strict_parser_bounds_depth_and_size() {
        let mut deep = String::new();
        for _ in 0..40 {
            deep.push_str(r#"{"a":"#);
        }
        deep.push_str("null");
        for _ in 0..40 {
            deep.push('}');
        }
        assert!(
            parse_arguments_strict(&deep).is_err(),
            "depth overflow must fail closed"
        );
        let huge = format!(r#"{{"blob":"{}"}}"#, "x".repeat(MAX_ARGUMENT_BYTES + 1));
        assert!(parse_arguments_strict(&huge).is_err());
    }
}
