use crate::types::Message;

pub const MISSING_TOOL_RESULT: &str = "No result recorded for this tool call.";

fn has_body(message: &Message) -> bool {
    message.content.is_some() || message.tool_calls.is_some()
}

fn is_tool_result(message: &Message) -> bool {
    message.role == "tool"
}

/// Repair a history so every request built from it is accepted by strict
/// chat-completions servers: assistant messages carry content or tool calls,
/// every tool call is answered exactly once, and no tool result is orphaned.
pub fn sanitize_history(history: &[Message]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(history.len());
    let mut pending: Vec<String> = Vec::new();

    for message in history {
        if is_tool_result(message) {
            let id = message.tool_call_id.clone().unwrap_or_default();
            if !pending.iter().any(|pending| pending == &id) {
                continue;
            }
            pending.retain(|pending| pending != &id);
            let mut result = message.clone();
            if result.content.is_none() {
                result.content = Some(String::new());
            }
            out.push(result);
            continue;
        }

        for id in pending.drain(..) {
            out.push(Message::tool_result(id, MISSING_TOOL_RESULT));
        }

        if message.role != "assistant" {
            if message.content.is_none() && message.tool_calls.is_none() {
                let mut fixed = message.clone();
                fixed.content = Some(String::new());
                out.push(fixed);
            } else {
                out.push(message.clone());
            }
            continue;
        }

        let mut assistant = message.clone();
        let calls: Vec<String> = assistant
            .tool_calls
            .iter()
            .flatten()
            .map(|call| call.id.clone())
            .collect();
        if calls.is_empty() {
            assistant.tool_calls = None;
            if !has_body(&assistant) {
                assistant.content = Some(String::new());
            }
            out.push(assistant);
            continue;
        }
        assistant.content = assistant.content.filter(|c| !c.is_empty());
        pending = calls;
        out.push(assistant);
    }

    for id in pending.drain(..) {
        out.push(Message::tool_result(id, MISSING_TOOL_RESULT));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{FunctionCall, ToolCall};

    fn tool_call(id: &str) -> ToolCall {
        ToolCall {
            id: id.to_string(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: "bash".to_string(),
                arguments: "{}".to_string(),
            },
        }
    }

    fn assistant_calls(ids: &[&str]) -> Message {
        Message {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(ids.iter().map(|id| tool_call(id)).collect()),
            tool_call_id: None,
        }
    }

    #[test]
    fn gives_contentless_assistant_messages_empty_content() {
        let broken = Message {
            role: "assistant".to_string(),
            content: None,
            tool_calls: None,
            tool_call_id: None,
        };
        let fixed = sanitize_history(&[Message::user("hi"), broken]);
        assert_eq!(fixed[1].content.as_deref(), Some(""));
    }

    #[test]
    fn answers_unanswered_tool_calls() {
        let repaired = sanitize_history(&[
            Message::user("hi"),
            assistant_calls(&["c1", "c2"]),
            Message::tool_result("c1", "done"),
            Message::user("next"),
        ]);
        assert_eq!(repaired.len(), 5);
        assert_eq!(repaired[3].tool_call_id.as_deref(), Some("c2"));
        assert_eq!(repaired[3].content.as_deref(), Some(MISSING_TOOL_RESULT));
        assert_eq!(repaired[4].role, "user");
    }

    #[test]
    fn closes_tool_calls_at_end_of_history() {
        let repaired = sanitize_history(&[Message::user("hi"), assistant_calls(&["c1"])]);
        assert_eq!(repaired.len(), 3);
        assert_eq!(repaired[2].role, "tool");
    }

    #[test]
    fn drops_orphan_and_duplicate_tool_results() {
        let repaired = sanitize_history(&[
            Message::user("hi"),
            Message::tool_result("ghost", "orphan"),
            assistant_calls(&["c1"]),
            Message::tool_result("c1", "first"),
            Message::tool_result("c1", "duplicate"),
            Message::user("next"),
        ]);
        assert_eq!(repaired.len(), 4);
        assert_eq!(repaired[1].role, "assistant");
        assert_eq!(repaired[2].content.as_deref(), Some("first"));
        assert_eq!(repaired[3].role, "user");
    }

    #[test]
    fn keeps_valid_history_unchanged() {
        let history = vec![
            Message::system("sys"),
            Message::user("hi"),
            assistant_calls(&["c1"]),
            Message::tool_result("c1", "out"),
            Message::assistant("done"),
        ];
        assert_eq!(sanitize_history(&history), history);
    }
}
