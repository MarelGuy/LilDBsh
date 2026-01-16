use anyhow::bail;
use clap::Parser;
use directories::ProjectDirs;
use reedline::{DefaultPrompt, DefaultPromptSegment, FileBackedHistory, Reedline, Signal};
use std::{
    io::{self, stdout, Write},
    path::Path,
    time::Duration,
};
use tokio::sync::mpsc::{self};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    transport::{self, Certificate, Channel, ClientTlsConfig, Endpoint},
    Streaming,
};
use tracing::{error, info};
use uuid::Uuid;

#[cfg(feature = "tracy")]
use std::alloc::System;

use crate::lildb::{
    lil_db_shell_service_client::LilDbShellServiceClient, ConnectToDbRequest, ConnectToDbResponse,
    DisconnectFromDbRequest, RunCommandRequest, RunCommandResponse,
};

pub mod lildb {
    tonic::include_proto!("lildb");
}

#[cfg(feature = "tracy")]
#[global_allocator]
static GLOBAL: tracy_client::ProfiledAllocator<System> =
    tracy_client::ProfiledAllocator::new(System, 100);

#[derive(Parser, Debug)]
#[command(name = "lildbsh")]
#[command(version = "0.1.0")]
#[command(about = "Shell client for LilDB", long_about = None)]
struct Cli {
    #[arg(short, long)]
    address: Option<String>,

    #[arg(short = 'c', long = "cert")]
    ca_cert_path: Option<String>,

    #[arg(short = 'd', long = "domain")]
    domain_override: Option<String>,
}

async fn connect_to_db(
    address: Option<String>,
    session_id: &str,
    ca_cert_path: Option<String>,
    domain_override: Option<String>,
) -> anyhow::Result<LilDbShellServiceClient<Channel>> {
    let input: String = if let Some(address) = address {
        address
    } else {
        print!("Please insert your LilDB address: ");
        stdout().flush()?;

        let mut stdin_input: String = String::new();

        io::stdin().read_line(&mut stdin_input)?;

        stdin_input.trim().to_string()
    };

    let max_retries: u8 = 3;
    let mut attempts: u8 = 0;

    let channel: Channel;

    loop {
        attempts += 1;

        info!(
            "Attempting to connect to {} (Attempt {}/{})",
            input, attempts, max_retries
        );

        let mut endpoint: Endpoint = Channel::from_shared(input.clone())?
            .keep_alive_while_idle(true)
            .keep_alive_timeout(Duration::from_secs(30));

        let use_tls: bool = input.starts_with("https") || ca_cert_path.is_some();

        if use_tls {
            let mut tls_config: ClientTlsConfig = ClientTlsConfig::new()
                .with_enabled_roots()
                .with_native_roots();

            if let Some(ref path) = ca_cert_path {
                let pem: String =
                    tokio::fs::read_to_string(path)
                        .await
                        .map_err(|e: io::Error| {
                            anyhow::anyhow!("Failed to read CA cert at {path}: {e}")
                        })?;

                let ca: Certificate = Certificate::from_pem(pem);
                tls_config = tls_config.ca_certificate(ca);
            }

            if let Some(ref domain) = domain_override {
                tls_config = tls_config.domain_name(domain);
            }

            endpoint = endpoint.tls_config(tls_config)?;
        }

        let channel_result: Result<Channel, transport::Error> = endpoint.connect().await;

        match channel_result {
            Ok(ch) => {
                info!("Successfully connected to {}.", input);

                channel = ch;

                break;
            }
            Err(e) => {
                error!("Connection attempt {} failed: {}", attempts, e);

                if attempts >= max_retries {
                    error!(
                        "Failed to connect to {} after {} attempts.",
                        input, max_retries
                    );

                    bail!("Exiting...")
                }

                tokio::time::sleep(Duration::from_secs(1)).await;

                info!("Retrying...");
            }
        }
    }

    let mut client: LilDbShellServiceClient<Channel> = LilDbShellServiceClient::new(channel);

    let response: ConnectToDbResponse = client
        .connect_to_db(ConnectToDbRequest {
            session_id: session_id.to_string(),
        })
        .await?
        .into_inner();

    if response.success {
        print!("{}\n\r", response.message);
    } else {
        error!("Failed to connect to\n\r");

        bail!("Max retries reached")
    }

    Ok(client)
}

async fn handle_shell(
    mut client: LilDbShellServiceClient<Channel>,
    session_id: String,
) -> anyhow::Result<()> {
    let (tx, rx): (
        mpsc::Sender<RunCommandRequest>,
        mpsc::Receiver<RunCommandRequest>,
    ) = mpsc::channel(32);

    let mut client_clone: LilDbShellServiceClient<Channel> = client.clone();

    tokio::spawn(async move {
        let stream: ReceiverStream<RunCommandRequest> = ReceiverStream::new(rx);

        match client_clone.run_command(stream).await {
            Ok(response) => {
                let mut inbound: Streaming<RunCommandResponse> = response.into_inner();

                while let Some(res) = inbound.message().await.unwrap_or(None) {
                    print!("\r\n{}", res.output);

                    let _ = stdout().flush();
                }
            }
            Err(e) => error!("Server connection lost: {}", e),
        }
    });

    let history_path: Option<std::path::PathBuf> =
        if let Some(proj_dirs) = ProjectDirs::from("com", "lildb", "lildbsh") {
            let data_dir: &Path = proj_dirs.data_dir();

            if let Err(e) = std::fs::create_dir_all(data_dir) {
                error!("Could not create history directory: {}", e);

                None
            } else {
                Some(data_dir.join("history.txt"))
            }
        } else {
            None
        };

    let history: FileBackedHistory = match history_path {
        Some(path) => FileBackedHistory::with_file(1000, path.into())?,
        None => FileBackedHistory::with_file(1000, "lildb_history.txt".into())?,
    };

    let mut line_editor: Reedline = Reedline::create().with_history(Box::new(history));

    let prompt: DefaultPrompt =
        DefaultPrompt::new(DefaultPromptSegment::Empty, DefaultPromptSegment::Empty);

    loop {
        let sig: Result<Signal, io::Error> = line_editor.read_line(&prompt);

        match sig {
            Ok(Signal::Success(buffer)) => {
                let trimmed: &str = buffer.trim();

                if trimmed == "exit" {
                    break;
                }

                if !trimmed.is_empty() {
                    if let Err(e) = tx.send(RunCommandRequest { command: buffer }).await {
                        error!("Failed to send command: {}", e);

                        break;
                    }
                }
            }
            Ok(Signal::CtrlC | Signal::CtrlD) => {
                println!("\r\nAborted.");

                break;
            }
            Err(err) => {
                error!("Error reading line: {:?}", err);

                break;
            }
        }
    }

    client
        .disconnect_from_db(DisconnectFromDbRequest {
            session_id: session_id,
        })
        .await?;

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt::init();

    #[cfg(feature = "tracy")]
    {
        info!("Tracy is active");
    }

    let cli: Cli = Cli::parse();

    let session_id: String = Uuid::new_v4().to_string();

    let client: LilDbShellServiceClient<Channel> = connect_to_db(
        cli.address,
        &session_id,
        cli.ca_cert_path,
        cli.domain_override,
    )
    .await?;

    handle_shell(client, session_id).await?;

    Ok(())
}
