use super::*;

#[tokio::test]
async fn timer_fires_after_interval() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const start = Date.now();
        await new Timer(20);
        const elapsed = Date.now() - start;
        transcript.push(new ErrorSection({ content: elapsed >= 15 ? "ok" : `too fast: ${elapsed}` }));
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
    assert_eq!(text_of(&frames[0]), "ok");
}

#[tokio::test]
async fn timer_fire_resolves_pending_await() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const t = new Timer(60_000);  // long enough that the test would hang if fire() didn't work
        queueMicrotask(() => t.fire());
        const start = Date.now();
        await t;
        const elapsed = Date.now() - start;
        transcript.push(new ErrorSection({ content: elapsed < 1000 ? "fast" : `slow: ${elapsed}` }));
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
    assert_eq!(text_of(&frames[0]), "fast");
}

#[tokio::test]
async fn timer_disable_then_fire_wakes_await() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const t = new Timer(60_000);
        queueMicrotask(() => {
            t.disable();
            t.fire();
        });
        const start = Date.now();
        await t;
        const elapsed = Date.now() - start;
        transcript.push(new ErrorSection({
            content: elapsed < 1000 ? "fast" : `slow: ${elapsed}`,
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
    assert_eq!(text_of(&frames[0]), "fast");
}

#[tokio::test]
async fn timer_reject_preserves_error_identity() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const t = new Timer(60_000);
        const original = new Error("nope");
        queueMicrotask(() => t.reject(original));
        try {
            await t;
            transcript.push(new ErrorSection({ content: "BUG: resolved" }));
        } catch (e) {
            transcript.push(new ErrorSection({
                content: `same=${e === original} msg=${e.message}`,
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
    assert_eq!(text_of(&frames[0]), "same=true msg=nope");
}

#[tokio::test]
async fn timer_rejected_is_terminal() {
    // After reject(), every mutating method throws.
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer, TimerError } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const t = new Timer(60_000);
        t.reject(new Error("done"));
        const results = [];
        for (const op of [
            ["reject", () => t.reject(new Error("again"))],
            ["disable", () => t.disable()],
            ["enable", () => t.enable()],
            ["fire", () => t.fire()],
            ["set", () => t.set({ delay: 1 })],
        ]) {
            try { op[1](); results.push(`${op[0]}: no-throw`); }
            catch (e) { results.push(`${op[0]}: threw`); }
        }
        transcript.push(new ErrorSection({ content: results.join("; ") }));
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
        "reject: threw; disable: threw; enable: threw; fire: threw; set: threw"
    );
}

#[tokio::test]
async fn timer_reject_with_timer_error_is_instance() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer, TimerError } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const t = new Timer(60_000);
        queueMicrotask(() => t.reject(new TimerError("boom")));
        try {
            await t;
            transcript.push(new ErrorSection({ content: "BUG: resolved" }));
        } catch (e) {
            transcript.push(new ErrorSection({
                content: `te=${e instanceof TimerError} err=${e instanceof Error} msg=${e.message}`,
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
    assert_eq!(text_of(&frames[0]), "te=true err=true msg=boom");
}

#[tokio::test]
async fn timer_reject_with_no_arg_rejects_with_default_timer_error() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const t = new Timer(60_000);
        queueMicrotask(() => t.reject());
        try {
            await t;
            transcript.push(new ErrorSection({ content: "BUG: resolved" }));
        } catch (e) {
            transcript.push(new ErrorSection({
                content: `caught: error=${e instanceof Error} name=${e.name} msg=${e.message}`,
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
        "caught: error=true name=TimerError msg=timer rejected"
    );
}

#[tokio::test]
async fn timer_disable_then_enable_revives() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const t = new Timer({ delay: 10 });
        t.disable();
        t.enable();
        await t;
        transcript.push(new ErrorSection({ content: t.enabled ? "enabled" : "still-off" }));
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
    assert_eq!(text_of(&frames[0]), "enabled");
}

#[tokio::test]
async fn timer_getters_reflect_schedule_and_state() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const t = new Timer({ delay: 100, interval: 50 });
        const before = `enabled=${t.enabled} delay=${t.delay} interval=${t.interval}`;
        t.disable();
        const after = `enabled=${t.enabled} delay=${t.delay} interval=${t.interval}`;
        transcript.push(new ErrorSection({ content: `${before} | ${after}` }));
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
        "enabled=true delay=100 interval=50 | enabled=false delay=100 interval=50"
    );
}

#[tokio::test]
async fn timer_repeat_ticks_multiple_times() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const tick = new Timer({ interval: 5 });
        let count = 0;
        for (let i = 0; i < 3; i += 1) { await tick; count += 1; }
        tick.disable();
        transcript.push(new ErrorSection({ content: `count=${count}` }));
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
    assert_eq!(text_of(&frames[0]), "count=3");
}

#[tokio::test]
async fn timer_non_repeat_second_await_resolves_immediately() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const t = new Timer(10);
        await t;
        const start = Date.now();
        await t;  // already fired — no wait
        const elapsed = Date.now() - start;
        transcript.push(new ErrorSection({ content: elapsed < 5 ? "instant" : `slow: ${elapsed}` }));
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
    assert_eq!(text_of(&frames[0]), "instant");
}

#[tokio::test]
async fn timer_constructor_rejects_garbage() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        new Timer("nope");
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
    let (_frames, result) = drive_one_cycle(&mut handle).await;
    let result = result.expect("workflow should have terminated");
    assert!(
        matches!(result, Err(WorkflowError::ScriptCaught { .. })),
        "got {result:?}"
    );
}

#[tokio::test]
async fn timer_object_delay_form() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        await new Timer({ delay: 5 });
        transcript.push(new ErrorSection({ content: "fired" }));
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
    assert_eq!(text_of(&frames[0]), "fired");
}

