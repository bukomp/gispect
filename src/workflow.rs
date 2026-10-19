//! GitHub Actions workflow files: whether a workflow can be started by
//! hand (`on: workflow_dispatch`) and which inputs it asks for.
//!
//! The YAML is read through the parser's event stream into a JSON value
//! with a node budget, because the stock loader copies an alias's whole
//! subtree for every use and a small file with nested aliases ("billion
//! laughs") would exhaust memory.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Result};
use serde_json::{Map, Value};
use yaml_rust2::parser::{Event, Parser};

/// Largest workflow file that is parsed.
pub const MAX_WORKFLOW_BYTES: usize = 1 << 20;

/// Most YAML nodes a workflow file may expand to (aliases included).
const MAX_NODES: usize = 200_000;

/// Deepest collection nesting accepted. Block collections (`- - - x`)
/// aren't depth-limited by the scanner, and dropping or cloning a deeply
/// nested value recurses, so a crafted file could overflow the stack.
const MAX_DEPTH: usize = 64;

/// The value type of a dispatch input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputKind {
    String,
    Boolean,
    Number,
    /// One of a fixed list of options.
    Choice(Vec<String>),
    /// The name of a deployment environment.
    Environment,
}

/// One `workflow_dispatch` input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DispatchInput {
    pub name: String,
    pub description: String,
    pub required: bool,
    /// Default value as text (`true`/`false` for booleans); may be empty.
    pub default: String,
    pub kind: InputKind,
}

/// The inputs of a manually runnable workflow, in file order; `None` when
/// the workflow has no `workflow_dispatch` trigger.
pub fn dispatch_inputs(yaml: &str) -> Result<Option<Vec<DispatchInput>>> {
    if yaml.len() > MAX_WORKFLOW_BYTES {
        bail!(
            "workflow file is larger than {} KB",
            MAX_WORKFLOW_BYTES >> 10
        );
    }
    let doc = load_first_document(yaml)?;
    let Some(on) = doc.get("on") else {
        return Ok(None);
    };
    let trigger = match on {
        Value::String(s) if s == "workflow_dispatch" => return Ok(Some(Vec::new())),
        Value::Array(items) => {
            let manual = items
                .iter()
                .any(|v| v.as_str() == Some("workflow_dispatch"));
            return Ok(manual.then(Vec::new));
        }
        Value::Object(map) => match map.get("workflow_dispatch") {
            Some(trigger) => trigger,
            None => return Ok(None),
        },
        _ => return Ok(None),
    };
    let Some(inputs) = trigger.get("inputs").and_then(Value::as_object) else {
        return Ok(Some(Vec::new()));
    };
    Ok(Some(
        inputs
            .iter()
            .map(|(name, spec)| parse_input(name, spec))
            .collect(),
    ))
}

