use std::collections::HashMap;

use irpc::{channel::oneshot, rpc_requests, Client};
use serde::{Deserialize, Serialize};

#[rpc_requests(message = TestMessage)]
#[derive(Debug, Serialize, Deserialize)]
enum TestProtocol {
    #[rpc(tx = oneshot::Sender<()>)]
    Put(PutRequest),
    #[rpc(tx = oneshot::Sender<Option<String>>)]
    Get(GetRequest),
    #[rpc(nested = NestedMessage)]
    Nested(NestedProtocol),
}

#[derive(Debug, Serialize, Deserialize)]
struct PutRequest {
    key: String,
    value: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct GetRequest {
    key: String,
}

#[rpc_requests(message = NestedMessage)]
#[derive(Debug, Serialize, Deserialize)]
enum NestedProtocol {
    #[rpc(tx = oneshot::Sender<()>)]
    Put(PutRequest2),
}

#[derive(Debug, Serialize, Deserialize)]
struct PutRequest2 {
    key: String,
    value: u32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (tx, rx) = tokio::sync::mpsc::channel(10);
    tokio::task::spawn(actor(rx));
    let client: Client<TestProtocol> = Client::from(tx);
    client
        .rpc(PutRequest {
            key: "foo".to_string(),
            value: "bar".to_string(),
        })
        .await?;
    let v = client
        .rpc(GetRequest {
            key: "foo".to_string(),
        })
        .await?;
    println!("{v:?}");
    assert_eq!(v.as_deref(), Some("bar"));
    client
        .map::<NestedProtocol>()
        .rpc(PutRequest2 {
            key: "foo".to_string(),
            value: 22,
        })
        .await?;
    let v = client
        .rpc(GetRequest {
            key: "foo".to_string(),
        })
        .await?;
    println!("{v:?}");
    assert_eq!(v.as_deref(), Some("22"));
    Ok(())
}

async fn actor(mut rx: tokio::sync::mpsc::Receiver<TestMessage>) {
    let mut store = HashMap::new();
    while let Some(msg) = rx.recv().await {
        match msg {
            TestMessage::Put(msg) => {
                store.insert(msg.inner.key, msg.inner.value);
                msg.tx.send(()).await.ok();
            }
            TestMessage::Get(msg) => {
                let res = store.get(&msg.key).cloned();
                msg.tx.send(res).await.ok();
            }
            TestMessage::Nested(inner) => match inner {
                NestedMessage::Put(msg) => {
                    store.insert(msg.inner.key, msg.inner.value.to_string());
                    msg.tx.send(()).await.ok();
                }
            },
        }
    }
}
