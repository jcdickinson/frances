use frances_worker_protocol::{Content, multiplex};
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

    let (left_read, left_write) = tokio::io::split(left);
    let (right_read, right_write) = tokio::io::split(right);
    let (_left_reader, left_writer) = multiplex(left_read, left_write);
    let (mut right_reader, _right_writer) = multiplex(right_read, right_write);

    let sender = tokio::spawn(async move {
        left_writer.send(send).await.unwrap();
    });
    let received: Message = right_reader.receive().await.unwrap().unwrap();
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
