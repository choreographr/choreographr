use crate::tools::ToolGroup;
use choreo_proto::ContextConfig;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    pub path: PathBuf,
    pub mtime: SystemTime,
    pub content: String,
}

pub struct ContextBundle {
    pub files: Vec<DiscoveredFile>,
    pub fingerprint: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct SkillFrontmatter {
    name: String,
    description: String,
}

#[derive(Debug, Clone)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone)]
pub struct LoadedSkill {
    pub name: String,
    pub body: String,
}

pub fn discover_context(working_dir: &Path, config: &ContextConfig) -> io::Result<ContextBundle> {
    let mut files = Vec::new();
    load_global_files(&mut files, config)?;
    load_project_files(working_dir, &mut files, config)?;
    let fingerprint = compute_fingerprint(&files);
    Ok(ContextBundle { files, fingerprint })
}

fn load_global_files(files: &mut Vec<DiscoveredFile>, config: &ContextConfig) -> io::Result<()> {
    if let Some(config_dir) = dirs::config_dir() {
        let path = config_dir.join("choreographr").join("AGENTS.md");
        if let Some(df) = try_load_file(&path) {
            files.push(df);
        }
    }

    if !config.disable_claude_code_prompt
        && let Some(home) = dirs::home_dir()
    {
        let path = home.join(".claude").join("CLAUDE.md");
        if let Some(df) = try_load_file(&path) {
            files.push(df);
        }
    }

    if let Some(home) = dirs::home_dir() {
        let path = home.join(".agents").join("AGENTS.md");
        if let Some(df) = try_load_file(&path) {
            files.push(df);
        }
    }

    Ok(())
}

fn load_project_files(
    working_dir: &Path,
    files: &mut Vec<DiscoveredFile>,
    config: &ContextConfig,
) -> io::Result<()> {
    let git_root = find_git_root(working_dir);
    let boundary = git_root.as_deref().unwrap_or_else(|| Path::new("/"));

    let mut seen = HashSet::new();
    let mut found = Vec::new();
    let mut current = Some(working_dir.to_path_buf());

    while let Some(dir) = current {
        for name in &config.context_file_names {
            let path = dir.join(name);
            if let Some(df) = try_load_file(&path)
                && seen.insert(df.path.clone())
            {
                found.push(df);
                break;
            }
        }

        if dir == boundary {
            break;
        }
        current = dir.parent().map(|p| p.to_path_buf());
    }

    found.reverse();
    files.append(&mut found);

    Ok(())
}

fn find_git_root(working_dir: &Path) -> Option<PathBuf> {
    let mut current = Some(working_dir.to_path_buf());
    while let Some(ref dir) = current {
        let git_path = dir.join(".git");
        if git_path.exists() {
            return Some(dir.clone());
        }
        let parent = dir.parent().map(|p| p.to_path_buf());
        if parent == current {
            break;
        }
        current = parent;
    }
    None
}

fn try_load_file(path: &Path) -> Option<DiscoveredFile> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return None;
    }
    let mtime = metadata.modified().ok()?;
    let content = fs::read_to_string(path).ok()?;
    let content = content.trim().to_string();
    if content.is_empty() {
        return None;
    }
    Some(DiscoveredFile {
        path: path.to_path_buf(),
        mtime,
        content,
    })
}

pub fn compute_fingerprint(files: &[DiscoveredFile]) -> u64 {
    let mut hasher = Sha256::new();
    let mut entries: Vec<(&Path, SystemTime)> =
        files.iter().map(|f| (f.path.as_path(), f.mtime)).collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    for (path, mtime) in &entries {
        hasher.update(path.as_os_str().as_encoded_bytes());
        if let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH) {
            hasher.update(dur.as_secs().to_le_bytes());
            hasher.update(dur.subsec_nanos().to_le_bytes());
        }
    }

    let hash = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&hash[..8]);
    u64::from_le_bytes(bytes)
}

pub fn assemble_context(bundle: &ContextBundle) -> String {
    if bundle.files.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    for file in &bundle.files {
        let path_display = file.path.display();
        out.push_str(&format!(
            "<agent_instructions path=\"{path_display}\">\n{}\n</agent_instructions>\n",
            file.content
        ));
    }
    out
}

