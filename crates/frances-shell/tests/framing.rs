use frances_shell::{RunOutcome, Shell, ShellOptions, WaitOpts};

#[tokio::test]
async fn unbalanced_quote_does_not_break_shell() {
    let mut shell = Shell::spawn(ShellOptions::default()).await.unwrap();
    let res = shell
        .run("echo \"unclosed", WaitOpts::default())
        .await
        .unwrap();
    match res {
        RunOutcome::Done { exit_code, output } => {
            assert_ne!(exit_code, 0, "unbalanced quote should fail");
            assert!(
                output.to_lowercase().contains("unexpected")
                    || output.to_lowercase().contains("eof")
                    || output.to_lowercase().contains("syntax"),
                "expected a parser error in output, got {output:?}",
            );
        }
        other => panic!("unexpected: {other:?}"),
    }
    let next = shell.run("echo alive", WaitOpts::default()).await.unwrap();
    assert!(matches!(next, RunOutcome::Done { exit_code: 0, .. }));
}

#[tokio::test]
async fn nonce_tamper_resistance() {
    // The user tries to set and freeze a fake __F_NONCE. Because the real
    // sentinel emitter has the literal nonce templated in by Rust, this
    // can't break framing.
    let mut shell = Shell::spawn(ShellOptions::default()).await.unwrap();
    let res = shell
        .run(
            "__F_NONCE=evil; readonly __F_NONCE; echo hi",
            WaitOpts::default(),
        )
        .await
        .unwrap();
    match res {
        RunOutcome::Done { exit_code, output } => {
            assert_eq!(exit_code, 0);
            assert!(output.contains("hi"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn dead_run_does_not_break_shell() {
    // Self-SIGKILL takes the wrapper bash down before the sentinel can
    // print, so the run ends Dead — but the shell stays usable and the
    // next run spawns a fresh bash against the same stored state.
    let mut shell = Shell::spawn(ShellOptions::default()).await.unwrap();
    let res = shell.run("kill -9 $$", WaitOpts::default()).await.unwrap();
    assert!(matches!(res, RunOutcome::Dead { .. }));
    let again = shell
        .run("echo recovered", WaitOpts::default())
        .await
        .unwrap();
    match again {
        RunOutcome::Done { exit_code, output } => {
            assert_eq!(exit_code, 0);
            assert!(output.contains("recovered"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn exit_frames_done_with_status() {
    // `exit N` exits the wrapper bash mid-source, but the EXIT trap still
    // runs teardown: the status is framed as Done and cwd persists.
    let mut shell = Shell::spawn(ShellOptions::default()).await.unwrap();
    let res = shell
        .run("echo before; cd /; exit 42", WaitOpts::default())
        .await
        .unwrap();
    match res {
        RunOutcome::Done { exit_code, output } => {
            assert_eq!(exit_code, 42);
            assert!(output.contains("before"));
        }
        other => panic!("unexpected: {other:?}"),
    }
    let after = shell.run("pwd", WaitOpts::default()).await.unwrap();
    match after {
        RunOutcome::Done { exit_code, output } => {
            assert_eq!(exit_code, 0);
            assert_eq!(output.trim(), "/", "cd before exit should persist");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn set_e_failure_frames_done() {
    // A user-script `set -e` death exits the wrapper bash, but the EXIT
    // trap frames the failing status instead of reporting Dead.
    let mut shell = Shell::spawn(ShellOptions::default()).await.unwrap();
    let res = shell
        .run("set -e; false; echo unreachable", WaitOpts::default())
        .await
        .unwrap();
    match res {
        RunOutcome::Done { exit_code, output } => {
            assert_eq!(exit_code, 1);
            assert!(!output.contains("unreachable"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn user_exit_trap_does_not_break_framing() {
    // A user script that installs its own EXIT trap replaces the
    // wrapper's. The wrapper's explicit teardown call still frames the
    // normal-completion path.
    let mut shell = Shell::spawn(ShellOptions::default()).await.unwrap();
    let res = shell
        .run("trap 'echo cleanup' EXIT; echo work", WaitOpts::default())
        .await
        .unwrap();
    match res {
        RunOutcome::Done { exit_code, output } => {
            assert_eq!(exit_code, 0);
            assert!(output.contains("work"));
        }
        other => panic!("unexpected: {other:?}"),
    }
    let next = shell.run("echo next", WaitOpts::default()).await.unwrap();
    assert!(matches!(next, RunOutcome::Done { exit_code: 0, .. }));
}

#[tokio::test]
async fn large_output_round_trip() {
    let mut shell = Shell::spawn(ShellOptions::default()).await.unwrap();
    let res = shell
        .run("yes hello | head -n 10000", WaitOpts::default())
        .await
        .unwrap();
    match res {
        RunOutcome::Done { exit_code, output } => {
            assert_eq!(exit_code, 0);
            let lines: Vec<&str> = output.lines().collect();
            assert_eq!(lines.len(), 10000);
            assert!(lines.iter().all(|l| *l == "hello"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}
