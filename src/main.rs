mod args;
mod broker;

use std::{io, process, str::Utf8Error};

use steamworks::SteamAPIInitError;
use thiserror::Error;

use crate::broker::Broker;

const DEFAULT_LISTEN: &str = "127.0.0.1:27420";

#[derive(Error, Debug)]
enum BrokerError {
    #[error("steam api error, {0}")]
    Api(#[from] SteamAPIInitError),
    #[error("create socket error, {0}")]
    CreateSocket(io::Error),
    #[error("send error, {0}")]
    Send(io::Error),
    #[error("io error, {0}")]
    Io(#[from] io::Error),
    #[error("invalid string, {0}")]
    Utf8(#[from] Utf8Error),
    #[error("invalid args, missing \"{0}\"")]
    Missing(&'static str),
    #[error("failed to parse \"{0}\"")]
    Parse(&'static str),
    #[error("{0}")]
    Custom(&'static str),
}

fn main() {
    println!("Welcome to Steam Broker!");

    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| DEFAULT_LISTEN.to_string());

    match Broker::new(&addr) {
        Ok(mut broker) => {
            if let Err(err) = broker.run() {
                println!("error: {err:?}");
                process::exit(1);
            }
        }
        Err(err) => {
            println!("error: failed to create socket: {err:?}");
            process::exit(1);
        }
    }
}
