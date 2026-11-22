use crate::types::Message;

pub const RESERVE_TOKENS: usize = 16_384;
pub const KEEP_RECENT_TOKENS: usize = 20_000;

pub fn estimate_message_tokens(message: &Message) -> usize {
    let content = message.content.as_deref().unwrap_or("").len();
    let reasoning = message.reasoning.as_deref().unwrap_or("").len();
    let calls = message
        .tool_calls
        .iter()
        .flatten()
        .map(|c| c.function.name.len() + c.function.arguments.len() + c.id.len())
        .sum::<usize>();
    (content + reasoning + calls).div_ceil(4)
}

pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

pub fn should_compact(est_tokens: usize, context_window: usize, reserve: usize) -> bool {
    est_tokens + reserve > context_window
}

fn is_cut_boundary(message: &Message) -> bool {
    matches!(message.role.as_str(), "user" | "assistant")
}

pub fn find_cut_point(messages: &[Message], keep_recent_tokens: usize) -> Option<usize> {
    let mut accumulated = 0usize;
    let mut cut = None;
    for i in (1..messages.len()).rev() {
        accumulated += estimate_message_tokens(&messages[i]);
        if accumulated >= keep_recent_tokens {
            cut = Some(i);
            break;
        }
    }
    let mut cut = cut?;
    while cut < messages.len() && !is_cut_boundary(&messages[cut]) {
        cut += 1;
    }
    if cut >= messages.len() || cut <= 1 {
        return None;
    }
    Some(cut)
}

pub const TOOL_RESULT_CHAR_LIMIT: usize = 2000;
pub const TOOL_CALL_CHAR_LIMIT: usize = 500;

fn text_of(message: &Message) -> String {
    message.content.clone().unwrap_or_default()
}

pub fn truncate_chars(text: &str, limit: usize) -> String {
    let total = text.chars().count();
    if total <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}\n[... truncated {} characters ...]", total - limit)
}

/// Flatten messages into plain text for summarization. Sending the raw message
/// list back to the model replays multi-kilobyte tool results and can make the
/// summary request larger than the context it is trying to shrink.
fn role_label(role: &str) -> String {
    let mut chars = role.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

pub fn serialize_conversation(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        match message.role.as_str() {
            "assistant" => {
                let text = text_of(message);
                if !text.trim().is_empty() {
                    out.push_str(&format!("[Assistant]: {}\n", text.trim()));
                }
                for call in message.tool_calls.iter().flatten() {
                    out.push_str(&format!(
                        "[Assistant tool calls]: {}({})\n",
                        call.function.name,
                        truncate_chars(&call.function.arguments, TOOL_CALL_CHAR_LIMIT)
                    ));
                }
            }
            "tool" => out.push_str(&format!(
                "[Tool result]: {}\n",
                truncate_chars(&text_of(message), TOOL_RESULT_CHAR_LIMIT)
            )),
            other => out.push_str(&format!(
                "[{}]: {}\n",
                role_label(other),
                text_of(message).trim()
            )),
        }
    }
    out
}

pub const SUMMARIZER_PROMPT: &str = "You compress coding-agent conversations. \
Write a structured summary of the conversation so far that lets work continue \
without the original messages. Use these sections: Objective; Summary of work \
done; Files and code touched; Key decisions and constraints; Open items and \
next steps. Be factual and specific. Output only the summary.";

pub fn build_summary_request(messages: &[Message], instructions: Option<&str>) -> Vec<Message> {
    let conversation = serialize_conversation(messages);
    let mut task = format!(
        "Summarize the conversation below according to the required sections.\n\n\
         Conversation:\n{conversation}"
    );
    if let Some(extra) = instructions.map(str::trim).filter(|s| !s.is_empty()) {
        task.push_str(&format!("\nAdditional instructions: {extra}"));
    }
    vec![Message::system(SUMMARIZER_PROMPT), Message::user(task)]
}

