use crate::provider::sse::parse_chunk;
use crate::provider::{Provider, ProviderError, auth::Credentials};
use crate::stream::{StreamAccumulator, StreamEvent};
use crate::types::{AssistantTurn, Message, StopReason};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;

pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_retries: u32,
}

impl OpenAiProvider {
    pub fn resolve() -> Result<Self, ProviderError> {
        let c = Credentials::resolve()?;
        Ok(Self::new(c.base_url, c.api_key, c.model))
    }

    pub fn new(
        base_url: impl Into<String>,
        api_key: impl Into<String>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key: api_key.into(),
            model: model.into(),
            max_retries: 3,
        }
    }

    pub fn with_max_retries(mut self, n: u32) -> Self {
        self.max_retries = n;
        self
    }

    fn request_body(&self, messages: &[Message]) -> serde_json::Value {
        let msgs: Vec<serde_json::Value> = messages.iter().map(message_to_openai).collect();
        serde_json::json!({
            "model": self.model,
            "messages": msgs,
            "stream": true,
        })
    }
}

/// Map an internal Message to an OpenAI chat message (v0: text only; tool mapping in Task 7).
fn message_to_openai(m: &Message) -> serde_json::Value {
    use crate::types::{ContentBlock, Role};
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let mut text = String::new();
    for b in &m.content {
        if let ContentBlock::Text { text: t } = b {
            text.push_str(t);
        }
    }
    serde_json::json!({ "role": role, "content": text })
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn complete(&self, messages: &[Message]) -> Result<AssistantTurn, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&self.request_body(messages))
            .send()
            .await
            .map_err(|e| ProviderError::Network(format!("send: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ProviderError::Api {
                status: status.as_u16(),
                body,
            });
        }

        let mut acc = StreamAccumulator::new();
        let mut stream = resp.bytes_stream().eventsource();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(event) => {
                    for se in parse_chunk(&event.data)? {
                        let is_end = matches!(se, StreamEvent::StreamEnd { .. });
                        acc.push(se);
                        if is_end {
                            break;
                        }
                    }
                }
                Err(_) => {
                    // Stream broke mid-way: keep partial content, end with Error (no retry, Q13).
                    acc.push(StreamEvent::StreamEnd {
                        reason: StopReason::Error,
                    });
                    let mut t = acc.finish();
                    t.interrupted = true;
                    return Ok(t);
                }
            }
        }
        Ok(acc.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use crate::types::{ContentBlock, Message, Role, StopReason};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    pub(crate) fn user(t: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: t.into() }],
        }
    }

    #[tokio::test]
    async fn complete_assembles_streamed_text() {
        let server = MockServer::start().await;
        let body = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(body)
                    .insert_header("content-type", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let p = OpenAiProvider::new(server.uri(), "sk-test", "gpt-4o-mini");
        let turn = p.complete(&[user("hey")]).await.unwrap();
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        match &turn.message.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hi"),
            other => panic!("expected text, got {other:?}"),
        }
    }
}