pub fn build_base_prompt(
    skills: &[SkillMeta],
    groups: &[ToolGroup],
    loaded_skills: &[LoadedSkill],
) -> String {
    let user_prompt = load_user_system_prompt();
    let mut base = user_prompt.unwrap_or_else(default_system_prompt);

    // Tool group listing (always shown)
    base.push_str("\n\n## Tool groups\n");
    base.push_str("Tools are organized into groups. Only **core**, **git**, and **shell** are active by default. Use the `load_tools` tool to activate additional groups and `unload_tools` to deactivate them.\n\n");
    for g in groups {
        base.push_str(&format!("- **{}**: {}\n", g.name, g.description));
    }

    if !skills.is_empty() {
        base.push_str("\n## Available skills\n");
        base.push_str("Use the `load_skill` tool to load a skill's full instructions when a task matches its description:\n\n");
        for skill in skills {
            base.push_str(&format!("- **{}**: {}\n", skill.name, skill.description));
        }
    }

    if !loaded_skills.is_empty() {
        base.push_str(
            "\n## Loaded skills\nThe following skills have been loaded and are active:\n\n",
        );
        for ls in loaded_skills {
            base.push_str(&format!(
                "<skill name=\"{name}\">\n{body}\n</skill>\n\n",
                name = ls.name,
                body = ls.body
            ));
        }
    }
    base
}

fn load_user_system_prompt() -> Option<String> {
    let config_dir = dirs::config_dir()?;
    let path = config_dir.join("choreographr").join("system.md");
    let content = fs::read_to_string(&path).ok()?;
    let content = content.trim().to_string();
    if content.is_empty() {
        None
    } else {
        Some(content)
    }
}

fn default_system_prompt() -> String {
    include_str!("../system.md").to_string()
}

pub fn discover_skills(working_dir: &Path) -> Vec<SkillMeta> {
    let mut skills = Vec::new();
    let mut seen = HashSet::new();

    if let Some(home) = dirs::home_dir() {
        scan_skills_dir(&home.join(".agents").join("skills"), &mut skills, &mut seen);
    }

    let git_root = find_git_root(working_dir);
    let boundary = git_root.as_deref().unwrap_or_else(|| Path::new("/"));
    let mut current = Some(working_dir.to_path_buf());
    while let Some(dir) = current {
        scan_skills_dir(&dir.join(".agents").join("skills"), &mut skills, &mut seen);
        if dir == boundary {
            break;
        }
        current = dir.parent().map(|p| p.to_path_buf());
    }

    skills
}

fn scan_skills_dir(dir: &Path, skills: &mut Vec<SkillMeta>, seen: &mut HashSet<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let skill_md = path.join("SKILL.md");
        if let Some(meta) = parse_skill_metadata(&skill_md)
            && seen.insert(skill_md.clone())
        {
            skills.push(meta);
        }
    }
}

fn parse_skill_metadata(path: &Path) -> Option<SkillMeta> {
    let content = fs::read_to_string(path).ok()?;
    let frontmatter = extract_yaml_frontmatter(&content)?;
    let fm: SkillFrontmatter = yaml_serde::from_str(&frontmatter).ok()?;
    Some(SkillMeta {
        name: fm.name,
        description: fm.description,
        path: path.to_path_buf(),
    })
}

fn extract_yaml_frontmatter(content: &str) -> Option<String> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return None;
    }
    let rest = content
        .strip_prefix("---")?
        .strip_prefix('\n')
        .unwrap_or(content.strip_prefix("---")?);
    let end = rest.find("\n---")?;
    Some(rest[..end].trim().to_string())
}

pub fn load_skill_body(name: &str, working_dir: &Path) -> Option<String> {
    let skills = discover_skills(working_dir);
    let meta = skills.into_iter().find(|s| s.name == name)?;
    let content = fs::read_to_string(&meta.path).ok()?;
    let body = extract_skill_body(&content)?;
    Some(body)
}

fn extract_skill_body(content: &str) -> Option<String> {
    let content = content.trim_start();
    if !content.starts_with("---") {
        return None;
    }
    let rest = content
        .strip_prefix("---")?
        .strip_prefix('\n')
        .unwrap_or(content.strip_prefix("---")?);
    let end = rest.find("\n---")?;
    let body = rest[end + 4..].trim().to_string();
    if body.is_empty() { None } else { Some(body) }
}

pub fn recheck_context(
    working_dir: &Path,
    config: &ContextConfig,
    old_fingerprint: u64,
) -> io::Result<Option<ContextBundle>> {
    let bundle = discover_context(working_dir, config)?;
    if bundle.fingerprint == old_fingerprint {
        Ok(None)
    } else {
        Ok(Some(bundle))
    }
}

