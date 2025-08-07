#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::run().await
}

mod proto {
    use std::collections::HashMap;

    use anyhow::Result;
    use iroh::{protocol::Router, Endpoint, NodeId};
    use irpc::{channel::oneshot, rpc_requests, Client, WithChannels};
    use irpc_iroh::IrohProtocol;
    use serde::{Deserialize, Serialize};

    const ALPN: &[u8] = b"iroh-irpc/simple/1";

    #[rpc_requests(message = FooMessage)]
    #[derive(Debug, Serialize, Deserialize)]
    pub enum FooProtocol {
        /// This is the get request.
        #[rpc(tx=oneshot::Sender<Option<String>>)]
        #[wrap(GetRequest, derive(Clone))]
        Get(String),

        /// This is the set request.
        #[rpc(tx=oneshot::Sender<Option<String>>)]
        #[wrap(SetRequest)]
        Set {
            /// This is the key
            key: String,
            /// This is the value
            value: String,
        },
    }

    pub async fn listen() -> Result<()> {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::task::spawn(actor(rx));
        let client = Client::<FooProtocol>::local(tx);

        let endpoint = Endpoint::builder().discovery_n0().bind().await?;
        let protocol = IrohProtocol::with_sender(client.as_local().unwrap());
        let router = Router::builder(endpoint).accept(ALPN, protocol).spawn();
        println!("node id: {}", router.endpoint().node_id());

        tokio::signal::ctrl_c().await?;
        router.shutdown().await?;
        Ok(())
    }

    async fn actor(mut rx: tokio::sync::mpsc::Receiver<FooMessage>) {
        let mut store = HashMap::new();
        while let Some(msg) = rx.recv().await {
            match msg {
                FooMessage::Get(msg) => {
                    let WithChannels { inner, tx, .. } = msg;
                    println!("handle request: {inner:?}");

                    // We can clone `inner` because we added the `Clone` derive to the `wrap` attribute:
                    let _ = inner.clone();

                    let GetRequest(key) = inner;
                    let value = store.get(&key).cloned();
                    tx.send(value).await.ok();
                }
                FooMessage::Set(msg) => {
                    let WithChannels { inner, tx, .. } = msg;
                    println!("handle request: {inner:?}");
                    let SetRequest { key, value } = inner;
                    let prev_value = store.insert(key, value);
                    tx.send(prev_value).await.ok();
                }
            }
        }
    }

    pub async fn connect(node_id: NodeId) -> Result<Client<FooProtocol>> {
        println!("connecting to {node_id}");
        let endpoint = Endpoint::builder().discovery_n0().bind().await?;
        let client = irpc_iroh::client(endpoint, node_id, ALPN);
        Ok(client)
    }
}

mod cli {
    use anyhow::Result;
    use clap::Parser;
    use iroh::NodeId;

    use crate::proto::{connect, listen, GetRequest, SetRequest};

    #[derive(Debug, Parser)]
    enum Cli {
        Listen,
        Connect {
            node_id: NodeId,
            #[clap(subcommand)]
            command: Command,
        },
    }

    #[derive(Debug, Parser)]
    enum Command {
        Get { key: String },
        Set { key: String, value: String },
    }

    pub async fn run() -> Result<()> {
        match Cli::parse() {
            Cli::Listen => listen().await?,
            Cli::Connect { node_id, command } => {
                let client = connect(node_id).await?;
                match command {
                    Command::Get { key } => {
                        println!("get '{key}'");
                        let value = client.rpc(GetRequest(key)).await?;
                        println!("{value:?}");
                    }
                    Command::Set { key, value } => {
                        println!("set '{key}' to '{value}'");
                        let value = client.rpc(SetRequest { key, value }).await?;
                        println!("OK (previous: {value:?})");
                    }
                }
            }
        }
        Ok(())
    }
}
