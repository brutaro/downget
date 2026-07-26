use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "downget", version, about = "Downloader HTTP(S) resiliente")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Add {
        url: String,
        #[arg(short, long)]
        output: Option<std::path::PathBuf>,
        #[arg(long, value_parser = parse_sha256)]
        sha256: Option<String>,
    },
    List,
    Resume {
        id: i64,
        #[arg(long)]
        url: Option<String>,
        #[arg(long, value_parser = parse_sha256)]
        sha256: Option<String>,
    },
    Cancel {
        id: i64,
        #[arg(long)]
        discard: bool,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Set {
        key: ConfigKey,
        #[arg(value_parser = parse_concurrency)]
        value: u8,
    },
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum ConfigKey {
    Concurrency,
}

pub fn parse_sha256(value: &str) -> std::result::Result<String, String> {
    if value.len() != 64 || !value.as_bytes().iter().all(u8::is_ascii_hexdigit) {
        return Err("SHA-256 deve ter exatamente 64 caracteres hexadecimais".to_owned());
    }
    Ok(value.to_ascii_lowercase())
}

pub fn parse_concurrency(value: &str) -> std::result::Result<u8, String> {
    let value = value
        .parse::<u8>()
        .map_err(|_| "concorrência deve ser um inteiro entre 1 e 8".to_owned())?;
    if !(1..=8).contains(&value) {
        return Err("concorrência deve estar entre 1 e 8".into());
    }
    Ok(value)
}
