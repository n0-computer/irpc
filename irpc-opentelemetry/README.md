# irpc-opentelemetry

OpenTelemetry span propagation for [irpc](https://github.com/n0-computer/irpc).

The client writes the context of the current span into each request, and the
server sets the parent of the request span from it, so a trace continues across
irpc calls.

## Version compatibility

Your application must use semver-compatible versions of `irpc`,
`opentelemetry`, and `tracing-opentelemetry` with this crate:

| irpc-opentelemetry | irpc | opentelemetry | tracing-opentelemetry |
|--------------------|------|---------------|-----------------------|
| 0.1                | 0.17 | 0.32          | 0.33                  |

With a second copy of one of them in your dependency tree, everything
compiles, but no span context reaches the remote:

- `opentelemetry` keeps the propagator that you register with
  `opentelemetry::global::set_text_map_propagator` in a static. A second copy
  has its own static, with a propagator that does nothing.
- This crate finds the `tracing-opentelemetry` layer in the subscriber by its
  type. The layer type of a second copy is a different type.
- irpc finds the layer of this crate in the subscriber by its type, too. The
  layer type of a second copy of irpc is a different type.

To check, run `cargo tree -i irpc`, `cargo tree -i opentelemetry`, and
`cargo tree -i tracing-opentelemetry`: each should print one version. If cargo
reports that the name is ambiguous, you have two copies.

This crate makes a breaking release whenever it moves to a new major version
of `irpc`, or a new version of `opentelemetry` or `tracing-opentelemetry`.

## Usage

Enable span propagation on the protocol with
`#[rpc_requests(..., span_propagation)]`, and add the layer of this crate to the
tracing subscriber, next to the `tracing-opentelemetry` layer:

```rust
use tracing_subscriber::{Registry, layer::SubscriberExt};

opentelemetry::global::set_text_map_propagator(
    opentelemetry_sdk::propagation::TraceContextPropagator::new(),
);
let tracer = opentelemetry::global::tracer("app");
let subscriber = Registry::default()
    .with(tracing_opentelemetry::layer().with_tracer(tracer))
    .with(irpc_opentelemetry::layer());
tracing::subscriber::set_global_default(subscriber)?;
```

Client and server both need the layer. See
[`examples/span_propagation.rs`](examples/span_propagation.rs) for a full
example that exports to Jaeger.

## License

Copyright 2026 N0, INC.

This project is licensed under either of

- Apache License, Version 2.0, ([LICENSE-APACHE](../LICENSE-APACHE) or
  http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](../LICENSE-MIT) or
  http://opensource.org/licenses/MIT)

at your option.
