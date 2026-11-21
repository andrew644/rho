use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::json;

use crate::types::ToolSpec;

pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub output: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self { output: output.into(), is_error: false }
    }

    pub fn err(output: impl Into<String>) -> Self {
        Self { output: output.into(), is_error: true }
    }
}

pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters(&self) -> serde_json::Value;
    fn execute(&self, args: &serde_json::Value) -> ToolResult;

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            kind: "function".to_string(),
            function: crate::types::ToolFunction {
                name: self.name().to_string(),
                description: self.description().to_string(),
                parameters: self.parameters(),
            },
        }
    }
}

pub fn all_tools(cwd: PathBuf) -> Vec<Box<dyn Tool>> {
    vec![
        Box::new(ReadTool { cwd: cwd.clone() }),
        Box::new(BashTool { cwd: cwd.clone() }),
        Box::new(EditTool { cwd: cwd.clone() }),
        Box::new(WriteTool { cwd }),
    ]
}

pub fn tool_names() -> Vec<&'static str> {
    vec!["read", "bash", "edit", "write"]
}

fn resolve(cwd: &Path, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

fn get_str<'a>(args: &'a serde_json::Value, key: &str) -> Result<&'a str, ToolResult> {
    args.get(key)
        .and_then(|v| v.as_str())
        .ok_or_else(|| ToolResult::err(format!("missing or invalid string parameter: {key}")))
}

pub struct ReadTool {
    pub cwd: PathBuf,
}

impl Tool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Text output is truncated to 2000 lines or 50KB. Use offset/limit for large files."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read (relative or absolute)" },
                "offset": { "type": "number", "description": "Line number to start reading from (1-indexed)" },
                "limit": { "type": "number", "description": "Maximum number of lines to read" }
            },
            "required": ["path"]
        })
    }

    fn execute(&self, args: &serde_json::Value) -> ToolResult {
        let path = match get_str(args, "path") {
            Ok(p) => resolve(&self.cwd, p),
            Err(e) => return e,
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("failed to read {}: {e}", path.display())),
        };
        let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(1).max(1) as usize;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(MAX_LINES)
            .min(MAX_LINES);
        let mut out = String::new();
        let mut lines_out = 0;
        let mut truncated = false;
        for (i, line) in content.lines().enumerate() {
            if i + 1 < offset {
                continue;
            }
            if lines_out >= limit || out.len() + line.len() + 1 > MAX_BYTES {
                truncated = true;
                break;
            }
            out.push_str(line);
            out.push('\n');
            lines_out += 1;
        }
        if truncated {
            out.push_str(&format!(
                "\n[truncated: showed {lines_out} lines from line {offset}; continue with offset {}]",
                offset + lines_out
            ));
        }
        ToolResult::ok(out)
    }
}

pub struct BashTool {
    pub cwd: PathBuf,
}

impl BashTool {
    fn run(&self, command: &str, timeout: Option<u64>) -> ToolResult {
        let mut child = match Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(&self.cwd)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("failed to spawn bash: {e}")),
        };
        let output = match timeout {
            Some(secs) => {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
                loop {
                    match child.try_wait() {
                        Ok(Some(_)) => break child.wait_with_output(),
                        Ok(None) if std::time::Instant::now() < deadline => {
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        Ok(None) => {
                            let _ = child.kill();
                            let _ = child.wait();
                            return ToolResult::err(format!("timeout:{secs}"));
                        }
                        Err(e) => return ToolResult::err(format!("wait failed: {e}")),
                    }
                }
            }
            None => child.wait_with_output(),
        };
        let output = match output {
            Ok(o) => o,
            Err(e) => return ToolResult::err(format!("failed to run command: {e}")),
        };
        let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !stderr.is_empty() {
            if !combined.is_empty() && !combined.ends_with('\n') {
                combined.push('\n');
            }
            combined.push_str(&stderr);
        }
        let combined = truncate_tail(&combined);
        if output.status.success() {
            ToolResult::ok(if combined.is_empty() {
                "(no output)".to_string()
            } else {
                combined
            })
        } else {
            let code = output.status.code().map(|c| c.to_string()).unwrap_or_default();
            ToolResult::err(format!("{combined}\nExit code: {code}"))
        }
    }
}

