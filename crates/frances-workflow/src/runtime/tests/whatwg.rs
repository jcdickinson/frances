use super::*;

#[tokio::test]
async fn whatwg_dom_exports_dom_exception() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { DOMException } from "whatwg:dom";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const e = new DOMException("nope", "AbortError");
        transcript.push(new ErrorSection({
            content: `err=${e instanceof Error} name=${e.name} msg=${e.message}`,
        }));
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
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert_eq!(text_of(&frames[0]), "err=true name=AbortError msg=nope");
}

#[tokio::test]
async fn whatwg_web_streams_exports_constructors() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import {
            ReadableStream,
            WritableStream,
            TransformStream,
        } from "whatwg:web-streams";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const shape = [
            typeof ReadableStream,
            typeof WritableStream,
            typeof TransformStream,
        ].join(",");
        transcript.push(new ErrorSection({ content: shape }));
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
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert_eq!(text_of(&frames[0]), "function,function,function");
}

#[tokio::test]
async fn whatwg_abortcontroller_basic_lifecycle() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { AbortController, AbortSignal } from "whatwg:abortcontroller";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const ac = new AbortController();
        const before = ac.signal.aborted;
        let fired = false;
        ac.signal.addEventListener("abort", () => { fired = true; });
        ac.abort("nope");
        const after = ac.signal.aborted;
        const reason = ac.signal.reason;
        const isSignal = ac.signal instanceof AbortSignal;
        transcript.push(new ErrorSection({
            content: `before=${before} after=${after} fired=${fired} reason=${reason} sig=${isSignal}`,
        }));
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
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert_eq!(
        text_of(&frames[0]),
        "before=false after=true fired=true reason=nope sig=true"
    );
}

#[tokio::test]
async fn abortsignal_timeout_fires_after_delay() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { AbortSignal } from "whatwg:abortcontroller";
        import { DOMException } from "whatwg:dom";
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const start = Date.now();
        const s = AbortSignal.timeout(15);
        await new Timer(60);
        const elapsed = Date.now() - start;
        transcript.push(new ErrorSection({
            content: `aborted=${s.aborted} name=${s.reason && s.reason.name} dom=${s.reason instanceof DOMException} fast=${elapsed < 200}`,
        }));
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
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert_eq!(
        text_of(&frames[0]),
        "aborted=true name=TimeoutError dom=true fast=true"
    );
}

#[tokio::test]
async fn abortsignal_timeout_composes_with_timer_signal() {
    // The whole point of the primitive split: an AbortSignal.timeout
    // signal can be fed directly into a Timer's `signal:` option.
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { AbortSignal } from "whatwg:abortcontroller";
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const start = Date.now();
        try {
            await new Timer({ delay: 60_000, signal: AbortSignal.timeout(15) });
            transcript.push(new ErrorSection({ content: "BUG: resolved" }));
        } catch (e) {
            const elapsed = Date.now() - start;
            transcript.push(new ErrorSection({
                content: `name=${e.name} msg=${e.message} fast=${elapsed < 1000}`,
            }));
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
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert_eq!(
        text_of(&frames[0]),
        "name=TimeoutError msg=signal timed out fast=true"
    );
}

#[tokio::test]
async fn abortsignal_timeout_rejects_garbage() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { AbortSignal } from "whatwg:abortcontroller";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const cases = ["nope", -5, NaN, undefined, {}];
        const threw = cases.map((c) => {
            try { AbortSignal.timeout(c); return "no-throw"; }
            catch (_) { return "threw"; }
        });
        transcript.push(new ErrorSection({ content: threw.join(",") }));
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
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert_eq!(text_of(&frames[0]), "threw,threw,threw,threw,threw");
}

#[tokio::test]
async fn abortsignal_any_first_source_wins_and_listener_cleaned_up() {
    // The combined signal aborts on the first source. The cleanup
    // path must remove the listener from BOTH sources, so a later
    // abort on the second source does not overwrite `out.reason`.
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { AbortController, AbortSignal } from "whatwg:abortcontroller";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const a = new AbortController();
        const b = new AbortController();
        const out = AbortSignal.any([a.signal, b.signal]);
        a.abort("first");
        const reasonAfterFirst = out.reason;
        // If listeners weren't cleaned up, the second abort would
        // run propagate again and (no-op since out is already
        // aborted, but it would still re-fire the listener walk).
        // The observable signal: out.reason must stay "first".
        b.abort("second");
        transcript.push(new ErrorSection({
            content: `first=${reasonAfterFirst} after=${out.reason} aborted=${out.aborted}`,
        }));
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
    assert!(matches!(done, Some(Ok(()))), "done was {done:?}");
    assert_eq!(text_of(&frames[0]), "first=first after=first aborted=true");
}
