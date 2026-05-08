use frances_shell::{RunOutcome, Shell, ShellOptions, WaitOpts};

fn opts() -> ShellOptions {
    ShellOptions::default()
}

async fn run_done(shell: &mut Shell, cmd: &str) -> (i32, String) {
    match shell.run(cmd, WaitOpts::default()).await.unwrap() {
        RunOutcome::Done { exit_code, output } => (exit_code, output),
        other => panic!("expected Done, got {other:?}"),
    }
}

#[tokio::test]
async fn echo_hello() {
    let mut shell = Shell::spawn(opts()).await.unwrap();
    let (code, out) = run_done(&mut shell, "echo hello").await;
    assert_eq!(code, 0);
    assert_eq!(out, "hello\n");
}

#[tokio::test]
async fn env_survives_across_calls() {
    let mut shell = Shell::spawn(opts()).await.unwrap();
    let (code, _) = run_done(&mut shell, "X=42").await;
    assert_eq!(code, 0);
    let (_, out) = run_done(&mut shell, "echo $X").await;
    assert_eq!(out, "42\n");
}

#[tokio::test]
async fn cwd_survives_across_calls() {
    let mut shell = Shell::spawn(opts()).await.unwrap();
    run_done(&mut shell, "cd /tmp").await;
    let (_, out) = run_done(&mut shell, "pwd").await;
    assert_eq!(out, "/tmp\n");
}

#[tokio::test]
async fn stderr_merged_into_stdout_in_order() {
    let mut shell = Shell::spawn(opts()).await.unwrap();
    let (_, out) = run_done(&mut shell, "echo err >&2; echo out").await;
    // Both lines must appear, in the order bash emitted them.
    assert!(out.contains("err"));
    assert!(out.contains("out"));
    assert!(
        out.find("err").unwrap() < out.find("out").unwrap(),
        "stderr should appear before stdout (got {out:?})",
    );
}

#[tokio::test]
async fn nonzero_exit_surfaces() {
    let mut shell = Shell::spawn(opts()).await.unwrap();
    let (code, _) = run_done(&mut shell, "false").await;
    assert_eq!(code, 1);
}

#[tokio::test]
async fn multi_line_script_works() {
    // Demonstrates that the caller can write bash code as-is, no escaping
    // or `bash -c '...'` wrapping.
    let mut shell = Shell::spawn(opts()).await.unwrap();
    let script = r#"
        for i in 1 2 3; do
            echo "line $i"
        done
    "#;
    let (code, out) = run_done(&mut shell, script).await;
    assert_eq!(code, 0);
    assert!(out.contains("line 1"));
    assert!(out.contains("line 2"));
    assert!(out.contains("line 3"));
}

#[tokio::test]
async fn function_definitions_persist() {
    let mut shell = Shell::spawn(opts()).await.unwrap();
    run_done(&mut shell, "greet() { echo \"hi $1\"; }").await;
    let (_, out) = run_done(&mut shell, "greet world").await;
    assert_eq!(out, "hi world\n");
}

#[tokio::test]
async fn init_script_runs_at_spawn() {
    let opts = ShellOptions {
        init_script: Some("export FOO=bar".into()),
        ..ShellOptions::default()
    };
    let mut shell = Shell::spawn(opts).await.unwrap();
    let (_, out) = run_done(&mut shell, "echo $FOO").await;
    assert_eq!(out, "bar\n");
}

#[tokio::test]
async fn cwd_option_is_respected() {
    let opts = ShellOptions {
        cwd: Some("/tmp".into()),
        ..ShellOptions::default()
    };
    let mut shell = Shell::spawn(opts).await.unwrap();
    let (_, out) = run_done(&mut shell, "pwd").await;
    assert_eq!(out, "/tmp\n");
}