pub fn compacted_history(system: &Message, summary: &str, kept: &[Message]) -> Vec<Message> {
    let mut history = vec![
        system.clone(),
        Message::user(format!("[Summary of earlier conversation]\n{summary}")),
    ];
    history.extend(kept.iter().cloned());
    history
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FunctionCall, ToolCall};

    fn big_message(role: &str, words: usize) -> Message {
        Message::with_content(role, "x".repeat(words * 4))
    }

    fn convo() -> Vec<Message> {
        let mut m = vec![Message::system("sys")];
        for i in 0..10 {
            m.push(Message::user(format!("u{i}")));
            m.push(Message::assistant(format!("a{i}")));
        }
        m
    }

    #[test]
    fn estimates_tokens_from_content() {
        assert!(estimate_message_tokens(&Message::user("abcd")) >= 1);
        let with_calls = Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "c1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "bash".into(),
                    arguments: "{\"command\":\"ls -la\"}".into(),
                },
            }]),
            tool_call_id: None,
            reasoning: None,
        };
        assert!(estimate_message_tokens(&with_calls) > 2);
        assert_eq!(estimate_tokens(&[]), 0);
    }

    #[test]
    fn reasoning_counts_toward_estimated_tokens() {
        let m = Message::assistant("abcd");
        let plain = estimate_message_tokens(&m);
        let mut with_reasoning = Message::assistant("abcd");
        with_reasoning.reasoning = Some("x".repeat(400));
        assert!(estimate_message_tokens(&with_reasoning) > plain);
    }

    #[test]
    fn summaries_omit_thinking_text() {
        let mut m = Message::assistant("visible answer");
        m.reasoning = Some("inner thoughts".to_string());
        let text = serialize_conversation(&[m]);
        assert!(text.contains("visible answer"));
        assert!(!text.contains("inner thoughts"));
    }

    #[test]
    fn triggers_near_context_limit() {
        assert!(should_compact(190_000, 200_000, 16_384));
        assert!(!should_compact(100_000, 200_000, 16_384));
    }

    #[test]
    fn cuts_at_user_boundary_after_keep_budget() {
        let mut m = vec![Message::system("sys")];
        m.push(Message::user("old question"));
        m.push(Message::assistant("old answer"));
        m.push(big_message("user", 9_000));
        m.push(big_message("assistant", 9_000));
        let cut = find_cut_point(&m, 10_000).unwrap();
        assert_eq!(cut, 3);
        assert_eq!(m[cut].role, "user");
    }

    #[test]
    fn never_splits_tool_runs() {
        let mut m = vec![Message::system("sys")];
        m.push(Message::user("old question"));
        m.push(Message::assistant("old answer"));
        m.push(Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "c1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "bash".into(),
                    arguments: "x".repeat(36_000),
                },
            }]),
            tool_call_id: None,
            reasoning: None,
        });
        m.push(Message {
            role: "tool".into(),
            content: Some("result".into()),
            tool_calls: None,
            tool_call_id: Some("c1".into()),
            reasoning: None,
        });
        m.push(big_message("user", 9_000));
        let cut = find_cut_point(&m, 10_000).unwrap();
        assert_eq!(m[cut].role, "assistant");
        assert!(m[cut].tool_calls.is_some());
        assert_eq!(m[cut + 1].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn returns_none_when_nothing_to_compact() {
        let m = convo();
        assert_eq!(find_cut_point(&m, KEEP_RECENT_TOKENS), None);
        let only_recent = vec![Message::system("s"), Message::user("u")];
        assert_eq!(find_cut_point(&only_recent, KEEP_RECENT_TOKENS), None);
    }

    #[test]
    fn summary_request_has_instructions() {
        let req = build_summary_request(&convo(), Some("focus on tests"));
        assert_eq!(req.first().unwrap().role, "system");
        assert_eq!(req.last().unwrap().role, "user");
        assert!(req
            .last()
            .unwrap()
            .content
            .as_deref()
            .unwrap()
            .contains("focus on tests"));
        let req = build_summary_request(&convo(), None);
        assert!(!req
            .last()
            .unwrap()
            .content
            .as_deref()
            .unwrap()
            .contains("Additional"));
    }

    #[test]
    fn compacted_history_keeps_system_summary_and_tail() {
        let history =
            compacted_history(&Message::system("sys"), "SUMMARY", &[Message::user("kept")]);
        assert_eq!(history.len(), 3);
        assert_eq!(history[0].role, "system");
        assert!(history[1].content.as_deref().unwrap().contains("SUMMARY"));
        assert_eq!(history[2].content.as_deref(), Some("kept"));
    }

    #[test]
    fn cuts_inside_a_long_tool_heavy_span() {
        let mut m = vec![Message::system("sys"), Message::user("start")];
        for i in 0..6 {
            m.push(big_message("assistant", 4_000));
            m.push(Message {
                role: "tool".into(),
                content: Some(format!("result {i}")),
                tool_calls: None,
                tool_call_id: Some(format!("c{i}")),
                reasoning: None,
            });
        }
        let cut = find_cut_point(&m, 5_000).unwrap();
        assert_eq!(m[cut].role, "assistant");
        assert!(cut > 1);
    }

    #[test]
    fn summary_request_serializes_and_caps_tool_results() {
        let mut m = vec![Message::system("sys"), Message::user("read the log")];
        m.push(Message {
            role: "assistant".into(),
            content: None,
            tool_calls: Some(vec![ToolCall {
                id: "c1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "bash".into(),
                    arguments: "{\"command\":\"cat big.log\"}".into(),
                },
            }]),
            tool_call_id: None,
            reasoning: None,
        });
        m.push(Message::tool_result("c1", "x".repeat(30_000)));
        m.push(Message::assistant("done"));

        let request = build_summary_request(&m, None);
        assert_eq!(request.len(), 2);
        let task = request[1].content.as_deref().unwrap();
        assert!(task.contains("[User]: read the log"));
        assert!(task.contains("[Assistant tool calls]: bash("));
        assert!(task.contains("[Tool result]:"));
        assert!(task.contains("truncated 28000 characters"));
        assert!(task.chars().count() < 6_000);
    }

    #[test]
    fn truncates_chars_with_marker() {
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(
            truncate_chars("abcdef", 2),
            "ab\n[... truncated 4 characters ...]"
        );
    }
}
