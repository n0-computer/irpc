//! Demonstrates the typical server-actor pattern over Unix domain sockets.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    cli::run().await
}

mod proto {
    use std::collections::HashMap;

    use anyhow::Result;
    use irpc::{channel::oneshot, rpc::RemoteService, rpc_requests, Client, WithChannels};
    use serde::{Deserialize, Serialize};

    #[rpc_requests(message = FooMessage)]
    #[derive(Debug, Serialize, Deserialize)]
    pub enum FooProtocol {
        #[rpc(tx = oneshot::Sender<Option<String>>)]
        #[wrap(GetRequest, derive(Clone))]
        Get(String),

        #[rpc(tx = oneshot::Sender<Option<String>>)]
        #[wrap(SetRequest)]
        Set { key: String, value: String },
    }

    pub async fn listen(path: &str) -> Result<()> {
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::task::spawn(actor(rx));
        let client = Client::<FooProtocol>::local(tx);

        let endpoint = irpc_uds::server_endpoint(path)?;
        let handler = FooProtocol::remote_handler(client.as_local().unwrap());
        println!("listening on {path}");

        irpc::rpc::listen(endpoint, handler).await;
        Ok(())
    }

    async fn actor(mut rx: tokio::sync::mpsc::Receiver<FooMessage>) {
        let mut store = HashMap::new();
        while let Some(msg) = rx.recv().await {
            match msg {
                FooMessage::Get(msg) => {
                    let WithChannels { inner, tx, .. } = msg;
                    println!("handle request: {inner:?}");
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

    pub async fn connect(path: &str) -> Result<Client<FooProtocol>> {
        println!("connecting to {path}");
        let client = irpc_uds::client(path).await?;
        Ok(client)
    }
}

mod cli {
    use anyhow::Result;
    use clap::Parser;

    use crate::proto::{connect, listen, GetRequest, SetRequest};

    #[derive(Debug, Parser)]
    enum Cli {
        Listen {
            #[clap(long, default_value = "/tmp/irpc-uds-example.sock")]
            path: String,
        },
        Connect {
            #[clap(long, default_value = "/tmp/irpc-uds-example.sock")]
            path: String,
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
            Cli::Listen { path } => listen(&path).await?,
            Cli::Connect { path, command } => {
                let client = connect(&path).await?;
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
