//! Full-text proposals share the same capability and optimistic-write rules as
//! patches. Preparation reads for the review UI; it never changes the target.
use super::*;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReplaceArgs {
    path: String,
    content: String,
    base_sha256: Option<String>,
}

pub struct WriteProposal {
    pub call: ToolCall,
    pub preview: Value,
}

pub(crate) fn validate_replace(arguments: &Value) -> Result<(), String> {
    if arguments.get("baseSha256").is_none() {
        return Err("baseSha256 is required; use null only for a new file".into());
    }
    let args: ReplaceArgs = serde_json::from_value(arguments.clone())
        .map_err(|_| "expected path, content and optional baseSha256")?;
    relative(&args.path, false)?;
    if args.content.len() > READ_LIMIT as usize {
        return Err("content exceeds 256 KiB".into());
    }
    if args.base_sha256.as_ref().is_some_and(|hash| {
        hash.len() != 64
            || !hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    }) {
        return Err("baseSha256 must be a lowercase SHA-256 digest or null".into());
    }
    Ok(())
}

fn open_target(directory: &Dir, path: &str, writable: bool) -> std::io::Result<cap_std::fs::File> {
    let (parent, name) = path.rsplit_once('/').unwrap_or((".", path));
    let parent = directory.open_dir(parent)?;
    let metadata = parent.symlink_metadata(name)?;
    if metadata.is_symlink() || !metadata.is_file() || metadata.nlink() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "target must be a regular file without symlinks or extra hard links",
        ));
    }
    let file = parent.open_with(name, file_io::options(writable).follow(FollowSymlinks::No))?;
    let opened = file.metadata()?;
    if !opened.is_file() || opened.nlink() != 1 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "invalid opened target",
        ));
    }
    Ok(file)
}

fn read_snapshot(
    file: &mut cap_std::fs::File,
    cancellation: &CancellationToken,
) -> Result<String, ToolError> {
    let bytes = file_io::read(file, cancellation)?;
    String::from_utf8(bytes).map_err(|_| "file is not UTF-8 text".into())
}

pub fn prepare_workspace_write(
    root: &Path,
    id: &str,
    path: &str,
    content: &str,
    cancellation: &CancellationToken,
) -> Result<WriteProposal, ToolError> {
    relative(path, false)?;
    if content.len() > READ_LIMIT as usize {
        return Err("content exceeds 256 KiB".into());
    }
    if cancellation.is_cancelled() {
        return Err("proposal cancelled".into());
    }
    let directory = Dir::open_ambient_dir(root, ambient_authority())
        .map_err(|_| "cannot open workspace capability")?;
    let snapshot = match open_target(&directory, path, false) {
        Ok(mut file) => Some(read_snapshot(&mut file, cancellation)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err("cannot prepare workspace target".into()),
    };
    if cancellation.is_cancelled() {
        return Err("proposal cancelled".into());
    }
    let base = snapshot.as_ref().map(|text| sha256(text.as_bytes()));
    let diff = diffy::create_patch(snapshot.as_deref().unwrap_or(""), content).to_string();
    Ok(WriteProposal {
        call: ToolCall {
            id: id.into(),
            name: "workspace_replace_text".into(),
            arguments: json!({"path":path,"content":content,"baseSha256":base}),
        },
        preview: json!({"path":path,"diff":diff,"createsFile":snapshot.is_none()}),
    })
}

pub(crate) fn execute_replace(
    directory: &Dir,
    arguments: &Value,
    cancellation: &CancellationToken,
) -> Result<Value, ToolError> {
    validate_replace(arguments)?;
    let args: ReplaceArgs =
        serde_json::from_value(arguments.clone()).map_err(|_| "invalid replacement arguments")?;
    let mut file = if let Some(base) = &args.base_sha256 {
        let mut file = open_target(directory, &args.path, true)
            .map_err(|_| "cannot open workspace target for replacement")?;
        let current = read_snapshot(&mut file, cancellation)?;
        if sha256(current.as_bytes()) != *base {
            return Err(ToolError::NeedsCorrection {
                code: "workspace_base_changed",
                message: "workspace file changed since proposal; reread it and prepare a new write"
                    .into(),
            });
        }
        file
    } else {
        if cancellation.is_cancelled() {
            return Err("tool cancelled before create".into());
        }
        directory
            .open_with(&args.path, OpenOptions::new().create_new(true).write(true))
            .map_err(|_| "cannot create new workspace target")?
    };
    // Past create_new, even cancellation can leave a newly created empty file.
    // Report uncertainty rather than claim the operation never happened.
    if cancellation.is_cancelled() {
        return Err(ToolError::OutcomeUnknown(
            "write cancelled after target opened; inspect target".into(),
        ));
    }
    file.seek(SeekFrom::Start(0))
        .map_err(|_| ToolError::OutcomeUnknown("cannot seek workspace target".into()))?;
    file.write_all(args.content.as_bytes())
        .map_err(|_| ToolError::OutcomeUnknown("workspace replacement write failed".into()))?;
    file.set_len(args.content.len() as u64)
        .map_err(|_| ToolError::OutcomeUnknown("workspace replacement truncate failed".into()))?;
    file.sync_all()
        .map_err(|_| ToolError::OutcomeUnknown("workspace replacement sync failed".into()))?;
    Ok(
        json!({"path":args.path,"sha256":sha256(args.content.as_bytes()),"bytesWritten":args.content.len()}),
    )
}
