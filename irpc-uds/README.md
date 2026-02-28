# irpc-uds

Unix domain socket transport for [irpc](https://crates.io/crates/irpc).

Uses QUIC over Unix datagram sockets with plaintext crypto.
This gives you multiplexed bidirectional streams with flow control,
without TLS overhead, over a local Unix socket.

## Usage

**Server:**

```rust
let (tx, rx) = tokio::sync::mpsc::channel(16);
tokio::task::spawn(actor(rx));
let client = irpc::Client::<MyProtocol>::local(tx);

let endpoint = irpc_uds::server_endpoint("/tmp/my.sock")?;
let handler = MyProtocol::remote_handler(client.as_local().unwrap());
irpc::rpc::listen(endpoint, handler).await;
```

**Client:**

```rust
let client: irpc::Client<MyProtocol> = irpc_uds::client("/tmp/my.sock").await?;
let value = client.rpc(GetRequest("key".into())).await?;
```

## Example

Run the demo (key-value store over UDS):

```sh
# Terminal 1: start the server
cargo run -p irpc-uds --example uds-demo -- listen

# Terminal 2: interact with the server
cargo run -p irpc-uds --example uds-demo -- connect set hello world
cargo run -p irpc-uds --example uds-demo -- connect get hello
```

## License

Copyright 2025 N0, INC.

This project is licensed under either of

 * Apache License, Version 2.0, ([LICENSE-APACHE](../LICENSE-APACHE) or
   http://www.apache.org/licenses/LICENSE-2.0)
 * MIT license ([LICENSE-MIT](../LICENSE-MIT) or
   http://opensource.org/licenses/MIT)

at your option.
