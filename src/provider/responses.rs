use std::time::Instant;

use futures_util::StreamExt;
use serde_json::{Value, json};

use super::{ProviderModel, provider_error};
use crate::{
    Continuation, ContinuationPart, KnutError, Model, ModelRequest, ModelResponse,
    ModelStreamEvent, ModelStreamSink, ToolCall, Usage,
};

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

impl ProviderModel {
    fn responses_body(
        &self,
        request: &ModelRequest,
        continuation: Option<&Continuation>,
    ) -> Result<Value, KnutError> {
        let mut input = Vec::new();
        if let Some(continuation) = continuation {
            for part in &continuation.parts {
                if part.kind != "openai_response_input" {
                    return Err(KnutError::Model(
                        "continuation belongs to another transport".to_owned(),
                    ));
                }
                let context: Value = serde_json::from_str(&part.value)
                    .map_err(|_| KnutError::Model("invalid Responses continuation".to_owned()))?;
                if context["origin"] != self.config.base_url
                    || context["model"] != self.config.model
                {
                    return Err(KnutError::Model(
                        "continuation belongs to another endpoint/model".to_owned(),
                    ));
                }
                input.extend(
                    context["input"]
                        .as_array()
                        .ok_or_else(|| {
                            KnutError::Model(
                                "Responses continuation has no input history".to_owned(),
                            )
                        })?
                        .iter()
                        .cloned(),
                );
            }
        }
        let mut user = request.instruction.clone();
        if !request.input.is_null() {
            user.push_str("\n\n");
            user.push_str(
                &serde_json::to_string_pretty(&request.input)
                    .map_err(|_| KnutError::Model("cannot encode model input".to_owned()))?,
            );
        }
        input.push(json!({"role": "user", "content": user}));
        let mut body = json!({
            "model": self.config.model,
            "input": input,
            "store": false,
            "stream": true,
            "include": ["reasoning.encrypted_content"],
        });
        if let Some(effort) = self.config.reasoning_effort {
            body["reasoning"] = json!({"effort": effort.as_str()});
        }
        Ok(body)
    }

