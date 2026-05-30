use super::*;

#[tokio::test]
async fn iterator_delivers_messages_in_order() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { inbox } from "frances:v1/inbox";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        import { exit } from "frances:v1/workflow";
        for await (const input of inbox) {
            transcript.push(new MarkdownSection({ content: "got:" + input.content }));
            if (input.content === "stop") { exit(); break; }
        }
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();

    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(frames.is_empty(), "got {frames:?}");
    assert!(done.is_none());

    handle
        .input_tx
        .send(InboxItem::Input(UserInput {
            content: "a".into(),
        }))
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert_eq!(text_of(&frames[0]), "got:a");
    assert!(done.is_none());

    handle
        .input_tx
        .send(InboxItem::Input(UserInput {
            content: "b".into(),
        }))
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert_eq!(text_of(&frames[0]), "got:b");
    assert!(done.is_none());

    handle
        .input_tx
        .send(InboxItem::Input(UserInput {
            content: "stop".into(),
        }))
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert_eq!(text_of(&frames[0]), "got:stop");
    assert!(matches!(done, Some(Ok(()))));
}

#[tokio::test]
async fn exit_unblocks_pending_next() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { inbox } from "frances:v1/inbox";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        import { exit } from "frances:v1/workflow";
        queueMicrotask(() => exit());
        for await (const _ of inbox) {
            transcript.push(new MarkdownSection({ content: "got input" }));
        }
        transcript.push(new MarkdownSection({ content: "after-loop" }));
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, result) = drive_to_done(&mut handle).await;
    assert!(matches!(result, Ok(())), "result was {result:?}");
    assert_eq!(text_of(&frames[0]), "after-loop");
}

#[tokio::test]
async fn symbol_async_iterator_returns_self() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { inbox } from "frances:v1/inbox";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        import { exit } from "frances:v1/workflow";
        const it = inbox[Symbol.asyncIterator]();
        transcript.push(new MarkdownSection({ content: it === inbox ? "same" : "different" }));
        exit();
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))));
    assert_eq!(text_of(&frames[0]), "same");
}

#[tokio::test]
async fn concurrent_next_fifo() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { inbox } from "frances:v1/inbox";
        import { transcript, MarkdownSection } from "frances:v1/sections";
        import { exit } from "frances:v1/workflow";
        const a = inbox.next();
        const b = inbox.next();
        const [ra, rb] = await Promise.all([a, b]);
        transcript.push(new MarkdownSection({ content: `${ra.value.content},${rb.value.content}` }));
        exit();
        "#,
    );
    let mut handle = rt
        .start(Invocation {
            source_path: file.path().to_path_buf(),
            args: Vec::new(),
            ..Default::default()
        })
        .await
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(frames.is_empty());
    assert!(done.is_none());

    handle
        .input_tx
        .send(InboxItem::Input(UserInput {
            content: "first".into(),
        }))
        .unwrap();
    handle
        .input_tx
        .send(InboxItem::Input(UserInput {
            content: "second".into(),
        }))
        .unwrap();
    let (frames, done) = drive_one_cycle(&mut handle).await;
    assert!(matches!(done, Some(Ok(()))));
    assert_eq!(text_of(&frames[0]), "first,second");
}
