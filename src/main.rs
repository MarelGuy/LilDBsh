use core::time::Duration;
use crossterm::event::{read, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::enable_raw_mode;
use lildb::{
    lil_db_shell_client::LilDbShellClient, ConnectRequest, DisconnectRequest, DisconnectResponse,
};
use lildb::{CommandRequest, CommandResponse};
use std::{
    env::args,
    error::Error,
    io::{stdout, Write},
    process,
};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Channel, Response, Streaming};
use tracing::{error, info};
pub mod lildb {
    tonic::include_proto!("lildb");
}

fn clear_input() -> Result<(), Box<dyn Error>> {
    print!("\x1B[2K\x1B[1G");
    print!(">> ");

    stdout().flush()?;

    Ok(())
}

fn read_input(input: &mut String, command_history: Vec<String>) -> Result<bool, Box<dyn Error>> {
    clear_input()?;

    let mut ch_len: usize = command_history.len();

    loop {
        if let Event::Key(KeyEvent {
            code,
            kind,
            modifiers,
            state: _,
        }) = read()?
        {
            if kind == KeyEventKind::Press {
                match (code, modifiers) {
                    (KeyCode::Enter, KeyModifiers::ALT) => {
                        print!("\n\r");
                        input.push('\n');
                    }
                    (KeyCode::Enter, _) => {
                        if !input.is_empty() {
                            break;
                        }
                    }
                    (KeyCode::Backspace, _) if !input.is_empty() => {
                        input.pop();
                        print!("\x1B[1D\x1B[K");
                    }
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(true),
                    (KeyCode::Char(c), _) => {
                        input.push(c);
                        print!("{}", c);
                    }
                    (KeyCode::Up, _) => {
                        if ch_len > 0 {
                            ch_len -= 1;

                            clear_input()?;

                            *input = command_history[ch_len].clone();

                            print!("{}", input);
                        }
                    }
                    (KeyCode::Down, _) => {
                        if ch_len < command_history.len() {
                            ch_len += 1;

                            if ch_len < command_history.len() {
                                clear_input()?;

                                *input = command_history[ch_len].clone();

                                print!("{}", input);
                            } else {
                                *input = String::from("");
                                clear_input()?;
                            }
                        }
                    }
                    _ => {} // _ => println!("{:?} {:?}", code, modifiers),
                }
            }
        }
        stdout().flush()?;
    }

    Ok(false)
}

fn check_args() -> String {
    let cmd_args: Vec<String> = args().collect::<Vec<String>>();

    let mut address: String = String::from("null");

    for (i, arg) in cmd_args.clone().into_iter().enumerate() {
        match arg.as_str() {
            "--help" | "-h" => {
                println!("Usage: lildbsh [--help | -h]");
                println!("               [--version | -v]");
                println!("               [--address | -a] <address>");

                process::exit(0);
            }
            "--version" | "-v" => {
                println!("LilDBsh 0.1.0");

                process::exit(0);
            }
            "--address" | "-a" => {
                if cmd_args.len() > i + 1 {
                    address = cmd_args[i + 1].clone();
                } else {
                    error!("No address provided, continuing as if nothing happened...");
                }
            }
            _ => {}
        }
    }

    address
}

async fn connect_to_db(
    address: &String,
    public_ip: &String,
) -> Result<LilDbShellClient<Channel>, Box<dyn Error>> {
    let mut input: String = String::new();

    if address != "null" {
        input = address.clone();
    } else {
        print!("Please insert your LilDB address:\n\r");

        stdout().flush()?;

        read_input(&mut input, Vec::new())?;

        print!("\n\r");
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

        let channel_result: Result<Channel, tonic::transport::Error> =
            Channel::from_shared(input.clone())
                .unwrap()
                .keep_alive_while_idle(true)
                .keep_alive_timeout(Duration::from_secs(30))
                .connect()
                .await;

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
                info!("Retrying...",);
            }
        }
    }

    let mut client: LilDbShellClient<Channel> = LilDbShellClient::new(channel);

    let response: lildb::ConnectResponse = client
        .connect_to_db(ConnectRequest {
            ip: public_ip.to_string(),
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
    mut command_history: Vec<String>,
    public_ip: String,
) -> Result<(), Box<dyn Error>> {
    loop {
        let (tx, rx): (Sender<CommandRequest>, Receiver<CommandRequest>) = mpsc::channel(4);
        let (tx_command, mut rx_command): (Sender<String>, Receiver<String>) = mpsc::channel(4);
        let (tx_disconnect, mut rx_disconnect): (Sender<bool>, Receiver<bool>) = mpsc::channel(4);

        let command_history_clone: Vec<String> = command_history.clone();

        tokio::spawn(async move {
            let mut command: String = String::new();

            let mut exit: bool = read_input(&mut command, command_history_clone).unwrap();

            if command == "exit" {
                exit = true;
            }

            tx.send(CommandRequest {
                command: command.to_owned(),
            })
            .await
            .unwrap();

            tx_command.send(command).await.unwrap();

            tx_disconnect.send(exit).await.unwrap();
        });

        if rx_disconnect.recv().await.unwrap() {
            let disconnection: DisconnectResponse = client
                .disconnect_from_db(DisconnectRequest {
                    ip: public_ip.to_string(),
                })
                .await?
                .into_inner();

            if disconnection.success {
                info!("\n\r{}", disconnection.message);

                break;
            }
        }

        let command: String = rx_command.recv().await.unwrap();
        command_history.push(command);

        let response: Response<Streaming<CommandResponse>> =
            client.run_command(ReceiverStream::new(rx)).await?;

        let mut inbound: Streaming<CommandResponse> = response.into_inner();

        while let Some(res) = inbound.message().await? {
            print!("\n\r{}", res.output);
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt::init();
    enable_raw_mode()?;

    let address: String = check_args();
    let public_ip: String = reqwest::get("https://api.ipify.org").await?.text().await?;

    let client: LilDbShellClient<Channel> = connect_to_db(&address, &public_ip).await?;

    let command_history: Vec<String> = Vec::new();

    handle_shell(client, command_history, public_ip).await?;

    Ok(())
}
