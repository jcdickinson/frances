use std::time::Duration;

use frances_worker_protocol::{
    Feed, FeedSender, PROTOCOL_VERSION, ProtocolReader, ProtocolWriter, Request, RequestKind,
    Response, ResponseKind, ShellCommand, ShellEvent, ShellEventKind, ShellOptions, ShellWait,
    multiplex,
};
use tokio::io::AsyncReadExt;

struct OpenShell {
    commands: FeedSender<ShellCommand>,
    events: Feed<ShellEvent>,
    id: u64,
}

#[tokio::test]
async fn multiple_shells_are_independent_protocol_resources() {
    let (client, worker) = tokio::io::duplex(256 * 1024);
    let worker = tokio::spawn(frances_worker::serve(worker));
    let (read, write) = tokio::io::split(client);
    let (mut reader, writer) = multiplex(read, write);

    let mut first = open_shell(&mut reader, &writer, 1).await;
    let mut second = open_shell(&mut reader, &writer, 2).await;
    let mut third = open_shell(&mut reader, &writer, 3).await;
    assert_ne!(first.id, second.id);
    assert_ne!(second.id, third.id);
    assert_ne!(first.id, third.id);

    run_with_persist(
        &first.commands,
        1,
        "export PRIVATE=one; sleep 0.1; printf first",
        vec!["PRIVATE".into()],
    )
    .await;
    run_with_persist(
        &second.commands,
        1,
        "export PRIVATE=two; sleep 0.1; printf second",
        vec!["PRIVATE".into()],
    )
    .await;
    run(&third.commands, 1, "printf third").await;

    let (first_output, second_output, third_output) = tokio::join!(
        output(&mut first.events, 1),
        output(&mut second.events, 1),
        output(&mut third.events, 1),
    );
    assert_eq!(first_output, "first");
    assert_eq!(second_output, "second");
    assert_eq!(third_output, "third");

    run(&first.commands, 2, "printf %s \"$PRIVATE\"").await;
    run(&second.commands, 2, "printf %s \"$PRIVATE\"").await;
    let (first_value, second_value) =
        tokio::join!(output(&mut first.events, 2), output(&mut second.events, 2),);
    assert_eq!(first_value, "one");
    assert_eq!(second_value, "two");

    drop(first);
    run(&second.commands, 3, "printf still-alive").await;
    assert_eq!(output(&mut second.events, 3).await, "still-alive");

    request(&mut reader, &writer, 4, RequestKind::Shutdown).await;
    worker.await.unwrap().unwrap();
}

async fn open_shell(
    reader: &mut ProtocolReader,
    writer: &ProtocolWriter,
    request_id: u64,
) -> OpenShell {
    let (commands, command_feed) = Feed::channel();
    let response = request(
        reader,
        writer,
        request_id,
        RequestKind::ShellOpen {
            options: ShellOptions::default(),
            commands: command_feed,
        },
    )
    .await;
    let ResponseKind::ShellOpened { shell, events } = response else {
        panic!("expected shell response");
    };
    OpenShell {
        commands,
        events,
        id: shell,
    }
}

async fn run(commands: &FeedSender<ShellCommand>, operation: u64, script: &str) {
    run_with_persist(commands, operation, script, Vec::new()).await;
}

async fn run_with_persist(
    commands: &FeedSender<ShellCommand>,
    operation: u64,
    script: &str,
    persist: Vec<String>,
) {
    commands
        .send(ShellCommand::Run {
            operation,
            script: script.to_owned(),
            stdin: None,
            persist,
            wait: ShellWait {
                quiet_ms: Some(Duration::from_secs(2).as_millis() as u64),
                max_ms: Some(Duration::from_secs(5).as_millis() as u64),
            },
        })
        .await
        .unwrap();
}

async fn output(events: &mut Feed<ShellEvent>, operation: u64) -> String {
    let mut output = Vec::new();
    loop {
        let event = events.next().await.unwrap().unwrap();
        assert_eq!(event.operation, operation);
        match event.kind {
            ShellEventKind::Output { content } => {
                content
                    .into_async_read()
                    .await
                    .unwrap()
                    .read_to_end(&mut output)
                    .await
                    .unwrap();
            }
            ShellEventKind::Done { exit_code } => {
                assert_eq!(exit_code, 0);
                return String::from_utf8(output).unwrap();
            }
            other => panic!("unexpected shell event: {other:?}"),
        }
    }
}

async fn request(
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
