use std::time::Duration;

use frances_shell::{QuietReason, RunOutcome, Shell, ShellOptions, WaitOpts};

#[tokio::test]
async fn quiet_trips_on_silent_command() {
    let mut shell = Shell::spawn(ShellOptions::default()).await.unwrap();
    let res = shell
        .run(
            "sleep 3",
            WaitOpts {
                quiet: Some(Duration::from_millis(150)),
                max: None,
            },
        )
        .await
        .unwrap();
    match res {
        RunOutcome::Quiet {
            reason: QuietReason::NoOutput,
            output,
        } => assert!(output.is_empty()),
        other => panic!("unexpected: {other:?}"),
    }
    // Interrupt rather than wait the full 3s.
    shell.interrupt().await.unwrap();
    let _ = shell.keep_waiting(WaitOpts::default()).await.unwrap();
}

#[tokio::test]
async fn max_trips_even_during_streaming() {
    let mut shell = Shell::spawn(ShellOptions::default()).await.unwrap();
    let res = shell
        .run(
            "yes",
            WaitOpts {
                quiet: None,
                max: Some(Duration::from_millis(100)),
            },
        )
        .await
        .unwrap();
    match res {
        RunOutcome::Quiet {
            reason: QuietReason::MaxElapsed,
            output,
        } => {
            assert!(!output.is_empty(), "yes should produce a torrent of output");
        }
        other => panic!("unexpected: {other:?}"),
    }
    shell.kill_running().await.unwrap();
    let _ = shell.keep_waiting(WaitOpts::default()).await.unwrap();
}

#[tokio::test]
async fn keep_waiting_resumes_after_quiet() {
    let mut shell = Shell::spawn(ShellOptions::default()).await.unwrap();
    let first = shell
        .run(
            "sleep 0.3; echo done",
            WaitOpts {
                quiet: Some(Duration::from_millis(80)),
                max: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(first, RunOutcome::Quiet { .. }));

    let second = shell
        .keep_waiting(WaitOpts {
            quiet: None,
            max: None,
        })
        .await
        .unwrap();
    match second {
        RunOutcome::Done { exit_code, output } => {
            assert_eq!(exit_code, 0);
            assert!(output.contains("done"));
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[tokio::test]
async fn interrupt_kills_running_command_keeps_shell_alive() {
    let mut shell = Shell::spawn(ShellOptions::default()).await.unwrap();
    // Use `&&` so that when sleep is killed (non-zero exit), the echo is
    // skipped — bash's `;` would run both statements regardless, since
    // interrupt only stops the currently-running command.
    let first = shell
        .run(
            "sleep 60 && echo too late",
            WaitOpts {
                quiet: Some(Duration::from_millis(100)),
                max: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(first, RunOutcome::Quiet { .. }));

    shell.interrupt().await.unwrap();
    let after = shell
        .keep_waiting(WaitOpts {
            quiet: Some(Duration::from_millis(200)),
            max: Some(Duration::from_secs(2)),
        })
        .await
        .unwrap();
    match after {
        RunOutcome::Done { exit_code, output } => {
            assert_ne!(exit_code, 0, "interrupted command should not exit 0");
            assert!(
                !output.contains("too late"),
                "echo after sleep must not have run",
            );
        }
        other => panic!("expected Done after interrupt, got {other:?}"),
    }
    assert!(shell.is_alive());

    let res = shell.run("echo alive", WaitOpts::default()).await.unwrap();
    assert!(matches!(res, RunOutcome::Done { exit_code: 0, .. }));
}
