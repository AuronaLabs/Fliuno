//! Modern MCP tool schemas are untrusted even when the endpoint is configured
//! by the host. Bound compilation, reference expansion, and validation work.
use jsonschema::{Draft, PatternOptions, Registry, Retrieve, Uri, Validator, uri};
use serde_json::Value;
use std::{
    sync::{Arc, LazyLock, Mutex, mpsc},
    time::{Duration, Instant},
};

const BASE: &str = "https://fliuno.invalid/mcp-tool-schema";
const MAX_BYTES: usize = 64 * 1024;
const MAX_DEPTH: usize = 32;
const MAX_SCHEMA_NODES: usize = 4096;
const MAX_INSTANCE_NODES: usize = 16_384;
const MAX_EXPANSION: usize = 1024;
const MAX_VALIDATION_WORK: usize = 4 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_millis(500);

// Initializing the library's bundled meta-schema is constant work shared by
// all servers. Keep it outside the deadline for evaluating untrusted input.
static MODERN_SCHEMA_READY: LazyLock<Result<(), String>> =
    LazyLock::new(|| compile_modern(&serde_json::json!({"type":"object"})).map(|_| ()));

#[derive(Clone)]
pub(super) struct ToolSchema {
    validator: Arc<Validator>,
    weight: Option<usize>,
}

impl ToolSchema {
    pub(super) fn compile(value: &Value, modern: bool, input: bool) -> Result<Self, String> {
        if fliuno_core::json_size(value).map_err(|_| "invalid MCP tool schema")? > MAX_BYTES {
            return Err("MCP tool schema exceeds 64 KiB".into());
        }
        if !modern {
            let validator =
                jsonschema::validator_for(value).map_err(|_| "unsupported MCP input schema")?;
            return Ok(Self {
                validator: Arc::new(validator),
                weight: None,
            });
        }
        if input && value.get("type").and_then(Value::as_str) != Some("object") {
            return Err("MCP input schema must declare an object root".into());
        }
        bounded_nodes(value, MAX_SCHEMA_NODES).ok_or("MCP tool schema is too deep or complex")?;
        MODERN_SCHEMA_READY.as_ref().map_err(Clone::clone)?;
        let value = value.clone();
        run(move || compile_modern(&value))?
    }

    pub(super) fn is_valid(&self, instance: &Value) -> Result<bool, String> {
        let Some(weight) = self.weight else {
            return Ok(self.validator.is_valid(instance));
        };
        let nodes = bounded_nodes(instance, MAX_INSTANCE_NODES)
            .ok_or("MCP schema instance is too deep or complex")?;
        if nodes.saturating_mul(weight) > MAX_VALIDATION_WORK {
            return Err("MCP schema validation work exceeds its budget".into());
        }
        let validator = Arc::clone(&self.validator);
        let instance = instance.clone();
        run(move || validator.is_valid(&instance))
    }
}

fn bounded_nodes(value: &Value, maximum: usize) -> Option<usize> {
    fn visit(value: &Value, depth: usize, remaining: &mut usize) -> Option<()> {
        if depth > MAX_DEPTH {
            return None;
        }
        *remaining = remaining.checked_sub(1)?;
        match value {
            Value::Object(children) => {
                for child in children.values() {
                    visit(child, depth + 1, remaining)?;
                }
            }
            Value::Array(children) => {
                for child in children {
                    visit(child, depth + 1, remaining)?;
                }
            }
            _ => {}
        }
        Some(())
    }
    let mut remaining = maximum;
    visit(value, 0, &mut remaining)?;
    Some(maximum - remaining)
}

#[derive(Clone, Copy)]
struct NoFetch;

impl Retrieve for NoFetch {
    fn retrieve(&self, _: &Uri<String>) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        Err("external MCP schema resources are disabled".into())
    }
}

fn compile_modern(value: &Value) -> Result<ToolSchema, String> {
    let draft = Draft::Draft202012.detect(value);
    if draft == Draft::Unknown {
        return Err("unsupported MCP JSON Schema dialect".into());
    }
    let unsupported = |_| "unsupported MCP tool schema or external reference".to_owned();
    let registry = Registry::new()
        .retriever(NoFetch)
        .retrieve_known_draft_meta_schemas(false)
        .add(BASE, value)
        .and_then(|builder| builder.prepare())
        .map_err(unsupported)?;
    let root = registry.resolver(uri::from_str(BASE).expect("constant schema base is valid"));
    let mut stack = vec![(value, root, draft, 0, false)];
    let mut weight = 0;
    // Follow the same scoped references as the validator. Count repeated
    // visits too, so a shallow DAG cannot hide exponential expansion.
    while let Some((schema, scope, inherited, depth, already_scoped)) = stack.pop() {
        weight += 1;
        if depth > MAX_DEPTH || weight > MAX_EXPANSION {
            return Err("MCP schema reference expansion exceeds its budget".into());
        }
        let draft = inherited.detect(schema);
        if draft == Draft::Unknown {
            return Err("unsupported MCP JSON Schema dialect".into());
        }
        let scope = if already_scoped {
            scope
        } else {
            scope
                .in_subresource(draft.create_resource_ref(schema))
                .map_err(unsupported)?
        };
        if schema.get("$dynamicRef").is_some() || schema.get("$recursiveRef").is_some() {
            return Err("dynamic MCP schema references are unsupported".into());
        }
        if let Some(reference) = schema.get("$ref") {
            let reference = reference
                .as_str()
                .filter(|s| s.starts_with('#'))
                .ok_or("MCP tool schema references must be local fragments")?;
            let (target, scope, draft) = scope.lookup(reference).map_err(unsupported)?.into_inner();
            stack.push((target, scope, draft, depth + 1, true));
        }
        stack.extend(
            draft
                .subresources_of(schema)
                .map(|child| (child, scope.clone(), draft, depth + 1, false)),
        );
    }
    let validator = jsonschema::options()
        .with_draft(draft)
        .with_base_uri(BASE)
        .with_retriever(NoFetch)
        .with_registry(&registry)
        .with_pattern_options(
            PatternOptions::regex()
                .size_limit(512 * 1024)
                .dfa_size_limit(256 * 1024),
        )
        .build(value)
        .map_err(|_| "unsupported MCP tool schema")?;
    Ok(ToolSchema {
        validator: Arc::new(validator),
        weight: Some(weight),
    })
}

