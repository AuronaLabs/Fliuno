//! Shared input contract for tools that approve a complete list of paths.
use super::*;

const MAX_TOTAL_PATH_BYTES: usize = 8 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchPaths<'a> {
    #[serde(borrow)]
    paths: Vec<&'a str>,
}

pub(super) fn parse(
    arguments: &Value,
    max_paths: usize,
    allow_root: bool,
) -> Result<Vec<&str>, String> {
    let args = BatchPaths::deserialize(arguments).map_err(|_| "expected workspace paths")?;
    if !(1..=max_paths).contains(&args.paths.len()) {
        return Err(format!("paths must contain 1..{max_paths} workspace paths"));
    }
    for (index, path) in args.paths.iter().enumerate() {
        relative(path, allow_root)?;
        if args.paths[..index].contains(path) {
            return Err("paths must be unique".into());
        }
    }
    if args.paths.iter().map(|path| path.len()).sum::<usize>() > MAX_TOTAL_PATH_BYTES {
        return Err("paths exceed the 8 KiB combined byte limit".into());
    }
    Ok(args.paths)
}

pub(super) fn schema(max_paths: usize) -> Value {
    json!({
        "type":"object", "required":["paths"], "additionalProperties":false,
        "properties":{"paths":{"type":"array","items":{"type":"string"},"minItems":1,"maxItems":max_paths,"uniqueItems":true}}
    })
}