fn parse_input(name: &str, spec: &Value) -> DispatchInput {
    let text = |key: &str| {
        spec.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let kind = match text("type").as_str() {
        "boolean" => InputKind::Boolean,
        "number" => InputKind::Number,
        "environment" => InputKind::Environment,
        "choice" => InputKind::Choice(
            spec.get("options")
                .and_then(Value::as_array)
                .map(|opts| {
                    opts.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        ),
        _ => InputKind::String,
    };
    let mut default = text("default");
    match &kind {
        InputKind::Boolean => {
            default = is_true(&default).to_string();
        }
        // GitHub preselects the first option when there's no default.
        InputKind::Choice(options) if default.is_empty() => {
            default = options.first().cloned().unwrap_or_default();
        }
        _ => {}
    }
    DispatchInput {
        name: name.to_string(),
        description: text("description"),
        required: is_true(&text("required")),
        default,
        kind,
    }
}

/// YAML 1.2 core-schema truth.
fn is_true(s: &str) -> bool {
    matches!(s, "true" | "True" | "TRUE")
}

/// An open collection while building the document.
enum Frame {
    Seq {
        anchor: usize,
        start: usize,
        items: Vec<Value>,
    },
    Map {
        anchor: usize,
        start: usize,
        map: Map<String, Value>,
        key: Option<String>,
    },
}

/// The first YAML document as JSON, scalars as strings. Mapping keys that
/// aren't scalars are stringified.
fn load_first_document(yaml: &str) -> Result<Value> {
    let mut parser = Parser::new_from_str(yaml);
    let mut stack: Vec<Frame> = Vec::new();
    // Anchored node and how many nodes it expands to.
    let mut anchors: HashMap<usize, (Value, usize)> = HashMap::new();
    let mut nodes = 0usize;
    let mut root: Option<Value> = None;

    loop {
        let (event, _) = parser
            .next_token()
            .map_err(|e| anyhow!("invalid workflow YAML: {e}"))?;
        let (value, anchor, size) = match event {
            Event::StreamEnd | Event::DocumentEnd => break,
            Event::Scalar(s, _, anchor, _) => {
                nodes += 1;
                (Value::String(s), anchor, 1)
            }
            Event::Alias(id) => {
                let (value, size) = anchors.get(&id).cloned().unwrap_or((Value::Null, 1));
                nodes += size;
                if nodes > MAX_NODES {
                    bail!("workflow YAML expands to too many nodes");
                }
                (value, 0, size)
            }
            Event::SequenceStart(..) | Event::MappingStart(..) if stack.len() >= MAX_DEPTH => {
                bail!("workflow YAML is nested too deeply");
            }
            Event::SequenceStart(anchor, _) => {
                nodes += 1;
                stack.push(Frame::Seq {
                    anchor,
                    start: nodes,
                    items: Vec::new(),
                });
                continue;
            }
            Event::MappingStart(anchor, _) => {
                nodes += 1;
                stack.push(Frame::Map {
                    anchor,
                    start: nodes,
                    map: Map::new(),
                    key: None,
                });
                continue;
            }
            Event::SequenceEnd | Event::MappingEnd => match stack.pop() {
                Some(Frame::Seq {
                    anchor,
                    start,
                    items,
                }) => (Value::Array(items), anchor, nodes - start + 1),
                Some(Frame::Map {
                    anchor, start, map, ..
                }) => (Value::Object(map), anchor, nodes - start + 1),
                None => bail!("invalid workflow YAML: unbalanced collection"),
            },
            _ => continue,
        };
        if nodes > MAX_NODES {
            bail!("workflow YAML expands to too many nodes");
        }
        if anchor > 0 {
            anchors.insert(anchor, (value.clone(), size));
        }
        match stack.last_mut() {
            Some(Frame::Seq { items, .. }) => items.push(value),
            Some(Frame::Map { map, key, .. }) => match key.take() {
                Some(k) => {
                    map.insert(k, value);
                }
                None => {
                    *key = Some(match value {
                        Value::String(s) => s,
                        other => other.to_string(),
                    })
                }
            },
            None => {
                root = Some(value);
                break;
            }
        }
    }
    Ok(root.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_dispatch_trigger() {
        assert_eq!(dispatch_inputs("on: push\njobs: {}\n").unwrap(), None);
        assert_eq!(dispatch_inputs("on: [push, pull_request]\n").unwrap(), None);
        assert_eq!(dispatch_inputs("on:\n  push:\n").unwrap(), None);
        assert_eq!(dispatch_inputs("name: x\n").unwrap(), None);
    }

    #[test]
    fn dispatch_without_inputs() {
        for yaml in [
            "on: workflow_dispatch\n",
            "on: [push, workflow_dispatch]\n",
            "on:\n  workflow_dispatch:\n  push:\n",
            "\"on\":\n  workflow_dispatch: {}\n",
        ] {
            assert_eq!(dispatch_inputs(yaml).unwrap(), Some(vec![]), "{yaml}");
        }
    }

    #[test]
    fn dispatch_inputs_in_file_order() {
        let yaml = r#"
on:
  workflow_dispatch:
    inputs:
      version:
        description: Version to release
        required: true
      dry_run:
        type: boolean
        default: true
      target:
        type: choice
        options: [staging, production]
      count:
        type: number
        default: 3
      env:
        type: environment
jobs: {}
"#;
        let inputs = dispatch_inputs(yaml).unwrap().unwrap();
        let names: Vec<&str> = inputs.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["version", "dry_run", "target", "count", "env"]);
        assert!(inputs[0].required);
        assert_eq!(inputs[0].description, "Version to release");
        assert_eq!(inputs[0].kind, InputKind::String);
        assert_eq!(inputs[1].kind, InputKind::Boolean);
        assert_eq!(inputs[1].default, "true");
        assert_eq!(
            inputs[2].kind,
            InputKind::Choice(vec!["staging".into(), "production".into()])
        );
        assert_eq!(inputs[2].default, "staging");
        assert_eq!(inputs[3].kind, InputKind::Number);
        assert_eq!(inputs[3].default, "3");
        assert_eq!(inputs[4].kind, InputKind::Environment);
        assert!(!inputs[4].required);
    }

    #[test]
    fn anchors_resolve() {
        let yaml = "x: &opts [a, b]\non:\n  workflow_dispatch:\n    inputs:\n      t:\n        type: choice\n        options: *opts\n";
        let inputs = dispatch_inputs(yaml).unwrap().unwrap();
        assert_eq!(
            inputs[0].kind,
            InputKind::Choice(vec!["a".into(), "b".into()])
        );
    }

    #[test]
    fn alias_bomb_is_refused() {
        let mut yaml = String::from("a0: &a0 [x, x, x, x, x, x, x, x, x, x]\n");
        for i in 1..12 {
            let p = i - 1;
            yaml.push_str(&format!(
                "a{i}: &a{i} [*a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}, *a{p}]\n"
            ));
        }
        assert!(dispatch_inputs(&yaml).is_err());
    }

    #[test]
    fn deep_nesting_is_refused() {
        let yaml = format!("{}x\n", "- ".repeat(100_000));
        let err = dispatch_inputs(&yaml).unwrap_err().to_string();
        assert!(err.contains("nested too deeply"), "{err}");
        let ok = format!(
            "a: {}x{}\non: workflow_dispatch\n",
            "[".repeat(20),
            "]".repeat(20)
        );
        assert!(dispatch_inputs(&ok).is_ok());
    }

    #[test]
    fn invalid_yaml_is_an_error() {
        assert!(dispatch_inputs("on: [unclosed\n").is_err());
    }
}
