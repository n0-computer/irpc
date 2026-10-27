use irpc::channel::mpsc;

#[tokio::test]
async fn mpsc_tokio_round_trip() {
    #[derive(Debug, PartialEq)]
    struct Message(u8);

    let (tx, rx) = tokio::sync::mpsc::channel(1);
    let tx = mpsc::Sender::from(tx);
    let rx = mpsc::Receiver::from(rx);
    assert!(!tx.is_rpc());
    // Cloning a sender must not require Clone on its message type.
    let tx = tokio::sync::mpsc::Sender::try_from(tx.clone()).unwrap();
    let mut rx = tokio::sync::mpsc::Receiver::try_from(rx).unwrap();
    tx.send(Message(7)).await.unwrap();
    assert_eq!(rx.recv().await, Some(Message(7)));
}

#[tokio::test]
async fn mpsc_failed_conversion_preserves_mapped_channels() {
    let (tx, rx) = mpsc::channel::<u8>(1);
    let tx = tx.with_map(|value: u8| value + 1);
    let rx = rx.map(|value| value * 2);
    let tx = tokio::sync::mpsc::Sender::try_from(tx).unwrap_err();
    let mut rx = tokio::sync::mpsc::Receiver::try_from(rx).unwrap_err();
    assert!(!tx.is_rpc());
    tx.clone().send(3).await.unwrap();
    assert_eq!(rx.recv().await.unwrap(), Some(8));
    drop(rx);
    tx.closed().await;
}
