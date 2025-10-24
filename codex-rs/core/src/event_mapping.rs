use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::ReasoningItem;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::items::WebSearchItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ReasoningItemReasoningSummary;
use codex_protocol::models::ResponseItem;
use codex_protocol::models::WebSearchAction;
use codex_protocol::user_input::UserInput;
use tracing::warn;

fn is_session_prefix(text: &str) -> bool {
    let trimmed = text.trim_start();
    let lowered = trimmed.to_ascii_lowercase();
    lowered.starts_with("<environment_context>") || lowered.starts_with("<user_instructions>")
}

fn parse_user_message(message: &[ContentItem]) -> Option<UserMessageItem> {
    let mut content: Vec<UserInput> = Vec::new();

    for content_item in message.iter() {
        match content_item {
            ContentItem::InputText { text } => {
                if is_session_prefix(text) {
                    return None;
                }
                content.push(UserInput::Text { text: text.clone() });
            }
            ContentItem::InputImage { image_url } => {
                content.push(UserInput::Image {
                    image_url: image_url.clone(),
                });
            }
            ContentItem::OutputText { text } => {
                if is_session_prefix(text) {
                    return None;
                }
                warn!("Output text in user message: {}", text);
            }
        }
    }

    Some(UserMessageItem::new(&content))
}

fn parse_agent_message(message: &[ContentItem]) -> AgentMessageItem {
    let mut content: Vec<AgentMessageContent> = Vec::new();
    for content_item in message.iter() {
        match content_item {
            ContentItem::OutputText { text } => {
                content.push(AgentMessageContent::Text { text: text.clone() });
            }
            _ => {
                warn!(
                    "Unexpected content item in agent message: {:?}",
                    content_item
                );
            }
        }
    }
    AgentMessageItem::new(&content)
}

pub fn parse_turn_item(item: &ResponseItem) -> Option<TurnItem> {
    match item {
        ResponseItem::Message { role, content, .. } => match role.as_str() {
            "user" => parse_user_message(content).map(TurnItem::UserMessage),
            "assistant" => Some(TurnItem::AgentMessage(parse_agent_message(content))),
            "system" => None,
            _ => None,
        },
        ResponseItem::Reasoning {
            id,
            summary,
            content,
            ..
        } => {
            let summary_text = summary
                .iter()
                .map(|entry| match entry {
                    ReasoningItemReasoningSummary::SummaryText { text } => text.clone(),
                })
                .collect();
            let raw_content = content
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|entry| match entry {
                    ReasoningItemContent::ReasoningText { text }
                    | ReasoningItemContent::Text { text } => text,
                })
                .collect();
            Some(TurnItem::Reasoning(ReasoningItem {
                id: id.clone(),
                summary_text,
                raw_content,
            }))
        }
        ResponseItem::WebSearchCall {
            id,
            action: WebSearchAction::Search { query },
            ..
        } => Some(TurnItem::WebSearch(WebSearchItem {
            id: id.clone().unwrap_or_default(),
            query: query.clone(),
        })),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::parse_turn_item;
    use codex_protocol::items::AgentMessageContent;
    use codex_protocol::items::TurnItem;
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ReasoningItemContent;
    use codex_protocol::models::ReasoningItemReasoningSummary;
    use codex_protocol::models::ResponseItem;
    use codex_protocol::models::WebSearchAction;
    use codex_protocol::user_input::UserInput;
    use pretty_assertions::assert_eq;

    #[test]
    fn parses_user_message_with_text_and_two_images() {
        let img1 = "https://example.com/one.png".to_string();
        let img2 = "https://example.com/two.jpg".to_string();

        let item = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "Hello world".to_string(),
                },
                ContentItem::InputImage {
                    image_url: img1.clone(),
                },
                ContentItem::InputImage {
                    image_url: img2.clone(),
                },
            ],
        };

        let turn_item = parse_turn_item(&item).expect("expected user message turn item");

        match turn_item {
            TurnItem::UserMessage(user) => {
                let expected_content = vec![
                    UserInput::Text {
                        text: "Hello world".to_string(),
                    },
                    UserInput::Image { image_url: img1 },
                    UserInput::Image { image_url: img2 },
                ];
                assert_eq!(user.content, expected_content);
            }
            other => panic!("expected TurnItem::UserMessage, got {other:?}"),
        }
    }

    #[test]
    fn parses_agent_message() {
        let item = ResponseItem::Message {
            id: Some("msg-1".to_string()),
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: "Hello from Codex".to_string(),
            }],
        };

        let turn_item = parse_turn_item(&item).expect("expected agent message turn item");

        match turn_item {
            TurnItem::AgentMessage(message) => {
                let Some(AgentMessageContent::Text { text }) = message.content.first() else {
                    panic!("expected agent message text content");
                };
                assert_eq!(text, "Hello from Codex");
            }
            other => panic!("expected TurnItem::AgentMessage, got {other:?}"),
        }
    }

    #[test]
    fn parses_reasoning_summary_and_raw_content() {
        let item = ResponseItem::Reasoning {
            id: "reasoning_1".to_string(),
            summary: vec![
                ReasoningItemReasoningSummary::SummaryText {
                    text: "Step 1".to_string(),
                },
                ReasoningItemReasoningSummary::SummaryText {
                    text: "Step 2".to_string(),
                },
            ],
            content: Some(vec![ReasoningItemContent::ReasoningText {
                text: "raw details".to_string(),
            }]),
            encrypted_content: None,
        };

        let turn_item = parse_turn_item(&item).expect("expected reasoning turn item");

        match turn_item {
            TurnItem::Reasoning(reasoning) => {
                assert_eq!(
                    reasoning.summary_text,
                    vec!["Step 1".to_string(), "Step 2".to_string()]
                );
                assert_eq!(reasoning.raw_content, vec!["raw details".to_string()]);
            }
            other => panic!("expected TurnItem::Reasoning, got {other:?}"),
        }
    }

    #[test]
    fn parses_reasoning_including_raw_content() {
        let item = ResponseItem::Reasoning {
            id: "reasoning_2".to_string(),
            summary: vec![ReasoningItemReasoningSummary::SummaryText {
                text: "Summarized step".to_string(),
            }],
            content: Some(vec![
                ReasoningItemContent::ReasoningText {
                    text: "raw step".to_string(),
                },
                ReasoningItemContent::Text {
                    text: "final thought".to_string(),
                },
            ]),
            encrypted_content: None,
        };

        let turn_item = parse_turn_item(&item).expect("expected reasoning turn item");

        match turn_item {
            TurnItem::Reasoning(reasoning) => {
                assert_eq!(reasoning.summary_text, vec!["Summarized step".to_string()]);
                assert_eq!(
                    reasoning.raw_content,
                    vec!["raw step".to_string(), "final thought".to_string()]
                );
            }
            other => panic!("expected TurnItem::Reasoning, got {other:?}"),
        }
    }

    #[test]
    fn parses_web_search_call() {
        let item = ResponseItem::WebSearchCall {
            id: Some("ws_1".to_string()),
            status: Some("completed".to_string()),
            action: WebSearchAction::Search {
                query: "weather".to_string(),
            },
        };

        let turn_item = parse_turn_item(&item).expect("expected web search turn item");

        match turn_item {
            TurnItem::WebSearch(search) => {
                assert_eq!(search.id, "ws_1");
                assert_eq!(search.query, "weather");
            }
            other => panic!("expected TurnItem::WebSearch, got {other:?}"),
        }
    }
}
