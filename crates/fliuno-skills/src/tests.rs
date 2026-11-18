use super::*;
use crate::metadata::parse;
#[test]
fn frontmatter_and_folder_names_are_validated() {
    let content = "---\nname: test-skill\ndescription: A useful skill\nallowed-tools: shell\n---\n# Instructions\n";
    let meta = parse(content, "test-skill").unwrap();
    assert_eq!(meta.allowed_tools.as_deref(), Some("shell"));
    assert!(parse(content, "other-folder").is_err());
    assert!(parse("# no frontmatter", "test").is_err());
    assert!(
        parse(
            "---\nname: test--skill\ndescription: test\n---\n",
            "test--skill"
        )
        .is_err()
    );
}
#[test]
fn catalog_progressive_read_does_not_execute_skill_instructions() {
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };
    let root = std::env::temp_dir().join(format!(
        "fliuno-skills-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(root.join("example")).unwrap();
    let content = "---\nname: example\ndescription: Test discovery\n---\nUNTRUSTED INSTRUCTIONS\n";
    fs::write(root.join("example/SKILL.md"), content).unwrap();
    let catalog = SkillCatalog::open(&root).unwrap();
    assert_eq!(catalog.list()[0].name, "example");
    assert!(
        !serde_json::to_string(&catalog.list())
            .unwrap()
            .contains("UNTRUSTED")
    );
    assert_eq!(catalog.read("example").unwrap(), content);
    assert!(catalog.read("../escape").is_err());
    drop(catalog);
    fs::remove_dir_all(root).unwrap();
}
