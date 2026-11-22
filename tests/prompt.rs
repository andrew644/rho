use rho::prompt::{build_system_prompt, load_context_files};
use std::path::Path;

#[test]
fn includes_sections_and_tools() {
    let d = tempfile::tempdir().unwrap();
    let prompt = build_system_prompt(Path::new(d.path()));
    assert!(prompt.contains("running inside rho"));
    assert!(prompt.contains("<tools>"));
    assert!(prompt.contains("- read: Read file contents"));
    assert!(prompt.contains("<rules>"));
    assert!(prompt.contains("<pwd>"));
    assert!(!prompt.contains("<project_context>"));
}

#[test]
fn includes_agents_md_context() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("AGENTS.md"), "Be nice.\n").unwrap();
    let files = load_context_files(d.path());
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].1, "Be nice.\n");
    let prompt = build_system_prompt(d.path());
    assert!(prompt.contains("<project_context>"));
    assert!(prompt.contains("Be nice."));
}
