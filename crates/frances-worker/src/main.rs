use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args != ["serve", "--stdio"] {
        eprintln!("usage: frances-worker serve --stdio");
        return ExitCode::FAILURE;
    }

    let stream = tokio::io::join(tokio::io::stdin(), tokio::io::stdout());
    match frances_worker::serve(stream).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("frances-worker: {error}");
            ExitCode::FAILURE
        }
    }
}
