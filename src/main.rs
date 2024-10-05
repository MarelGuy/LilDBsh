use crossterm::event::{read, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::enable_raw_mode;
use lildb::lil_db_shell_client::LilDbShellClient;
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

fn read_input(input: &mut String) -> Result<(), Box<dyn Error>> {
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
            if kind == KeyEventKind::Release {
                match (code, modifiers) {
                    (KeyCode::Enter, KeyModifiers::ALT) => {
                        println!();
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
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        process::exit(0);
                    }
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
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;

    println!("Please insert your LilDB address (no http://): ");

    let mut input = String::new();
    read_input(&mut input)?;

    println!();

    let mut client: LilDbShellClient<Channel> =
        LilDbShellClient::connect(format!("http://{}", input)).await?;

    loop {
        let (tx, rx): (Sender<CommandRequest>, Receiver<CommandRequest>) = mpsc::channel(4);

        tokio::spawn(async move {
            let mut command = String::new();

            read_input(&mut command).unwrap();

            tx.send(CommandRequest {
                command: command.to_owned(),
            })
            .await
            .unwrap();
        });

        let response: Response<Streaming<CommandResponse>> =
            client.run_command(ReceiverStream::new(rx)).await?;

        let mut inbound: Streaming<CommandResponse> = response.into_inner();

        while let Some(res) = inbound.message().await? {
            println!("\n{}", res.output);

            if res.output.is_empty() {
                process::exit(0);
            }
        }
    }
}
