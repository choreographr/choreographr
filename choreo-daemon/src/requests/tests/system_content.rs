//! system content tests (see the parent `tests` module).

use super::*;

#[test]
fn build_system_content_with_working_dir() {
    let (mut session, registry, _dir) = setup_build_system_content_session();
    let content = test_build_content(&mut session, &registry, &[]);
    assert!(content.contains("Tool groups"));
    assert!(content.contains("core"));
    assert!(content.contains("Project rules"));
}

#[test]
fn build_system_content_without_working_dir() {
    // A dir-less session still receives the full base prompt (identity, tool
    // groups, skills, title); only project context files are absent.
    let mut session = SessionState::empty();
    let registry = ToolRegistry::new().build();
    let content = test_build_content(&mut session, &registry, &[]);
    assert!(content.contains("Tool groups"));
    // The structural marker for injected project-context files is absent —
    // a stronger, more durable check than grepping for a filename that the
    // base prompt could legitimately mention someday.
    assert!(!content.contains("<agent_instructions"));
}

#[test]
fn build_system_content_includes_loaded_skills() {
    let (mut session, registry, _dir) = setup_build_system_content_session();
    session.loaded_skill_bodies.push(LoadedSkill {
        name: "loaded-test".to_string(),
        body: "Loaded body text.".to_string(),
    });
    let content = test_build_content(&mut session, &registry, &[]);
    assert!(content.contains("Loaded skills"));
    assert!(content.contains("loaded-test"));
    assert!(content.contains("Loaded body text."));
}

#[test]
fn build_system_content_populates_context_cache() {
    let (mut session, registry, _dir) = setup_build_system_content_session();
    assert!(session.context_cache.is_none());

    let _ = test_build_content(&mut session, &registry, &[]);
    assert!(
        session.context_cache.is_some(),
        "context_cache should be populated after first call"
    );
    let (fp, _) = session.context_cache.as_ref().unwrap();
    assert!(*fp > 0, "fingerprint should be non-zero");
}

#[test]
fn build_system_content_includes_pending_hints() {
    let (mut session, registry, _dir) = setup_build_system_content_session();
    let pending_hints = vec!["Hint about subdirectory config.".to_string()];
    let content = test_build_content(&mut session, &registry, &pending_hints);
    assert!(content.contains("New context from project subdirectories"));
    assert!(content.contains("Hint about subdirectory config."));
}

#[test]
fn build_system_content_includes_session_title() {
    let (mut session, registry, _dir) = setup_build_system_content_session();
    session.config.title = Some("Refactoring the database layer".into());
    let content = test_build_content(&mut session, &registry, &[]);
    assert!(content.contains("## Current Session Title"));
    assert!(content.contains("Refactoring the database layer"));
}

#[test]
fn build_system_content_omits_empty_title() {
    let (mut session, registry, _dir) = setup_build_system_content_session();
    // Title is None by default — no "Current Session Title" section.
    let content = test_build_content(&mut session, &registry, &[]);
    assert!(!content.contains("## Current Session Title"));

    // Also omit when the title is an empty string.
    session.config.title = Some(String::new());
    let content2 = test_build_content(&mut session, &registry, &[]);
    assert!(!content2.contains("## Current Session Title"));
}
