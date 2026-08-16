use frances_worker_protocol::{Connection, Content};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

#[derive(Serialize, Deserialize)]
struct Message {
    name: String,
    content: Content,
}

#[tokio::test]
async fn content_round_trips_outside_json() {
    let (left, right) = tokio::io::duplex(4096);
    let send = Message {
        name: "example".into(),
        content: Content::from_bytes(b"hello\0world".to_vec()),
    };

    let sender = tokio::spawn(async move {
        Connection::new(left).send(&send).await.unwrap();
    });
    let received: Message = Connection::new(right).receive().await.unwrap().unwrap();
    sender.await.unwrap();

    assert_eq!(received.name, "example");
    let mut bytes = Vec::new();
    received
        .content
        .into_async_read()
        .await
        .unwrap()
        .read_to_end(&mut bytes)
        .await
        .unwrap();
    assert_eq!(bytes, b"hello\0world");
}

#[test]
fn content_cannot_be_serialized_as_plain_json() {
    let value = Message {
        name: "example".into(),
        content: Content::from_bytes(Vec::new()),
    };

    assert!(serde_json::to_string(&value).is_err());
}
