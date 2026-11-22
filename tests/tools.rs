use rho::tools::{truncate_tail, BashTool, EditTool, ReadTool, Tool, ToolResult, WriteTool};
use serde_json::json;
use std::path::PathBuf;

fn dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn read_returns_content() {
    let d = dir();
    std::fs::write(d.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let tool = ReadTool {
        pwd: d.path().to_path_buf(),
    };
    let r = tool.execute(&json!({ "path": "a.txt" }));
    assert!(!r.is_error);
    assert_eq!(r.output, "one\ntwo\nthree\n");
}

#[test]
fn read_offset_limit_and_truncation() {
    let d = dir();
    let content: String = (1..=100).map(|i| format!("line {i}\n")).collect();
    std::fs::write(d.path().join("big.txt"), &content).unwrap();
    let tool = ReadTool {
        pwd: d.path().to_path_buf(),
    };
    let r = tool.execute(&json!({ "path": "big.txt", "offset": 50, "limit": 10 }));
    assert!(r.output.starts_with("line 50\n"));
    assert!(r.output.contains("line 59"));
    assert!(!r.output.contains("line 60\n["));
    assert!(r.output.contains("truncated"));

    let limited: String = (1..=3000).map(|i| format!("{i}\n")).collect();
    std::fs::write(d.path().join("huge.txt"), &limited).unwrap();
    let r = tool.execute(&json!({ "path": "huge.txt" }));
    assert!(r.output.contains("truncated"));
    assert!(r.output.starts_with("1\n"));
}

#[test]
fn read_missing_file_is_error() {
    let d = dir();
    let tool = ReadTool {
        pwd: d.path().to_path_buf(),
    };
    let r = tool.execute(&json!({ "path": "nope.txt" }));
    assert!(r.is_error);
}

#[test]
fn bash_runs_and_captures_output_and_exit_code() {
    let d = dir();
    let tool = BashTool {
        pwd: d.path().to_path_buf(),
    };
    let r = tool.execute(&json!({ "command": "echo hello" }));
    assert_eq!(r.output, "hello\n");
    let r = tool.execute(&json!({ "command": "echo err >&2; false" }));
    assert!(r.is_error);
    assert!(r.output.contains("err"));
    assert!(r.output.contains("Exit code: 1"));
    let r = tool.execute(&json!({ "command": "true" }));
    assert_eq!(r.output, "(no output)");
}

#[test]
fn bash_timeout() {
    let d = dir();
    let tool = BashTool {
        pwd: d.path().to_path_buf(),
    };
    let r = tool.execute(&json!({ "command": "sleep 5", "timeout": 1 }));
    assert!(r.is_error);
    assert!(r.output.contains("timeout:1"));
}

#[test]
fn truncate_tail_keeps_end() {
    let text: String = (1..=3000).map(|i| format!("{i}\n")).collect();
    let t = truncate_tail(&text);
    assert!(t.contains("3000"));
    assert!(t.contains("earlier lines omitted"));
    assert!(!t.starts_with("1\n"));
}

#[test]
fn edit_applies_multiple_disjoint_edits() {
    let d = dir();
    std::fs::write(d.path().join("f.txt"), "alpha\nbeta\ngamma\n").unwrap();
    let tool = EditTool {
        pwd: d.path().to_path_buf(),
    };
    let r = tool.execute(&json!({
        "path": "f.txt",
        "edits": [
            { "oldText": "alpha", "newText": "ALPHA" },
            { "oldText": "gamma", "newText": "GAMMA\nextra" }
        ]
    }));
    assert!(!r.is_error, "{:?}", r.output);
    assert_eq!(
        std::fs::read_to_string(d.path().join("f.txt")).unwrap(),
        "ALPHA\nbeta\nGAMMA\nextra\n"
    );
}

#[test]
fn edit_rejects_missing_nonunique_and_overlapping() {
    let d = dir();
    std::fs::write(d.path().join("f.txt"), "x x y\n").unwrap();
    let tool = EditTool {
        pwd: d.path().to_path_buf(),
    };
    let r =
        tool.execute(&json!({ "path": "f.txt", "edits": [{ "oldText": "z", "newText": "q" }] }));
    assert!(r.is_error);
    let r =
        tool.execute(&json!({ "path": "f.txt", "edits": [{ "oldText": "x", "newText": "q" }] }));
    assert!(r.is_error && r.output.contains("not unique"));
    let r = tool.execute(&json!({
        "path": "f.txt",
        "edits": [
            { "oldText": "x x", "newText": "a" },
            { "oldText": " x y", "newText": "b" }
        ]
    }));
    assert!(r.is_error && r.output.contains("overlap"));
    assert_eq!(
        std::fs::read_to_string(d.path().join("f.txt")).unwrap(),
        "x x y\n"
    );
}

#[test]
fn write_creates_file_and_parent_dirs() {
    let d = dir();
    let tool = WriteTool {
        pwd: d.path().to_path_buf(),
    };
    let r = tool.execute(&json!({ "path": "sub/dir/f.txt", "content": "hi" }));
    assert!(!r.is_error);
    assert_eq!(
        std::fs::read_to_string(d.path().join("sub/dir/f.txt")).unwrap(),
        "hi"
    );
}

#[test]
fn missing_params_are_errors() {
    let d = dir();
    let pwd: PathBuf = d.path().to_path_buf();
    assert!(ReadTool { pwd: pwd.clone() }.execute(&json!({})).is_error);
    assert!(BashTool { pwd: pwd.clone() }.execute(&json!({})).is_error);
    assert!(
        EditTool { pwd: pwd.clone() }
            .execute(&json!({ "path": "x" }))
            .is_error
    );
    assert!(WriteTool { pwd }.execute(&json!({ "path": "x" })).is_error);
}

#[test]
fn tool_results_constructors() {
    assert!(!ToolResult::ok("a").is_error);
    assert!(ToolResult::err("a").is_error);
}
