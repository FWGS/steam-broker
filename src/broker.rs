use std::{
    fs,
    io::{ErrorKind, Read, Write},
    net::{SocketAddrV4, TcpListener, TcpStream},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    str,
    thread::sleep,
    time::Duration,
};

use steamworks::{Client, SteamId, User};

use crate::{BrokerError, args::Args};

const FRAME_HEADER: &[u8; 4] = b"SBRK";
const FRAME_HEADER_SIZE: usize = 4;
const FRAME_LENGTH_SIZE: usize = 2;
const MAX_PAYLOAD_SIZE: usize = 4096;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

const RESPONSE_HEADER: &[u8] = b"sb_connect\n";

struct SteamApi {
    client: Client,
    user: User,
    app_id: u32,
}

impl SteamApi {
    fn new(app_id: u32) -> Result<Self, BrokerError> {
        // Steamworks SDK picks AppID from steam_appid.txt in cwd at init time.
        fs::write("steam_appid.txt", app_id.to_string()).map_err(BrokerError::Io)?;

        println!("Initializing Steam with AppID {app_id}...");

        let client = Client::init()?;

        let utils = client.utils();
        println!("Utils:");
        println!("AppId: {:?}", utils.app_id());

        let user = client.user();
        println!("User:");
        println!("SteamID: {:?}", user.steam_id());

        Ok(Self {
            client,
            user,
            app_id,
        })
    }
}

fn appid_for_gamedir(gamedir: &str) -> Option<u32> {
    match gamedir.to_ascii_lowercase().as_str() {
        "cstrike" => Some(10),       // Counter-Strike 1.6
        "tfc" => Some(20),           // Team Fortress Classic
        "dod" => Some(30),           // Day of Defeat
        "dmc" => Some(40),           // Deathmatch Classic
        "gearbox" => Some(50),       // Half-Life: Opposing Force
        "ricochet" => Some(60),      // Ricochet
        "valve" => Some(70),         // Half-Life
        "czero" => Some(80),         // Counter-Strike: Condition Zero
        "czeror" => Some(100),       // Counter-Strike: Condition Zero — Deleted Scenes
        "bshift" => Some(130),       // Half-Life: Blue Shift
        "cstrike_beta" => Some(150), // Counter-Strike 1.6 beta
        _ => None,
    }
}

const FALLBACK_APP_ID: u32 = 70;

#[derive(Copy, Clone, PartialEq, Eq)]
enum State {
    Idle,
    Active,
    TicketRequested {
        challenge: i32,
        serveradr: SocketAddrV4,
    },
}

enum SessionResult {
    Continue,
    Terminate,
}

pub struct Broker {
    listener: TcpListener,
    api: Option<SteamApi>,
    _scratch: ScratchDir,
}

impl Broker {
    pub fn new(addr: &str) -> Result<Self, BrokerError> {
        let scratch = ScratchDir::new()?;
        std::env::set_current_dir(scratch.path()).map_err(BrokerError::Io)?;
        println!("Scratch directory: {}", scratch.path().display());

        let listener = TcpListener::bind(addr).map_err(BrokerError::CreateSocket)?;
        listener.set_nonblocking(true)?;
        println!("Started TCP server at {}", listener.local_addr()?);

        Ok(Self {
            listener,
            api: None,
            _scratch: scratch,
        })
    }

    pub fn run(&mut self) -> Result<(), BrokerError> {
        loop {
            if let Some(api) = self.api.as_ref() {
                api.client.run_callbacks();
            }

            let (stream, peer) = match self.listener.accept() {
                Ok(x) => x,
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    sleep(POLL_INTERVAL);
                    continue;
                }
                Err(e) => return Err(e.into()),
            };

            println!("Accepted connection from {peer}");

            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(POLL_INTERVAL))?;
            stream.set_nodelay(true).ok();

            let mut session = Session {
                stream,
                rx_buffer: Vec::with_capacity(MAX_PAYLOAD_SIZE),
                state: State::Idle,
                api: &mut self.api,
            };

            match session.run() {
                Ok(SessionResult::Continue) => {
                    println!("session ended, awaiting next connection");
                }
                Ok(SessionResult::Terminate) => {
                    println!("sb_terminate received, exiting for restart");
                    return Ok(());
                }
                Err(err) => {
                    println!("session error: {err}");
                    if self.api.is_some() {
                        println!("steam was initialized, exiting for restart");
                        return Ok(());
                    }
                }
            }
        }
    }
}

