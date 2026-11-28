use super::*;
use std::fs;

struct Project(std::path::PathBuf);

impl Project {
    fn new() -> Self {
        let path = std::env::temp_dir().join(new_id("fliuno-guide"));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn upgrades_existing_v76_workspace_and_preserves_consent_on_reentry() {
    let project = Project::new();
    fs::write(project.0.join(PROJECT_GUIDE_PATH), "Review changes.").unwrap();
    let db = project.0.join("v76.db");
    let root = {
        let mut kernel = Kernel::open(&db, Box::new(DemoProvider)).unwrap();
        kernel.open_workspace(project.0.to_str().unwrap()).unwrap()
    };
    let mark_old = || {
        let connection = Connection::open(&db).unwrap();
        connection.pragma_update(None, "user_version", 76).unwrap();
    };
    Connection::open(&db)
        .unwrap()
        .execute("DROP TABLE workspace_project_guides", [])
        .unwrap();
    mark_old();
    let enabled = {
        let mut kernel = Kernel::open(&db, Box::new(DemoProvider)).unwrap();
        let preview = kernel.preview_project_guide(&root.id).unwrap();
        assert_eq!(preview.state.revision, 0);
        kernel
            .set_project_guide(WorkspaceSetProjectGuideParams {
                workspace_id: root.id.clone(),
                expected_revision: 0,
                sha256: Some(preview.guide.unwrap().sha256),
            })
            .unwrap()
    };
    mark_old();
    let kernel = Kernel::open(&db, Box::new(DemoProvider)).unwrap();
    assert_eq!(kernel.workspace_project_guide(&root.id).unwrap(), enabled);
    assert_eq!(
        kernel
            .db
            .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
            .unwrap(),
        crate::storage::SCHEMA_VERSION
    );
}

#[test]
fn consent_is_workspace_scoped_frozen_and_revocable() {
    let project = Project::new();
    let other = Project::new();
    let db = project.0.join("profile.db");
    let path = project.0.join(PROJECT_GUIDE_PATH);
    fs::write(&path, "Prefer exact edits.\r\n").unwrap();
    fs::write(other.0.join(PROJECT_GUIDE_PATH), "Prefer exact edits.\r\n").unwrap();
    let mut kernel = Kernel::open(&db, Box::new(DemoProvider)).unwrap();
    let root = kernel.open_workspace(project.0.to_str().unwrap()).unwrap();
    let session = kernel.create_session(&root.id).unwrap();
    let other_root = kernel.open_workspace(other.0.to_str().unwrap()).unwrap();
    let other_session = kernel.create_session(&other_root.id).unwrap();
    let preview = kernel.preview_project_guide(&root.id).unwrap();
    assert_eq!(preview.state.revision, 0);
    let first = preview.guide.unwrap();
    let update = |expected_revision, sha256| WorkspaceSetProjectGuideParams {
        workspace_id: root.id.clone(),
        expected_revision,
        sha256,
    };
    fs::write(&path, "Run the selected check.\r\n").unwrap();
    assert!(matches!(
        kernel.set_project_guide(update(0, Some(first.sha256))),
        Err(Error::Conflict(_))
    ));
    let preview = kernel
        .preview_project_guide(&root.id)
        .unwrap()
        .guide
        .unwrap();
    let enabled = kernel
        .set_project_guide(update(0, Some(preview.sha256.clone())))
        .unwrap();
    let options = ContextOptions {
        project_guide_revision: Some(enabled.revision),
        ..Default::default()
    };
    assert!(
        kernel
            .session_context(&other_session.id, options.clone())
            .is_err()
    );
    assert!(
        kernel
            .session_context(
                &session.id,
                ContextOptions {
                    mode: ContextMode::Isolated,
                    ..options.clone()
                }
            )
            .is_err()
    );
    assert!(
        kernel
            .session_context(&session.id, ContextOptions::default())
            .unwrap()
            .project_guide
            .is_none()
    );
    let (task, created) = kernel
        .create_task_with_context(
            &session.id,
            "Use the guide",
            Some("guide-1"),
            TaskBackend::Model,
            Some(options.clone()),
        )
        .unwrap();
    assert!(created);
    assert_eq!(
        task.context.as_ref().unwrap().project_guide.as_ref(),
        Some(&preview)
    );
    let prompt = task_context_prompt(&task).unwrap().unwrap();
    assert!(prompt.contains("configured permissions take precedence"));
    assert!(prompt.contains(r#""text":"Run the selected check.\r\n""#));
    assert_eq!(
        kernel
            .task_context_attribution(&task.id)
            .unwrap()
            .project_guide,
        Some(ProjectGuideAttribution::from(&preview))
    );
    drop(kernel);

    fs::write(&path, "Guide changed after task start").unwrap();
    let mut kernel = Kernel::open(&db, Box::new(DemoProvider)).unwrap();
    assert_eq!(kernel.workspace_project_guide(&root.id).unwrap(), enabled);
    let (retry, created) = kernel
        .create_task_with_context(
            &session.id,
            "Use the guide",
            Some("guide-1"),
            TaskBackend::Model,
            Some(options.clone()),
        )
        .unwrap();
    assert!(!created);
    assert_eq!(retry.id, task.id);
    assert_eq!(task_context_prompt(&retry).unwrap().unwrap(), prompt);
    assert!(matches!(
        kernel.session_context(&session.id, options.clone()),
        Err(Error::Conflict(_))
    ));
    fs::remove_file(&path).unwrap();
    let disabled = kernel
        .set_project_guide(update(enabled.revision, None))
        .unwrap();
    assert_eq!(disabled.revision, enabled.revision + 1);
    assert_eq!(disabled.enabled_sha256, None);
    assert!(kernel.session_context(&session.id, options).is_err());
}

#[test]
fn preview_requires_root_regular_utf8_file_and_reports_limits() {
    let project = Project::new();
    let path = project.0.join(PROJECT_GUIDE_PATH);
    fs::create_dir(project.0.join("nested")).unwrap();
    let nested = project.0.join("nested").join(PROJECT_GUIDE_PATH);
    fs::write(&nested, "Nested guide").unwrap();
    let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
    let workspace = kernel.open_workspace(project.0.to_str().unwrap()).unwrap();
    let preview = |k: &Kernel| k.preview_project_guide(&workspace.id).unwrap();
    assert!(preview(&kernel).guide.is_none());
    fs::hard_link(&nested, &path).unwrap();
    assert!(
        preview(&kernel)
            .read_error
            .unwrap()
            .contains("without links")
    );
    fs::remove_file(&path).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&nested, &path).unwrap();
        assert!(
            preview(&kernel)
                .read_error
                .unwrap()
                .contains("without links")
        );
        fs::remove_file(&path).unwrap();
    }
    fs::write(&path, [0xFF]).unwrap();
    assert!(preview(&kernel).read_error.unwrap().contains("UTF-8"));
    fs::write(&path, "x".repeat(MAX_PROJECT_GUIDE_BYTES + 1)).unwrap();
    assert!(preview(&kernel).read_error.unwrap().contains("exceeds"));
    fs::write(&path, "x".repeat(2100)).unwrap();
    let digest = preview(&kernel).guide.unwrap().sha256;
    let enabled = kernel
        .set_project_guide(WorkspaceSetProjectGuideParams {
            workspace_id: workspace.id.clone(),
            expected_revision: 0,
            sha256: Some(digest),
        })
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    assert!(matches!(kernel.session_context(&session.id, ContextOptions{
        max_bytes:1024, project_guide_revision:Some(enabled.revision), ..Default::default()
    }), Err(Error::Invalid(message)) if message.contains("maxBytes")));
}