pub fn subdirectory_hints(
    tool_name: &str,
    arguments_json: &str,
    working_dir: Option<&Path>,
    known_paths: &[PathBuf],
) -> Option<(String, Vec<PathBuf>)> {
    let target_path = extract_tool_path(tool_name, arguments_json)?;
    let resolved = crate::tools::confine_path(&target_path, working_dir).ok()?;
    let parent = resolved.parent()?;
    let working_dir_canonical = working_dir
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let working_dir_canonical = working_dir_canonical
        .canonicalize()
        .unwrap_or_else(|_| working_dir_canonical.clone());

    let mut hints = Vec::new();
    let mut new_paths = Vec::new();
    let mut current = Some(parent.to_path_buf());
    while let Some(dir) = current {
        if !dir.starts_with(&working_dir_canonical) || dir == working_dir_canonical {
            break;
        }

        for name in &["AGENTS.md", "CLAUDE.md"] {
            let path = dir.join(name);
            if known_paths.iter().any(|kp| kp == &path) {
                continue;
            }
            if let Some(content) = read_hint_file(&path) {
                hints.push((path.clone(), content));
                new_paths.push(path);
                break;
            }
        }

        current = dir.parent().map(|p| p.to_path_buf());
    }

    if hints.is_empty() {
        return None;
    }

    let mut out = String::from("Context from subdirectory:\n\n");
    for (path, content) in hints.iter().rev() {
        out.push_str(&format!(
            "<agent_instructions path=\"{}\">\n{}\n</agent_instructions>\n",
            path.display(),
            content
        ));
    }
    Some((out, new_paths))
}

fn extract_tool_path(tool_name: &str, arguments_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(arguments_json).ok()?;
    match tool_name {
        "read_file" | "read_file_range" | "write_file" | "edit_file" => {
            v.get("path")?.as_str().map(|s| s.to_string())
        }
        "list_files" => v
            .get("path")
            .or_else(|| v.get("directory"))
            .and_then(|p| p.as_str())
            .map(|s| s.to_string()),
        "grep" => v
            .get("path")
            .and_then(|p| p.as_str())
            .map(|s| s.to_string()),
        "find" => v
            .get("path")
            .and_then(|p| p.as_str())
            .map(|s| s.to_string()),
        _ => None,
    }
}

