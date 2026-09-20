# Knut

Knut is an experimental agentic harness in Rust.

The core idea is to spend expensive generative reasoning only when it is useful. Fast, bounded System One decisions route work between clarification, retrieval, tools, fast generation, and deeper reasoning.

```text
user prompt
    |
    v
System 0        deterministic rules / cache
    |
    v
System 1        Jev-style bounded decisions
    |
    +---- ask user
    +---- retrieve
    +---- tool
    +---- fast model
    `---- reasoner
              |
              v
        validated plan/tree
              |
              v
      System 1 chooses next edge
```

## Prototype goals

- Keep control flow explicit and testable.
- Treat System One as a router and edge selector, not a text generator.
- Batch independent routing judgments when possible.
- Escalate on uncertainty instead of trusting a weak route.
- Keep permissions and side effects outside model judgment.
- Make every routing decision observable and evaluable.
- Stay model-provider agnostic above thin adapters.

## Current scope

The first draft contains:

- typed routing decisions,
- a `SystemOne` trait,
- confidence-gated escalation,
- a capability-oriented tool registry,
- a small runtime that turns decisions into executable next actions,
- a static System One implementation for tests and local experiments.

It deliberately does **not** contain a planner, behavior-tree executor, LLM provider, or Jev HTTP adapter yet. Those are isolated follow-up pieces rather than assumptions baked into the core.

## Example

```rust
use knut::{
    Action, Decision, ModelTier, Risk, Route, StaticSystemOne, Knut,
};

# async fn demo() -> Result<(), knut::KnutError> {
let system_one = StaticSystemOne::new(Decision {
    route: Route::Generate,
    confidence: 0.94,
    retrieval: None,
    capability: None,
    model_tier: ModelTier::Fast,
    risk: Risk::Low,
    parallelizable: false,
});

let knut = Knut::new(system_one).with_confidence_floor(0.75);
let action = knut.next("Explain this error", vec![]).await?;

assert_eq!(action, Action::Generate(ModelTier::Fast));
# Ok(())
# }
```

## Design rule

If Knut can write the valid `match` arms before asking the model, System One is a candidate. If the output space cannot be bounded ahead of time, hand the work to a generative model.

The interesting experiment is not merely `Jev -> LLM`. It is a loop:

```text
System One -> cheap action -> System One verifies/routes
                              |
                              +-- confident -> continue
                              `-- uncertain -> stronger model
```

That makes fast judgment part of the runtime rather than a one-time front door.
