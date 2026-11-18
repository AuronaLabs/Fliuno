use super::*;
#[test]
fn staged_executable_checks_actual_copied_bytes_and_never_overwrites_a_target() {
    let root = std::env::temp_dir().join(format!("fliuno-program-pin-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).unwrap();
    let source = root.join("source");
    std::fs::write(&source, b"approved executable bytes").unwrap();
    let expected = program_digest(&source).unwrap();
    std::fs::write(&source, b"replaced between registration and staging").unwrap();
    assert!(
        stage(&source, &root.join("mismatch"), Some(&expected))
            .unwrap_err()
            .to_string()
            .contains("pinned SHA-256")
    );
    let current = program_digest(&source).unwrap();
    stage(&source, &root.join("staged"), Some(&current)).unwrap();
    assert_eq!(program_digest(&root.join("staged")).unwrap(), current);
    assert!(stage(&source, &root.join("staged"), Some(&current)).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(any(windows, target_os = "linux"))]
#[test]
fn mismatched_native_pin_refuses_process_creation() {
    let error = crate::spawn(&crate::LaunchSpec {
        program: std::env::current_exe().unwrap(),
        program_sha256: Some("0".repeat(64)),
        assets: vec![],
        args: vec![],
        env: Default::default(),
        resources: Default::default(),
        linux_cgroup_parent: None,
    })
    .err()
    .unwrap();
    assert!(error.to_string().contains("pinned SHA-256"), "{error}");
}
