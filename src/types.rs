use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "tool_call_type")]
    pub kind: String,
    pub function: FunctionCall,
}

fn tool_call_type() -> String {
    "function".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Message {
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(
        default,
        rename = "reasoning_content",
        skip_serializing_if = "Option::is_none"
    )]
    pub reasoning: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::with_content("system", content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::with_content("user", content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::with_content("assistant", content)
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: Some(tool_call_id.into()),
            reasoning: None,
        }
    }

    pub fn with_content(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: Some(content.into()),
            tool_calls: None,
            tool_call_id: None,
            reasoning: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolSpec {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ToolFunction,
}

#[derive(Debug, Clone, Serialize)]
pub struct ToolFunction {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
}

#[derive(Debug, Clone, Default)]
pub struct AssistantMessage {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: Option<String>,
    pub usage: Usage,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_serializes_as_reasoning_content_when_present() {
        let mut m = Message::assistant("answer");
        m.reasoning = Some("thinking".to_string());
        let json = serde_json::to_value(&m).unwrap();
        assert_eq!(json["reasoning_content"], "thinking");
        assert!(json.get("tool_call_id").is_none());
    }

    #[test]
    fn reasoning_round_trips_and_is_omitted_when_absent() {
        let m: Message = serde_json::from_value(serde_json::json!({
            "role": "assistant",
            "content": "a",
            "reasoning_content": "t"
        }))
        .unwrap();
        assert_eq!(m.reasoning.as_deref(), Some("t"));
        let without: Message =
            serde_json::from_value(serde_json::json!({"role": "user", "content": "u"})).unwrap();
        assert!(without.reasoning.is_none());
        assert!(!serde_json::to_string(&without)
            .unwrap()
            .contains("reasoning"));
    }
}
