use frances_shell::{RunOutcome, Shell, ShellError, ShellOptions, WaitOpts};

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
    assert!(shell.is_alive());
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
    assert!(shell.is_alive());
}

#[tokio::test]
async fn exit_kills_shell() {
    let mut shell = Shell::spawn(ShellOptions::default()).await.unwrap();
    let res = shell.run("exit", WaitOpts::default()).await.unwrap();
    assert!(matches!(res, RunOutcome::Dead { .. }));
    assert!(!shell.is_alive());
    let again = shell.run("echo nope", WaitOpts::default()).await;
    assert!(matches!(again, Err(ShellError::Dead)));
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
