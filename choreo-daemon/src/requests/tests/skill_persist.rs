//! skill persist tests (see the parent `tests` module).

use super::*;

#[test]
fn extract_json_string_gets_value() {
    let json = r#"{"name": "test-skill", "path": "src/main.rs"}"#;
    assert_eq!(
        extract_json_string(json, "name").as_deref(),
        Some("test-skill")
    );
    assert_eq!(
        extract_json_string(json, "path").as_deref(),
        Some("src/main.rs")
    );
}

#[test]
fn extract_json_string_missing_key() {
    assert_eq!(extract_json_string(r#"{"other": "val"}"#, "name"), None);
}

#[test]
fn extract_json_string_invalid_json() {
    assert_eq!(extract_json_string("not json", "name"), None);
}

// -- persist_loaded_skill tests -----------------------------------------

#[test]
fn persist_loaded_skill_adds_to_session() {
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join(".agents/skills/test-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "\
---\n\
name: test-skill\n\
description: A test skill\n\
---\n\
Hello, this is the skill body.\n\
---\n",
    )
    .unwrap();

    let mut session = SessionState::empty();
    session.config.working_dir = Some(dir.path().to_path_buf());
    assert!(session.loaded_skill_bodies.is_empty());

    persist_loaded_skill(&mut session, "load_skill", r#"{"name": "test-skill"}"#);

    assert_eq!(session.loaded_skill_bodies.len(), 1);
    assert_eq!(session.loaded_skill_bodies[0].name, "test-skill");
    assert!(session.loaded_skill_bodies[0].body.contains("skill body"));
}

#[test]
fn persist_loaded_skill_skips_non_load_skill() {
    let mut session = SessionState::empty();
    persist_loaded_skill(&mut session, "read_file", r#"{"path": "Cargo.toml"}"#);
    assert!(session.loaded_skill_bodies.is_empty());
}

#[test]
fn persist_loaded_skill_skips_missing_name() {
    let mut session = SessionState::empty();
    session.config.working_dir = Some(PathBuf::from("/tmp"));
    persist_loaded_skill(&mut session, "load_skill", r"{}");
    assert!(session.loaded_skill_bodies.is_empty());
}

#[test]
fn persist_loaded_skill_without_working_dir_unknown_skill_not_added() {
    // A dir-less session has no project scope, but the load path must not
    // panic; an obviously absent skill name leaves the accumulator empty. The
    // discovered-skill cache is seeded EMPTY so the persistence path resolves
    // against the cached (production) snapshot instead of falling back to
    // ambient discovery — the test is deterministic and never touches the
    // developer's real ~/.agents/skills.
    let mut session = SessionState::empty();
    session.discovered_skills = Some(Vec::new());
    persist_loaded_skill(
        &mut session,
        "load_skill",
        r#"{"name": "definitely-no-such-skill-xyz"}"#,
    );
    assert!(session.loaded_skill_bodies.is_empty());
}

#[test]
fn persist_loaded_skill_reads_body_from_cached_skills() {
    // The persistence path must reuse the session's discovered-skill cache
    // rather than re-walking the filesystem: populate the cache with a meta
    // whose SKILL.md lives in a temp dir and confirm its body is read.
    let dir = tempfile::tempdir().unwrap();
    let skill_md = dir.path().join("SKILL.md");
    std::fs::write(
        &skill_md,
        "---\nname: cached-skill\ndescription: cached\n---\n\ncached body text\n---\n",
    )
    .unwrap();

    let mut session = SessionState::empty();
    session.discovered_skills = Some(vec![crate::context::SkillMeta {
        name: "cached-skill".into(),
        description: "cached".into(),
        path: skill_md,
    }]);

    persist_loaded_skill(&mut session, "load_skill", r#"{"name": "cached-skill"}"#);

    assert_eq!(session.loaded_skill_bodies.len(), 1);
    assert_eq!(session.loaded_skill_bodies[0].name, "cached-skill");
    assert!(
        session.loaded_skill_bodies[0]
            .body
            .contains("cached body text"),
        "body: {}",
        session.loaded_skill_bodies[0].body
    );
}

// -- build_system_content tests -----------------------------------------
