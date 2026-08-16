use std::time::Duration;

use frances_worker_protocol::{
    Feed, PROTOCOL_VERSION, ProtocolReader, ProtocolWriter, Request, RequestKind, Response,
    ResponseKind, ShellId, ShellOptions, ShellOutput, ShellWaitQuiet, multiplex,
};
use tokio::io::AsyncReadExt;

struct OpenShell {
    output: Feed<ShellOutput>,
    id: ShellId,
}

#[tokio::test]
async fn multiple_shells_are_independent_protocol_resources() {
    let (client, worker) = tokio::io::duplex(256 * 1024);
    let worker = tokio::spawn(frances_worker::serve(worker));
    let (read, write) = tokio::io::split(client);
    let (mut reader, writer) = multiplex(read, write);
    let mut request_id = 1;

    let mut first = open_shell(&mut reader, &writer, &mut request_id).await;
    let mut second = open_shell(&mut reader, &writer, &mut request_id).await;
    let mut third = open_shell(&mut reader, &writer, &mut request_id).await;
    assert_ne!(first.id, second.id);
    assert_ne!(second.id, third.id);
    assert_ne!(first.id, third.id);

    run_with_persist(
        &mut reader,
        &writer,
        &mut request_id,
        first.id,
        "export PRIVATE=one; sleep 0.1; printf first",
        vec!["PRIVATE".into()],
    )
    .await;
    run_with_persist(
        &mut reader,
        &writer,
        &mut request_id,
        second.id,
        "export PRIVATE=two; sleep 0.1; printf second",
        vec!["PRIVATE".into()],
    )
    .await;
    run(
        &mut reader,
        &writer,
        &mut request_id,
        third.id,
        "printf third",
    )
    .await;

    assert_eq!(
        finished_output(&mut reader, &writer, &mut request_id, &mut first).await,
        "first"
    );
    assert_eq!(
        finished_output(&mut reader, &writer, &mut request_id, &mut second).await,
        "second"
    );
    assert_eq!(
        finished_output(&mut reader, &writer, &mut request_id, &mut third).await,
        "third"
    );

    run(
        &mut reader,
        &writer,
        &mut request_id,
        first.id,
        "printf %s \"$PRIVATE\"",
    )
    .await;
    run(
        &mut reader,
        &writer,
        &mut request_id,
        second.id,
        "printf %s \"$PRIVATE\"",
    )
    .await;
    assert_eq!(
        finished_output(&mut reader, &writer, &mut request_id, &mut first).await,
        "one"
    );
    assert_eq!(
        finished_output(&mut reader, &writer, &mut request_id, &mut second).await,
        "two"
    );

    close_shell(&mut reader, &writer, &mut request_id, first.id).await;
    run(
        &mut reader,
        &writer,
        &mut request_id,
        second.id,
        "printf still-alive",
    )
    .await;
    assert_eq!(
        finished_output(&mut reader, &writer, &mut request_id, &mut second).await,
        "still-alive"
    );

    request(&mut reader, &writer, &mut request_id, RequestKind::Shutdown).await;
    worker.await.unwrap().unwrap();
}

async fn open_shell(
    reader: &mut ProtocolReader,
    writer: &ProtocolWriter,
    request_id: &mut u64,
) -> OpenShell {
    let response = request(
        reader,
        writer,
        request_id,
        RequestKind::ShellOpen {
            options: ShellOptions::default(),
        },
    )
    .await;
    let ResponseKind::ShellOpened { shell, output } = response else {
        panic!("expected shell response");
    };
    OpenShell { output, id: shell }
}

async fn run(
    reader: &mut ProtocolReader,
    writer: &ProtocolWriter,
    request_id: &mut u64,
    shell: ShellId,
    script: &str,
) {
    run_with_persist(reader, writer, request_id, shell, script, Vec::new()).await;
}

async fn run_with_persist(
    reader: &mut ProtocolReader,
    writer: &ProtocolWriter,
    request_id: &mut u64,
    shell: ShellId,
    script: &str,
    persist: Vec<String>,
) {
    let response = request(
        reader,
        writer,
        request_id,
        RequestKind::ShellRun {
            shell,
            script: script.to_owned(),
            stdin: None,
            persist,
        },
    )
    .await;
    assert!(matches!(response, ResponseKind::Unit));
}

async fn finished_output(
    reader: &mut ProtocolReader,
    writer: &ProtocolWriter,
    request_id: &mut u64,
    shell: &mut OpenShell,
) -> String {
    tokio::time::timeout(Duration::from_secs(5), async {
        let response = request(
            reader,
            writer,
            request_id,
            RequestKind::ShellWaitQuiet {
                shell: shell.id,
                quiet_ms: Duration::from_secs(2).as_millis() as u64,
            },
        )
        .await;
        assert!(matches!(
            response,
            ResponseKind::ShellWaitQuiet(ShellWaitQuiet::Exit)
        ));

        let mut output = Vec::new();
        loop {
            match shell.output.next().await.unwrap().unwrap() {
                ShellOutput::Output { content } => {
                    content
                        .into_async_read()
                        .await
                        .unwrap()
                        .read_to_end(&mut output)
                        .await
                        .unwrap();
                }
                ShellOutput::Exit { exit_code } => {
                    assert_eq!(exit_code, 0);
                    return String::from_utf8(output).unwrap();
                }
            }
        }
    })
    .await
    .expect("shell did not finish")
}

async fn close_shell(
    reader: &mut ProtocolReader,
    writer: &ProtocolWriter,
    request_id: &mut u64,
    shell: ShellId,
) {
    let response = request(
        reader,
        writer,
        request_id,
        RequestKind::ShellClose { shell },
    )
    .await;
    assert!(matches!(response, ResponseKind::Unit));
}

async fn request(
    reader: &mut ProtocolReader,
    writer: &ProtocolWriter,
    request_id: &mut u64,
    kind: RequestKind,
) -> ResponseKind {
    let id = *request_id;
    *request_id += 1;
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