struct Session<'a> {
    stream: TcpStream,
    rx_buffer: Vec<u8>,
    state: State,
    api: &'a mut Option<SteamApi>,
}

impl Session<'_> {
    fn run(&mut self) -> Result<SessionResult, BrokerError> {
        loop {
            if let Some(api) = self.api.as_ref() {
                api.client.run_callbacks();
            }

            match self.read_chunk()? {
                ReadOutcome::Closed => {
                    println!("connection closed by peer");
                    self.cleanup_active_ticket();
                    if self.api.is_some() {
                        println!("steam was initialized, treating disconnect as sb_terminate");
                        return Ok(SessionResult::Terminate);
                    }
                    return Ok(SessionResult::Continue);
                }
                ReadOutcome::DataOrIdle => {}
            }

            while let Some(payload) = self.try_parse_frame()? {
                if let SessionResult::Terminate = self.handle_command(&payload)? {
                    return Ok(SessionResult::Terminate);
                }
            }
        }
    }

    fn read_chunk(&mut self) -> Result<ReadOutcome, BrokerError> {
        let mut buf = [0u8; 4096];
        match self.stream.read(&mut buf) {
            Ok(0) => Ok(ReadOutcome::Closed),
            Ok(n) => {
                if self.rx_buffer.len() + n > MAX_PAYLOAD_SIZE * 2 {
                    return Err(BrokerError::Custom("rx buffer overflow"));
                }
                self.rx_buffer.extend_from_slice(&buf[..n]);
                Ok(ReadOutcome::DataOrIdle)
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Ok(ReadOutcome::DataOrIdle)
            }
            Err(e) => Err(e.into()),
        }
    }

    fn try_parse_frame(&mut self) -> Result<Option<Vec<u8>>, BrokerError> {
        if self.rx_buffer.len() < FRAME_HEADER_SIZE + FRAME_LENGTH_SIZE {
            return Ok(None);
        }

        if &self.rx_buffer[..FRAME_HEADER_SIZE] != FRAME_HEADER {
            return Err(BrokerError::Custom("invalid frame magic"));
        }

        let len_bytes = [self.rx_buffer[4], self.rx_buffer[5]];
        let payload_size = u16::from_le_bytes(len_bytes) as usize;
        if payload_size > MAX_PAYLOAD_SIZE {
            return Err(BrokerError::Custom("frame too large"));
        }

        let total_size = FRAME_HEADER_SIZE + FRAME_LENGTH_SIZE + payload_size;
        if self.rx_buffer.len() < total_size {
            return Ok(None);
        }

        let payload = self.rx_buffer[FRAME_HEADER_SIZE + FRAME_LENGTH_SIZE..total_size].to_vec();
        self.rx_buffer.drain(..total_size);
        Ok(Some(payload))
    }

    fn handle_command(&mut self, payload: &[u8]) -> Result<SessionResult, BrokerError> {
        let text = str::from_utf8(payload)?;
        let mut parts = text.splitn(2, char::is_whitespace);
        let cmd = parts.next().unwrap_or("").trim();
        let rest = parts.next().unwrap_or("").as_bytes();

        println!("got {cmd}");

        match cmd {
            "sb_gamedir" => self.handle_gamedir(rest)?,
            "sb_connect" => self.handle_connect(rest)?,
            "sb_disconnect" => self.handle_disconnect(rest)?,
            "sb_terminate" => {
                self.cleanup_active_ticket();
                return Ok(SessionResult::Terminate);
            }
            _ => return Err(BrokerError::Custom("unknown command")),
        }

        Ok(SessionResult::Continue)
    }

    fn handle_gamedir(&mut self, args: &[u8]) -> Result<(), BrokerError> {
        if self.state != State::Idle {
            return Err(BrokerError::Custom("session already active"));
        }

        let mut args = Args::new(args)?;
        let gamedir = args.next().ok_or(BrokerError::Missing("gamedir"))?;
        let app_id = appid_for_gamedir(gamedir).unwrap_or_else(|| {
            println!(
                "warning: unknown gamedir \"{gamedir}\", falling back to AppID {FALLBACK_APP_ID}"
            );
            FALLBACK_APP_ID
        });
        println!("activating session for gamedir \"{gamedir}\" (AppID {app_id})");

        match self.api.as_ref() {
            Some(existing) if existing.app_id != app_id => {
                // Steamworks SDK can't be re-initialized under a different AppID in-process.
                return Err(BrokerError::Custom(
                    "broker already initialized with a different AppID; sb_terminate first",
                ));
            }
            Some(_) => {}
            None => {
                *self.api = Some(SteamApi::new(app_id)?);
            }
        }

        self.state = State::Active;
        Ok(())
    }

    fn handle_connect(&mut self, args: &[u8]) -> Result<(), BrokerError> {
        if self.state != State::Active {
            return Err(BrokerError::Custom("session not active"));
        }

        let mut args = Args::new(args)?;
        println!("handle_connect: {}", args.as_str());

        // sb_connect <ip:port> <server_steamid> <secure 0|1> <challenge>
        let serveradr: SocketAddrV4 = args.parse("ip addr")?;
        let game_server_steam_id: u64 = args.parse("steam id")?;
        let secure_int: i32 = args.parse("secure")?;
        let secure = secure_int != 0;
        let challenge: i32 = args.parse("challenge")?;

        let api = self
            .api
            .as_ref()
            .expect("steam api initialized in active state");

        println!(
            "initiate_game_connection: {serveradr} {game_server_steam_id} {secure} {challenge}"
        );
        #[allow(deprecated)]
        let ticket = api
            .user
            .initiate_game_connection(SteamId::from_raw(game_server_steam_id), serveradr, secure)
            .ok_or(BrokerError::Custom("steam refused to issue auth ticket"))?;

        self.state = State::TicketRequested {
            challenge,
            serveradr,
        };

        println!("steam ticket size: {}, sending response", ticket.len());

        // payload: "sb_connect\n" + i32 challenge LE + u64 steamid LE + u32 size LE + ticket
        let steam_id = api.user.steam_id().raw();
        let mut payload = Vec::with_capacity(RESPONSE_HEADER.len() + 4 + 8 + 4 + ticket.len());
        payload.extend_from_slice(RESPONSE_HEADER);
        payload.extend_from_slice(&challenge.to_le_bytes());
        payload.extend_from_slice(&steam_id.to_le_bytes());
        payload.extend_from_slice(&(ticket.len() as u32).to_le_bytes());
        payload.extend_from_slice(&ticket);

        self.send_frame(&payload)?;
        Ok(())
    }

    fn handle_disconnect(&mut self, args: &[u8]) -> Result<(), BrokerError> {
        let State::TicketRequested {
            challenge: requested,
            serveradr: _,
        } = self.state
        else {
            return Err(BrokerError::Custom("no ticket requested"));
        };

        let mut args = Args::new(args)?;

        // sb_disconnect <ip:port> <challenge>
        let serveradr: SocketAddrV4 = args.parse("ip addr")?;
        let challenge: i32 = args.parse("challenge")?;

        if challenge != requested {
            return Err(BrokerError::Custom("challenge mismatch"));
        }

        let api = self
            .api
            .as_ref()
            .expect("steam api initialized in ticket state");
        #[allow(deprecated)]
        api.user.terminate_game_connection(serveradr);

        self.state = State::Active;
        Ok(())
    }

    fn send_frame(&mut self, payload: &[u8]) -> Result<(), BrokerError> {
        if payload.len() > MAX_PAYLOAD_SIZE {
            return Err(BrokerError::Custom("response payload too large"));
        }

        let mut frame = Vec::with_capacity(FRAME_HEADER_SIZE + FRAME_LENGTH_SIZE + payload.len());
        frame.extend_from_slice(FRAME_HEADER);
        frame.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        frame.extend_from_slice(payload);

        self.stream.write_all(&frame).map_err(BrokerError::Send)?;
        Ok(())
    }

    fn cleanup_active_ticket(&mut self) {
        if let State::TicketRequested { serveradr, .. } = self.state {
            if let Some(api) = self.api.as_ref() {
                println!("cleaning up dangling ticket for {serveradr}");
                #[allow(deprecated)]
                api.user.terminate_game_connection(serveradr);
            }
            self.state = State::Active;
        }
    }
}

enum ReadOutcome {
    DataOrIdle,
    Closed,
}

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Result<Self, BrokerError> {
        let path = PathBuf::from(format!("/tmp/steam-broker-{:08x}", fastrand::u32(..)));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .map_err(BrokerError::Io)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