pub fn truncate_tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut start = lines.len().saturating_sub(MAX_LINES);
    let mut out = lines[start..].join("\n");
    if out.len() > MAX_BYTES {
        let mut cut = out.len() - MAX_BYTES;
        while !out.is_char_boundary(cut) {
            cut += 1;
        }
        start += out[..cut].matches('\n').count() + 1;
        out = out[cut..].to_string();
    }
    if start > 0 {
        format!("[{start} earlier lines omitted]\n{out}")
    } else if text.ends_with('\n') && !out.is_empty() && !out.ends_with('\n') {
        format!("{out}\n")
    } else {
        out
    }
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to the last 2000 lines or 50KB."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "Shell command to execute" },
                "timeout": { "type": "number", "description": "Timeout in seconds (optional, no default timeout)" }
            },
            "required": ["command"]
        })
    }

    fn execute(&self, args: &serde_json::Value) -> ToolResult {
        let command = match get_str(args, "command") {
            Ok(c) => c,
            Err(e) => return e,
        };
        let timeout = args.get("timeout").and_then(|v| v.as_u64());
        self.run(command, timeout)
    }
}

pub struct EditTool {
    pub cwd: PathBuf,
}

impl Tool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a file with exact text replacement. Each edits[].oldText must match a unique, non-overlapping region of the original file."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit (relative or absolute)" },
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements. Each edit is matched against the original file.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": { "type": "string", "description": "Exact text to replace. Must be unique and non-overlapping." },
                            "newText": { "type": "string", "description": "Replacement text." }
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    fn execute(&self, args: &serde_json::Value) -> ToolResult {
        let path = match get_str(args, "path") {
            Ok(p) => resolve(&self.cwd, p),
            Err(e) => return e,
        };
        let edits = match args.get("edits").and_then(|v| v.as_array()) {
            Some(e) if !e.is_empty() => e,
            _ => return ToolResult::err("edits must contain at least one replacement"),
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return ToolResult::err(format!("failed to read {}: {e}", path.display())),
        };

        let mut matches: Vec<(usize, usize, &str)> = Vec::new();
        for edit in edits {
            let old = match edit.get("oldText").and_then(|v| v.as_str()) {
                Some(s) if !s.is_empty() => s,
                _ => return ToolResult::err("each edit needs a non-empty oldText"),
            };
            let count = content.matches(old).count();
            if count == 0 {
                return ToolResult::err(format!("oldText not found in {}: {old:?}", path.display()));
            }
            if count > 1 {
                return ToolResult::err(format!("oldText is not unique ({count} matches) in {}", path.display()));
            }
            let start = content.find(old).unwrap();
            matches.push((start, start + old.len(), old));
        }
        matches.sort_by_key(|(s, _, _)| *s);
        for w in matches.windows(2) {
            if w[1].0 < w[0].1 {
                return ToolResult::err("edits overlap; merge them into one edit");
            }
        }

        let mut result = String::with_capacity(content.len());
        let mut pos = 0;
        for (start, end, old) in &matches {
            let new = edits
                .iter()
                .find(|e| e.get("oldText").and_then(|v| v.as_str()) == Some(*old))
                .and_then(|e| e.get("newText").and_then(|v| v.as_str()))
                .unwrap_or("");
            result.push_str(&content[pos..*start]);
            result.push_str(new);
            pos = *end;
        }
        result.push_str(&content[pos..]);

        match std::fs::write(&path, result) {
            Ok(()) => ToolResult::ok(format!("Edited {}", path.display())),
            Err(e) => ToolResult::err(format!("failed to write {}: {e}", path.display())),
        }
    }
}

pub struct WriteTool {
    pub cwd: PathBuf,
}

impl Tool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories."
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to write (relative or absolute)" },
                "content": { "type": "string", "description": "Content to write to the file" }
            },
            "required": ["path", "content"]
        })
    }

    fn execute(&self, args: &serde_json::Value) -> ToolResult {
        let path = match get_str(args, "path") {
            Ok(p) => resolve(&self.cwd, p),
            Err(e) => return e,
        };
        let content = match get_str(args, "content") {
            Ok(c) => c,
            Err(e) => return e,
        };
        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return ToolResult::err(format!("failed to create {}: {e}", parent.display()));
            }
        }
        match std::fs::write(&path, content) {
            Ok(()) => ToolResult::ok(format!("Wrote {} bytes to {}", content.len(), path.display())),
            Err(e) => ToolResult::err(format!("failed to write {}: {e}", path.display())),
        }
    }
}
