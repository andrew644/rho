use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub body: String,
}

pub fn template_dirs(user_dir: PathBuf, cwd: &Path) -> Vec<PathBuf> {
    vec![user_dir, cwd.join(".rho").join("prompts")]
}

pub fn load_templates(dirs: &[PathBuf]) -> Vec<Template> {
    let mut templates = Vec::new();
    let mut names: Vec<String> = Vec::new();
    for dir in dirs {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if names.iter().any(|n| n == stem) {
                continue;
            }
            let Ok(content) = fs::read_to_string(&path) else {
                continue;
            };
            names.push(stem.to_string());
            templates.push(parse_template(stem, &content));
        }
    }
    templates
}

pub fn parse_template(name: &str, content: &str) -> Template {
    let mut description = String::new();
    let mut argument_hint = None;
    let mut body = content;

    if let Some(rest) = content.strip_prefix("---\n") {
        if let Some(end) = rest.find("\n---") {
            let front = &rest[..end];
            let mut after = &rest[end + 4..];
            if let Some(stripped) = after.strip_prefix('\n') {
                after = stripped;
            }
            body = after;
            for line in front.lines() {
                if let Some(v) = line.strip_prefix("description:") {
                    description = v.trim().trim_matches('"').to_string();
                } else if let Some(v) = line.strip_prefix("argument-hint:") {
                    let v = v.trim().trim_matches('"');
                    if !v.is_empty() {
                        argument_hint = Some(v.to_string());
                    }
                }
            }
        }
    }

    if description.is_empty() {
        description = body
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .chars()
            .take(80)
            .collect();
    }

    Template {
        name: name.to_string(),
        description,
        argument_hint,
        body: body.to_string(),
    }
}

pub fn tokenize_args(input: &str) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                if c == '\\' && chars.peek() == Some(&q) {
                    current.push(q);
                    chars.next();
                } else if c == q {
                    quote = None;
                } else {
                    current.push(c);
                }
            }
            None => {
                if (c == '"' || c == '\'') && current.is_empty() && !started {
                    quote = Some(c);
                    started = true;
                } else if c.is_whitespace() {
                    if started {
                        args.push(std::mem::take(&mut current));
                        started = false;
                    }
                } else {
                    started = true;
                    current.push(c);
                }
            }
        }
    }
    if started {
        args.push(current);
    }
    args
}

fn arg_at(args: &[String], n: usize) -> Option<String> {
    args.get(n.checked_sub(1)?).cloned()
}

fn expand_ref(inner: &str, args: &[String]) -> Option<String> {
    if let Some((head, default)) = inner.split_once(":-") {
        let head = head.trim();
        let value = if head == "@" {
            args.join(" ")
        } else {
            arg_at(args, head.parse::<usize>().unwrap_or(0)).unwrap_or_default()
        };
        return Some(if value.is_empty() {
            default.to_string()
        } else {
            value
        });
    }
    if let Some(rest) = inner.strip_prefix('@') {
        let rest = rest.strip_prefix(':').unwrap_or("");
        if rest.is_empty() {
            return Some(args.join(" "));
        }
        let (n, len) = rest.split_once(':').unwrap_or((rest, ""));
        let n: usize = n.parse().ok()?;
        let len: usize = if len.is_empty() {
            usize::MAX
        } else {
            len.parse().ok()?
        };
        let start = n.saturating_sub(1).min(args.len());
        let end = start.saturating_add(len).min(args.len());
        return Some(args[start..end].join(" "));
    }
    if let Ok(n) = inner.parse::<usize>() {
        return Some(arg_at(args, n).unwrap_or_default());
    }
    None
}

pub fn expand_template(body: &str, args: &[String]) -> String {
    let mut out = String::with_capacity(body.len());
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'$' {
            let start = i;
            while i < bytes.len() && bytes[i] != b'$' {
                i += 1;
            }
            out.push_str(&body[start..i]);
            continue;
        }
        let j = i + 1;
        if j >= bytes.len() {
            out.push('$');
            i += 1;
            continue;
        }
        if bytes[j] == b'{' {
            if let Some(close) = body[j + 1..].find('}') {
                let inner = &body[j + 1..j + 1 + close];
                match expand_ref(inner.trim(), args) {
                    Some(value) => out.push_str(&value),
                    None => out.push_str(&format!("${{{inner}}}")),
                }
                i = j + 2 + close;
                continue;
            }
            out.push('$');
            i += 1;
            continue;
        }
        if bytes[j] == b'@' {
            out.push_str(&args.join(" "));
            i = j + 1;
            continue;
        }
        if bytes[j] == b'$' {
            out.push('$');
            i = j + 1;
            continue;
        }
        if bytes[j].is_ascii_digit() {
            let mut k = j;
            while k < bytes.len() && bytes[k].is_ascii_digit() {
                k += 1;
            }
            let n: usize = body[j..k].parse().unwrap_or(0);
            out.push_str(&arg_at(args, n).unwrap_or_default());
            i = k;
            continue;
        }
        if body[j..].starts_with("ARGUMENTS") {
            out.push_str(&args.join(" "));
            i = j + "ARGUMENTS".len();
            continue;
        }
        out.push('$');
        i += 1;
    }
    out
}

pub fn render_help(templates: &[Template]) -> String {
    let mut out = String::new();
    for t in templates {
        let hint = t.argument_hint.as_deref().unwrap_or("");
        out.push_str(&format!("/{} {hint} — {}\n", t.name, t.description));
    }
    out
}

