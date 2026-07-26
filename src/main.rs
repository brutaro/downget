use clap::Parser;
use downget::{app::App, cli::Cli};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let result = match App::new() {
        Ok(app) => app.run(cli).await,
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        eprintln!("Erro: {error}");
        std::process::exit(downget::exit_code(&error));
    }
}