fn read_hint_file(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let content = content.trim().to_string();
    if content.is_empty() {
        None
    } else {
        Some(content)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_file(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        fs::create_dir_all(dir).unwrap();
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    #[test]
    fn test_extract_yaml_frontmatter() {
        let content = "---\nname: test-skill\ndescription: A test skill\n---\n\n# Body";
        let fm = extract_yaml_frontmatter(content).unwrap();
        assert!(fm.contains("name: test-skill"));
        assert!(fm.contains("description: A test skill"));
    }

    #[test]
    fn test_extract_skill_body() {
        let content =
            "---\nname: test\ndescription: desc\n---\n\nThis is the body.\nMultiple lines.";
        let body = extract_skill_body(content).unwrap();
        assert_eq!(body, "This is the body.\nMultiple lines.");
    }

    #[test]
    fn test_extract_tool_path() {
        let path = extract_tool_path("read_file", r#"{"path": "src/main.rs"}"#).unwrap();
        assert_eq!(path, "src/main.rs");

        let path = extract_tool_path("list_files", r#"{"directory": "/tmp"}"#).unwrap();
        assert_eq!(path, "/tmp");

        assert!(extract_tool_path("git_status", r#"{}"#).is_none());
    }

    #[test]
    fn test_fingerprint_deterministic() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "AGENTS.md", "Test content");

        let bundle1 = discover_context(tmp.path(), &ContextConfig::default()).unwrap();
        let bundle2 = discover_context(tmp.path(), &ContextConfig::default()).unwrap();

        assert_eq!(bundle1.fingerprint, bundle2.fingerprint);
    }

    #[test]
    fn test_discover_context_from_tempdir() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "AGENTS.md", "Project rules");

        let bundle = discover_context(tmp.path(), &ContextConfig::default()).unwrap();
        assert!(!bundle.files.is_empty());
        assert!(
            bundle
                .files
                .iter()
                .any(|f| f.content.contains("Project rules"))
        );
    }

    #[test]
    fn test_recheck_unchanged() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "AGENTS.md", "unchanging");

        let config = ContextConfig::default();
        let bundle = discover_context(tmp.path(), &config).unwrap();
        let result = recheck_context(tmp.path(), &config, bundle.fingerprint).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn test_recheck_changed() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "AGENTS.md", "version 1");

        filetime::set_file_mtime(
            tmp.path().join("AGENTS.md"),
            filetime::FileTime::from_unix_time(0, 0),
        )
        .unwrap();

        let config = ContextConfig::default();
        let bundle = discover_context(tmp.path(), &config).unwrap();
        let fp = bundle.fingerprint;

        write_file(tmp.path(), "AGENTS.md", "version 2");

        let result = recheck_context(tmp.path(), &config, fp).unwrap();
        assert!(result.is_some());
        assert_ne!(result.unwrap().fingerprint, fp);
    }

    #[test]
    fn test_assemble_context_format() {
        let tmp = TempDir::new().unwrap();
        write_file(tmp.path(), "AGENTS.md", "test content");

        let bundle = discover_context(tmp.path(), &ContextConfig::default()).unwrap();
        let assembled = assemble_context(&bundle);

        assert!(assembled.contains("<agent_instructions"));
        assert!(assembled.contains("test content"));
        assert!(assembled.contains("</agent_instructions>"));
    }

    #[test]
    fn test_subdirectory_hints() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("subdir");
        fs::create_dir_all(&sub).unwrap();
        write_file(&sub, "AGENTS.md", "subdir hints");
        write_file(&sub, "file.txt", "hello");

        let file_path = sub.join("file.txt");
        let args = serde_json::json!({"path": file_path.to_str().unwrap()}).to_string();
        let hints = subdirectory_hints("read_file", &args, Some(tmp.path()), &[]);

        assert!(hints.is_some());
        let (hint_text, new_paths) = hints.unwrap();
        assert!(hint_text.contains("subdir hints"));
        assert!(new_paths.contains(&sub.join("AGENTS.md")));
    }

    #[test]
    fn test_subdirectory_hints_skips_known() {
        let tmp = TempDir::new().unwrap();
        let sub = tmp.path().join("subdir");
        fs::create_dir_all(&sub).unwrap();
        let agents_path = sub.join("AGENTS.md");
        write_file(&sub, "AGENTS.md", "already known");
        write_file(&sub, "file.txt", "hello");

        let file_path = sub.join("file.txt");
        let args = serde_json::json!({"path": file_path.to_str().unwrap()}).to_string();
        let hints = subdirectory_hints("read_file", &args, Some(tmp.path()), &[agents_path]);
        assert!(hints.is_none());
    }

    #[test]
    fn test_subdirectory_hints_tracks_multiple_new_paths() {
        let tmp = TempDir::new().unwrap();
        let sub0 = tmp.path().join("sub0");
        let sub1 = sub0.join("sub1");
        fs::create_dir_all(&sub1).unwrap();
        write_file(&sub0, "AGENTS.md", "outer hints");
        write_file(&sub1, "AGENTS.md", "inner hints");
        write_file(&sub1, "file.txt", "hello");

        let file_path = sub1.join("file.txt");
        let args = serde_json::json!({"path": file_path.to_str().unwrap()}).to_string();
        let hints = subdirectory_hints("read_file", &args, Some(tmp.path()), &[]);

        assert!(hints.is_some());
        let (hint_text, new_paths) = hints.unwrap();
        assert!(hint_text.contains("outer hints"));
        assert!(hint_text.contains("inner hints"));
        assert_eq!(new_paths.len(), 2);
    }

    #[test]
    fn test_skill_discovery() {
        let tmp = TempDir::new().unwrap();
        let skills_dir = tmp.path().join(".agents").join("skills").join("test-skill");
        fs::create_dir_all(&skills_dir).unwrap();
        write_file(
            &skills_dir,
            "SKILL.md",
            "---\nname: test-skill\ndescription: A test skill for testing\n---\n\n# Instructions",
        );

        let skills = discover_skills(tmp.path());
        assert!(skills.iter().any(|s| s.name == "test-skill"));
        assert!(
            skills
                .iter()
                .any(|s| s.description == "A test skill for testing")
        );
    }

    #[test]
    fn test_build_base_prompt_includes_skills() {
        let skills = vec![SkillMeta {
            name: "test-skill".to_string(),
            description: "A test skill".to_string(),
            path: PathBuf::from("/fake/SKILL.md"),
        }];

        let prompt = build_base_prompt(&skills, &[], &[]);
        assert!(prompt.contains("test-skill"));
        assert!(prompt.contains("A test skill"));
        assert!(prompt.contains("load_skill"));
    }

    #[test]
    fn test_build_base_prompt_includes_loaded_skills() {
        let loaded = vec![LoadedSkill {
            name: "loaded-skill".to_string(),
            body: "Loaded skill body content.".to_string(),
        }];

        let prompt = build_base_prompt(&[], &[], &loaded);
        assert!(prompt.contains("Loaded skills"));
        assert!(prompt.contains("loaded-skill"));
        assert!(prompt.contains("Loaded skill body content."));
        assert!(prompt.contains("<skill name=\"loaded-skill\">"));
    }

    #[test]
    fn test_load_skill_body() {
        let tmp = TempDir::new().unwrap();
        let skills_dir = tmp.path().join(".agents").join("skills").join("test-skill");
        fs::create_dir_all(&skills_dir).unwrap();
        write_file(
            &skills_dir,
            "SKILL.md",
            "---\nname: test-skill\ndescription: A test skill\n---\n\nThis is the skill body content.",
        );

        let body = load_skill_body("test-skill", tmp.path()).unwrap();
        assert!(body.contains("skill body content"));
    }
}