type Job = Box<dyn FnOnce() + Send>;

static WORKERS: LazyLock<Option<mpsc::SyncSender<Job>>> = LazyLock::new(|| {
    let (send, receive) = mpsc::sync_channel::<Job>(8);
    let receive = Arc::new(Mutex::new(receive));
    for index in 0..2 {
        let receive = Arc::clone(&receive);
        std::thread::Builder::new()
            .name(format!("mcp-schema-{index}"))
            .spawn(move || {
                loop {
                    let job = receive
                        .lock()
                        .ok()
                        .and_then(|receiver| receiver.recv().ok());
                    let Some(job) = job else {
                        break;
                    };
                    job();
                }
            })
            .ok()?;
    }
    Some(send)
});

fn run<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    let deadline = Instant::now() + DEADLINE;
    let (send, receive) = mpsc::sync_channel(1);
    WORKERS
        .as_ref()
        .ok_or("MCP schema workers unavailable")?
        .try_send(Box::new(move || {
            if Instant::now() < deadline {
                let _ = send.send(work());
            }
        }))
        .map_err(|_| "MCP schema workers busy")?;
    receive
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| "MCP schema evaluation exceeded its deadline".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn modern_schema_supports_local_anchors_and_explicit_dialects() {
        let nested = json!({"$id":"urn:fliuno:root","type":"object",
            "$defs":{"text":{"$anchor":"label","type":"string"}},
            "properties":{"name":{"$ref":"#label"}}});
        let schema = ToolSchema::compile(&nested, true, true).unwrap();
        assert!(schema.is_valid(&json!({"name":"label"})).unwrap());
        assert!(!schema.is_valid(&json!({"name":42})).unwrap());
        let draft7 = json!({"$schema":"http://json-schema.org/draft-07/schema#","type":"array",
            "items":[{"type":"string"} ],"additionalItems":false});
        let schema = ToolSchema::compile(&draft7, true, false).unwrap();
        assert!(schema.is_valid(&json!(["legacy tuple dialect"])).unwrap());
        assert!(!schema.is_valid(&json!(["not", "a tuple"])).unwrap());
    }

    #[test]
    fn modern_compiler_rejects_external_unknown_recursive_and_over_budget_schemas() {
        let mut deep = json!({"type":"string"});
        for _ in 0..MAX_DEPTH {
            deep = json!({"type":"array","items":deep});
        }
        let mut defs = serde_json::Map::new();
        defs.insert("0".into(), json!({"type":"object"}));
        for n in 1..14 {
            let prior = format!("#/$defs/{}", n - 1);
            defs.insert(
                n.to_string(),
                json!({"allOf":[{"$ref":prior},{"$ref":prior}]}),
            );
        }
        for (schema, expected) in [
            (deep, "too deep"),
            (json!({"$ref":"file:///outside.json"}), "external reference"),
            (
                json!({"$ref":"https://example.invalid/schema"}),
                "external reference",
            ),
            (
                json!({"$schema":"https://example.invalid/unknown-dialect"}),
                "dialect",
            ),
            (json!({"$ref":"#"}), "reference expansion"),
            (
                json!({"$defs":defs,"$ref":"#/$defs/13"}),
                "reference expansion",
            ),
        ] {
            assert!(
                matches!(ToolSchema::compile(&schema, true, false), Err(error) if error.contains(expected)),
                "expected rejection for {expected}"
            );
        }
        assert!(ToolSchema::compile(&json!({"type":"array"}), true, true).is_err());
    }

    #[test]
    fn evaluation_deadline_returns_without_waiting_for_a_worker() {
        let (_release, hold) = mpsc::channel::<()>();
        let error = run(move || hold.recv()).unwrap_err();
        assert!(error.contains("deadline"));
        assert_eq!(run(|| 7).unwrap(), 7);
    }
}
