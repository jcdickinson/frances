use frances_worker_protocol::{
    Connection, Content, PROTOCOL_VERSION, Request, RequestKind, Response, ResponseKind,
};
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn filesystem_round_trips_through_server() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("nested").join("example.bin");
    let (client_stream, worker_stream) = tokio::io::duplex(16 * 1024);
    let worker = tokio::spawn(frances_worker::serve(worker_stream));
    let mut connection = Connection::new(client_stream);

    send(
        &mut connection,
        1,
        RequestKind::FsCreateDirAll {
            path: path.parent().unwrap().to_path_buf(),
        },
    )
    .await;
    send(
        &mut connection,
        2,
        RequestKind::FsWrite {
            path: path.clone(),
            content: Content::from_bytes(b"hello\0worker".to_vec()),
        },
    )
    .await;
    let response = send(
        &mut connection,
        3,
        RequestKind::FsRead { path: path.clone() },
    )
    .await;

    let ResponseKind::Content(content) = response else {
        panic!("expected content response");
    };
    let mut bytes = Vec::new();
    content
        .into_async_read()
        .await
        .unwrap()
        .read_to_end(&mut bytes)
        .await
        .unwrap();
    assert_eq!(bytes, b"hello\0worker");

    send(&mut connection, 4, RequestKind::Shutdown).await;
    worker.await.unwrap().unwrap();
}

async fn send<S>(connection: &mut Connection<S>, id: u64, kind: RequestKind) -> ResponseKind
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    connection
        .send(&Request {
            version: PROTOCOL_VERSION,
            id,
            kind,
        })
        .await
        .unwrap();
    let response: Response = connection.receive().await.unwrap().unwrap();
    assert_eq!(response.id, id);
    response.result.unwrap()
}
