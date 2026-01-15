use directories::ProjectDirs;
use lildb::{
    lil_db_shell_client::LilDbShellClient, CommandRequest, ConnectRequest, DisconnectRequest,
};
use reedline::{DefaultPrompt, DefaultPromptSegment, FileBackedHistory, Reedline, Signal};
use std::{
    env::args,
    io::{stdout, Write},
    process,
    time::Duration,
};
use tokio::sync::mpsc::{self};
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::{Certificate, Channel, ClientTlsConfig};
use tracing::{error, info};

#[cfg(feature = "tracy")]
use std::alloc::System;

pub mod lildb {
    tonic::include_proto!("lildb");
}

#[cfg(feature = "tracy")]
#[global_allocator]
static GLOBAL: tracy_client::ProfiledAllocator<System> =
    tracy_client::ProfiledAllocator::new(System, 100);

fn check_args() -> (String, Option<String>, Option<String>) {
    let cmd_args: Vec<String> = args().collect::<Vec<String>>();

    let mut address: String = String::from("null");
    let mut ca_cert_path: Option<String> = None;
    let mut domain_override: Option<String> = None;

    for (i, arg) in cmd_args.clone().into_iter().enumerate() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("Usage: lildbsh [--help | -h]");
                println!("               [--version | -v]");
                println!("               [--address | -a] <address>");
                println!("               [--cert | -c] <ca_cert_path>");
                println!("               [--domain | -d] <domain>");

                process::exit(0);
            }
            "--version" | "-v" => {
                println!("LilDBsh 0.1.0");

                process::exit(0);
            }
            "--address" | "-a" => {
                if cmd_args.len() > i + 1 {
                    address.clone_from(&cmd_args[i + 1]);
                } else {
                    error!("No address provided, continuing as if nothing happened...");
                }
            }
            "--cert" | "-c" => {
                if i + 1 < cmd_args.len() {
                    ca_cert_path = Some(cmd_args[i + 1].clone());
                } else {
                    error!("No certificate path provided...");
                }
            }
            "--domain" | "-d" => {
                if i + 1 < cmd_args.len() {
                    domain_override = Some(cmd_args[i + 1].clone());
                }
            }
            _ => {}
        }
    }

    (address, ca_cert_path, domain_override)
}

async fn connect_to_db(
    address: &String,
    public_ip: &String,
    ca_cert_path: Option<String>,
    domain_override: Option<String>,
) -> anyhow::Result<LilDbShellClient<Channel>> {
    let mut input: String = String::new();

    if address == "null" {
        print!("Please insert your LilDB address: ");
        stdout().flush()?;

        let mut stdin_input = String::new();

        std::io::stdin().read_line(&mut stdin_input)?;

        input = stdin_input.trim().to_string();
    } else {
        input.clone_from(address);
    }

    let max_retries: i32 = 3;
    let mut attempts: i32 = 0;

    let channel: Channel;

    loop {
        attempts += 1;

        info!(
            "Attempting to connect to {} (Attempt {}/{})",
            input, attempts, max_retries
        );

        let mut endpoint = Channel::from_shared(input.clone())?
            .keep_alive_while_idle(true)
            .keep_alive_timeout(Duration::from_secs(30));

        let use_tls = input.starts_with("https") || ca_cert_path.is_some();

        if use_tls {
            let mut tls_config = ClientTlsConfig::new()
                .with_enabled_roots()
                .with_native_roots();

            if let Some(ref path) = ca_cert_path {
                let pem = tokio::fs::read_to_string(path)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to read CA cert at {path}: {e}"))?;

                let ca = Certificate::from_pem(pem);
                tls_config = tls_config.ca_certificate(ca);
            }

            if let Some(ref domain) = domain_override {
                tls_config = tls_config.domain_name(domain);
            }

            endpoint = endpoint.tls_config(tls_config)?;
        }

        let channel_result = endpoint.connect().await;

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

                    process::exit(0);
                }

                tokio::time::sleep(Duration::from_secs(1)).await;

                info!("Retrying...");
            }
        }
    }

    let mut client: LilDbShellClient<Channel> = LilDbShellClient::new(channel);

    let response: lildb::ConnectResponse = client
        .connect_to_db(ConnectRequest {
            ip: public_ip.clone(),
        })
        .await?
        .into_inner();

    if response.success {
        print!("{}\n\r", response.message);
    } else {
        error!("Failed to connect to\n\r");
        process::exit(1);
    }

    Ok(client)
}

async fn handle_shell(
    mut client: LilDbShellClient<Channel>,
    public_ip: String,
) -> anyhow::Result<()> {
    let (tx, rx) = mpsc::channel(32);

    let mut client_clone = client.clone();

    tokio::spawn(async move {
        let stream = ReceiverStream::new(rx);

        match client_clone.run_command(stream).await {
            Ok(response) => {
                let mut inbound = response.into_inner();

                while let Some(res) = inbound.message().await.unwrap_or(None) {
                    print!("\r\n{}", res.output);

                    let _ = stdout().flush();
                }
            }
            Err(e) => error!("Server connection lost: {}", e),
        }
    });

    let history_path = if let Some(proj_dirs) = ProjectDirs::from("com", "lildb", "lildbsh") {
        let data_dir = proj_dirs.data_dir();

        if let Err(e) = std::fs::create_dir_all(data_dir) {
            error!("Could not create history directory: {}", e);

            None
        } else {
            Some(data_dir.join("history.txt"))
        }
    } else {
        None
    };

    let history = match history_path {
        Some(path) => FileBackedHistory::with_file(1000, path.into())?,
        None => FileBackedHistory::with_file(1000, "lildb_history.txt".into())?,
    };

    let mut line_editor = Reedline::create().with_history(Box::new(history));

    let prompt = DefaultPrompt::new(DefaultPromptSegment::Empty, DefaultPromptSegment::Empty);

    loop {
        let sig = line_editor.read_line(&prompt);

        match sig {
            Ok(Signal::Success(buffer)) => {
                let trimmed = buffer.trim();

                if trimmed == "exit" {
                    break;
                }

                if !trimmed.is_empty() {
                    if let Err(e) = tx.send(CommandRequest { command: buffer }).await {
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

    let _ = client
        .disconnect_from_db(DisconnectRequest { ip: public_ip })
        .await;

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

    let (address, ca_cert_path, domain_override) = check_args();
    let public_ip: String = reqwest::get("https://api.ipify.org").await?.text().await?;

    let client: LilDbShellClient<Channel> =
        connect_to_db(&address, &public_ip, ca_cert_path, domain_override).await?;

    handle_shell(client, public_ip).await?;

    Ok(())
}
