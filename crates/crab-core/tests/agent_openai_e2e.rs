use crab_core::agent::{Agent, AgentConfig};
use crab_core::gate::PermissionGate;
use crab_core::provider::openai::OpenAiProvider;
use crab_core::session::Session;
use crab_core::state::SessionState;
use crab_core::tools::register_all;
use crab_core::types::{ContentBlock, StopReason};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// Turn 1: request a read of `target`. Turn 2 (after tool_result): return text.
struct TwoTurn {
    target: String,
    calls: Arc<AtomicUsize>,
}

impl Respond for TwoTurn {
    fn respond(&self, _req: &Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let sse = if n == 0 {
            // tool_call: read {"path":"<target>"}, arguments split across two chunks
            let args = format!(r#"{{"path":"{}"}}"#, self.target);
            let (a, b) = args.split_at(args.len() / 2);
            let esc = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
            format!(
                "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"c1\",\"function\":{{\"name\":\"read\",\"arguments\":\"\"}}}}]}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":\"{}\"}}}}]}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{\"tool_calls\":[{{\"index\":0,\"function\":{{\"arguments\":\"{}\"}}}}]}},\"finish_reason\":null}}]}}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n",
                esc(a),
                esc(b)
            )
        } else {
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done reading\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_string()
        };
        ResponseTemplate::new(200)
            .set_body_string(sse)
            .insert_header("content-type", "text/event-stream")
    }
}

#[tokio::test]
async fn agent_drives_real_read_tool_via_openai_provider() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("note.txt");
    std::fs::write(&target, "the answer is 42").unwrap();

    let server = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .respond_with(TwoTurn {
            target: target.to_str().unwrap().into(),
            calls: calls.clone(),
        })
        .mount(&server)
        .await;

    let provider = OpenAiProvider::new(server.uri(), "sk-test", "m");
    let tools = register_all(SessionState::default(), PermissionGate::auto_approve());
    let session = Session::create(dir.path()).unwrap();
    let mut agent = Agent::new(Box::new(provider), tools, session, AgentConfig { max_turns: 10 });

    let reason = agent.run("read the note".into()).await.unwrap();
    assert_eq!(reason, StopReason::EndTurn);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "two model round-trips");

    // tool result (file contents) fed back on the second request
    let second = &agent.provider_calls()[1];
    let has_tool_result = second
        .iter()
        .flat_map(|m| &m.content)
        .any(|b| matches!(b, ContentBlock::ToolResult { content, .. } if content.contains("42")));
    assert!(has_tool_result, "tool result with file contents fed back");
}
