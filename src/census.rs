use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::ModelTier;
use crate::model::{ExpectedArtifact, ModelIdentity, ModelRequest, ModelResponse, Usage};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallPurpose {
    #[default]
    Response,
    Planning,
    PlanRepair,
    PlanExecution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallOutcome {
    Returned,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCallRecord {
    pub purpose: CallPurpose,
    pub identity: ModelIdentity,
    pub tier: ModelTier,
    pub expected_artifact: ExpectedArtifact,
    pub streaming: bool,
    pub outcome: CallOutcome,
    pub elapsed_ms: u64,
    pub usage: Usage,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ModelCallReport {
    pub version: u32,
    pub scope: String,
    pub run_succeeded: bool,
    pub elapsed_ms: u64,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub unknown_usage_calls: usize,
    pub calls: Vec<ModelCallRecord>,
}

impl ModelCallReport {
    pub fn new(calls: Vec<ModelCallRecord>, run_succeeded: bool, elapsed_ms: u64) -> Self {
        Self {
            version: 1,
            scope: "generator calls; excludes Jev, cached-token breakdown and costs".to_owned(),
            run_succeeded,
            elapsed_ms,
            input_tokens: calls
                .iter()
                .try_fold(0u64, |sum, call| sum.checked_add(call.usage.input_tokens?)),
            output_tokens: calls
                .iter()
                .try_fold(0u64, |sum, call| sum.checked_add(call.usage.output_tokens?)),
            unknown_usage_calls: calls
                .iter()
                .filter(|call| {
                    call.usage.input_tokens.is_none() || call.usage.output_tokens.is_none()
                })
                .count(),
            calls,
        }
    }
}

type Records = Arc<Mutex<Vec<ModelCallRecord>>>;

tokio::task_local! {
    static ACTIVE: Records;
}

// The scope follows polled child futures, not independently spawned tasks.
pub async fn capture_model_calls<F: std::future::Future>(
    future: F,
) -> (F::Output, Vec<ModelCallRecord>) {
    let records = Records::default();
    let _guard = CaptureOnDrop {
        records: Arc::clone(&records),
        parent: ACTIVE.try_with(Arc::clone).ok(),
    };
    let output = ACTIVE.scope(Arc::clone(&records), future).await;
    let calls = std::mem::take(&mut *records.lock().expect("model census lock"));
    let _ = ACTIVE.try_with(|parent| {
        parent
            .lock()
            .expect("model census lock")
            .extend(calls.iter().cloned());
    });
    (output, calls)
}

struct CaptureOnDrop {
    records: Records,
    parent: Option<Records>,
}

impl Drop for CaptureOnDrop {
    fn drop(&mut self) {
        if let Some(parent) = &self.parent {
            parent
                .lock()
                .expect("model census lock")
                .extend(self.records.lock().expect("model census lock").drain(..));
        }
    }
}

pub(crate) struct CallMeasurement {
    records: Option<Records>,
    record: ModelCallRecord,
    started: Instant,
}

impl CallMeasurement {
    pub(crate) fn start(
        request: &ModelRequest,
        identity: ModelIdentity,
        tier: ModelTier,
        streaming: bool,
    ) -> Self {
        Self {
            records: ACTIVE.try_with(Arc::clone).ok(),
            record: ModelCallRecord {
                purpose: request.purpose,
                identity,
                tier,
                expected_artifact: request.expected_artifact,
                streaming,
                outcome: CallOutcome::Cancelled,
                elapsed_ms: 0,
                usage: Usage::default(),
            },
            started: Instant::now(),
        }
    }

    pub(crate) fn finish(mut self, result: &Result<ModelResponse, crate::KnutError>) {
        match result {
            Ok(response) => {
                self.record.outcome = CallOutcome::Returned;
                self.record.usage = response.usage;
            }
            Err(_) => self.record.outcome = CallOutcome::Failed,
        }
    }
}

impl Drop for CallMeasurement {
    fn drop(&mut self) {
        if let Some(records) = &self.records {
            self.record.elapsed_ms =
                self.started.elapsed().as_millis().min(u64::MAX as u128) as u64;
            records
                .lock()
                .expect("model census lock")
                .push(self.record.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AcceptAllVerifier, ComputeCascade, KnutError, Model};
    use async_trait::async_trait;

    struct FakeModel {
        fails: bool,
        streaming: bool,
    }

    #[async_trait]
    impl Model for FakeModel {
        fn identity(&self) -> ModelIdentity {
            ModelIdentity {
                provider: "fixture".to_owned(),
                model: "fixture".to_owned(),
                tier: ModelTier::Reasoner,
            }
        }

        fn capabilities(&self) -> crate::ModelCapabilities {
            crate::ModelCapabilities {
                streaming: self.streaming,
                ..crate::ModelCapabilities::buffered_text()
            }
        }

        async fn complete(&self, _: &ModelRequest) -> Result<ModelResponse, KnutError> {
            if self.fails {
                Err(KnutError::Model("private provider failure".to_owned()))
            } else {
                Ok(ModelResponse::text(
                    "private response",
                    self.identity(),
                    Usage::known(7, 3),
                    std::time::Duration::ZERO,
                ))
            }
        }
    }

    #[tokio::test]
    async fn failures_and_escalations_are_separate_calls_without_payloads() {
        let cascade = ComputeCascade::empty()
            .with_fast(FakeModel {
                fails: true,
                streaming: false,
            })
            .with_reasoner(FakeModel {
                fails: false,
                streaming: false,
            });
        let request = ModelRequest::new("private prompt", ExpectedArtifact::Text)
            .with_input(serde_json::json!({"api_key": "private key"}))
            .with_purpose(CallPurpose::PlanExecution);
        let (result, calls) =
            capture_model_calls(cascade.run(&request, ModelTier::Fast, &AcceptAllVerifier)).await;
        assert!(result.is_ok());
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].outcome, CallOutcome::Failed);
        assert_eq!(calls[0].tier, ModelTier::Fast);
        assert_eq!(calls[1].tier, ModelTier::Reasoner);
        assert!(
            calls
                .iter()
                .all(|call| call.purpose == CallPurpose::PlanExecution)
        );
        let report = ModelCallReport::new(calls, true, 1);
        assert_eq!(report.input_tokens, None);
        assert_eq!(report.output_tokens, None);
        assert_eq!(report.unknown_usage_calls, 1);
        assert!(!serde_json::to_string(&report).unwrap().contains("private"));
    }

    #[tokio::test]
    async fn missing_models_do_not_count_as_calls() {
        let cascade = ComputeCascade::empty();
        let request = ModelRequest::new("x", ExpectedArtifact::Text);
        let (result, calls) =
            capture_model_calls(cascade.run(&request, ModelTier::Fast, &AcceptAllVerifier)).await;
        assert!(result.is_err());
        assert!(calls.is_empty());
    }

    #[tokio::test]
    async fn streaming_and_buffered_fallback_are_counted_once() {
        for streaming in [false, true] {
            let cascade = ComputeCascade::empty().with_reasoner(FakeModel {
                fails: false,
                streaming,
            });
            let request = ModelRequest::new("x", ExpectedArtifact::Text);
            let mut sink = crate::BufferedSink::new();
            let (result, calls) = capture_model_calls(cascade.run_streaming(
                &request,
                ModelTier::Reasoner,
                &AcceptAllVerifier,
                &mut sink,
            ))
            .await;
            assert!(result.is_ok());
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].streaming, streaming);
            assert_eq!(calls[0].usage, Usage::known(7, 3));
        }
    }

    #[tokio::test]
    async fn concurrent_capture_scopes_are_isolated() {
        let cascade = ComputeCascade::empty().with_reasoner(FakeModel {
            fails: false,
            streaming: false,
        });
        let a = ModelRequest::new("a", ExpectedArtifact::Text).with_purpose(CallPurpose::Planning);
        let b =
            ModelRequest::new("b", ExpectedArtifact::Text).with_purpose(CallPurpose::PlanRepair);
        let ((_, first), (_, second)) = tokio::join!(
            capture_model_calls(cascade.run(&a, ModelTier::Reasoner, &AcceptAllVerifier)),
            capture_model_calls(cascade.run(&b, ModelTier::Reasoner, &AcceptAllVerifier)),
        );
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert_eq!(first[0].purpose, CallPurpose::Planning);
        assert_eq!(second[0].purpose, CallPurpose::PlanRepair);
    }

    #[tokio::test]
    async fn unfinished_measurement_is_cancelled_not_free() {
        let (_, calls) = capture_model_calls(async {
            let request = ModelRequest::new("x", ExpectedArtifact::Text);
            let model = FakeModel {
                fails: false,
                streaming: false,
            };
            let _measurement =
                CallMeasurement::start(&request, model.identity(), ModelTier::Reasoner, false);
        })
        .await;
        assert_eq!(calls[0].outcome, CallOutcome::Cancelled);
        assert_eq!(calls[0].usage, Usage::default());
    }

    #[test]
    fn totals_preserve_partial_unknowns_and_overflow() {
        let mut record = ModelCallRecord {
            purpose: CallPurpose::Response,
            identity: FakeModel {
                fails: false,
                streaming: false,
            }
            .identity(),
            tier: ModelTier::Reasoner,
            expected_artifact: ExpectedArtifact::Text,
            streaming: false,
            outcome: CallOutcome::Returned,
            elapsed_ms: 0,
            usage: Usage {
                input_tokens: Some(2),
                output_tokens: None,
            },
        };
        let report = ModelCallReport::new(vec![record.clone(), record.clone()], true, 0);
        assert_eq!(report.input_tokens, Some(4));
        assert_eq!(report.output_tokens, None);
        record.usage = Usage::known(u64::MAX, 0);
        let report = ModelCallReport::new(vec![record.clone(), record], true, 0);
        assert_eq!(report.input_tokens, None);
        assert_eq!(report.output_tokens, Some(0));
    }
    #[tokio::test]
    async fn nested_runtime_census_reaches_the_outer_run_report_once() {
        let cascade = ComputeCascade::empty().with_reasoner(FakeModel {
            fails: false,
            streaming: false,
        });
        let request = ModelRequest::new("test", ExpectedArtifact::Text);
        let ((result, inner), outer) = capture_model_calls(capture_model_calls(cascade.run(
            &request,
            ModelTier::Reasoner,
            &AcceptAllVerifier,
        )))
        .await;
        result.unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(outer, inner);
    }
}
