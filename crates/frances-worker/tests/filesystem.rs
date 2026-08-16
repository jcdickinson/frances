use frances_worker_protocol::{
    Content, ErrorCode, FsWriteMode, PROTOCOL_VERSION, ProtocolReader, ProtocolWriter, Request,
    RequestKind, Response, ResponseKind, multiplex,
};
use tokio::io::AsyncReadExt;

#[tokio::test]
async fn filesystem_round_trips_through_server() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("nested").join("example.bin");
    let (client_stream, worker_stream) = tokio::io::duplex(16 * 1024);
    let worker = tokio::spawn(frances_worker::serve(worker_stream));
    let (read, write) = tokio::io::split(client_stream);
    let (mut reader, writer) = multiplex(read, write);

    send(
        &mut reader,
        &writer,
        1,
        RequestKind::FsCreateDirAll {
            path: path.parent().unwrap().to_path_buf(),
        },
    )
    .await;
    send(
        &mut reader,
        &writer,
        2,
        RequestKind::FsWrite {
            path: path.clone(),
            content: Content::from_bytes(b"hello\0worker".to_vec()),
            mode: FsWriteMode::Overwrite,
        },
    )
    .await;
    let response = send(
        &mut reader,
        &writer,
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

    send(&mut reader, &writer, 4, RequestKind::Shutdown).await;
    worker.await.unwrap().unwrap();
}

#[tokio::test]
async fn create_new_is_atomic_and_does_not_clobber() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("example.txt");
    let (client_stream, worker_stream) = tokio::io::duplex(16 * 1024);
    let worker = tokio::spawn(frances_worker::serve(worker_stream));
    let (read, write) = tokio::io::split(client_stream);
    let (mut reader, writer) = multiplex(read, write);

    send(
        &mut reader,
        &writer,
        1,
        RequestKind::FsWrite {
            path: path.clone(),
            content: Content::from_bytes(b"first".to_vec()),
            mode: FsWriteMode::CreateNew,
        },
    )
    .await;

    writer
        .send(Request {
            version: PROTOCOL_VERSION,
            id: 2,
            kind: RequestKind::FsWrite {
                path: path.clone(),
                content: Content::from_bytes(b"second".to_vec()),
                mode: FsWriteMode::CreateNew,
            },
        })
        .await
        .unwrap();
    let response: Response = reader.receive().await.unwrap().unwrap();
    let error = response.result.unwrap_err();
    assert_eq!(error.error.code, ErrorCode::AlreadyExists);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "first");

    send(&mut reader, &writer, 3, RequestKind::Shutdown).await;
    worker.await.unwrap().unwrap();
}

async fn send(
    reader: &mut ProtocolReader,
    writer: &ProtocolWriter,
    id: u64,
    kind: RequestKind,
) -> ResponseKind {
    writer
        .send(Request {
            version: PROTOCOL_VERSION,
            id,
            kind,
        })
        .await
        .unwrap();
    let response: Response = reader.receive().await.unwrap().unwrap();
    assert_eq!(response.id, id);
    response.result.unwrap()
}
