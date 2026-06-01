use frances_shell::{RunOutcome, Shell, ShellOptions, WaitOpts};

async fn run_done_str(shell: &mut Shell, cmd: &str) -> String {
    match shell.run(cmd, WaitOpts::default()).await.unwrap() {
        RunOutcome::Done { output, .. } => output,
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn two_shells_have_independent_state() {
    let (mut a, mut b) = tokio::join!(
        async { Shell::spawn(ShellOptions::default()).await.unwrap() },
        async { Shell::spawn(ShellOptions::default()).await.unwrap() },
    );

    run_done_str(&mut a, "cd /tmp; X=alpha").await;
    run_done_str(&mut b, "cd /; X=beta").await;

    let (cwd_a, cwd_b) = tokio::join!(run_done_str(&mut a, "pwd"), run_done_str(&mut b, "pwd"));
    assert_eq!(cwd_a, "/tmp\n");
    assert_eq!(cwd_b, "/\n");

    let (x_a, x_b) = tokio::join!(
        run_done_str(&mut a, "echo $X"),
        run_done_str(&mut b, "echo $X")
    );
    assert_eq!(x_a, "alpha\n");
    assert_eq!(x_b, "beta\n");
}

#[tokio::test]
async fn many_shells_in_parallel_dont_interfere() {
    const N: usize = 8;

    let mut handles = Vec::with_capacity(N);
    for i in 0..N {
        handles.push(tokio::spawn(async move {
            let mut s = Shell::spawn(ShellOptions::default()).await.unwrap();
            match s
                .run(&format!("echo shell-{i}"), WaitOpts::default())
                .await
                .unwrap()
            {
                RunOutcome::Done { output, .. } => output,
                other => panic!("shell {i}: {other:?}"),
            }
        }));
    }

    for (i, h) in handles.into_iter().enumerate() {
        let out = h.await.unwrap();
        assert_eq!(out, format!("shell-{i}\n"));
    }
}
