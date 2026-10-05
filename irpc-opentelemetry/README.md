# irpc-opentelemetry

OpenTelemetry span propagation for [irpc](https://github.com/n0-computer/irpc).

The client writes the context of the current span into each request, and the
server sets the parent of the request span from it, so a trace continues across
irpc calls.

## Version compatibility

Your application must use the same semver-compatible versions of
`opentelemetry` and `tracing-opentelemetry` as this crate:

| irpc-opentelemetry | opentelemetry | tracing-opentelemetry |
|--------------------|---------------|-----------------------|
| 0.1                | 0.32          | 0.33                  |

Both crates keep their state in statics: the global propagator and the
`tracing-opentelemetry` layer. With a second copy of either in your dependency
tree, everything compiles, but irpc silently propagates nothing. To check,
run `cargo tree -i opentelemetry` and `cargo tree -i tracing-opentelemetry`:
each should print one version. If cargo reports that the name is ambiguous,
you have two copies.

This crate makes a breaking release whenever it moves to a new version of
`opentelemetry` or `tracing-opentelemetry`.

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