    pub(super) async fn responses_turn(
        &self,
        request: &ModelRequest,
        continuation: Option<&Continuation>,
        sink: &mut (dyn ModelStreamSink + Send),
    ) -> Result<ModelResponse, KnutError> {
        let started = Instant::now();
        let body = self.responses_body(request, continuation)?;
        let token = if let Some(client_id) = self.config.chatgpt_client() {
            crate::openai_auth::access_token(client_id).await?
        } else {
            self.config.api_key.clone()
        };
        let response = self
            .http
            .post(format!("{}/responses", self.config.base_url))
            .bearer_auth(token)
            .json(&body)
            .send()
            .await
            .map_err(|_| KnutError::ModelUnavailable("Responses transport failed".to_owned()))?;
        if !response.status().is_success() {
            let status = response.status();
            let detail = response.text().await.unwrap_or_default();
            return Err(provider_error(Some(status), &detail));
        }
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        let mut data = String::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(
                &chunk.map_err(|_| incomplete(sink, "Responses stream interrupted"))?,
            );
            if bytes.len() > MAX_FRAME_BYTES {
                return Err(incomplete(
                    sink,
                    "Responses stream frame exceeds size limit",
                ));
            }
            while let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
                let line = std::str::from_utf8(&bytes[..newline])
                    .map_err(|_| incomplete(sink, "invalid UTF-8 in Responses stream"))?
                    .trim_end_matches('\r')
                    .to_owned();
                bytes.drain(..=newline);
                if let Some(payload) = line.strip_prefix("data:") {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(payload.trim_start_matches(' '));
                    if data.len() > MAX_FRAME_BYTES {
                        return Err(incomplete(
                            sink,
                            "Responses stream frame exceeds size limit",
                        ));
                    }
                } else if line.is_empty() && !data.is_empty() {
                    let frame: Value = serde_json::from_str(&data)
                        .map_err(|_| incomplete(sink, "malformed Responses stream event"))?;
                    data.clear();
                    match frame["type"].as_str() {
                        Some("response.output_text.delta") => {
                            sink.on_event(ModelStreamEvent::TextDelta {
                                text: required_string(&frame, "delta")?.to_owned(),
                            })
                        }
                        Some("response.reasoning_summary_text.delta") => {
                            sink.on_event(ModelStreamEvent::ReasoningDelta {
                                text: required_string(&frame, "delta")?.to_owned(),
                            })
                        }
                        Some("response.output_item.added")
                            if frame["item"]["type"] == "function_call" =>
                        {
                            sink.on_event(ModelStreamEvent::ToolCallStarted {
                                id: required_string(&frame["item"], "call_id")?.to_owned(),
                                name: required_string(&frame["item"], "name")?.to_owned(),
                            });
                        }
                        Some("response.completed") => {
                            let result =
                                self.decode_response(&frame["response"], &body, started.elapsed())?;
                            for call in &result.tool_calls {
                                sink.on_event(ModelStreamEvent::ToolCallArgumentsDelta {
                                    id: call.id.clone(),
                                    delta: call.arguments.to_string(),
                                });
                                sink.on_event(ModelStreamEvent::ToolCallEnded {
                                    id: call.id.clone(),
                                });
                            }
                            sink.on_event(ModelStreamEvent::Completed {
                                usage: result.usage,
                            });
                            return Ok(result);
                        }
                        Some("response.failed" | "error") => {
                            sink.on_event(ModelStreamEvent::Incomplete {
                                reason: "Responses inference failed".to_owned(),
                            });
                            let error = if frame["type"] == "error" {
                                &frame
                            } else {
                                &frame["response"]
                            };
                            return Err(provider_error(None, &error.to_string()));
                        }
                        Some("response.incomplete") => {
                            return Err(incomplete(sink, "Responses inference incomplete"));
                        }
                        Some(_) => {}
                        None => return Err(incomplete(sink, "Responses event has no type")),
                    }
                }
            }
        }
        Err(incomplete(
            sink,
            "Responses stream ended without response.completed",
        ))
    }

    fn decode_response(
        &self,
        response: &Value,
        request: &Value,
        latency: std::time::Duration,
    ) -> Result<ModelResponse, KnutError> {
        if response["status"] != "completed" || !response["error"].is_null() {
            return Err(KnutError::Model(
                "Responses terminal event is not successful".to_owned(),
            ));
        }
        let output = response["output"]
            .as_array()
            .ok_or_else(|| KnutError::Model("Responses envelope has no output array".to_owned()))?;
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        for item in output {
            if item["status"]
                .as_str()
                .is_some_and(|status| status != "completed")
            {
                return Err(KnutError::Model(
                    "Responses output item is incomplete".to_owned(),
                ));
            }
            match required_string(item, "type")? {
                "message" => {
                    let parts = item["content"].as_array().ok_or_else(|| {
                        KnutError::Model("Responses message has no content".to_owned())
                    })?;
                    for part in parts {
                        match required_string(part, "type")? {
                            "output_text" => content.push_str(required_string(part, "text")?),
                            "refusal" => {
                                return Err(KnutError::Model(
                                    "Responses model refused the request".to_owned(),
                                ));
                            }
                            _ => {
                                return Err(KnutError::Model(
                                    "unsupported Responses content type".to_owned(),
                                ));
                            }
                        }
                    }
                }
                "function_call" => tool_calls.push(ToolCall {
                    id: required_string(item, "call_id")?.to_owned(),
                    name: required_string(item, "name")?.to_owned(),
                    arguments: serde_json::from_str(required_string(item, "arguments")?).map_err(
                        |_| {
                            KnutError::Model(
                                "Responses function arguments are invalid JSON".to_owned(),
                            )
                        },
                    )?,
                }),
                "reasoning" => {}
                _ => {
                    return Err(KnutError::Model(
                        "unsupported Responses output item".to_owned(),
                    ));
                }
            }
        }
        if content.is_empty() && tool_calls.is_empty() {
            return Err(KnutError::Model(
                "Responses returned no answer or tool calls".to_owned(),
            ));
        }
        let mut history = request["input"].as_array().unwrap().clone();
        history.extend(output.iter().cloned());
        let mut identity = self.identity();
        identity.model = required_string(response, "model")?.to_owned();
        Ok(ModelResponse {
            content, identity, latency, tool_calls,
            usage: Usage {
                input_tokens: response["usage"]["input_tokens"].as_u64(),
                output_tokens: response["usage"]["output_tokens"].as_u64(),
            },
            continuation: Continuation { parts: vec![ContinuationPart {
                kind: "openai_response_input".to_owned(),
                value: json!({"origin": self.config.base_url, "model": self.config.model, "input": history}).to_string(),
            }] },
        })
    }
}

