use std::{
    fs,
    io::{Cursor, ErrorKind, Read, Write},
    net::{SocketAddrV4, TcpListener, TcpStream},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    str,
    thread::sleep,
    time::{Duration, Instant},
    collections::{HashMap, HashSet},
};

use png::{BitDepth, ColorType, Encoder};
use steamworks::{Client, FriendFlags, FriendState, SteamId, User, Friends};

use crate::{BrokerError, args::Args};

const FRAME_HEADER: &[u8; 4] = b"SBRK";
const FRAME_HEADER_SIZE: usize = 4;
const FRAME_LENGTH_SIZE: usize = 2;
const MAX_PAYLOAD_SIZE: usize = 4096;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

const RESPONSE_HEADER: &[u8] = b"sb_connect\n";
const PLAYER_RESPONSE_HEADER: &[u8] = b"sb_playerx\n";

const AVATAR_SIZE: u32 = 32;
const AVATAR_PENDING_TIMEOUT: Duration = Duration::from_secs(1);

const PLAYER_FIELD_NAME: u32 = 1 << 0;
const PLAYER_FIELD_AVATAR_SMALL: u32 = 1 << 1;
const PLAYER_FIELD_AVATAR_MEDIUM: u32 = 1 << 2; // reserved: not populated yet, kept for wire compatibility
const PLAYER_FIELD_AVATAR_LARGE: u32 = 1 << 3; // reserved: not populated yet, kept for wire compatibility
const PLAYER_FIELD_RELATIONSHIP: u32 = 1 << 4;
const PLAYER_FIELD_COUNTRY: u32 = 1 << 5; // reserved: not populated yet, kept for wire compatibility
const PLAYER_FIELD_GAME: u32 = 1 << 6;
const PLAYER_FIELD_RICH_PRESENCE: u32 = 1 << 7; // reserved: not populated yet, kept for wire compatibility
const PLAYER_FIELD_PERSONA_STATE: u32 = 1 << 8;

const PLAYER_FIELD_TYPE_NAME: u8 = 1;
const PLAYER_FIELD_TYPE_AVATAR_SMALL: u8 = 2;
const PLAYER_FIELD_TYPE_AVATAR_MEDIUM: u8 = 3; // reserved
const PLAYER_FIELD_TYPE_AVATAR_LARGE: u8 = 4; // reserved
const PLAYER_FIELD_TYPE_RELATIONSHIP: u8 = 5;
const PLAYER_FIELD_TYPE_COUNTRY: u8 = 6; // reserved
const PLAYER_FIELD_TYPE_GAME: u8 = 7;
const PLAYER_FIELD_TYPE_RICH_PRESENCE: u8 = 8; // reserved
const PLAYER_FIELD_TYPE_PERSONA_STATE: u8 = 9;

const PLAYER_RELATIONSHIP_NONE: u8 = 0;
const PLAYER_RELATIONSHIP_FRIEND: u8 = 1;
const PLAYER_RELATIONSHIP_BLOCKED: u8 = 2;
const PLAYER_RELATIONSHIP_FRIENDSHIP_REQUESTED: u8 = 3;
const PLAYER_RELATIONSHIP_REQUESTING_FRIENDSHIP: u8 = 4;

fn encode_avatar_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, BrokerError> {
    let expected_len = (width * height * 4) as usize;
    if rgba.len() != expected_len {
        return Err(BrokerError::Custom("unexpected avatar buffer size"));
    }

    let mut out = Vec::new();
    {
        let mut encoder = Encoder::new(Cursor::new(&mut out), width, height);
        encoder.set_color(ColorType::Rgba);
        encoder.set_depth(BitDepth::Eight);
        encoder.set_compression(png::Compression::High);
        let mut writer = encoder
            .write_header()
            .map_err(|_| BrokerError::Custom("png header write failed"))?;
        writer
            .write_image_data(rgba)
            .map_err(|_| BrokerError::Custom("png data write failed"))?;
    }
    Ok(out)
}

enum AvatarFetchState {
    Ready(Vec<u8>),
    Pending,
    Unavailable,
}

struct AvatarStore {
    ready: HashMap<SteamId, Vec<u8>>,
    pending_since: HashMap<SteamId, Instant>,
    unavailable: HashSet<SteamId>,
}

impl AvatarStore {
    fn new() -> Self {
        Self {
            ready: HashMap::new(),
            pending_since: HashMap::new(),
            unavailable: HashSet::new(),
        }
    }
}

