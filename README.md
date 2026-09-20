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

The core contains:

- typed routing decisions,
- a `SystemOne` trait,
- deterministic System 0 fast paths (named rules, routing cache),
- batched ingress judgments in one System One call,
- confidence-gated escalation,
- a capability-oriented tool registry with bounded candidate discovery,
- a model tier abstraction with a bounded compute cascade,
- a validated behavior-tree core with cooperative cancellation,
- an edge selector so System One re-routes after node results,
- a policy layer: permission and side-effect approval are Rust, not judgment,
- a System Two planner that produces validated plans only,
- eval traces, shadow routing, replay, and offline benchmarks.

Open follow-ups live in the issue tracker; the Jev HTTP adapter (#2) is the next isolated piece.

## CLI playground

```console
$ cargo run -- route weather
weather -> Tool { capability: "weather" } (confidence 1.00, via system-0)

$ cargo run -- route "find my notes" --verbose
$ cargo run -- demo-tree          # execute the canned validated tree
$ cargo run -- eval               # hybrid routing vs always-reasoner
$ cargo run -- repl               # route prompts interactively
```

The playground runs fully offline against a deterministic mock System One; live Jev routing is issue #2.

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
