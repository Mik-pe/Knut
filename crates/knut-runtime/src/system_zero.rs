use std::{
    collections::HashMap,
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
};

use serde_json::Value;

use crate::{Decision, DecisionInput, ModelTier, Risk, Route};

/// What a single System 0 rule concludes about an input.
#[derive(Debug, Clone, PartialEq)]
pub enum RuleVerdict {
    /// The answer is mechanically knowable; skip System One.
    Decide(Decision),
    /// A hard limit or permission denial; never route.
    Blocked { reason: String },
    /// No deterministic answer; hand off to System One.
    Pass,
}

/// The first non-pass rule outcome, carrying the rule name for traces.
#[derive(Debug, Clone, PartialEq)]
pub struct SystemZeroOutcome {
    pub rule: String,
    pub verdict: RuleVerdict,
}

/// A deterministic fast-path rule. Rules must be exact: a rule that cannot
/// name why it fired has no place here.
pub trait SystemZeroRule: Send + Sync {
    fn name(&self) -> &str;
    fn evaluate(&self, input: &DecisionInput) -> RuleVerdict;
}

/// In-memory cache of prior routing decisions.
///
/// Keys cover every decision-relevant input: prompt, the full capability
/// set, and the serialized state. Anything else that matters later must be
/// added to the key before it affects routing.
#[derive(Default)]
pub struct RoutingCache {
    entries: Mutex<HashMap<u64, Decision>>,
}

impl RoutingCache {
    fn key(input: &DecisionInput) -> u64 {
        let mut capabilities = input.capabilities.clone();
        capabilities.sort();

        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        input.prompt.hash(&mut hasher);
        capabilities.hash(&mut hasher);
        input.state.to_string().hash(&mut hasher);
        hasher.finish()
    }

    fn get(&self, input: &DecisionInput) -> Option<Decision> {
        self.entries
            .lock()
            .expect("routing cache poisoned")
            .get(&Self::key(input))
            .cloned()
    }

    /// Record the decision System One produced for this input.
    pub fn store(&self, input: &DecisionInput, decision: &Decision) {
        // Never cache a decision that cannot be gated sensibly later.
        if !decision.confidence.is_finite() {
            return;
        }

        self.entries
            .lock()
            .expect("routing cache poisoned")
            .insert(Self::key(input), decision.clone());
    }
}

impl SystemZeroRule for RoutingCache {
    fn name(&self) -> &str {
        "routing-cache"
    }

    fn evaluate(&self, input: &DecisionInput) -> RuleVerdict {
        match self.get(input) {
            Some(decision) => RuleVerdict::Decide(decision),
            None => RuleVerdict::Pass,
        }
    }
}

/// Hard limit: an empty prompt carries nothing to route.
pub struct InvalidInputRule;

impl SystemZeroRule for InvalidInputRule {
    fn name(&self) -> &str {
        "invalid-input"
    }

    fn evaluate(&self, input: &DecisionInput) -> RuleVerdict {
        if input.prompt.trim().is_empty() {
            RuleVerdict::Blocked {
                reason: "prompt is empty".to_owned(),
            }
        } else {
            RuleVerdict::Pass
        }
    }
}

/// The runtime supplied a fact that the requested capability is unavailable.
///
/// Availability comes only from `state.unavailable_capabilities`, never from
/// guessing what a prompt "means".
pub struct UnavailableCapabilityRule;

impl SystemZeroRule for UnavailableCapabilityRule {
    fn name(&self) -> &str {
        "capability-unavailable"
    }

    fn evaluate(&self, input: &DecisionInput) -> RuleVerdict {
        let requested = input.prompt.trim();
        let Some(unavailable) = input.state.get("unavailable_capabilities") else {
            return RuleVerdict::Pass;
        };

        let Value::Array(entries) = unavailable else {
            return RuleVerdict::Pass;
        };

        for entry in entries {
            if entry.as_str() == Some(requested) {
                return RuleVerdict::Blocked {
                    reason: format!("capability {requested:?} is unavailable"),
                };
            }
        }

        RuleVerdict::Pass
    }
}

/// Exact selection: the prompt is literally a capability ID.
///
/// This is a command, not a keyword heuristic. `"weather"` routes to the
/// weather capability; "what is the weather" never matches.
pub struct ExplicitCapabilityRule;

impl SystemZeroRule for ExplicitCapabilityRule {
    fn name(&self) -> &str {
        "explicit-capability"
    }

    fn evaluate(&self, input: &DecisionInput) -> RuleVerdict {
        let requested = input.prompt.trim();

        if input.capabilities.iter().any(|c| c == requested) {
            RuleVerdict::Decide(Decision {
                route: Route::Act,
                confidence: 1.0,
                retrieval: None,
                capability: Some(requested.to_owned()),
                model_tier: ModelTier::Fast,
                risk: Risk::Low,
                parallelizable: false,
            })
        } else {
            RuleVerdict::Pass
        }
    }
}

/// The deterministic layer in front of System One.
///
/// Rules run in registration order and the first non-pass verdict wins.
/// The default rule set contains only exact, nameable decisions.
pub struct SystemZero {
    rules: Vec<Arc<dyn SystemZeroRule>>,
    cache: Arc<RoutingCache>,
}

impl Default for SystemZero {
    fn default() -> Self {
        Self::with_default_rules()
    }
}

impl SystemZero {
    /// No rules at all; every input falls through to System One.
    pub fn empty() -> Self {
        Self {
            rules: Vec::new(),
            cache: Arc::new(RoutingCache::default()),
        }
    }

    /// The built-in exact-match rule set plus the routing cache.
    pub fn with_default_rules() -> Self {
        Self::empty()
            .with_rule(InvalidInputRule)
            .with_rule(UnavailableCapabilityRule)
            .with_rule(ExplicitCapabilityRule)
            .with_cache()
    }

    /// Add a rule at the end of the chain.
    pub fn with_rule(mut self, rule: impl SystemZeroRule + 'static) -> Self {
        self.rules.push(Arc::new(rule));
        self
    }

    /// Enable the routing cache as the final rule.
    pub fn with_cache(mut self) -> Self {
        self.rules.push(self.cache.clone());
        self
    }

    /// Handle for recording decisions System One produced.
    pub fn cache(&self) -> Arc<RoutingCache> {
        self.cache.clone()
    }

    /// Run the chain; `None` means every rule passed.
    pub fn evaluate(&self, input: &DecisionInput) -> Option<SystemZeroOutcome> {
        for rule in &self.rules {
            match rule.evaluate(input) {
                RuleVerdict::Pass => continue,
                verdict => {
                    return Some(SystemZeroOutcome {
                        rule: rule.name().to_owned(),
                        verdict,
                    });
                }
            }
        }

        None
    }
}