pub fn expand_at_references(cwd: &Path, text: &str, max_bytes: usize) -> String {
    let mut additions = Vec::new();
    for token in text.split_whitespace() {
        let Some(path) = token.strip_prefix('@') else {
            continue;
        };
        if path.is_empty() {
            continue;
        }
        let p = Path::new(path);
        let full = if p.is_absolute() {
            p.to_path_buf()
        } else {
            cwd.join(p)
        };
        let content = match fs::read_to_string(&full) {
            Ok(c) => c,
            Err(e) => {
                additions.push(format!("@{path}: unable to read ({e})"));
                continue;
            }
        };
        let content = truncate_head(&content, max_bytes);
        additions.push(format!("[File: {path}]\n{content}"));
    }
    if additions.is_empty() {
        text.to_string()
    } else {
        format!("{}\n\n{text}", additions.join("\n\n"))
    }
}

fn truncate_head(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[truncated]", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_frontmatter() {
        let t = parse_template(
            "review",
            "---\ndescription: Review staged git changes\nargument-hint: \"[focus]\"\n---\nReview the changes.\n",
        );
        assert_eq!(t.description, "Review staged git changes");
        assert_eq!(t.argument_hint.as_deref(), Some("[focus]"));
        assert_eq!(t.body, "Review the changes.\n");
    }

    #[test]
    fn falls_back_to_first_line() {
        let t = parse_template("x", "\n# Title here\nbody");
        assert_eq!(t.description, "# Title here");
        assert!(t.argument_hint.is_none());
    }

    #[test]
    fn tokenizes_shell_style() {
        assert_eq!(tokenize_args(""), Vec::<String>::new());
        assert_eq!(tokenize_args("a b"), vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            tokenize_args("\"a b\" c"),
            vec!["a b".to_string(), "c".to_string()]
        );
        assert_eq!(
            tokenize_args("'a b' c"),
            vec!["a b".to_string(), "c".to_string()]
        );
        assert_eq!(
            tokenize_args("it doesn't matter"),
            vec![
                "it".to_string(),
                "doesn't".to_string(),
                "matter".to_string()
            ]
        );
        assert_eq!(
            tokenize_args("\"unterminated"),
            vec!["unterminated".to_string()]
        );
        assert_eq!(
            tokenize_args("\"the \\\"fast\\\" path\""),
            vec!["the \"fast\" path".to_string()]
        );
    }

    #[test]
    fn expands_positional_args() {
        let args = tokenize_args("alpha \"beta gamma\" delta");
        assert_eq!(
            expand_template("$1 / $2 / $3 / $9", &args),
            "alpha / beta gamma / delta / "
        );
        assert_eq!(
            expand_template("$@ and $ARGUMENTS", &args),
            "alpha beta gamma delta and alpha beta gamma delta"
        );
    }

    #[test]
    fn expands_defaults_and_slices() {
        let none: Vec<String> = Vec::new();
        assert_eq!(expand_template("${1:-correctness}", &none), "correctness");
        assert_eq!(expand_template("${@:-everything}", &none), "everything");
        let args = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        assert_eq!(expand_template("${1:-x}", &args), "a");
        assert_eq!(expand_template("${@:-x}", &args), "a b c d");
        assert_eq!(expand_template("${@:2}", &args), "b c d");
        assert_eq!(expand_template("${@:2:2}", &args), "b c");
    }

    #[test]
    fn leaves_unknown_dollar_syntax_alone() {
        assert_eq!(
            expand_template("cost $ and ${weird} and $X", &[]),
            "cost $ and ${weird} and $X"
        );
        assert_eq!(expand_template("literal $$var", &[]), "literal $var");
    }

    #[test]
    fn loads_from_dirs_with_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user");
        let proj = dir.path().join("proj").join(".rho").join("prompts");
        fs::create_dir_all(&user).unwrap();
        fs::create_dir_all(&proj).unwrap();
        fs::write(user.join("deploy.md"), "user deploy").unwrap();
        fs::write(user.join("notes.txt"), "ignored").unwrap();
        fs::write(proj.join("deploy.md"), "project deploy").unwrap();
        fs::write(
            proj.join("test.md"),
            "---\ndescription: run tests\n---\ntest $@",
        )
        .unwrap();
        let templates = load_templates(&template_dirs(user, &dir.path().join("proj")));
        assert_eq!(templates.len(), 2);
        let deploy = templates.iter().find(|t| t.name == "deploy").unwrap();
        assert_eq!(deploy.body, "user deploy");
        let test = templates.iter().find(|t| t.name == "test").unwrap();
        assert_eq!(test.description, "run tests");
    }

    #[test]
    fn expands_at_references_from_cwd() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "FILE CONTENT").unwrap();
        let out = expand_at_references(dir.path(), "explain @a.txt", 1000);
        assert!(out.contains("FILE CONTENT"), "{out}");
        assert!(out.ends_with("explain @a.txt"));
        let out = expand_at_references(dir.path(), "missing @nope.txt", 1000);
        assert!(out.contains("unable to read"));
        assert_eq!(
            expand_at_references(dir.path(), "no refs here", 1000),
            "no refs here"
        );
    }

    #[test]
    fn truncates_large_at_reference() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("big.txt"), "x".repeat(500)).unwrap();
        let out = expand_at_references(dir.path(), "@big.txt", 100);
        assert!(out.contains("[truncated]"));
        assert!(!out.contains(&"x".repeat(200)));
    }
}
