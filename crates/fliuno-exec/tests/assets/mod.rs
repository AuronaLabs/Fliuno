use super::*;
use sha2::{Digest, Sha256};
fn bundled(directory: &Directory, bytes: &[u8]) -> ProgramConfig {
    let path = directory.0.join("companion.txt");
    fs::write(&path, bytes).unwrap();
    let mut program = config(if cfg!(windows) {
        ProcessIsolation::WindowsAppContainer
    } else {
        ProcessIsolation::LinuxLandlock
    });
    program.assets = vec![fliuno_sandbox::StagedFile {
        source: path,
        path: "data/config.txt".into(),
        sha256: format!("{:x}", Sha256::digest(bytes)),
        executable: false,
    }];
    program
}

#[test]
fn pinned_companion_is_read_inside_isolation_and_changed_bytes_fail_after_consent() {
    let directory = Directory::new();
    let program = bundled(&directory, "运行资源\n".as_bytes());
    let args = json!({"args":["asset","data/config.txt"]});
    let (mut kernel, session) = setup(&directory, args.clone(), program.clone());
    let completed = approve(&mut kernel, &session);
    assert_eq!(completed.status, TaskStatus::Completed, "{completed:?}");
    let report = output(&completed);
    assert_eq!(report["success"], true, "{report}");
    let probe: Value = serde_json::from_str(report["stdout"]["data"].as_str().unwrap()).unwrap();
    assert_eq!(probe["asset"], "运行资源\n");
    assert_eq!(probe["isolated"], true);
    let (mut kernel, session) = setup(&directory, args, program.clone());
    let waiting = kernel
        .start_task(&session.id, "change after consent")
        .unwrap();
    fs::write(&program.assets[0].source, b"changed after proposal").unwrap();
    let failed = kernel
        .respond_approval(
            &waiting.id,
            &waiting.pending_approval.unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap();
    assert_eq!(failed.status, TaskStatus::Failed, "{failed:?}");
    assert!(
        failed
            .output
            .unwrap()
            .contains("cannot launch isolated program")
    );
}

#[test]
fn restart_with_a_new_bundle_cannot_rebind_pending_execution_approval() {
    let directory = Directory::new();
    let database = directory.0.join("kernel.db");
    let program = bundled(&directory, b"before");
    let args = json!({"args":["asset","data/config.txt"]});
    let mut kernel = Kernel::open(&database, Box::new(Provider(args.clone()))).unwrap();
    register_config(
        &mut kernel,
        Config {
            programs: vec![program],
        },
    )
    .unwrap();
    let workspace = kernel
        .open_workspace(directory.0.to_str().unwrap())
        .unwrap();
    let session = kernel.create_session(&workspace.id).unwrap();
    let waiting = kernel.start_task(&session.id, "old bundle").unwrap();
    let old_name = waiting.pending_approval.as_ref().unwrap().call.name.clone();
    drop(kernel);
    let changed = bundled(&directory, b"after");
    let mut kernel = Kernel::open(&database, Box::new(Provider(args))).unwrap();
    register_config(
        &mut kernel,
        Config {
            programs: vec![changed],
        },
    )
    .unwrap();
    assert!(!kernel.tool_specs().iter().any(|spec| spec.name == old_name));
    let failed = kernel
        .respond_approval(
            &waiting.id,
            &waiting.pending_approval.unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap();
    assert_eq!(failed.status, TaskStatus::Failed);
}
