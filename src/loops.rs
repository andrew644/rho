use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum LoopMode {
    Forever,
    Count(usize),
    Duration(Duration),
    Until { condition: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoopSpec {
    pub mode: LoopMode,
    pub prompt: String,
}

impl LoopSpec {
    pub fn describe_mode(&self) -> String {
        match &self.mode {
            LoopMode::Forever => "forever".to_string(),
            LoopMode::Count(n) => format!("{n} times"),
            LoopMode::Duration(d) => format!("for {}s", d.as_secs()),
            LoopMode::Until { condition } => format!("until: {condition}"),
        }
    }
}

fn parse_duration(token: &str) -> Option<Duration> {
    let bytes = token.as_bytes();
    if bytes.is_empty() {
        return None;
    }
    let mut total = 0u64;
    let mut num = String::new();
    for b in bytes {
        match b {
            b'0'..=b'9' => num.push(*b as char),
            _ => {
                let n: u64 = num.parse().ok()?;
                num.clear();
                let unit = *b as char;
                let mult = match unit {
                    's' => 1,
                    'm' => 60,
                    'h' => 3600,
                    'd' => 86400,
                    _ => return None,
                };
                total += n * mult;
            }
        }
    }
    if !num.is_empty() || total == 0 {
        return None;
    }
    Some(Duration::from_secs(total))
}

pub fn parse_loop_spec(input: &str) -> Result<LoopSpec, String> {
    let input = input.trim();
    if input.is_empty() {
        return Err(
            "usage: /loop [forever | <n> | <duration e.g. 10m> | until \"<condition>\"] <task>"
                .to_string(),
        );
    }
    let (mode_token, rest) = input
        .split_once(char::is_whitespace)
        .map(|(a, b)| (a, b.trim_start()))
        .unwrap_or((input, ""));

    if mode_token == "until" {
        let rest = rest
            .strip_prefix('"')
            .ok_or_else(|| "usage: /loop until \"<condition>\" <task>".to_string())?;
        let end = rest
            .find('"')
            .ok_or_else(|| "unterminated condition string in /loop until".to_string())?;
        let condition = rest[..end].trim().to_string();
        let prompt = rest[end + 1..].trim().to_string();
        if condition.is_empty() {
            return Err("until condition must not be empty".to_string());
        }
        if prompt.is_empty() {
            return Err("missing task prompt after condition".to_string());
        }
        return Ok(LoopSpec {
            mode: LoopMode::Until { condition },
            prompt,
        });
    }

    let mode = if mode_token == "forever" || mode_token == "inf" {
        LoopMode::Forever
    } else if let Ok(n) = mode_token.parse::<usize>() {
        if n == 0 {
            return Err("loop count must be > 0".to_string());
        }
        LoopMode::Count(n)
    } else if let Some(d) = parse_duration(mode_token) {
        LoopMode::Duration(d)
    } else {
        return Err(format!("unknown loop spec: {mode_token}"));
    };

    if rest.is_empty() {
        return Err("missing task prompt".to_string());
    }
    Ok(LoopSpec {
        mode,
        prompt: rest.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_forever_and_count() {
        assert_eq!(
            parse_loop_spec("forever run tests").unwrap(),
            LoopSpec {
                mode: LoopMode::Forever,
                prompt: "run tests".into()
            }
        );
        assert_eq!(
            parse_loop_spec("5   do the thing ").unwrap(),
            LoopSpec {
                mode: LoopMode::Count(5),
                prompt: "do the thing".into()
            }
        );
        assert!(parse_loop_spec("0 task").is_err());
        assert!(parse_loop_spec("forever").is_err());
    }

    #[test]
    fn parses_durations() {
        assert_eq!(parse_duration("10m"), Some(Duration::from_secs(600)));
        assert_eq!(parse_duration("90s"), Some(Duration::from_secs(90)));
        assert_eq!(parse_duration("1h30m"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_duration("10"), None);
        assert_eq!(parse_duration("m"), None);
        let spec = parse_loop_spec("30s poll the build").unwrap();
        assert_eq!(spec.mode, LoopMode::Duration(Duration::from_secs(30)));
        assert_eq!(spec.prompt, "poll the build");
    }

    #[test]
    fn parses_until() {
        let spec = parse_loop_spec("until \"are all tests green?\" keep fixing failures").unwrap();
        assert_eq!(
            spec.mode,
            LoopMode::Until {
                condition: "are all tests green?".into()
            }
        );
        assert_eq!(spec.prompt, "keep fixing failures");
        assert!(parse_loop_spec("until no quotes here").is_err());
        assert!(parse_loop_spec("until \"cond\"").is_err());
    }

    #[test]
    fn unknown_spec_is_error() {
        assert!(parse_loop_spec("banana task").is_err());
        assert!(parse_loop_spec("").is_err());
    }
}