fn required_string<'a>(value: &'a Value, name: &str) -> Result<&'a str, KnutError> {
    value[name]
        .as_str()
        .ok_or_else(|| KnutError::Model(format!("Responses field {name} is missing or invalid")))
}

fn incomplete(sink: &mut (dyn ModelStreamSink + Send), reason: &str) -> KnutError {
    sink.on_event(ModelStreamEvent::Incomplete {
        reason: reason.to_owned(),
    });
    KnutError::Model(reason.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProviderTransport;
    use crate::provider::tests::{FixtureServer, sse};
    use crate::{BufferedSink, ExpectedArtifact, ModelTier, ProviderConfig};

    fn response(text: &str) -> Value {
        json!({"status":"completed", "model":"gpt-6.1-sol", "output":[
            {"type":"reasoning","encrypted_content":"opaque-state","summary":[]},
            {"type":"message","role":"assistant","status":"completed","content":[{"type":"output_text","text":text}]}
        ],"usage":{"input_tokens":17,"output_tokens":9}})
    }

    fn model(base: String) -> ProviderModel {
        ProviderModel::new(
            ProviderConfig::new("fixture-key", base, "gpt-6.1-sol")
                .with_transport(ProviderTransport::Responses),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn responses_streams_and_replays_full_encrypted_context() {
        let envelope = response("pong 🦉");
        let wire = sse(&json!({"type":"response.output_text.delta","delta":"pong 🦉"}).to_string())
            + &sse(&json!({"type":"response.completed","response":envelope}).to_string());
        let (server, _) =
            FixtureServer::start_chunked(vec![(200, wire.clone()), (200, wire)], 1).await;
        let model = model(server.base_url());
        let request = ModelRequest::new("Reply with pong", ExpectedArtifact::Text);
        let mut sink = BufferedSink::new();
        let first = model.stream(&request, &mut sink).await.unwrap();
        assert_eq!(first.content, "pong 🦉");
        assert_eq!(
            first.usage,
            Usage {
                input_tokens: Some(17),
                output_tokens: Some(9)
            }
        );
        let second = model
            .continue_turn(Some(&first.continuation), &request)
            .await
            .unwrap();
        assert_eq!(second.identity.tier, ModelTier::Reasoner);
        let sent = server.requests();
        let headers = server.headers();
        assert!(headers[0].starts_with("POST /responses HTTP/1.1"));
        assert!(
            headers[0]
                .to_ascii_lowercase()
                .contains("authorization: bearer fixture-key")
        );
        assert_eq!(sent[0]["store"], false);
        assert_eq!(sent[0]["stream"], true);
        assert!(sent[0].get("messages").is_none());
        assert_eq!(sent[1]["input"][1]["encrypted_content"], "opaque-state");
        assert_eq!(sent[1]["input"][2]["role"], "assistant");
        assert_eq!(sent[1]["input"].as_array().unwrap().len(), 4);
    }

    #[tokio::test]
    async fn late_subscription_error_never_becomes_success() {
        let wire = sse(&json!({"type":"response.output_text.delta","delta":"partial"}).to_string())
            + &sse(&json!({"type":"response.failed","response":{"error":{"code":"subscription_sharing_usage_limit_exceeded"}}}).to_string());
        let (server, _) = FixtureServer::start(vec![(200, wire)]).await;
        let sink = &mut BufferedSink::new();
        let error = model(server.base_url())
            .stream(&ModelRequest::new("task", ExpectedArtifact::Text), sink)
            .await
            .unwrap_err();
        assert!(matches!(error, KnutError::ModelRateLimit(_)));
        assert!(
            !sink
                .events()
                .iter()
                .any(|e| matches!(e, ModelStreamEvent::Completed { .. }))
        );
    }

    #[tokio::test]
    async fn eof_incomplete_and_malformed_tool_calls_are_refused() {
        let mut bad_call = response("");
        bad_call["output"] =
            json!([{"type":"function_call","call_id":"call","name":"read","arguments":"{broken"}]);
        let cases = [
            sse(r#"{"type":"response.output_text.delta","delta":"partial"}"#),
            sse(r#"{"type":"response.incomplete"}"#),
            sse(&json!({"type":"response.completed","response":bad_call}).to_string()),
        ];
        for wire in cases {
            let (server, _) = FixtureServer::start(vec![(200, wire)]).await;
            let mut sink = BufferedSink::new();
            assert!(
                model(server.base_url())
                    .stream(
                        &ModelRequest::new("task", ExpectedArtifact::Text),
                        &mut sink
                    )
                    .await
                    .is_err()
            );
            assert!(
                !sink
                    .events()
                    .iter()
                    .any(|e| matches!(e, ModelStreamEvent::Completed { .. }))
            );
        }
    }

    #[tokio::test]
    async fn multiple_function_calls_are_complete_before_publication() {
        let mut envelope = response("");
        envelope["output"] = json!([
            {"type":"function_call", "status":"completed", "call_id":"first", "name":"read", "arguments":"{\"path\":\"a\"}"},
            {"type":"function_call", "status":"completed", "call_id":"second", "name":"read", "arguments":"{\"path\":\"b\"}"}
        ]);
        let wire = sse(&json!({"type":"response.completed","response":envelope}).to_string());
        let (server, _) = FixtureServer::start(vec![(200, wire)]).await;
        let mut sink = BufferedSink::new();
        let answer = model(server.base_url())
            .stream(
                &ModelRequest::new("task", ExpectedArtifact::Text),
                &mut sink,
            )
            .await
            .unwrap();
        assert_eq!(answer.tool_calls.len(), 2);
        assert_eq!(answer.tool_calls[1].arguments, json!({"path":"b"}));
        let events = sink.events();
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, ModelStreamEvent::ToolCallEnded { .. }))
                .count(),
            2
        );
        assert!(matches!(
            events.last(),
            Some(ModelStreamEvent::Completed { .. })
        ));
    }

    #[tokio::test]
    async fn models_are_listed_from_the_authenticated_catalog() {
        let (server, _) = FixtureServer::start(vec![(
            200,
            json!({"data":[{"id":"gpt-6.1-sol"},{"id":"gpt-5-codex"}]}).to_string(),
        )])
        .await;
        let catalog = model(server.base_url()).list_models().await.unwrap();
        assert_eq!(catalog[1].0, "gpt-5-codex");
    }

    #[test]
    fn response_configuration_does_not_send_chat_fields_or_foreign_continuations() {
        let model = ProviderModel::new(
            ProviderConfig::openai("key", "gpt-6.1-sol")
                .with_reasoning_effort(super::super::ReasoningEffort::XHigh),
        )
        .unwrap();
        let request = ModelRequest::new("task", ExpectedArtifact::Json);
        let body = model.responses_body(&request, None).unwrap();
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert!(body.get("reasoning_effort").is_none());
        let foreign = Continuation {
            parts: vec![ContinuationPart {
                kind: "reasoning_content".to_owned(),
                value: "private".to_owned(),
            }],
        };
        assert!(model.responses_body(&request, Some(&foreign)).is_err());
        let mut valid = response("answer");
        valid["usage"] = Value::Null;
        let decoded = model
            .decode_response(&valid, &body, std::time::Duration::ZERO)
            .unwrap();
        assert_eq!(decoded.usage, Usage::default());
        let other = ProviderModel::new(ProviderConfig::openai("key", "gpt-5-codex")).unwrap();
        assert!(
            other
                .responses_body(&request, Some(&decoded.continuation))
                .is_err()
        );
    }
}
