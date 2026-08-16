use frances_worker_protocol::{Content, Feed, multiplex};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;

#[derive(Serialize, Deserialize)]
struct Open {
    input: Feed<Packet>,
}

#[derive(Serialize, Deserialize)]
struct Opened {
    output: Feed<Packet>,
}

#[derive(Serialize, Deserialize)]
struct Packet {
    sequence: u64,
    content: Content,
}

#[tokio::test]
async fn feeds_transfer_in_both_directions_with_nested_content() {
    let (client, worker) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client);
    let (worker_read, worker_write) = tokio::io::split(worker);
    let (mut client_reader, client_writer) = multiplex(client_read, client_write);
    let (mut worker_reader, worker_writer) = multiplex(worker_read, worker_write);

    let worker = tokio::spawn(async move {
        let mut request: Open = worker_reader.receive().await.unwrap().unwrap();
        let (output, output_feed) = Feed::channel();
        worker_writer
            .send(Opened {
                output: output_feed,
            })
            .await
            .unwrap();

        let packet = request.input.next().await.unwrap().unwrap();
        let bytes = read(packet.content).await;
        output
            .send(Packet {
                sequence: packet.sequence + 1,
                content: Content::from_bytes(bytes.into_iter().rev().collect()),
            })
            .await
            .unwrap();
    });

    let (input, input_feed) = Feed::channel();
    client_writer
        .send(Open { input: input_feed })
        .await
        .unwrap();
    let mut response: Opened = client_reader.receive().await.unwrap().unwrap();
    input
        .send(Packet {
            sequence: 41,
            content: Content::from_bytes(b"abc\0def".to_vec()),
        })
        .await
        .unwrap();

    let packet = response.output.next().await.unwrap().unwrap();
    assert_eq!(packet.sequence, 42);
    assert_eq!(read(packet.content).await, b"fed\0cba");
    worker.await.unwrap();
}

#[tokio::test]
async fn dropping_receiver_cancels_only_that_feed() {
    let (client, worker) = tokio::io::duplex(64 * 1024);
    let (client_read, client_write) = tokio::io::split(client);
    let (worker_read, worker_write) = tokio::io::split(worker);
    let (mut client_reader, client_writer) = multiplex(client_read, client_write);
    let (mut worker_reader, worker_writer) = multiplex(worker_read, worker_write);
    let (cancelled, saw_cancel) = tokio::sync::oneshot::channel();

    tokio::spawn(async move {
        let _request: Open = worker_reader.receive().await.unwrap().unwrap();
        let (output, output_feed) = Feed::channel();
        worker_writer
            .send(Opened {
                output: output_feed,
            })
            .await
            .unwrap();
        for sequence in 0..100 {
            if output
                .send(Packet {
                    sequence,
                    content: Content::from_bytes(Vec::new()),
                })
                .await
                .is_err()
            {
                let _ = cancelled.send(());
                return;
            }
        }
    });

    let (_input, input_feed) = Feed::channel();
    client_writer
        .send(Open { input: input_feed })
        .await
        .unwrap();
    let response: Opened = client_reader.receive().await.unwrap().unwrap();
    drop(response.output);

    tokio::time::timeout(std::time::Duration::from_secs(2), saw_cancel)
        .await
        .expect("feed cancellation timed out")
        .unwrap();
}

async fn read(content: Content) -> Vec<u8> {
    let mut bytes = Vec::new();
    content
        .into_async_read()
        .await
        .unwrap()
        .read_to_end(&mut bytes)
        .await
        .unwrap();
    bytes
}
