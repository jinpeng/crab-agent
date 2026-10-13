use crate::provider::ProviderError;
use crate::stream::StreamEvent;
use crate::types::StopReason;

/// Single SSE data payload -> 0..n StreamEvents. Pure function (Q13).
/// One chunk can carry text delta, tool_call fragments, and finish_reason together.
pub fn parse_chunk(data: &str) -> Result<Vec<StreamEvent>, ProviderError> {
    let data = data.trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(Vec::new());
    }
    let v: serde_json::Value = serde_json::from_str(data)
        .map_err(|e| ProviderError::Network(format!("bad SSE json: {e}: {data}")))?;
    let choice = &v["choices"][0];
    if choice.is_null() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let delta = &choice["delta"];

    if let Some(c) = delta["content"].as_str()
        && !c.is_empty()
    {
        out.push(StreamEvent::TextDelta(c.to_string()));
    }

    if let Some(fr) = choice["finish_reason"].as_str() {
        let reason = match fr {
            "stop" => StopReason::EndTurn,
            "tool_calls" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        };
        out.push(StreamEvent::StreamEnd { reason });
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::StreamEvent;
    use crate::types::StopReason;

    #[test]
    fn text_delta_parsed() {
        let e = parse_chunk(
            r#"{"choices":[{"index":0,"delta":{"content":"hello"},"finish_reason":null}]}"#,
        )
        .unwrap();
        assert_eq!(e, vec![StreamEvent::TextDelta("hello".into())]);
    }

    #[test]
    fn finish_reason_stop_maps_to_end_turn() {
        let e =
            parse_chunk(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#).unwrap();
        assert_eq!(
            e,
            vec![StreamEvent::StreamEnd {
                reason: StopReason::EndTurn
            }]
        );
    }

    #[test]
    fn done_sentinel_produces_empty() {
        assert_eq!(parse_chunk("[DONE]").unwrap(), Vec::<StreamEvent>::new());
    }

    #[test]
    fn empty_delta_produces_empty() {
        let e = parse_chunk(
            r#"{"choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]}"#,
        )
        .unwrap();
        assert_eq!(e, Vec::<StreamEvent>::new());
    }

    #[test]
    fn malformed_json_is_error_not_panic() {
        assert!(parse_chunk("{not json").is_err());
    }
}
