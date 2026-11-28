//! Discover service-owned project checks without creating any execution authority.
use super::*;
use cap_fs_ext::{DirExt, MetadataExt};
use cap_std::fs::Dir;
use std::{
    io,
    path::{Component, Path},
};
use workspace_path::{is_reparse_point, open_workspace_root};

const MAX_CHECKS: usize = 32;

impl Kernel {
    pub fn workspace_project_checks(&self, workspace_id: &str) -> Result<WorkspaceProjectChecks> {
        let workspace = self.get_workspace(workspace_id)?;
        let mut checks = Vec::new();
        let mut truncated = false;
        let mut root = None;
        for (name, tool) in self.active_model_tools() {
            let Some(check) = tool.project_check() else {
                continue;
            };
            if crate::interaction::is_internal(name)
                || self.model_tool_revoked(name, &tool.authority())
            {
                continue;
            }
            if checks.len() == MAX_CHECKS {
                truncated = true;
                break;
            }
            let root =
                root.get_or_insert_with(|| open_workspace_root(Path::new(&workspace.root)).ok());
            checks.push(ProjectCheckSummary {
                tool_name: name.clone(),
                label: check.label,
                description: check.description,
                inputs: check
                    .inputs
                    .into_iter()
                    .map(|path| ProjectCheckInput {
                        status: input_status(root.as_ref(), &path),
                        path,
                    })
                    .collect(),
            });
        }
        Ok(WorkspaceProjectChecks {
            workspace_id: workspace.id,
            checks,
            truncated,
        })
    }
}

fn input_status(root: Option<&Dir>, path: &str) -> ProjectCheckInputStatus {
    use ProjectCheckInputStatus::*;
    let Some(root) = root else { return Unavailable };
    if path.contains(['\\', ':'])
        || path
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return Unsupported;
    }
    match input_metadata(root, Path::new(path)) {
        Ok(true) => Present,
        Ok(false) => Unsupported,
        Err(error) if error.kind() == io::ErrorKind::NotFound => Missing,
        Err(_) => Unavailable,
    }
}

fn input_metadata(root: &Dir, path: &Path) -> io::Result<bool> {
    let mut dir = root.try_clone()?;
    let mut parts = path.components().peekable();
    while let Some(Component::Normal(name)) = parts.next() {
        let metadata = dir.symlink_metadata(name)?;
        if is_reparse_point(&metadata) {
            return Ok(false);
        }
        if parts.peek().is_none() {
            return Ok(metadata.is_file() && metadata.nlink() == 1);
        }
        if !metadata.is_dir() {
            return Ok(false);
        }
        dir = dir.open_dir_nofollow(name)?;
        if is_reparse_point(&dir.dir_metadata()?) {
            return Ok(false);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn checks_report_only_confined_regular_inputs() {
        use ProjectCheckInputStatus::*;
        let root = std::env::temp_dir().join(new_id("fliuno-check-catalog"));
        fs::create_dir_all(root.join("src")).unwrap();
        let file = root.join("src/input.txt");
        fs::write(&file, b"private bytes").unwrap();
        let dir = open_workspace_root(&fs::canonicalize(&root).unwrap()).unwrap();
        assert_eq!(input_status(Some(&dir), "src/input.txt"), Present);
        assert_eq!(input_status(Some(&dir), "src/missing.txt"), Missing);
        assert_eq!(input_status(Some(&dir), "missing/input.txt"), Missing);
        assert_eq!(input_status(Some(&dir), "src"), Unsupported);
        assert_eq!(input_status(Some(&dir), "../outside.txt"), Unsupported);
        assert_eq!(input_status(None, "src/input.txt"), Unavailable);
        fs::hard_link(&file, root.join("linked.txt")).unwrap();
        assert_eq!(input_status(Some(&dir), "linked.txt"), Unsupported);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("src", root.join("linked-dir")).unwrap();
            assert_eq!(
                input_status(Some(&dir), "linked-dir/input.txt"),
                Unsupported
            );
        }
        drop(dir);
        fs::remove_dir_all(root).unwrap();
    }
}
