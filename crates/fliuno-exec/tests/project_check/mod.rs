use super::*;
use fliuno_exec::ProjectCheckConfig;
use sha2::{Digest, Sha256};

const PATH: &str = "src/greeting.txt";
const GOOD: &str = "Welcome to Fliuno\n";

fn project() -> Directory {
    let dir = Directory::new();
    fs::create_dir(dir.0.join("src")).unwrap();
    fs::write(dir.0.join(PATH), GOOD).unwrap();
    fs::write(dir.0.join("unselected.txt"), "not visible to the program").unwrap();
    dir
}

fn program() -> ProgramConfig {
    let mut program = config(if cfg!(windows) {
        ProcessIsolation::WindowsAppContainer
    } else {
        ProcessIsolation::LinuxLandlock
    });
    program.args = vec!["project-check".into(), PATH.into(), GOOD.trim().into()];
    program.project_check = Some(ProjectCheckConfig {
        label: "Greeting check".into(),
        description: "Check the greeting matches the expected text".into(),
        inputs: vec![PATH.into()],
    });
    program
}

fn inputs(content: &str) -> Value {
    json!({"inputs":{(PATH):format!("{:x}", Sha256::digest(content))}})
}

#[test]
fn native_check_uses_only_the_approved_snapshot_and_reports_pass_or_failure() {
    let dir = project();
    for (source, expected) in [(GOOD, 0), ("Incorrect greeting\n", 9)] {
        fs::write(dir.0.join(PATH), source).unwrap();
        let args = inputs(source);
        let (mut kernel, session) = setup(&dir, args.clone(), program());
        let task = kernel.start_task(&session.id, "check greeting").unwrap();
        let pending = task.pending_approval.as_ref().unwrap();
        let plan = pending.execution_plan.as_ref().unwrap();
        assert_eq!(plan["kind"], "project_check");
        assert_eq!(plan["args"], json!(program().args));
        assert_eq!(plan["inputs"], args["inputs"]);
        assert_eq!(plan["cwd"], "private_project_snapshot");
        assert_eq!(plan["workspaceWrites"], false);
        assert!(!dir.0.join("check-scratch.txt").exists());
        let completed = kernel
            .respond_approval(&task.id, &pending.id, ApprovalDecision::AllowOnce)
            .unwrap();
        assert_eq!(completed.status, TaskStatus::Completed, "{completed:?}");
        let result = output(&completed);
        assert_eq!(result["projectCheck"], *plan);
        assert_eq!(result["termination"], "exited");
        assert_eq!(result["exitCode"], expected, "{result}");
        assert_eq!(result["success"], expected == 0);
        if expected == 0 {
            assert_eq!(result["stdout"]["data"], "greeting check passed\n");
        } else {
            assert_eq!(result["stderr"]["data"], "greeting check failed\n");
        }
        assert_eq!(fs::read_to_string(dir.0.join(PATH)).unwrap(), source);
        assert!(!dir.0.join("check-scratch.txt").exists());
    }
}

#[test]
fn changed_input_is_rejected_before_launch_and_calls_cannot_enlarge_scope() {
    let dir = project();
    let args = inputs(GOOD);
    let (mut kernel, session) = setup(&dir, args.clone(), program());
    let task = kernel.start_task(&session.id, "check greeting").unwrap();
    fs::write(dir.0.join(PATH), "Changed after approval was requested\n").unwrap();
    let done = kernel
        .respond_approval(
            &task.id,
            &task.pending_approval.as_ref().unwrap().id,
            ApprovalDecision::AllowOnce,
        )
        .unwrap();
    assert_eq!(output(&done)["status"], "not_applied");
    assert_eq!(output(&done)["code"], "project_check_input_changed");
    assert!(output(&done).get("exitCode").is_none());
    for arguments in [
        json!({"inputs":args["inputs"],"args":["different-command"]}),
        json!({"inputs":{(PATH):args["inputs"][PATH],"unselected.txt":"0".repeat(64)}}),
    ] {
        let (mut kernel, session) = setup(&dir, arguments, program());
        let rejected = kernel.start_task(&session.id, "check greeting").unwrap();
        assert_eq!(rejected.status, TaskStatus::Failed);
        assert!(rejected.pending_approval.is_none());
    }
}

#[test]
fn registration_rejects_paths_that_escape_or_alias_the_staged_bundle() {
    let mut kernel = Kernel::in_memory(Box::new(DemoProvider)).unwrap();
    for paths in [
        vec!["../outside.txt"],
        vec!["src/check.txt", "SRC/other.txt"],
        vec!["src", PATH],
        vec!["program.exe"],
    ] {
        let mut candidate = program();
        candidate.project_check.as_mut().unwrap().inputs =
            paths.into_iter().map(str::to_owned).collect();
        assert!(
            register_config(
                &mut kernel,
                Config {
                    programs: vec![candidate]
                }
            )
            .is_err()
        );
    }
    let mut candidate = program();
    candidate.isolation = ProcessIsolation::TrustedHost;
    assert!(
        register_config(
            &mut kernel,
            Config {
                programs: vec![candidate]
            }
        )
        .is_err()
    );
}