#[tokio::test]
async fn timer_delay_then_interval_combo() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const tick = new Timer({ delay: 30, interval: 5 });
        const t0 = Date.now();
        await tick;
        const first = Date.now() - t0;
        await tick;
        const second = Date.now() - t0;
        await tick;
        const third = Date.now() - t0;
        tick.disable();
        const ok = first >= 25 && (second - first) < 25 && (third - second) < 25;
        transcript.push(new ErrorSection({ content: ok ? "ok" : `bad: ${first} ${second} ${third}` }));
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
    assert_eq!(text_of(&frames[0]), "ok");
}

#[tokio::test]
async fn timer_object_needs_delay_or_interval() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        new Timer({});
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
    let (_frames, result) = drive_one_cycle(&mut handle).await;
    let result = result.expect("workflow should have terminated");
    assert!(
        matches!(result, Err(WorkflowError::ScriptCaught { .. })),
        "got {result:?}"
    );
}

#[tokio::test]
async fn timer_set_after_cancel_reuses_timer() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const t = new Timer(60_000);
        t.disable();
        // Cancelled — without set(), the next await would reject.
        t.set({ delay: 10 });
        await t;
        transcript.push(new ErrorSection({ content: "ok" }));
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
    assert_eq!(text_of(&frames[0]), "ok");
}

#[tokio::test]
async fn timer_set_changes_schedule_and_resets_fired_once() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const t = new Timer({ delay: 5 });
        await t;             // fires, fired_once = true
        t.set({ interval: 15 });
        const t0 = Date.now();
        await t;
        await t;
        const elapsed = Date.now() - t0;
        transcript.push(new ErrorSection({ content: elapsed >= 25 ? "ok" : `too fast: ${elapsed}` }));
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
    assert_eq!(text_of(&frames[0]), "ok");
}

#[tokio::test]
async fn timer_set_rejects_empty_args() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        const t = new Timer(10);
        t.set({});
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
    let (_frames, result) = drive_one_cycle(&mut handle).await;
    let result = result.expect("workflow should have terminated");
    assert!(
        matches!(result, Err(WorkflowError::ScriptCaught { .. })),
        "got {result:?}"
    );
}

#[tokio::test]
async fn timer_exit_unblocks_pending_await() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        import { exit } from "frances:v1/workflow";
        const t = new Timer(60_000);
        queueMicrotask(() => exit());
        await t;  // should resolve when the workflow closes, not reject
        transcript.push(new ErrorSection({ content: "after-await" }));
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
    assert_eq!(text_of(&frames[0]), "after-await");
}

