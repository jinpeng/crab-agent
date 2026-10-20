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

    // tool_call fragments (possibly several in one chunk)
    if let Some(tcs) = delta["tool_calls"].as_array() {
        for tc in tcs {
            let index = tc["index"].as_u64().unwrap_or(0) as usize;
            // First chunk: has id + function.name -> ToolUseStart
            if let (Some(id), Some(name)) = (tc["id"].as_str(), tc["function"]["name"].as_str()) {
                out.push(StreamEvent::ToolUseStart {
                    index,
                    id: id.to_string(),
                    name: name.to_string(),
                });
            }
            // arguments fragment -> ToolInputDelta (first chunk's empty arguments yields none)
            if let Some(args) = tc["function"]["arguments"].as_str()
                && !args.is_empty()
            {
                out.push(StreamEvent::ToolInputDelta {
                    index,
                    json_fragment: args.to_string(),
                });
            }
        }
    }

    // Token usage frame. DeepSeek sends `usage` in the SAME chunk as the final
    // finish_reason; intermediate chunks carry `usage: null` and must be ignored.
    // Pushed BEFORE StreamEnd so consumers see usage while the stream is still open.
    if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
        let prompt_tokens = u["prompt_tokens"].as_u64().unwrap_or(0) as u32;
        let completion_tokens = u["completion_tokens"].as_u64().unwrap_or(0) as u32;
        // DeepSeek-style cache accounting (hit = cache read, miss = cache write).
        let cache_read_tokens = u["prompt_cache_hit_tokens"].as_u64().unwrap_or(0) as u32;
        let cache_write_tokens = u["prompt_cache_miss_tokens"].as_u64().unwrap_or(0) as u32;
        out.push(StreamEvent::Usage {
            prompt_tokens,
            completion_tokens,
            cache_read_tokens,
            cache_write_tokens,
        });
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
    fn usage_chunk_with_cache_fields_maps_four_way() {
        let e = parse_chunk(
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"prompt_cache_hit_tokens":3,"prompt_cache_miss_tokens":2}}"#
        )
        .unwrap();
        assert!(
            e.contains(&StreamEvent::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                cache_read_tokens: 3,
                cache_write_tokens: 2,
            }),
            "got {e:?}"
        );
    }

    #[test]
    fn usage_chunk_without_cache_defaults_zero() {
        let e = parse_chunk(
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#
        )
        .unwrap();
        assert!(
            e.contains(&StreamEvent::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            }),
            "got {e:?}"
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

    #[test]
    fn tool_call_first_chunk_starts_slot() {
        let e = parse_chunk(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":""}}]},"finish_reason":null}]}"#)
            .unwrap();
        assert_eq!(
            e,
            vec![StreamEvent::ToolUseStart {
                index: 0,
                id: "call_1".into(),
                name: "read".into()
            }]
        );
    }

    #[test]
    fn tool_call_argument_fragment() {
        let e = parse_chunk(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"pa\"}"}}]},"finish_reason":null}]}"#)
            .unwrap();
        assert_eq!(
            e,
            vec![StreamEvent::ToolInputDelta {
                index: 0,
                json_fragment: "{\"pa\"}".into()
            }]
        );
    }

    #[test]
    fn missing_index_defaults_to_zero() {
        let e = parse_chunk(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"arguments":"{}"}}]},"finish_reason":null}]}"#)
            .unwrap();
        assert_eq!(
            e,
            vec![StreamEvent::ToolInputDelta {
                index: 0,
                json_fragment: "{}".into()
            }]
        );
    }

    #[test]
    fn finish_reason_tool_calls_maps_to_tool_use() {
        let e = parse_chunk(r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}"#)
            .unwrap();
        assert_eq!(
            e,
            vec![StreamEvent::StreamEnd {
                reason: StopReason::ToolUse
            }]
        );
    }

    #[test]
    fn multi_tool_call_same_chunk_both_indexed() {
        let e = parse_chunk(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read","arguments":""}},{"index":1,"id":"b","function":{"name":"write","arguments":""}}]},"finish_reason":null}]}"#)
            .unwrap();
        assert_eq!(e.len(), 2);
        assert!(matches!(e[0], StreamEvent::ToolUseStart { index: 0, .. }));
        assert!(matches!(e[1], StreamEvent::ToolUseStart { index: 1, .. }));
    }

    #[test]
    fn usage_frame_with_finish_reason_yields_both_events_usage_first() {
        // DeepSeek real shape: finish_reason and usage share the final chunk.
        let e = parse_chunk(r#"{"choices":[{"index":0,"delta":{"content":""},"finish_reason":"stop"}],"usage":{"prompt_tokens":9,"completion_tokens":1,"total_tokens":10}}"#)
            .unwrap();
        assert!(e.iter().any(|x| matches!(
            x,
            StreamEvent::Usage {
                prompt_tokens: 9,
                completion_tokens: 1,
                ..
            }
        )));
        assert!(e.iter().any(|x| matches!(
            x,
            StreamEvent::StreamEnd {
                reason: StopReason::EndTurn
            }
        )));
        let pos_usage = e
            .iter()
            .position(|x| matches!(x, StreamEvent::Usage { .. }))
            .unwrap();
        let pos_end = e
            .iter()
            .position(|x| matches!(x, StreamEvent::StreamEnd { .. }))
            .unwrap();
        assert!(pos_usage < pos_end, "Usage must precede StreamEnd: {e:?}");
    }

    #[test]
    fn usage_null_frame_ignored() {
        // DeepSeek intermediate chunks carry `usage: null`.
        let e = parse_chunk(
            "{\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}],\"usage\":null}",
        )
        .unwrap();
        assert_eq!(e, vec![StreamEvent::TextDelta("hi".into())]);
    }
}
