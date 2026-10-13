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

    fn request_body(&self, messages: &[Message], tools: &[serde_json::Value]) -> serde_json::Value {
        let msgs: Vec<serde_json::Value> = messages.iter().flat_map(messages_to_openai).collect();
        let mut body = serde_json::json!({
            "model": self.model,
            "messages": msgs,
            "stream": true,
        });
        if !tools.is_empty() {
            // OpenAI tool schema: {"type":"function","function":{...}}.
            body["tools"] = serde_json::Value::Array(
                tools
                    .iter()
                    .map(|s| serde_json::json!({ "type": "function", "function": s }))
                    .collect(),
            );
        }
        body
    }
}

/// Map an internal Message to one-or-more OpenAI chat messages. A user message with
/// several tool_results expands to several `role:tool` messages (OpenAI protocol).
fn messages_to_openai(m: &Message) -> Vec<serde_json::Value> {
    use crate::types::{ContentBlock, Role};
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    let mut out = Vec::new();
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for b in &m.content {
        match b {
            ContentBlock::Text { text: t } => text.push_str(t),
            ContentBlock::ToolUse { id, name, input } => {
                tool_calls.push(serde_json::json!({
                    "id": id, "type": "function",
                    "function": { "name": name, "arguments": input.to_string() }
                }));
            }
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                out.push(serde_json::json!({
                    "role": "tool", "tool_call_id": tool_use_id, "content": content
                }));
            }
        }
    }

    if !text.is_empty() || !tool_calls.is_empty() || out.is_empty() {
        let mut msg = serde_json::json!({ "role": role });
        if !text.is_empty() {
            msg["content"] = serde_json::Value::String(text);
        }
        if !tool_calls.is_empty() {
            msg["tool_calls"] = serde_json::Value::Array(tool_calls);
        }
        if msg.get("content").is_none() && msg.get("tool_calls").is_none() {
            msg["content"] = serde_json::Value::String(String::new());
        }
        out.insert(0, msg);
    }
    out
}

#[async_trait]
impl Provider for OpenAiProvider {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<AssistantTurn, ProviderError> {
        let mut attempt = 0u32;
        loop {
            match self.complete_once(messages, tools).await {
                Ok(t) => return Ok(t),
                Err(ProviderError::Api { status, body }) if status == 429 || status >= 500 => {
                    attempt += 1;
                    if attempt > self.max_retries {
                        return Err(ProviderError::Retryable(format!(
                            "gave up after {} attempts: {} {}",
                            self.max_retries, status, body
                        )));
                    }
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(ProviderError::Network(e)) => {
                    // Connection failed before any content was produced: safe to retry.
                    attempt += 1;
                    if attempt > self.max_retries {
                        return Err(ProviderError::Retryable(format!("gave up: {e}")));
                    }
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }
}

fn backoff(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(100 * 2u64.pow(attempt.saturating_sub(1)))
}

impl OpenAiProvider {
    /// One attempt. Streaming errors return an interrupted turn (Ok), NOT Err — so the
    /// retry loop above never retries once content has started (Q13).
    async fn complete_once(
        &self,
        messages: &[Message],
        tools: &[serde_json::Value],
    ) -> Result<AssistantTurn, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&self.request_body(messages, tools))
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
        'outer: while let Some(ev) = stream.next().await {
            match ev {
                Ok(event) => {
                    for se in parse_chunk(&event.data)? {
                        let is_end = matches!(se, StreamEvent::StreamEnd { .. });
                        acc.push(se);
                        if is_end {
                            break 'outer; // finish_reason reached: stop consuming the stream
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
        let turn = p.complete(&[user("hey")], &[]).await.unwrap();
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        match &turn.message.content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "hi"),
            other => panic!("expected text, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use crate::provider::{Provider, ProviderError};
    use crate::types::{ContentBlock, Message, Role, StopReason};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn user(t: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: t.into() }],
        }
    }

    const OK_SSE: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

    struct Flaky {
        n: Arc<AtomicUsize>,
        fail_times: usize,
    }
    impl Respond for Flaky {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            let i = self.n.fetch_add(1, Ordering::SeqCst);
            if i < self.fail_times {
                ResponseTemplate::new(429).set_body_string("rate limited")
            } else {
                ResponseTemplate::new(200)
                    .set_body_string(OK_SSE)
                    .insert_header("content-type", "text/event-stream")
            }
        }
    }

    #[tokio::test]
    async fn retries_429_then_succeeds() {
        let server = MockServer::start().await;
        let n = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .respond_with(Flaky {
                n: n.clone(),
                fail_times: 2,
            })
            .mount(&server)
            .await;
        let p = OpenAiProvider::new(server.uri(), "k", "m").with_max_retries(3);
        let turn = p.complete(&[user("x")], &[]).await.unwrap();
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
        assert_eq!(n.load(Ordering::SeqCst), 3); // 2 fail + 1 ok
    }

    #[tokio::test]
    async fn gives_up_after_max_retries() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rl"))
            .mount(&server)
            .await;
        let p = OpenAiProvider::new(server.uri(), "k", "m").with_max_retries(2);
        let r = p.complete(&[user("x")], &[]).await;
        assert!(matches!(
            r,
            Err(ProviderError::Retryable(_)) | Err(ProviderError::Api { .. })
        ));
    }

    struct CountThenStream {
        n: Arc<AtomicUsize>,
    }
    impl Respond for CountThenStream {
        fn respond(&self, _req: &Request) -> ResponseTemplate {
            self.n.fetch_add(1, Ordering::SeqCst);
            // 200 + partial content, then stream ends without [DONE] (no error, just EOF)
            ResponseTemplate::new(200)
                .set_body_string("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"},\"finish_reason\":null}]}\n\n")
                .insert_header("content-type", "text/event-stream")
        }
    }

