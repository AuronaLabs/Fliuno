//! Local pipe entry point for onboarding before a profile exists on disk.
use fliuno_config::{MAX_MODEL_CATALOG_DRAFT_BYTES, ModelCatalogDraft};
use std::{
    ffi::OsString,
    io::{self, IsTerminal, Read, Write},
};
use zeroize::Zeroizing;

pub(crate) fn run(
    mut args: impl Iterator<Item = OsString>,
) -> Result<(), Box<dyn std::error::Error>> {
    let stdin = io::stdin();
    if args.next().is_some() || stdin.is_terminal() {
        return Err("expected a draft and its key over a local input pipe".into());
    }
    let mut input = Zeroizing::new(Vec::with_capacity(
        MAX_MODEL_CATALOG_DRAFT_BYTES as usize + 1,
    ));
    stdin
        .lock()
        .take(MAX_MODEL_CATALOG_DRAFT_BYTES + 1)
        .read_to_end(&mut input)?;
    if input.len() as u64 > MAX_MODEL_CATALOG_DRAFT_BYTES {
        return Err("model catalog draft exceeds its size limit".into());
    }
    let input = std::str::from_utf8(&input).map_err(|_| "invalid model catalog draft encoding")?;
    let (draft, key) = input
        .split_once('\n')
        .ok_or("expected a draft and its key")?;
    let draft: ModelCatalogDraft =
        serde_json::from_str(draft).map_err(|_| "invalid model catalog draft")?;
    let preview = fliuno_providers::preview_model_catalog(draft, key)?;
    serde_json::to_writer(io::stdout().lock(), &preview)?;
    io::stdout().lock().write_all(b"\n")?;
    Ok(())
}
