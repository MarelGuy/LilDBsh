use core::time::Duration;
use crossterm::event::{read, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::enable_raw_mode;
use lildb::{
    lil_db_shell_client::LilDbShellClient, ConnectRequest, DisconnectRequest, DisconnectResponse,
};
use lildb::{CommandRequest, CommandResponse};
use std::{
    error::Error,
    io::{stdout, Write},
    process,
};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{transport::Channel, Response, Streaming};
pub mod lildb {
    tonic::include_proto!("lildb");
}

fn read_input(input: &mut String) -> Result<bool, Box<dyn Error>> {
    print!(">> ");
    stdout().flush()?;

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
                        stdout().flush()?;
                    }
                    (KeyCode::Enter, _) => {
                        if !input.is_empty() {
                            stdout().flush()?;
                            break;
                        }
                    }
                    (KeyCode::Backspace, _) if !input.is_empty() => {
                        input.pop();
                        print!("\x1B[1D\x1B[K");
                        stdout().flush()?;
                    }
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(true),
                    (KeyCode::Char(c), _) => {
                        input.push(c);
                        print!("{}", c);
                        stdout().flush()?;
                    }
                    _ => {} // _ => println!("{:?} {:?}", code, modifiers),
                }
            }
        }
    }

    Ok(false)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;

    print!("Please insert your LilDB address (no http://):\n\r");

    stdout().flush()?;

    let mut input = String::new();
    read_input(&mut input)?;

    print!("\n\r");

    let channel: Channel = Channel::from_shared(format!("http://{}", input))
        .unwrap()
        .keep_alive_while_idle(true)
        .keep_alive_timeout(Duration::from_secs(30))
        .connect()
        .await?;

    let mut client: LilDbShellClient<Channel> = LilDbShellClient::new(channel);

    let public_ip: String = reqwest::get("https://api.ipify.org").await?.text().await?;

    let response: lildb::ConnectResponse = client
        .connect_to_db(ConnectRequest {
            ip: public_ip.to_string(),
        })
        .await?
        .into_inner();

    if response.success {
        print!("{}!\n\r", response.message);
    } else {
        print!("Failed to connect to\n\r");

        process::exit(1);
    }

    loop {
        let (tx, rx): (Sender<CommandRequest>, Receiver<CommandRequest>) = mpsc::channel(4);
        let (tx_disconnect, mut rx_disconnect): (Sender<bool>, Receiver<bool>) = mpsc::channel(4);

        tokio::spawn(async move {
            let mut command = String::new();

            let mut exit: bool = read_input(&mut command).unwrap();

            if command == "exit" {
                exit = true;
            }

            tx.send(CommandRequest {
                command: command.to_owned(),
            })
            .await
            .unwrap();

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
                print!("\n\r{}!\n\r", disconnection.message);

                break;
            }
        }

        let response: Response<Streaming<CommandResponse>> =
            client.run_command(ReceiverStream::new(rx)).await?;

        let mut inbound: Streaming<CommandResponse> = response.into_inner();

        while let Some(res) = inbound.message().await? {
            print!("\n\r{}\n\r", res.output);

            if res.output.is_empty() {
                process::exit(0);
            }
        }
    }

    Ok(())
}