#[derive(Clone)]
struct PlayerSnapshot {
    steamid: SteamId,
    name: Option<String>,
    avatar_small: Option<Vec<u8>>,
    avatar_medium: Option<Vec<u8>>, // reserved: always None today (Steam's 64x64 avatar)
    avatar_large: Option<Vec<u8>>,  // reserved: always None today (Steam's 184x184 avatar)
    relationship: u8,
    game_app_id: Option<u32>,
    persona_state: Option<u8>,
    country: Option<String>,                    // reserved: always None today
    rich_presence: Option<Vec<(String, String)>>, // reserved: always None today
}

struct PlayerInfoStore {
    players: HashMap<SteamId, PlayerSnapshot>,
}

impl PlayerInfoStore {
    fn new() -> Self {
        Self {
            players: HashMap::new(),
        }
    }
}

struct SteamService {
    client: Client,
    user: User,
    friends: Friends,
    avatar_store: AvatarStore,
    player_info_store: PlayerInfoStore,
    app_id: u32,
}

impl SteamService {
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

        let friends = client.friends();

        Ok(Self {
            client,
            user,
            friends,
            avatar_store: AvatarStore::new(),
            player_info_store: PlayerInfoStore::new(),
            app_id,
        })
    }

    fn poll_avatar(&mut self, steamid: SteamId) -> AvatarFetchState {
        if let Some(png) = self.avatar_store.ready.get(&steamid) {
            return AvatarFetchState::Ready(png.clone());
        }

        if self.avatar_store.unavailable.contains(&steamid) {
            return AvatarFetchState::Unavailable;
        }

        if let Some(started) = self.avatar_store.pending_since.get(&steamid) {
            if started.elapsed() > AVATAR_PENDING_TIMEOUT {
                println!("Avatar request timed out: {}", steamid.raw());
                self.avatar_store.pending_since.remove(&steamid);
                self.avatar_store.unavailable.insert(steamid);
                return AvatarFetchState::Unavailable;
            }
            return AvatarFetchState::Pending;
        }

        println!("Requesting avatar: {}", steamid.raw());
        self.friends.request_user_information(steamid, false);
        self.avatar_store
            .pending_since
            .insert(steamid, Instant::now());
        AvatarFetchState::Pending
    }

    fn process_pending_avatars(&mut self) {
        let pending: Vec<SteamId> = self.avatar_store.pending_since.keys().copied().collect();

        for steamid in pending {
            let friend = self.friends.get_friend(steamid);

            let Some(raw_rgba) = friend.small_avatar() else {
                continue;
            };

            match encode_avatar_png(&raw_rgba, AVATAR_SIZE, AVATAR_SIZE) {
                Ok(png_bytes) => {
                    println!(
                        "Avatar ready: {} (raw {} B -> png {} B)",
                        steamid.raw(),
                        raw_rgba.len(),
                        png_bytes.len()
                    );
                    self.avatar_store.ready.insert(steamid, png_bytes);
                    self.avatar_store.pending_since.remove(&steamid);
                    self.capture_player_snapshot(steamid);
                }
                Err(e) => {
                    println!("Avatar encode failed for {}: {e}", steamid.raw());
                    self.avatar_store.pending_since.remove(&steamid);
                    self.avatar_store.unavailable.insert(steamid);
                }
            }
        }
    }

    fn capture_player_snapshot(&mut self, steamid: SteamId) {
        let friend = self.friends.get_friend(steamid);

        let name = {
            let value = friend.name();
            if value.is_empty() {
                None
            } else {
                Some(value)
            }
        };

        let relationship = if friend.has_friend(FriendFlags::IMMEDIATE) {
            PLAYER_RELATIONSHIP_FRIEND
        } else if friend.has_friend(FriendFlags::BLOCKED) {
            PLAYER_RELATIONSHIP_BLOCKED
        } else if friend.has_friend(FriendFlags::FRIENDSHIP_REQUESTED) {
            PLAYER_RELATIONSHIP_FRIENDSHIP_REQUESTED
        } else if friend.has_friend(FriendFlags::REQUESTING_FRIENDSHIP) {
            PLAYER_RELATIONSHIP_REQUESTING_FRIENDSHIP
        } else {
            PLAYER_RELATIONSHIP_NONE
        };

        let persona_state = Some(match friend.state() {
            FriendState::Offline => 0,
            FriendState::Online => 1,
            FriendState::Busy => 2,
            FriendState::Away => 3,
            FriendState::Snooze => 4,
            FriendState::LookingToTrade => 5,
            FriendState::LookingToPlay => 6,
            FriendState::Invisible => 7,
        });

        let game_app_id = friend
            .game_played()
            .map(|game| game.game.app_id().0);

        let avatar_small = self.avatar_store.ready.get(&steamid).cloned();

        let snapshot = PlayerSnapshot {
            steamid,
            name,
            avatar_small,
            avatar_medium: None,
            avatar_large: None,
            relationship,
            game_app_id,
            persona_state,
            country: None,
            rich_presence: None,
        };

        self.player_info_store.players.insert(steamid, snapshot);
    }

    fn request_player_snapshot(&mut self, steamid: SteamId) {
        if !self.player_info_store.players.contains_key(&steamid) {
            self.capture_player_snapshot(steamid);
        }

        let _ = self.poll_avatar(steamid);
    }

    fn try_finalize_player_snapshot(&mut self, steamid: SteamId) -> Option<PlayerSnapshot> {
        match self.poll_avatar(steamid) {
            AvatarFetchState::Pending => return None,
            AvatarFetchState::Ready(png) => {
                if let Some(entry) = self.player_info_store.players.get_mut(&steamid) {
                    entry.avatar_small = Some(png);
                }
            }
            AvatarFetchState::Unavailable => {}
        }

        self.player_info_store.players.get(&steamid).cloned()
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
    steam: Option<SteamService>,
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
            steam: None,
            _scratch: scratch,
        })
    }

    pub fn run(&mut self) -> Result<(), BrokerError> {
        loop {
            if let Some(steam) = self.steam.as_ref() {
                steam.client.run_callbacks();
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
                steam: &mut self.steam,
                pending_player_replies: Vec::new(),
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
                    if self.steam.is_some() {
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
    steam: &'a mut Option<SteamService>,
    pending_player_replies: Vec<SteamId>,
}

impl Session<'_> {
    fn run(&mut self) -> Result<SessionResult, BrokerError> {
        loop {
            if let Some(steam) = self.steam.as_mut() {
                steam.client.run_callbacks();
                steam.process_pending_avatars();
            }

            self.flush_ready_player_replies()?;

            match self.read_chunk()? {
                ReadOutcome::Closed => {
                    println!("connection closed by peer");
                    self.cleanup_active_ticket();
                    if self.steam.is_some() {
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

    fn flush_ready_player_replies(&mut self) -> Result<(), BrokerError> {
        if self.pending_player_replies.is_empty() {
            return Ok(());
        }

        let Some(steam) = self.steam.as_mut() else {
            return Ok(());
        };

        let mut still_pending = Vec::with_capacity(self.pending_player_replies.len());
        let mut ready_snapshots = Vec::new();

        for steamid in self.pending_player_replies.drain(..) {
            match steam.try_finalize_player_snapshot(steamid) {
                Some(snapshot) => ready_snapshots.push(snapshot),
                None => still_pending.push(steamid),
            }
        }

        self.pending_player_replies = still_pending;

        for snapshot in ready_snapshots {
            self.send_player_snapshot_response(snapshot)?;
        }

        Ok(())
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
            "sb_get_player" => self.handle_get_player(rest)?,
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

        match self.steam.as_ref() {
            Some(existing) if existing.app_id != app_id => {
                // Steamworks SDK can't be re-initialized under a different AppID in-process.
                return Err(BrokerError::Custom(
                    "broker already initialized with a different AppID; sb_terminate first",
                ));
            }
            Some(_) => {}
            None => {
                *self.steam = Some(SteamService::new(app_id)?);
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

        let steam = self
            .steam
            .as_ref()
            .expect("steam service initialized in active state");

        println!(
            "initiate_game_connection: {serveradr} {game_server_steam_id} {secure} {challenge}"
        );
        #[allow(deprecated)]
        let ticket = steam
            .user
            .initiate_game_connection(SteamId::from_raw(game_server_steam_id), serveradr, secure)
            .ok_or(BrokerError::Custom("steam refused to issue auth ticket"))?;

        self.state = State::TicketRequested {
            challenge,
            serveradr,
        };

        println!("steam ticket size: {}, sending response", ticket.len());

        // payload: "sb_connect\n" + i32 challenge LE + u64 steamid LE + u32 size LE + ticket
        let steam_id = steam.user.steam_id().raw();
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

        let steam = self
            .steam
            .as_ref()
            .expect("steam service initialized in ticket state");
        #[allow(deprecated)]
        steam.user.terminate_game_connection(serveradr);

        self.state = State::Active;
        Ok(())
    }

    fn handle_get_player(&mut self, args: &[u8]) -> Result<(), BrokerError> {
        if self.state == State::Idle {
            return Err(BrokerError::Custom("session not active"));
        }

        let mut args = Args::new(args)?;
        let steamid_raw: u64 = args.parse("steam id")?;
        let steamid = SteamId::from_raw(steamid_raw);

        self.steam
            .as_mut()
            .expect("steam service initialized when session active")
            .request_player_snapshot(steamid);

        if !self.pending_player_replies.contains(&steamid) {
            self.pending_player_replies.push(steamid);
        }

        Ok(())
    }

    fn write_player_field(payload: &mut Vec<u8>, field_type: u8, data: &[u8]) -> bool {
        const FIELD_HEADER_SIZE: usize = 1 + 4;

        if payload.len() + FIELD_HEADER_SIZE + data.len() > MAX_PAYLOAD_SIZE {
            return false;
        }

        payload.push(field_type);
        payload.extend_from_slice(&(data.len() as u32).to_le_bytes());
        payload.extend_from_slice(data);

        true
    }

    fn send_player_snapshot_response(&mut self, snapshot: PlayerSnapshot) -> Result<(), BrokerError> {
        let mut payload = Vec::with_capacity(MAX_PAYLOAD_SIZE);
        payload.extend_from_slice(PLAYER_RESPONSE_HEADER);
        payload.extend_from_slice(&snapshot.steamid.raw().to_le_bytes());

        let flags_offset = payload.len();
        payload.extend_from_slice(&0u32.to_le_bytes());

        let mut flags = 0u32;

        if let Some(name) = snapshot.name.as_ref() {
            if Self::write_player_field(&mut payload, PLAYER_FIELD_TYPE_NAME, name.as_bytes()) {
                flags |= PLAYER_FIELD_NAME;
            }
        }

        if let Some(avatar) = snapshot.avatar_small.as_ref() {
            if Self::write_player_field(&mut payload, PLAYER_FIELD_TYPE_AVATAR_SMALL, avatar) {
                flags |= PLAYER_FIELD_AVATAR_SMALL;
            }
        }

        if let Some(avatar_medium) = snapshot.avatar_medium.as_ref() {
            if Self::write_player_field(
                &mut payload,
                PLAYER_FIELD_TYPE_AVATAR_MEDIUM,
                avatar_medium,
            ) {
                flags |= PLAYER_FIELD_AVATAR_MEDIUM;
            }
        }

        if let Some(avatar_large) = snapshot.avatar_large.as_ref() {
            if Self::write_player_field(
                &mut payload,
                PLAYER_FIELD_TYPE_AVATAR_LARGE,
                avatar_large,
            ) {
                flags |= PLAYER_FIELD_AVATAR_LARGE;
            }
        }

        if Self::write_player_field(
            &mut payload,
            PLAYER_FIELD_TYPE_RELATIONSHIP,
            &[snapshot.relationship],
        ) {
            flags |= PLAYER_FIELD_RELATIONSHIP;
        }

        if let Some(country) = snapshot.country.as_ref() {
            if Self::write_player_field(
                &mut payload,
                PLAYER_FIELD_TYPE_COUNTRY,
                country.as_bytes(),
            ) {
                flags |= PLAYER_FIELD_COUNTRY;
            }
        }

        if let Some(game_app_id) = snapshot.game_app_id {
            if Self::write_player_field(
                &mut payload,
                PLAYER_FIELD_TYPE_GAME,
                &game_app_id.to_le_bytes(),
            ) {
                flags |= PLAYER_FIELD_GAME;
            }
        }

        if let Some(rich_presence) = snapshot.rich_presence.as_ref() {
            let mut data = Vec::new();

            for (key, value) in rich_presence {
                if key.len() > u16::MAX as usize || value.len() > u16::MAX as usize {
                    continue;
                }

                data.extend_from_slice(&(key.len() as u16).to_le_bytes());
                data.extend_from_slice(key.as_bytes());
                data.extend_from_slice(&(value.len() as u16).to_le_bytes());
                data.extend_from_slice(value.as_bytes());
            }

            if !data.is_empty()
                && Self::write_player_field(&mut payload, PLAYER_FIELD_TYPE_RICH_PRESENCE, &data)
            {
                flags |= PLAYER_FIELD_RICH_PRESENCE;
            }
        }

        if let Some(persona_state) = snapshot.persona_state {
            if Self::write_player_field(
                &mut payload,
                PLAYER_FIELD_TYPE_PERSONA_STATE,
                &[persona_state],
            ) {
                flags |= PLAYER_FIELD_PERSONA_STATE;
            }
        }

        payload[flags_offset..flags_offset + 4]
            .copy_from_slice(&flags.to_le_bytes());

        println!(
            "sending player info: steamid={} flags=0x{:08x} payload={} B",
            snapshot.steamid.raw(),
            flags,
            payload.len()
        );

        self.send_frame(&payload)
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
            if let Some(steam) = self.steam.as_ref() {
                println!("cleaning up dangling ticket for {serveradr}");
                #[allow(deprecated)]
                steam.user.terminate_game_connection(serveradr);
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