    #[tokio::test]
    async fn mid_stream_eof_does_not_retry() {
        let server = MockServer::start().await;
        let n = Arc::new(AtomicUsize::new(0));
        Mock::given(method("POST"))
            .respond_with(CountThenStream { n: n.clone() })
            .mount(&server)
            .await;
        let p = OpenAiProvider::new(server.uri(), "k", "m").with_max_retries(3);
        let _turn = p.complete(&[user("x")], &[]).await.unwrap();
        assert_eq!(n.load(Ordering::SeqCst), 1, "stream started = no retry");
    }
}

#[cfg(test)]
mod tool_mapping_tests {
    use super::*;
    use crate::types::{ContentBlock, Message, Role};

    #[test]
    fn assistant_tool_use_maps_to_tool_calls() {
        let m = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "read".into(),
                input: serde_json::json!({"path":"x"}),
            }],
        };
        let v = messages_to_openai(&m);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["role"], "assistant");
        assert_eq!(v[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(v[0]["tool_calls"][0]["function"]["name"], "read");
        // arguments must be stringified JSON (OpenAI protocol)
        assert_eq!(
            v[0]["tool_calls"][0]["function"]["arguments"],
            r#"{"path":"x"}"#
        );
    }

    #[test]
    fn tool_result_maps_to_tool_role_message() {
        let m = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "file contents".into(),
                is_error: false,
            }],
        };
        let v = messages_to_openai(&m);
        assert!(v.iter().any(|msg| msg["role"] == "tool"
            && msg["tool_call_id"] == "call_1"
            && msg["content"] == "file contents"));
    }

    #[test]
    fn multiple_tool_results_expand_to_separate_tool_messages() {
        let m = Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "a".into(),
                    content: "ra".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "b".into(),
                    content: "rb".into(),
                    is_error: false,
                },
            ],
        };
        let v = messages_to_openai(&m);
        let tool_msgs: Vec<_> = v.iter().filter(|x| x["role"] == "tool").collect();
        assert_eq!(tool_msgs.len(), 2);
    }
}

#[cfg(test)]
mod tools_in_request_tests {
    use super::*;
    use crate::provider::Provider;
    use crate::types::{ContentBlock, Message, Role};
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    /// Captures the request body so we can assert tools were advertised.
    struct Capture {
        seen: Arc<StdMutex<Vec<serde_json::Value>>>,
    }
    impl Respond for Capture {
        fn respond(&self, req: &Request) -> ResponseTemplate {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
            self.seen.lock().unwrap().push(body);
            ResponseTemplate::new(200)
                .set_body_string("data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n")
                .insert_header("content-type", "text/event-stream")
        }
    }

    #[tokio::test]
    async fn request_includes_tools_schema() {
        let server = MockServer::start().await;
        let seen = Arc::new(StdMutex::new(Vec::new()));
        Mock::given(method("POST"))
            .respond_with(Capture { seen: seen.clone() })
            .mount(&server)
            .await;
        let p = OpenAiProvider::new(server.uri(), "k", "m");
        let msgs = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }];
        let tools = vec![serde_json::json!({
            "name": "read", "description": "read a file",
            "parameters": {"type":"object","properties":{"path":{"type":"string"}}}
        })];
        p.complete(&msgs, &tools).await.unwrap();

        let body = &seen.lock().unwrap()[0];
        assert!(
            body.get("tools").is_some(),
            "request must carry tools schema"
        );
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["name"], "read");
    }

    #[tokio::test]
    async fn empty_tools_omits_tools_field() {
        let server = MockServer::start().await;
        let seen = Arc::new(StdMutex::new(Vec::new()));
        Mock::given(method("POST"))
            .respond_with(Capture { seen: seen.clone() })
            .mount(&server)
            .await;
        let p = OpenAiProvider::new(server.uri(), "k", "m");
        let msgs = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: "hi".into() }],
        }];
        p.complete(&msgs, &[]).await.unwrap();
        let body = &seen.lock().unwrap()[0];
        assert!(body.get("tools").is_none(), "no tools = no tools field");
    }
}