#[tokio::test]
async fn timer_reject_with_object_preserves_identity() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const t = new Timer(60_000);
        const payload = { kind: "custom", n: 42 };
        queueMicrotask(() => t.reject(payload));
        try {
            await t;
            transcript.push(new ErrorSection({ content: "BUG: resolved" }));
        } catch (e) {
            transcript.push(new ErrorSection({
                content: `same=${e === payload} kind=${e.kind} n=${e.n}`,
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
    assert_eq!(text_of(&frames[0]), "same=true kind=custom n=42");
}

#[tokio::test]
async fn timer_signal_already_aborted_rejects_immediately() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { AbortController } from "whatwg:abortcontroller";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const ac = new AbortController();
        ac.abort("pre-aborted");
        const t = new Timer({ delay: 60_000, signal: ac.signal });
        const start = Date.now();
        try {
            await t;
            transcript.push(new ErrorSection({ content: "BUG: resolved" }));
        } catch (e) {
            const elapsed = Date.now() - start;
            transcript.push(new ErrorSection({
                content: `caught=${e} fast=${elapsed < 100}`,
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
    assert_eq!(text_of(&frames[0]), "caught=pre-aborted fast=true");
}

#[tokio::test]
async fn timer_signal_aborts_mid_wait() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { AbortController } from "whatwg:abortcontroller";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const ac = new AbortController();
        const t = new Timer({ delay: 60_000, signal: ac.signal });
        queueMicrotask(() => ac.abort(new Error("user cancelled")));
        const start = Date.now();
        try {
            await t;
            transcript.push(new ErrorSection({ content: "BUG: resolved" }));
        } catch (e) {
            const elapsed = Date.now() - start;
            transcript.push(new ErrorSection({
                content: `err=${e instanceof Error} msg=${e.message} fast=${elapsed < 1000}`,
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
    assert_eq!(text_of(&frames[0]), "err=true msg=user cancelled fast=true");
}

#[tokio::test]
async fn timer_signal_reason_preserved_verbatim() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        import { AbortController } from "whatwg:abortcontroller";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const ac = new AbortController();
        const reason = { kind: "signal-reason", id: 7 };
        const t = new Timer({ delay: 60_000, signal: ac.signal });
        queueMicrotask(() => ac.abort(reason));
        try {
            await t;
            transcript.push(new ErrorSection({ content: "BUG: resolved" }));
        } catch (e) {
            transcript.push(new ErrorSection({
                content: `same=${e === reason} kind=${e.kind} id=${e.id}`,
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
    assert_eq!(text_of(&frames[0]), "same=true kind=signal-reason id=7");
}

#[tokio::test]
async fn timer_signal_listener_removed_on_terminal() {
    // After the timer settles via reject(), an abort on the
    // original signal must not double-fire on the timer — the
    // listener should have been removed at terminal transition.
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer, TimerError } from "frances:v1/io";
        import { AbortController } from "whatwg:abortcontroller";
        import { transcript, ErrorSection } from "frances:v1/sections";
        const ac = new AbortController();
        const t = new Timer({ delay: 60_000, signal: ac.signal });
        t.reject(new Error("manual"));
        // After reject, the timer is terminal. Aborting the signal
        // should not throw / not mutate anything observable.
        ac.abort("late");
        try {
            await t;
            transcript.push(new ErrorSection({ content: "BUG: resolved" }));
        } catch (e) {
            // We rejected with our own Error before abort fired —
            // the late abort must not have replaced the reason.
            transcript.push(new ErrorSection({
                content: `msg=${e.message} aborted=${ac.signal.aborted}`,
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
    assert_eq!(text_of(&frames[0]), "msg=manual aborted=true");
}

#[tokio::test]
async fn timer_non_signal_object_rejected_by_constructor() {
    let rt = Runtime::new(StubDeps::default()).unwrap();
    let file = write_source(
        "js",
        r#"
        import { Timer } from "frances:v1/io";
        new Timer({ delay: 10, signal: { aborted: false } });  // missing addEventListener
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
    let (_frames, result) = drive_one_cycle(&mut handle).await;
    let result = result.expect("workflow should have terminated");
    assert!(
        matches!(result, Err(WorkflowError::ScriptCaught { .. })),
        "got {result:?}"
    );
}

// ---- whatwg:* smoke tests --------------------------------------------
//
// These verify the module-library wiring (the import resolves, the
// named exports are present), not the polyfill internals. The
// polyfill upstreams have their own test suites; we just care that
// our virtual-module declaration didn't fumble.
