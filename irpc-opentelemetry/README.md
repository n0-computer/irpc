# irpc-opentelemetry

OpenTelemetry span propagation for [irpc](https://github.com/n0-computer/irpc).

The client writes the context of its request span into each request, and the
server sets the parent of its request span from it, so a trace continues across
irpc calls.

## Version compatibility

Your application must use semver-compatible versions of `irpc`,
`opentelemetry`, and `tracing-opentelemetry` with this crate:

| irpc-opentelemetry | irpc | opentelemetry | tracing-opentelemetry |
|--------------------|------|---------------|-----------------------|
| 0.1                | 0.17 | 0.33          | 0.34                  |

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

## Filtering

irpc creates its request spans at debug level, so that a console filter at
info level does not prefix every log line with them. The macro-generated spans
have the target of the module that defines the protocol.

The server sets the parent of its request span from the context that the
client sent. It can only do that if the `tracing-opentelemetry` layer records
the request span. If a filter drops the span, nothing fails: the server starts
a new trace instead of joining the trace of the client. So give the
`tracing-opentelemetry` layer its own filter that lets the request spans
through, and keep the console filter at info:

```rust
use tracing_subscriber::{EnvFilter, Layer, Registry, layer::SubscriberExt};

let subscriber = Registry::default()
    .with(
        tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_filter(EnvFilter::new("info,my_protocol=debug")),
    )
    .with(irpc_opentelemetry::layer())
    .with(tracing_subscriber::fmt::layer().with_filter(EnvFilter::new("info")));
```

Replace `my_protocol` with the module path of the protocol. The directive
also sends the debug events of that module to OpenTelemetry, so a protocol in a
module of its own keeps the traces small. irpc's own debug events have the
target `irpc` and stay out.

A single filter for the whole subscriber, such as
`tracing_subscriber::fmt().with_env_filter("info")`, cannot do both: at info
level it drops the request spans, and at debug level the console shows them.

The client does not need its request span recorded. If the
`tracing-opentelemetry` layer drops it, the client sends the context of the
span that it is in.

## License

Copyright 2026 N0, INC.

This project is licensed under either of

- Apache License, Version 2.0, ([LICENSE-APACHE](../LICENSE-APACHE) or
  http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](../LICENSE-MIT) or
  http://opensource.org/licenses/MIT)

at your option.
