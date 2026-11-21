use std::path::Path;

const CONTEXT_FILES: [&str; 1] = ["AGENTS.md"];

pub fn load_context_files(cwd: &Path) -> Vec<(String, String)> {
    CONTEXT_FILES
        .iter()
        .filter_map(|name| {
            let path = cwd.join(name);
            let content = std::fs::read_to_string(&path).ok()?;
            Some((name.to_string(), content))
        })
        .collect()
}

fn tool_snippets() -> Vec<(&'static str, &'static str)> {
    vec![
        ("read", "Read file contents"),
        ("bash", "Execute bash commands (ls, grep, find, etc.)"),
        (
            "edit",
            "Make precise file edits with exact text replacement, including multiple disjoint edits in one call",
        ),
        ("write", "Create or overwrite files"),
    ]
}

pub fn build_system_prompt(cwd: &Path) -> String {
    let mut sections = Vec::new();

    sections.push(
        "You are a coding assistant running inside rho, a coding agent harness. \
         Help users inspect files, run commands, and modify code."
            .to_string(),
    );

    let tools_list = tool_snippets()
        .iter()
        .map(|(name, snippet)| format!("- {name}: {snippet}"))
        .collect::<Vec<_>>()
        .join("\n");
    sections.push(format!(
        "<tools>\n{tools_list}\n</tools>"
    ));

    let rules = [
        "Use bash for file operations like ls, rg, find",
        "Use read to examine files instead of cat or sed.",
        "Use edit for targeted changes. Batch changes to one file in a single call.",
        "Each edits[].oldText must exactly and uniquely match the original file. Keep matches short and non-overlapping.",
        "Use write only for new files or complete rewrites.",
        "Be concise in your responses",
        "Show file paths clearly when working with files",
    ];
    sections.push(format!(
        "<rules>\n{}\n</rules>",
        rules
            .iter()
            .map(|r| format!("- {r}"))
            .collect::<Vec<_>>()
            .join("\n")
    ));

    let context_files = load_context_files(cwd);
    if !context_files.is_empty() {
        let rendered = context_files
            .iter()
            .map(|(path, content)| format!("<project_instructions path=\"{path}\">\n{content}\n</project_instructions>"))
            .collect::<Vec<_>>()
            .join("\n\n");
        sections.push(format!(
            "<project_context>\nProject-specific instructions and guidelines:\n\n{rendered}\n</project_context>"
        ));
    }

    sections.push(format!("<cwd>\n{}\n</cwd>", cwd.display()));

    sections.join("\n\n")
}
