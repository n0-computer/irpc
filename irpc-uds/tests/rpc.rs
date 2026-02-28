//! Integration tests for irpc-uds: RPC over Unix domain sockets.

use std::{collections::HashMap, path::PathBuf};

use irpc::{
    channel::{mpsc, oneshot},
    rpc::RemoteService,
    rpc_requests, Client, WithChannels,
};
use serde::{Deserialize, Serialize};
use testresult::TestResult;

#[rpc_requests(message = StoreMessage)]
#[derive(Debug, Serialize, Deserialize)]
enum StoreProtocol {
    #[rpc(tx = oneshot::Sender<Option<String>>)]
    #[wrap(GetRequest, derive(Clone))]
    Get(String),

    #[rpc(tx = oneshot::Sender<Option<String>>)]
    #[wrap(SetRequest)]
    Set { key: String, value: String },

    #[rpc(tx = mpsc::Sender<(String, String)>)]
    #[wrap(ListRequest)]
    List,
}

async fn actor(mut rx: tokio::sync::mpsc::Receiver<StoreMessage>) {
    let mut store = HashMap::new();
    while let Some(msg) = rx.recv().await {
        match msg {
            StoreMessage::Get(msg) => {
                let WithChannels { inner, tx, .. } = msg;
                let GetRequest(key) = inner;
                let value = store.get(&key).cloned();
                tx.send(value).await.ok();
            }
            StoreMessage::Set(msg) => {
                let WithChannels { inner, tx, .. } = msg;
                let SetRequest { key, value } = inner;
                let prev_value = store.insert(key, value);
                tx.send(prev_value).await.ok();
            }
            StoreMessage::List(msg) => {
                let WithChannels { tx, .. } = msg;
                for (k, v) in &store {
                    if tx.send((k.clone(), v.clone())).await.is_err() {
                        break;
                    }
                }
            }
        }
    }
}

fn sock_path(name: &str) -> PathBuf {
    let dir = std::env::temp_dir();
    dir.join(format!("irpc-uds-test-{name}-{}.sock", std::process::id()))
}

async fn setup(name: &str) -> TestResult<(Client<StoreProtocol>, PathBuf)> {
    let path = sock_path(name);
    let _ = std::fs::remove_file(&path);

    let (tx, rx) = tokio::sync::mpsc::channel(16);
    tokio::task::spawn(actor(rx));
    let local_client = Client::<StoreProtocol>::local(tx);

    let endpoint = irpc_uds::server_endpoint(&path)?;
    let handler = StoreProtocol::remote_handler(local_client.as_local().unwrap());
    tokio::task::spawn(async move {
        irpc::rpc::listen(endpoint, handler).await;
    });

    // Small delay to let server start listening
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let client: Client<StoreProtocol> = irpc_uds::client(&path).await?;
    Ok((client, path))
}

#[tokio::test]
async fn rpc_get_set_roundtrip() -> TestResult<()> {
    let (client, path) = setup("get-set").await?;

    // Get non-existent key
    let value = client.rpc(GetRequest("hello".into())).await?;
    assert_eq!(value, None);

    // Set a value
    let prev = client
        .rpc(SetRequest {
            key: "hello".into(),
            value: "world".into(),
        })
        .await?;
    assert_eq!(prev, None);

    // Get the value back
    let value = client.rpc(GetRequest("hello".into())).await?;
    assert_eq!(value, Some("world".into()));

    // Overwrite
    let prev = client
        .rpc(SetRequest {
            key: "hello".into(),
            value: "updated".into(),
        })
        .await?;
    assert_eq!(prev, Some("world".into()));

    let _ = std::fs::remove_file(&path);
    Ok(())
}

#[tokio::test]
async fn rpc_streaming() -> TestResult<()> {
    let (client, path) = setup("streaming").await?;

    // Insert some values
    client
        .rpc(SetRequest {
            key: "a".into(),
            value: "1".into(),
        })
        .await?;
    client
        .rpc(SetRequest {
            key: "b".into(),
            value: "2".into(),
        })
        .await?;

    // List all entries via streaming
    let mut rx = client.server_streaming(ListRequest, 16).await?;
    let mut entries = Vec::new();
    while let Some(entry) = rx.recv().await? {
        entries.push(entry);
    }
    entries.sort();
    assert_eq!(
        entries,
        vec![
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
        ]
    );

    let _ = std::fs::remove_file(&path);
    Ok(())
}

#[tokio::test]
async fn rpc_concurrent_clients() -> TestResult<()> {
    let path = sock_path("concurrent");
    let _ = std::fs::remove_file(&path);

    let (tx, rx) = tokio::sync::mpsc::channel(64);
    tokio::task::spawn(actor(rx));
    let local_client = Client::<StoreProtocol>::local(tx);

    let endpoint = irpc_uds::server_endpoint(&path)?;
    let handler = StoreProtocol::remote_handler(local_client.as_local().unwrap());
    tokio::task::spawn(async move {
        irpc::rpc::listen(endpoint, handler).await;
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Spawn multiple concurrent clients, staggered slightly to avoid overwhelming
    let mut handles = Vec::new();
    for i in 0..5 {
        let path = path.clone();
        handles.push(tokio::spawn(async move {
            let client: Client<StoreProtocol> = irpc_uds::client(&path).await.unwrap();
            let key = format!("key-{i}");
            let value = format!("value-{i}");
            client
                .rpc(SetRequest {
                    key: key.clone(),
                    value: value.clone(),
                })
                .await
                .unwrap();
            let got = client.rpc(GetRequest(key)).await.unwrap();
            assert_eq!(got, Some(value));
        }));
    }

    for h in handles {
        h.await?;
    }

    let _ = std::fs::remove_file(&path);
    Ok(())
}
