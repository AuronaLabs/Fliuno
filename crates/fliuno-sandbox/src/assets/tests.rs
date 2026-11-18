use super::*;
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("fliuno-assets-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        fs::write(root.join("source"), b"approved bytes").unwrap();
        Self(root)
    }
    fn file(&self, path: &str) -> StagedFile {
        StagedFile {
            source: self.0.join("source"),
            path: path.into(),
            sha256: format!("{:x}", Sha256::digest(b"approved bytes")),
            executable: false,
        }
    }
    fn scratch(&self) -> PathBuf {
        let path = self.0.join("scratch");
        fs::create_dir(&path).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn staged_bytes_are_private_copies_and_changed_sources_fail_pins() {
    let fixture = Fixture::new();
    let file = fixture.file("runtime/data.txt");
    let scratch = fixture.scratch();
    stage(&scratch, std::slice::from_ref(&file)).unwrap();
    assert_eq!(
        fs::read(scratch.join("runtime/data.txt")).unwrap(),
        b"approved bytes"
    );
    fs::write(&file.source, b"modified after approval").unwrap();
    assert_eq!(
        fs::read(scratch.join("runtime/data.txt")).unwrap(),
        b"approved bytes"
    );
    let other = fixture.0.join("other");
    fs::create_dir(&other).unwrap();
    assert!(
        stage(&other, &[file])
            .unwrap_err()
            .to_string()
            .contains("pinned SHA-256")
    );
}

#[test]
fn complete_manifest_rejects_escapes_devices_case_and_prefix_collisions_before_copy() {
    let fixture = Fixture::new();
    for path in [
        "",
        "/absolute",
        "../escape",
        "a/../b",
        "a//b",
        "a/",
        "C:/escape",
        "a\\b",
        "file:stream",
        "CON",
        "Lpt1.txt",
        "a/NUL.x",
        "a.",
        "program",
        "program.exe",
        "PROGRAM.exe/a",
    ] {
        assert!(
            validate_staged_files(&[fixture.file(path)]).is_err(),
            "accepted {path:?}"
        );
    }
    for paths in [
        vec!["a", "A"],
        vec!["a", "a-guard", "a/child"],
        vec!["Lib/a", "lib/b"],
    ] {
        let files = paths.iter().map(|p| fixture.file(p)).collect::<Vec<_>>();
        let scratch = fixture.scratch();
        assert!(stage(&scratch, &files).is_err(), "accepted {paths:?}");
        assert_eq!(fs::read_dir(&scratch).unwrap().count(), 0);
        fs::remove_dir(scratch).unwrap();
    }
}

#[test]
fn sparse_large_files_and_excessive_manifests_are_rejected_before_reading() {
    let fixture = Fixture::new();
    let file = fixture.file("payload");
    fs::OpenOptions::new()
        .write(true)
        .open(&file.source)
        .unwrap()
        .set_len(MAX_ASSET_BYTES + 1)
        .unwrap();
    assert!(validate_staged_files(std::slice::from_ref(&file)).is_err());
    fs::OpenOptions::new()
        .write(true)
        .open(&file.source)
        .unwrap()
        .set_len(MAX_ASSET_BYTES)
        .unwrap();
    let files: Vec<_> = (0..5)
        .map(|index| {
            let mut file = file.clone();
            file.path = format!("part-{index}");
            file
        })
        .collect();
    // Reuse a sparse source: five valid individual sizes exceed the bundle
    // budget. Validation must fail without hashing or copying these bytes.
    let scratch = fixture.scratch();
    assert!(
        stage(&scratch, &files)
            .unwrap_err()
            .to_string()
            .contains("512 MiB")
    );
    assert_eq!(fs::read_dir(scratch).unwrap().count(), 0);
    assert!(validate_staged_files(&vec![file; MAX_BUNDLE_FILES + 1]).is_err());
}

#[test]
fn bundle_identity_binds_hash_destination_source_and_initial_mode() {
    let fixture = Fixture::new();
    let file = fixture.file("payload");
    let identity = staged_files_digest(std::slice::from_ref(&file)).unwrap();
    let mut changed = file.clone();
    changed.sha256 = "0".repeat(64);
    assert_ne!(identity, staged_files_digest(&[changed]).unwrap());
    let mut changed = file.clone();
    changed.path = "other".into();
    assert_ne!(identity, staged_files_digest(&[changed]).unwrap());
    let mut changed = file.clone();
    changed.source = fixture.0.join("another");
    assert_ne!(identity, staged_files_digest(&[changed]).unwrap());
    let mut changed = file;
    changed.executable = true;
    assert_ne!(identity, staged_files_digest(&[changed]).unwrap());
}

#[cfg(unix)]
#[test]
fn symlink_sources_and_inserted_destination_links_cannot_escape_staging_root() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new();
    let mut file = fixture.file("inside/payload");
    let scratch = fixture.scratch();
    let outside = fixture.0.join("outside");
    fs::create_dir(&outside).unwrap();
    symlink(&outside, scratch.join("inside")).unwrap();
    assert!(stage(&scratch, std::slice::from_ref(&file)).is_err());
    assert!(!outside.join("payload").exists());
    let link = fixture.0.join("source-link");
    symlink(&file.source, &link).unwrap();
    file.source = link;
    assert!(validate_staged_files(&[file]).is_err());
}
