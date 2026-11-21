use crate::types::Message;

pub const RESERVE_TOKENS: usize = 16_384;
pub const KEEP_RECENT_TOKENS: usize = 20_000;

pub fn estimate_message_tokens(message: &Message) -> usize {
    let content = message.content.as_deref().unwrap_or("").len();
    let calls = message
        .tool_calls
        .iter()
        .flatten()
        .map(|c| c.function.name.len() + c.function.arguments.len() + c.id.len())
        .sum::<usize>();
    (content + calls).div_ceil(4)
}

pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

pub fn should_compact(est_tokens: usize, context_window: usize, reserve: usize) -> bool {
    est_tokens + reserve > context_window
}

fn is_cut_boundary(message: &Message) -> bool {
    message.role == "user"
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
    let mut cut = cut.unwrap_or(messages.len().saturating_sub(1));
    if cut >= messages.len() {
        cut = messages.len() - 1;
    }
    while cut < messages.len() && !is_cut_boundary(&messages[cut]) {
        cut += 1;
    }
    if cut >= messages.len() || cut <= 1 {
        return None;
    }
    Some(cut)
}

pub const SUMMARIZER_PROMPT: &str = "You compress coding-agent conversations. \
Write a structured summary of the conversation so far that lets work continue \
without the original messages. Use these sections: Objective; Summary of work \
done; Files and code touched; Key decisions and constraints; Open items and \
next steps. Be factual and specific. Output only the summary.";

pub fn build_summary_request(messages: &[Message], instructions: Option<&str>) -> Vec<Message> {
    let mut request = vec![Message::system(SUMMARIZER_PROMPT)];
    request.extend(messages.iter().cloned());
    let mut task =
        "Summarize the conversation above according to the required sections.".to_string();
    if let Some(extra) = instructions.map(str::trim).filter(|s| !s.is_empty()) {
        task.push_str(&format!("\nAdditional instructions: {extra}"));
    }
    request.push(Message::user(task));
    request
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
        };
        assert!(estimate_message_tokens(&with_calls) > 2);
        assert_eq!(estimate_tokens(&[]), 0);
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
        m.push(big_message("assistant", 9_000));
        m.push(Message {
            role: "tool".into(),
            content: Some("result".into()),
            tool_calls: None,
            tool_call_id: Some("c1".into()),
        });
        m.push(big_message("user", 9_000));
        let cut = find_cut_point(&m, 10_000).unwrap();
        assert_eq!(cut, m.len() - 1);
        assert_eq!(m[cut].role, "user");
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
}
