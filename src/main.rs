use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::io::IsTerminal;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
use windows_sys::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
#[cfg(windows)]
use windows_sys::Win32::System::Console::{
    DISABLE_NEWLINE_AUTO_RETURN, ENABLE_PROCESSED_OUTPUT, ENABLE_VIRTUAL_TERMINAL_PROCESSING,
    GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE, SetConsoleMode,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const INTERNAL_SERVER_ARG: &str = "--coder-server";
const STATE_VERSION: u32 = 1;
const RING_LIMIT: usize = 1024 * 1024;

const FRAME_STDOUT: u8 = 1;
const FRAME_EXIT: u8 = 2;
const FRAME_STDIN: u8 = 10;
const FRAME_RESIZE: u8 = 11;
const FRAME_DETACH: u8 = 12;

const DETACH_HINT: &str = "\r\n[coder] Detached. Run `coder` again to reattach.\r\n";
const CURSOR_SHOW: &[u8] = b"\x1b[?25h";
const CURSOR_HIDE: &[u8] = b"\x1b[?25l";

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ServerConfig {
    version: u32,
    session: String,
    cwd: PathBuf,
    args: Vec<String>,
    state_path: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SessionState {
    version: u32,
    session: String,
    port: u16,
    server_pid: u32,
    cwd: PathBuf,
    args: Vec<String>,
    started_at_ms: u128,
}

#[derive(Debug)]
struct ClientOptions {
    session: String,
    codex_args: Vec<String>,
}

#[derive(Clone)]
struct ClientSink {
    id: u64,
    tx: mpsc::Sender<ServerMessage>,
}

enum ServerMessage {
    Output(Vec<u8>),
    Exit(i32),
}

struct Shared {
    clients: Mutex<Vec<ClientSink>>,
    pty_writer: Mutex<Option<Box<dyn Write + Send>>>,
    pty_master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    ring: Mutex<VecDeque<u8>>,
    exit_code: Mutex<Option<i32>>,
    oom_detected: AtomicBool,
    next_client_id: AtomicU64,
    terminal_cols: AtomicU64,
    terminal_rows: AtomicU64,
    cursor_col: AtomicU64,
    cursor_row: AtomicU64,
}

struct RunningCodex {
    exit_rx: mpsc::Receiver<i32>,
    pty_done_rx: mpsc::Receiver<()>,
}

struct RawModeGuard {
    enabled: bool,
    #[cfg(windows)]
    _output_mode: Option<WindowsOutputModeGuard>,
}

impl RawModeGuard {
    fn enable() -> Self {
        #[cfg(windows)]
        let output_mode = WindowsOutputModeGuard::enable().ok();
        let enabled = crossterm::terminal::enable_raw_mode().is_ok();
        Self {
            enabled,
            #[cfg(windows)]
            _output_mode: output_mode,
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.enabled {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

#[cfg(windows)]
struct WindowsOutputModeGuard {
    handle: HANDLE,
    original_mode: u32,
}

#[cfg(windows)]
impl WindowsOutputModeGuard {
    fn enable() -> io::Result<Self> {
        let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let mut original_mode = 0;
        if unsafe { GetConsoleMode(handle, &mut original_mode) } == 0 {
            return Err(io::Error::last_os_error());
        }

        let new_mode = original_mode
            | ENABLE_PROCESSED_OUTPUT
            | ENABLE_VIRTUAL_TERMINAL_PROCESSING
            | DISABLE_NEWLINE_AUTO_RETURN;
        if unsafe { SetConsoleMode(handle, new_mode) } == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            handle,
            original_mode,
        })
    }
}

#[cfg(windows)]
impl Drop for WindowsOutputModeGuard {
    fn drop(&mut self) {
        let _ = unsafe { SetConsoleMode(self.handle, self.original_mode) };
    }
}

#[derive(Debug)]
struct ResolvedCodexCommand {
    program: OsString,
    args: Vec<OsString>,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("coder: {err}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    debug_log("run start");
    let mut args = env::args().skip(1).collect::<Vec<_>>();

    if args.first().map(String::as_str) == Some(INTERNAL_SERVER_ARG) {
        if args.len() != 2 {
            return Err(format!("{INTERNAL_SERVER_ARG} expects a config path").into());
        }

        let config_path = PathBuf::from(args.remove(1));
        let text = fs::read_to_string(&config_path)?;
        let config = serde_json::from_str::<ServerConfig>(&text)?;
        debug_log(format!(
            "server mode session={} args={}",
            config.session,
            config.args.join(" ")
        ));
        return run_server(config);
    }

    let options = parse_client_args(args);

    let state_path = state_path(&options.session)?;

    if should_attach_to_existing(&options.codex_args)
        && let Some(stream) = connect_existing(&state_path)?
    {
        let code = run_client(stream)?;
        process::exit(code);
    }

    if should_run_codex_direct(&options.codex_args) {
        let code = run_codex_direct(&options.codex_args)?;
        process::exit(code);
    }

    if state_path.exists() && !should_attach_to_existing(&options.codex_args) {
        let code = run_codex_direct(&options.codex_args)?;
        process::exit(code);
    }

    ensure_codex_available(&options.codex_args)?;
    let config_path = write_server_config(&options.session, &state_path, &options.codex_args)?;
    start_detached_server(&config_path)?;

    let stream = wait_for_server(&state_path, Duration::from_secs(10))?;
    let code = run_client(stream)?;
    process::exit(code);
}

fn parse_client_args(args: Vec<String>) -> ClientOptions {
    let session = env::var("CODER_SESSION")
        .map(|value| sanitize_session_name(&value))
        .unwrap_or_else(|_| derived_session_name(&args).unwrap_or_else(|| "default".to_string()));

    ClientOptions {
        session,
        codex_args: args,
    }
}

fn derived_session_name(args: &[String]) -> Option<String> {
    let resume_index = first_positional_index(args)?;
    if args.get(resume_index).map(String::as_str) != Some("resume") {
        return None;
    }

    let resume_args = &args[resume_index + 1..];
    if resume_args.is_empty() {
        return None;
    }

    if let Some(target) = resume_target(resume_args) {
        return Some(resume_session_name(&target));
    }

    Some(format!(
        "resume-{}",
        short_hash(&(env::current_dir().ok(), args))
    ))
}

fn sanitize_session_name(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
            out.push(ch);
        }
    }

    if out.is_empty() {
        "default".to_string()
    } else {
        out
    }
}

fn resume_session_name(target: &str) -> String {
    let sanitized = sanitize_session_name(target);
    if sanitized.len() <= 80 {
        format!("resume-{sanitized}")
    } else {
        format!("resume-{}", short_hash(&target))
    }
}

fn short_hash<T: Hash>(value: &T) -> String {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn resume_target(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return args.get(i + 1).cloned();
        }

        if option_takes_value(arg) {
            i += if has_inline_value(arg) { 1 } else { 2 };
            continue;
        }

        if arg.starts_with('-') {
            i += 1;
            continue;
        }

        return Some(arg.to_string());
    }

    None
}

fn should_attach_to_existing(args: &[String]) -> bool {
    args.is_empty() || first_positional(args).as_deref() == Some("resume")
}

fn should_run_codex_direct(args: &[String]) -> bool {
    if args.is_empty() {
        return false;
    }

    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "--version" | "-V"))
    {
        return true;
    }

    match first_positional(args).as_deref() {
        Some("resume") => false,
        Some(command) => is_direct_codex_command(command),
        None => has_unknown_option(args),
    }
}

fn is_direct_codex_command(command: &str) -> bool {
    matches!(
        command,
        "exec"
            | "e"
            | "review"
            | "login"
            | "logout"
            | "mcp"
            | "plugin"
            | "mcp-server"
            | "app-server"
            | "remote-control"
            | "app"
            | "completion"
            | "update"
            | "doctor"
            | "sandbox"
            | "debug"
            | "apply"
            | "a"
            | "fork"
            | "cloud"
            | "features"
            | "help"
    )
}

fn has_unknown_option(args: &[String]) -> bool {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return false;
        }

        if option_takes_value(arg) {
            i += if has_inline_value(arg) { 1 } else { 2 };
            continue;
        }

        if arg.starts_with('-') {
            if is_known_flag(arg) {
                i += 1;
                continue;
            }

            return true;
        }

        return false;
    }

    false
}

fn first_positional(args: &[String]) -> Option<String> {
    first_positional_index(args).map(|index| args[index].to_string())
}

fn first_positional_index(args: &[String]) -> Option<usize> {
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            return None;
        }

        if option_takes_value(arg) {
            i += if has_inline_value(arg) { 1 } else { 2 };
            continue;
        }

        if arg.starts_with('-') {
            i += 1;
            continue;
        }

        return Some(i);
    }

    None
}

fn option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-c" | "--config"
            | "-i"
            | "--image"
            | "-m"
            | "--model"
            | "--local-provider"
            | "-p"
            | "--profile"
            | "-s"
            | "--sandbox"
            | "-C"
            | "--cd"
            | "--add-dir"
            | "-a"
            | "--ask-for-approval"
            | "--remote"
            | "--remote-auth-token-env"
            | "--enable"
            | "--disable"
    ) || arg.starts_with("--config=")
        || arg.starts_with("--image=")
        || arg.starts_with("--model=")
        || arg.starts_with("--local-provider=")
        || arg.starts_with("--profile=")
        || arg.starts_with("--sandbox=")
        || arg.starts_with("--cd=")
        || arg.starts_with("--add-dir=")
        || arg.starts_with("--ask-for-approval=")
        || arg.starts_with("--remote=")
        || arg.starts_with("--remote-auth-token-env=")
        || arg.starts_with("--enable=")
        || arg.starts_with("--disable=")
}

fn has_inline_value(arg: &str) -> bool {
    arg.starts_with("--") && arg.contains('=')
}

fn is_known_flag(arg: &str) -> bool {
    matches!(
        arg,
        "--oss"
            | "--strict-config"
            | "--dangerously-bypass-approvals-and-sandbox"
            | "--dangerously-bypass-hook-trust"
            | "--search"
            | "--no-alt-screen"
    )
}

fn modifier_code(modifiers: crossterm::event::KeyModifiers) -> Option<u8> {
    let shift = modifiers.contains(crossterm::event::KeyModifiers::SHIFT);
    let alt = modifiers.contains(crossterm::event::KeyModifiers::ALT);
    let ctrl = modifiers.contains(crossterm::event::KeyModifiers::CONTROL);

    match (shift, alt, ctrl) {
        (false, false, false) => None,
        (true, false, false) => Some(2),
        (false, true, false) => Some(3),
        (true, true, false) => Some(4),
        (false, false, true) => Some(5),
        (true, false, true) => Some(6),
        (false, true, true) => Some(7),
        (true, true, true) => Some(8),
    }
}

fn modified_csi(final_byte: char, modifiers: crossterm::event::KeyModifiers) -> Vec<u8> {
    if let Some(code) = modifier_code(modifiers) {
        format!("\x1b[1;{code}{final_byte}").into_bytes()
    } else {
        format!("\x1b[{final_byte}").into_bytes()
    }
}

fn modified_tilde(prefix: u8, modifiers: crossterm::event::KeyModifiers) -> Vec<u8> {
    if let Some(code) = modifier_code(modifiers) {
        format!("\x1b[{prefix};{code}~").into_bytes()
    } else {
        format!("\x1b[{prefix}~").into_bytes()
    }
}

fn encode_key_event(event: crossterm::event::KeyEvent) -> Option<Vec<u8>> {
    use crossterm::event::KeyCode;
    use crossterm::event::KeyModifiers;

    if matches!(event.kind, crossterm::event::KeyEventKind::Release) {
        return None;
    }

    let modifiers = event.modifiers;
    let alt_gr_printable = matches!(
        event.code,
        KeyCode::Char(ch)
            if modifiers.contains(KeyModifiers::ALT)
                && modifiers.contains(KeyModifiers::CONTROL)
                && !ch.is_control()
    );
    let alt_prefix = if modifiers.contains(KeyModifiers::ALT) && !alt_gr_printable {
        vec![0x1b]
    } else {
        Vec::new()
    };

    let mut encoded = match event.code {
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Left => modified_csi('D', modifiers),
        KeyCode::Right => modified_csi('C', modifiers),
        KeyCode::Up => modified_csi('A', modifiers),
        KeyCode::Down => modified_csi('B', modifiers),
        KeyCode::Home => modified_csi('H', modifiers),
        KeyCode::End => modified_csi('F', modifiers),
        KeyCode::PageUp => modified_tilde(5, modifiers),
        KeyCode::PageDown => modified_tilde(6, modifiers),
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Delete => modified_tilde(3, modifiers),
        KeyCode::Insert => modified_tilde(2, modifiers),
        KeyCode::Esc => vec![0x1b],
        KeyCode::F(number) => encode_function_key(number, modifiers),
        KeyCode::Char(ch) if alt_gr_printable => ch.to_string().into_bytes(),
        KeyCode::Char(ch) if modifiers.contains(KeyModifiers::CONTROL) => encode_control_char(ch)?,
        KeyCode::Char(ch) => shifted_printable_char(ch, modifiers)
            .to_string()
            .into_bytes(),
        _ => return None,
    };

    if modifiers.contains(KeyModifiers::ALT)
        && !alt_gr_printable
        && !matches!(event.code, KeyCode::Esc)
    {
        let mut prefixed = alt_prefix;
        prefixed.append(&mut encoded);
        Some(prefixed)
    } else {
        Some(encoded)
    }
}

fn shifted_printable_char(ch: char, modifiers: crossterm::event::KeyModifiers) -> char {
    if !modifiers.contains(crossterm::event::KeyModifiers::SHIFT) {
        return ch;
    }

    match ch {
        'a'..='z' => ch.to_ascii_uppercase(),
        '`' => '~',
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        _ => ch,
    }
}

fn encode_control_char(ch: char) -> Option<Vec<u8>> {
    let lower = ch.to_ascii_lowercase();
    match lower {
        '@' | ' ' => Some(vec![0x00]),
        '[' => Some(vec![0x1b]),
        '\\' => Some(vec![0x1c]),
        ']' => Some(vec![0x1d]),
        '^' => Some(vec![0x1e]),
        '_' => Some(vec![0x1f]),
        'a'..='z' => Some(vec![(lower as u8) - b'a' + 1]),
        _ => None,
    }
}

fn encode_function_key(number: u8, modifiers: crossterm::event::KeyModifiers) -> Vec<u8> {
    let base = match number {
        1 => return modified_ss3_or_csi('P', modifiers),
        2 => return modified_ss3_or_csi('Q', modifiers),
        3 => return modified_ss3_or_csi('R', modifiers),
        4 => return modified_ss3_or_csi('S', modifiers),
        5 => 15,
        6 => 17,
        7 => 18,
        8 => 19,
        9 => 20,
        10 => 21,
        11 => 23,
        12 => 24,
        _ => return Vec::new(),
    };

    modified_tilde(base, modifiers)
}

fn modified_ss3_or_csi(final_byte: char, modifiers: crossterm::event::KeyModifiers) -> Vec<u8> {
    if let Some(code) = modifier_code(modifiers) {
        format!("\x1b[1;{code}{final_byte}").into_bytes()
    } else {
        format!("\x1bO{final_byte}").into_bytes()
    }
}

fn app_dir() -> Result<PathBuf> {
    let base = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| env::var_os("APPDATA").map(PathBuf::from))
        .unwrap_or_else(env::temp_dir);
    let dir = base.join("coder");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn state_path(session: &str) -> Result<PathBuf> {
    Ok(app_dir()?.join(format!("{session}.state.json")))
}

fn config_path(session: &str) -> Result<PathBuf> {
    Ok(app_dir()?.join(format!("{session}.config.json")))
}

fn write_server_config(session: &str, state_path: &Path, codex_args: &[String]) -> Result<PathBuf> {
    let config = ServerConfig {
        version: STATE_VERSION,
        session: session.to_string(),
        cwd: env::current_dir()?,
        args: codex_args.to_vec(),
        state_path: state_path.to_path_buf(),
    };

    let path = config_path(session)?;
    fs::write(&path, serde_json::to_vec_pretty(&config)?)?;
    let _ = fs::remove_file(state_path);
    Ok(path)
}

fn start_detached_server(config_path: &Path) -> Result<()> {
    let exe = env::current_exe()?;
    let mut command = Command::new(exe);
    command
        .arg(INTERNAL_SERVER_ARG)
        .arg(config_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW);
    }

    command.spawn()?;
    Ok(())
}

fn connect_existing(state_path: &Path) -> Result<Option<TcpStream>> {
    if !state_path.exists() {
        return Ok(None);
    }

    let state = match read_state(state_path) {
        Ok(state) => state,
        Err(_) => {
            let _ = fs::remove_file(state_path);
            return Ok(None);
        }
    };

    let addr = SocketAddr::from(([127, 0, 0, 1], state.port));
    match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
        Ok(stream) => {
            stream.set_nodelay(true).ok();
            Ok(Some(stream))
        }
        Err(_) => {
            let _ = fs::remove_file(state_path);
            Ok(None)
        }
    }
}

fn wait_for_server(state_path: &Path, timeout: Duration) -> Result<TcpStream> {
    let started = SystemTime::now();
    loop {
        if let Some(stream) = connect_existing(state_path)? {
            return Ok(stream);
        }

        if started.elapsed().unwrap_or_default() > timeout {
            return Err("timed out waiting for coder broker to start".into());
        }

        thread::sleep(Duration::from_millis(100));
    }
}

fn read_state(state_path: &Path) -> Result<SessionState> {
    Ok(serde_json::from_slice(&fs::read(state_path)?)?)
}

fn write_state(config: &ServerConfig, listener: &TcpListener) -> Result<()> {
    let port = listener.local_addr()?.port();
    let state = SessionState {
        version: STATE_VERSION,
        session: config.session.clone(),
        port,
        server_pid: process::id(),
        cwd: config.cwd.clone(),
        args: config.args.clone(),
        started_at_ms: now_ms(),
    };

    fs::write(&config.state_path, serde_json::to_vec_pretty(&state)?)?;
    Ok(())
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn run_server(config: ServerConfig) -> Result<()> {
    debug_log(format!("server start session={}", config.session));
    env::set_current_dir(&config.cwd)?;

    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    listener.set_nonblocking(true)?;
    write_state(&config, &listener)?;
    debug_log(format!("server listen {}", listener.local_addr()?));

    let shared = Arc::new(Shared {
        clients: Mutex::new(Vec::new()),
        pty_writer: Mutex::new(None),
        pty_master: Mutex::new(None),
        ring: Mutex::new(VecDeque::with_capacity(RING_LIMIT)),
        exit_code: Mutex::new(None),
        oom_detected: AtomicBool::new(false),
        next_client_id: AtomicU64::new(1),
        terminal_cols: AtomicU64::new(120),
        terminal_rows: AtomicU64::new(30),
        cursor_col: AtomicU64::new(1),
        cursor_row: AtomicU64::new(1),
    });

    let mut running = Some(spawn_codex_process(&config, Arc::clone(&shared))?);
    let mut restart_at = None::<Instant>;
    let mut exit_seen_at = None::<Instant>;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                debug_log("server accepted client");
                stream.set_nodelay(true).ok();
                stream.set_nonblocking(false).ok();
                let client_shared = Arc::clone(&shared);
                thread::spawn(move || handle_client(stream, client_shared));
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(err.into()),
        }

        if let Some(when) = restart_at
            && Instant::now() >= when
        {
            broadcast_notice(
                &shared,
                "\r\n[coder] Restarting Codex after out-of-memory crash.\r\n",
            );
            running = Some(spawn_codex_process(&config, Arc::clone(&shared))?);
            restart_at = None;
        }

        if exit_seen_at.is_none()
            && restart_at.is_none()
            && let Some(active) = running.as_ref()
        {
            if let Some((code, reason)) = poll_codex_exit(active) {
                if should_restart_after_exit(&shared, code) {
                    schedule_oom_restart(&shared, code);
                    running = None;
                    restart_at = Some(Instant::now() + oom_restart_delay());
                } else {
                    mark_session_exited(&config.state_path, &shared, code, reason);
                    exit_seen_at = Some(Instant::now());
                }
            }
        }

        if exit_seen_at
            .map(|seen| seen.elapsed() > Duration::from_secs(2))
            .unwrap_or(false)
        {
            return Ok(());
        }
    }
}

fn spawn_codex_process(config: &ServerConfig, shared: Arc<Shared>) -> Result<RunningCodex> {
    shared.oom_detected.store(false, Ordering::Relaxed);
    *shared.exit_code.lock().expect("exit code mutex poisoned") = None;

    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 30,
        cols: 120,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let resolved = resolve_codex_command(&config.args);
    let mut command = CommandBuilder::new(&resolved.program);
    for arg in resolved.args {
        command.arg(arg);
    }
    command.cwd(&config.cwd);

    let mut child = pair.slave.spawn_command(command)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;
    *shared.pty_writer.lock().expect("pty writer mutex poisoned") = Some(writer);
    *shared.pty_master.lock().expect("pty master mutex poisoned") = Some(pair.master);

    let (pty_done_tx, pty_done_rx) = mpsc::channel();
    let output_shared = Arc::clone(&shared);
    thread::spawn(move || read_pty_output(&mut reader, output_shared, pty_done_tx));

    let (exit_tx, exit_rx) = mpsc::channel();
    thread::spawn(move || {
        let code = match child.wait() {
            Ok(status) => status.exit_code() as i32,
            Err(_) => 1,
        };
        let _ = exit_tx.send(code);
    });

    Ok(RunningCodex {
        exit_rx,
        pty_done_rx,
    })
}

fn poll_codex_exit(active: &RunningCodex) -> Option<(i32, &'static str)> {
    if let Ok(code) = active.exit_rx.try_recv() {
        Some((code, "child exit"))
    } else if active.pty_done_rx.try_recv().is_ok() {
        Some((0, "pty closed"))
    } else {
        None
    }
}

fn mark_session_exited(state_path: &Path, shared: &Shared, code: i32, reason: &str) {
    debug_log(format!("{reason} code={code}"));
    let _ = fs::remove_file(state_path);
    *shared.pty_writer.lock().expect("pty writer mutex poisoned") = None;
    *shared.pty_master.lock().expect("pty master mutex poisoned") = None;
    *shared.exit_code.lock().expect("exit code mutex poisoned") = Some(code);
    let notice = format!("\r\n[coder] codex exited with code {code}\r\n").into_bytes();
    push_ring(shared, &notice);
    broadcast_output(shared, notice);
    broadcast_exit(shared, code);
}

fn should_restart_after_exit(shared: &Shared, code: i32) -> bool {
    shared.oom_detected.load(Ordering::Relaxed) || is_oom_exit_code(code)
}

fn is_oom_exit_code(code: i32) -> bool {
    matches!(code, 134 | 137 | -1073741801)
}

fn schedule_oom_restart(shared: &Shared, code: i32) {
    debug_log(format!("oom restart scheduled code={code}"));
    *shared.pty_writer.lock().expect("pty writer mutex poisoned") = None;
    *shared.pty_master.lock().expect("pty master mutex poisoned") = None;
    *shared.exit_code.lock().expect("exit code mutex poisoned") = None;
    let delay = oom_restart_delay();
    let notice = format!(
        "\r\n[coder] Codex appears to have exited due to out of memory (code {code}). Restarting in {} seconds.\r\n",
        delay.as_secs()
    );
    broadcast_notice(shared, &notice);
}

fn oom_restart_delay() -> Duration {
    env::var("CODER_OOM_RESTART_DELAY_SECONDS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or_else(|| Duration::from_secs(180))
}

fn broadcast_notice(shared: &Shared, notice: &str) {
    let bytes = notice.as_bytes().to_vec();
    push_ring(shared, &bytes);
    broadcast_output(shared, bytes);
}

fn read_pty_output(
    reader: &mut Box<dyn Read + Send>,
    shared: Arc<Shared>,
    done_tx: mpsc::Sender<()>,
) {
    let mut buf = [0u8; 8192];
    let mut terminal_output = TerminalOutputFilter::default();
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                debug_log(format!("pty output bytes={n}"));
                let raw = &buf[..n];
                if looks_like_oom_output(raw) {
                    shared.oom_detected.store(true, Ordering::Relaxed);
                }
                let chunk = terminal_output.process(&shared, raw);
                if chunk.is_empty() {
                    continue;
                }
                push_ring(&shared, &chunk);
                broadcast_output(&shared, chunk);
            }
            Err(_) => break,
        }
    }
    debug_log("pty output reader ended");
    let _ = done_tx.send(());
}

fn looks_like_oom_output(chunk: &[u8]) -> bool {
    let text = String::from_utf8_lossy(chunk).to_ascii_lowercase();
    text.contains("javascript heap out of memory")
        || text.contains("heap out of memory")
        || text.contains("allocation failed")
        || text.contains("reached heap limit")
        || text.contains("fatal process out of memory")
        || text.contains("out of memory")
}

#[derive(Clone, Copy)]
enum TerminalQuery {
    Status,
    CursorPosition,
    DecCursorPosition,
}

#[derive(Default)]
struct TerminalOutputFilter {
    pending: Vec<u8>,
}

impl TerminalOutputFilter {
    fn process(&mut self, shared: &Shared, chunk: &[u8]) -> Vec<u8> {
        let mut input = Vec::with_capacity(self.pending.len() + chunk.len());
        input.append(&mut self.pending);
        input.extend_from_slice(chunk);

        let mut output = Vec::with_capacity(input.len());
        let mut i = 0;

        while i < input.len() {
            if let Some((query, len)) = terminal_query_at(&input[i..]) {
                answer_terminal_query(shared, query);
                i += len;
                continue;
            }

            if input[i] == 0x1b {
                if starts_incomplete_escape(&input[i..]) {
                    self.pending.extend_from_slice(&input[i..]);
                    break;
                }

                if let Some(len) = ansi_escape_len(&input[i..]) {
                    update_cursor_from_escape(shared, &input[i..i + len]);
                    output.extend_from_slice(&input[i..i + len]);
                    i += len;
                    continue;
                }
            }

            update_cursor_from_byte(shared, input[i]);
            output.push(input[i]);
            i += 1;
        }

        output
    }
}

fn terminal_query_at(input: &[u8]) -> Option<(TerminalQuery, usize)> {
    if input.starts_with(b"\x1b[?6n") {
        Some((TerminalQuery::DecCursorPosition, 5))
    } else if input.starts_with(b"\x1b[6n") {
        Some((TerminalQuery::CursorPosition, 4))
    } else if input.starts_with(b"\x1b[5n") {
        Some((TerminalQuery::Status, 4))
    } else {
        None
    }
}

fn answer_terminal_query(shared: &Shared, query: TerminalQuery) {
    let response = match query {
        TerminalQuery::Status => b"\x1b[0n".to_vec(),
        TerminalQuery::CursorPosition => {
            let (row, col) = cursor_position(shared);
            format!("\x1b[{row};{col}R").into_bytes()
        }
        TerminalQuery::DecCursorPosition => {
            let (row, col) = cursor_position(shared);
            format!("\x1b[?{row};{col}R").into_bytes()
        }
    };

    let mut writer = shared.pty_writer.lock().expect("pty writer mutex poisoned");
    if let Some(writer) = writer.as_mut() {
        let _ = writer.write_all(&response);
        let _ = writer.flush();
    }
}

fn ansi_escape_len(input: &[u8]) -> Option<usize> {
    if input.len() < 2 || input[0] != 0x1b {
        return None;
    }

    match input[1] {
        b'[' => input
            .iter()
            .enumerate()
            .skip(2)
            .find(|(_, byte)| (0x40..=0x7e).contains(&**byte))
            .map(|(index, _)| index + 1),
        b']' => {
            let mut i = 2;
            while i < input.len() {
                if input[i] == 0x07 {
                    return Some(i + 1);
                }

                if input[i] == 0x1b && input.get(i + 1) == Some(&b'\\') {
                    return Some(i + 2);
                }

                i += 1;
            }
            None
        }
        b'O' if input.len() >= 3 => Some(3),
        _ => Some(2),
    }
}

fn starts_incomplete_escape(input: &[u8]) -> bool {
    if input.is_empty() || input[0] != 0x1b {
        return false;
    }

    if input.len() == 1 {
        return true;
    }

    match input[1] {
        b'[' => ansi_escape_len(input).is_none(),
        b']' => ansi_escape_len(input).is_none(),
        b'O' => input.len() < 3,
        _ => false,
    }
}

fn update_cursor_from_escape(shared: &Shared, sequence: &[u8]) {
    if sequence.len() < 3 || !sequence.starts_with(b"\x1b[") {
        return;
    }

    let final_byte = *sequence.last().unwrap_or(&0);
    let params = csi_params(&sequence[2..sequence.len() - 1]);
    let (rows, cols) = terminal_size(shared);
    let (mut row, mut col) = cursor_position(shared);

    match final_byte {
        b'A' => row = row.saturating_sub(csi_param(&params, 0, 1)).max(1),
        b'B' => row = (row + csi_param(&params, 0, 1)).min(rows),
        b'C' => col = (col + csi_param(&params, 0, 1)).min(cols),
        b'D' => col = col.saturating_sub(csi_param(&params, 0, 1)).max(1),
        b'G' => col = csi_param(&params, 0, 1).clamp(1, cols),
        b'd' => row = csi_param(&params, 0, 1).clamp(1, rows),
        b'H' | b'f' => {
            row = csi_param(&params, 0, 1).clamp(1, rows);
            col = csi_param(&params, 1, 1).clamp(1, cols);
        }
        _ => return,
    }

    set_cursor_position(shared, row, col);
}

fn csi_params(input: &[u8]) -> Vec<Option<u64>> {
    let mut params = Vec::new();
    let mut current = Vec::new();

    for &byte in input {
        match byte {
            b'0'..=b'9' => current.push(byte),
            b';' => {
                params.push(parse_csi_param(&current));
                current.clear();
            }
            b'?' | b'>' | b'=' | b' ' => {}
            _ => current.clear(),
        }
    }

    params.push(parse_csi_param(&current));
    params
}

fn parse_csi_param(input: &[u8]) -> Option<u64> {
    if input.is_empty() {
        None
    } else {
        std::str::from_utf8(input).ok()?.parse::<u64>().ok()
    }
}

fn csi_param(params: &[Option<u64>], index: usize, default: u64) -> u64 {
    params.get(index).copied().flatten().unwrap_or(default)
}

fn update_cursor_from_byte(shared: &Shared, byte: u8) {
    let (rows, cols) = terminal_size(shared);
    let (mut row, mut col) = cursor_position(shared);

    match byte {
        b'\r' => col = 1,
        b'\n' => row = (row + 1).min(rows),
        0x08 => col = col.saturating_sub(1).max(1),
        b'\t' => col = ((col + 7) / 8 * 8 + 1).min(cols),
        0x20..=0x7e | 0x80..=0xff => {
            if col >= cols {
                col = 1;
                row = (row + 1).min(rows);
            } else {
                col += 1;
            }
        }
        _ => {}
    }

    set_cursor_position(shared, row, col);
}

fn terminal_size(shared: &Shared) -> (u64, u64) {
    (
        shared.terminal_rows.load(Ordering::Relaxed).max(1),
        shared.terminal_cols.load(Ordering::Relaxed).max(1),
    )
}

fn cursor_position(shared: &Shared) -> (u64, u64) {
    let (rows, cols) = terminal_size(shared);
    (
        shared.cursor_row.load(Ordering::Relaxed).clamp(1, rows),
        shared.cursor_col.load(Ordering::Relaxed).clamp(1, cols),
    )
}

fn set_cursor_position(shared: &Shared, row: u64, col: u64) {
    let (rows, cols) = terminal_size(shared);
    shared
        .cursor_row
        .store(row.clamp(1, rows), Ordering::Relaxed);
    shared
        .cursor_col
        .store(col.clamp(1, cols), Ordering::Relaxed);
}

fn push_ring(shared: &Shared, chunk: &[u8]) {
    let mut ring = shared.ring.lock().expect("ring mutex poisoned");
    ring.extend(chunk.iter().copied());
    while ring.len() > RING_LIMIT {
        ring.pop_front();
    }
}

fn broadcast_output(shared: &Shared, chunk: Vec<u8>) {
    let mut clients = shared.clients.lock().expect("clients mutex poisoned");
    clients.retain(|client| client.tx.send(ServerMessage::Output(chunk.clone())).is_ok());
}

fn broadcast_exit(shared: &Shared, code: i32) {
    let mut clients = shared.clients.lock().expect("clients mutex poisoned");
    clients.retain(|client| client.tx.send(ServerMessage::Exit(code)).is_ok());
}

fn remove_client(shared: &Shared, client_id: u64) {
    let mut clients = shared.clients.lock().expect("clients mutex poisoned");
    clients.retain(|client| client.id != client_id);
}

fn client_count(shared: &Shared) -> usize {
    shared.clients.lock().expect("clients mutex poisoned").len()
}

fn handle_client(mut stream: TcpStream, shared: Arc<Shared>) {
    let client_id = shared.next_client_id.fetch_add(1, Ordering::Relaxed);
    debug_log(format!("client {client_id} handler start"));
    let (tx, rx) = mpsc::channel();

    {
        let mut clients = shared.clients.lock().expect("clients mutex poisoned");
        clients.push(ClientSink {
            id: client_id,
            tx: tx.clone(),
        });
        debug_log(format!(
            "client {client_id} attached clients={}",
            clients.len()
        ));
    }

    let mut initial = b"\x1b[2J\x1b[H".to_vec();
    {
        let ring = shared.ring.lock().expect("ring mutex poisoned");
        initial.extend(ring.iter().copied());
    }
    let _ = tx.send(ServerMessage::Output(initial));

    if let Some(code) = *shared.exit_code.lock().expect("exit code mutex poisoned") {
        let _ = tx.send(ServerMessage::Exit(code));
    }

    drop(tx);

    let mut write_stream = match stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => {
            remove_client(&shared, client_id);
            return;
        }
    };

    thread::spawn(move || {
        while let Ok(message) = rx.recv() {
            match message {
                ServerMessage::Output(chunk) => {
                    if write_frame(&mut write_stream, FRAME_STDOUT, &chunk).is_err() {
                        debug_log("client writer output send failed");
                        break;
                    }
                }
                ServerMessage::Exit(code) => {
                    let _ = write_frame(&mut write_stream, FRAME_EXIT, &code.to_le_bytes());
                    debug_log(format!("client writer sent exit={code}"));
                    break;
                }
            }
        }
        debug_log("client writer ended");
    });

    loop {
        let frame = match read_frame(&mut stream) {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                debug_log(format!("client {client_id} input eof"));
                break;
            }
            Err(err) => {
                debug_log(format!("client {client_id} input error {err}"));
                break;
            }
        };

        let (kind, payload) = frame;
        match kind {
            FRAME_STDIN => {
                let mut writer = shared.pty_writer.lock().expect("pty writer mutex poisoned");
                if let Some(writer) = writer.as_mut() {
                    if writer.write_all(&payload).is_err() {
                        break;
                    }
                    let _ = writer.flush();
                }
            }
            FRAME_RESIZE if payload.len() == 4 => {
                let cols = u16::from_le_bytes([payload[0], payload[1]]);
                let rows = u16::from_le_bytes([payload[2], payload[3]]);
                shared
                    .terminal_cols
                    .store(u64::from(cols).max(1), Ordering::Relaxed);
                shared
                    .terminal_rows
                    .store(u64::from(rows).max(1), Ordering::Relaxed);
                let (row, col) = cursor_position(&shared);
                set_cursor_position(&shared, row, col);
                let mut master = shared.pty_master.lock().expect("pty master mutex poisoned");
                if let Some(master) = master.as_mut() {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
            FRAME_DETACH => {
                debug_log(format!("client {client_id} explicit detach"));
                break;
            }
            _ => {}
        }
    }

    debug_log(format!("client {client_id} input loop ended"));
    remove_client(&shared, client_id);
    debug_log(format!(
        "client {client_id} detached clients={}",
        client_count(&shared)
    ));
}

fn run_client(mut stream: TcpStream) -> Result<i32> {
    debug_log("foreground client start");
    stream.set_nodelay(true).ok();
    let mut input_stream = stream.try_clone()?;
    send_resize(&mut input_stream)?;
    debug_log("foreground sent resize");

    let _raw = RawModeGuard::enable();
    let input_running = Arc::new(AtomicBool::new(true));
    let input_running_thread = Arc::clone(&input_running);
    thread::spawn(move || {
        if io::stdin().is_terminal() {
            run_event_input_loop(&input_running_thread, &mut input_stream);
        } else {
            run_byte_input_loop(&input_running_thread, &mut input_stream);
        }
    });

    let (read_tx, read_rx) = mpsc::channel();
    thread::spawn(move || {
        loop {
            match read_frame(&mut stream) {
                Ok(Some((kind, payload))) => {
                    if read_tx
                        .send(ClientReadMessage::Frame(kind, payload))
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = read_tx.send(ClientReadMessage::Closed);
                    break;
                }
                Err(err) => {
                    let _ = read_tx.send(ClientReadMessage::Error(err));
                    break;
                }
            }
        }
    });

    let mut stdout = io::stdout().lock();
    let mut output_filter = ForegroundOutputFilter::default();
    let mut exit_code = 0;
    loop {
        let message = if output_filter.has_pending_cursor_show() {
            match read_rx.recv_timeout(Duration::from_millis(25)) {
                Ok(message) => message,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    let filtered = output_filter.flush_pending();
                    stdout.write_all(&filtered)?;
                    stdout.flush()?;
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match read_rx.recv() {
                Ok(message) => message,
                Err(_) => break,
            }
        };

        match message {
            ClientReadMessage::Frame(kind, payload) => match kind {
                FRAME_STDOUT => {
                    debug_log(format!("foreground stdout frame bytes={}", payload.len()));
                    let filtered = output_filter.process(&payload);
                    stdout.write_all(&filtered)?;
                    stdout.flush()?;
                }
                FRAME_EXIT => {
                    debug_log("foreground exit frame");
                    if payload.len() == 4 {
                        exit_code =
                            i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
                    }
                    break;
                }
                _ => {}
            },
            ClientReadMessage::Closed => break,
            ClientReadMessage::Error(err) => return Err(err.into()),
        }
    }

    let filtered = output_filter.flush_pending();
    stdout.write_all(&filtered)?;
    stdout.flush()?;
    debug_log("foreground read loop ended");
    input_running.store(false, Ordering::Relaxed);
    Ok(exit_code)
}

enum ClientReadMessage {
    Frame(u8, Vec<u8>),
    Closed,
    Error(io::Error),
}

#[derive(Default)]
struct ForegroundOutputFilter {
    pending_cursor_show: bool,
}

impl ForegroundOutputFilter {
    fn process(&mut self, payload: &[u8]) -> Vec<u8> {
        let mut output = Vec::with_capacity(payload.len());
        let mut i = 0;

        while i < payload.len() {
            if payload[i..].starts_with(CURSOR_SHOW) {
                self.pending_cursor_show = true;
                i += CURSOR_SHOW.len();
                continue;
            }

            if payload[i..].starts_with(CURSOR_HIDE) {
                self.pending_cursor_show = false;
                output.extend_from_slice(CURSOR_HIDE);
                i += CURSOR_HIDE.len();
                continue;
            }

            if payload[i] == 0x1b
                && let Some(len) = ansi_escape_len(&payload[i..])
            {
                output.extend_from_slice(&payload[i..i + len]);
                i += len;
                continue;
            }

            self.flush_pending_cursor_show(&mut output);
            output.push(payload[i]);
            i += 1;
        }

        output
    }

    fn has_pending_cursor_show(&self) -> bool {
        self.pending_cursor_show
    }

    fn flush_pending(&mut self) -> Vec<u8> {
        let mut output = Vec::new();
        self.flush_pending_cursor_show(&mut output);
        output
    }

    fn flush_pending_cursor_show(&mut self, output: &mut Vec<u8>) {
        if self.pending_cursor_show {
            output.extend_from_slice(CURSOR_SHOW);
            self.pending_cursor_show = false;
        }
    }
}

fn wait_with_socket_alive(running: &AtomicBool, stream: &TcpStream) {
    while running.load(Ordering::Relaxed) {
        let _ = stream.peer_addr();
        thread::sleep(Duration::from_millis(100));
    }
}

fn run_event_input_loop(running: &AtomicBool, input_stream: &mut TcpStream) {
    while running.load(Ordering::Relaxed) {
        let event = match crossterm::event::read() {
            Ok(event) => event,
            Err(err) => {
                debug_log(format!("foreground event input error {err}"));
                wait_with_socket_alive(running, input_stream);
                break;
            }
        };

        match event {
            crossterm::event::Event::Key(key) => {
                let Some(encoded) = encode_key_event(key) else {
                    continue;
                };

                if encoded.contains(&0x1d) {
                    detach_input_stream(input_stream);
                    break;
                }

                if write_frame(input_stream, FRAME_STDIN, &encoded).is_err() {
                    break;
                }
            }
            crossterm::event::Event::Paste(text) => {
                if write_frame(input_stream, FRAME_STDIN, text.as_bytes()).is_err() {
                    break;
                }
            }
            crossterm::event::Event::Resize(cols, rows) => {
                let mut payload = Vec::with_capacity(4);
                payload.extend(cols.to_le_bytes());
                payload.extend(rows.to_le_bytes());
                if write_frame(input_stream, FRAME_RESIZE, &payload).is_err() {
                    break;
                }
            }
            _ => {}
        }
    }
}

fn run_byte_input_loop(running: &AtomicBool, input_stream: &mut TcpStream) {
    let mut stdin = io::stdin().lock();
    let mut buf = [0u8; 4096];
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => {
                debug_log("foreground stdin eof");
                wait_with_socket_alive(running, input_stream);
                break;
            }
            Ok(n) => n,
            Err(_) => {
                debug_log("foreground stdin error");
                wait_with_socket_alive(running, input_stream);
                break;
            }
        };

        if buf[..n].contains(&0x1d) {
            detach_input_stream(input_stream);
            break;
        }

        let filtered = normalize_terminal_input(&buf[..n]);
        if filtered.is_empty() {
            continue;
        }

        if write_frame(input_stream, FRAME_STDIN, &filtered).is_err() {
            break;
        }
    }
}

fn detach_input_stream(input_stream: &mut TcpStream) {
    debug_log("foreground detach requested");
    let _ = write_frame(input_stream, FRAME_DETACH, &[]);
    let _ = io::stdout().write_all(DETACH_HINT.as_bytes());
    let _ = io::stdout().flush();
    let _ = input_stream.shutdown(Shutdown::Both);
}

fn normalize_terminal_input(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;

    while i < input.len() {
        if input[i] == 0x1b && i + 2 < input.len() && input[i + 1] == b'[' {
            let mut j = i + 2;
            if j < input.len() && input[j] == b'?' {
                j += 1;
            }

            let digits_start = j;
            while j < input.len() && (input[j].is_ascii_digit() || input[j] == b';') {
                j += 1;
            }

            if j < input.len() && (input[j] == b'R' || input[j] == b'n') && j > digits_start {
                i = j + 1;
                continue;
            }
        }

        out.push(input[i]);
        i += 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_shared() -> Shared {
        Shared {
            clients: Mutex::new(Vec::new()),
            pty_writer: Mutex::new(None),
            pty_master: Mutex::new(None),
            ring: Mutex::new(VecDeque::with_capacity(RING_LIMIT)),
            exit_code: Mutex::new(None),
            oom_detected: AtomicBool::new(false),
            next_client_id: AtomicU64::new(1),
            terminal_cols: AtomicU64::new(120),
            terminal_rows: AtomicU64::new(30),
            cursor_col: AtomicU64::new(1),
            cursor_row: AtomicU64::new(1),
        }
    }

    fn process_test_output(shared: &Shared, chunk: &[u8]) -> Vec<u8> {
        TerminalOutputFilter::default().process(shared, chunk)
    }

    #[test]
    fn preserves_del_backspace_byte_input() {
        assert_eq!(normalize_terminal_input(b"abc\x7f"), b"abc\x7f");
    }

    #[test]
    fn broadcasts_output_to_all_clients() {
        let shared = test_shared();
        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();

        {
            let mut clients = shared.clients.lock().expect("clients mutex poisoned");
            clients.push(ClientSink {
                id: 1,
                tx: first_tx,
            });
            clients.push(ClientSink {
                id: 2,
                tx: second_tx,
            });
        }

        broadcast_output(&shared, b"hello".to_vec());

        match first_rx.recv_timeout(Duration::from_millis(100)).unwrap() {
            ServerMessage::Output(chunk) => assert_eq!(chunk, b"hello"),
            ServerMessage::Exit(code) => panic!("unexpected exit {code}"),
        }
        match second_rx.recv_timeout(Duration::from_millis(100)).unwrap() {
            ServerMessage::Output(chunk) => assert_eq!(chunk, b"hello"),
            ServerMessage::Exit(code) => panic!("unexpected exit {code}"),
        }
    }

    #[test]
    fn removing_one_client_keeps_others_attached() {
        let shared = test_shared();
        let (first_tx, first_rx) = mpsc::channel();
        let (second_tx, second_rx) = mpsc::channel();

        {
            let mut clients = shared.clients.lock().expect("clients mutex poisoned");
            clients.push(ClientSink {
                id: 1,
                tx: first_tx,
            });
            clients.push(ClientSink {
                id: 2,
                tx: second_tx,
            });
        }

        remove_client(&shared, 1);
        assert_eq!(client_count(&shared), 1);
        broadcast_exit(&shared, 42);

        assert!(first_rx.recv_timeout(Duration::from_millis(100)).is_err());
        match second_rx.recv_timeout(Duration::from_millis(100)).unwrap() {
            ServerMessage::Exit(code) => assert_eq!(code, 42),
            ServerMessage::Output(chunk) => panic!("unexpected output {chunk:?}"),
        }
    }

    #[test]
    fn encodes_backspace_as_del() {
        let event = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Backspace,
            crossterm::event::KeyModifiers::NONE,
        );

        assert_eq!(encode_key_event(event).as_deref(), Some(&b"\x7f"[..]));
    }

    #[test]
    fn encodes_shift_arrows() {
        let event = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Left,
            crossterm::event::KeyModifiers::SHIFT,
        );

        assert_eq!(encode_key_event(event).as_deref(), Some(&b"\x1b[1;2D"[..]));
    }

    #[test]
    fn encodes_altgr_brackets_as_printable() {
        let modifiers =
            crossterm::event::KeyModifiers::CONTROL | crossterm::event::KeyModifiers::ALT;

        let open = crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Char('['), modifiers);
        let close =
            crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Char(']'), modifiers);

        assert_eq!(encode_key_event(open).as_deref(), Some(&b"["[..]));
        assert_eq!(encode_key_event(close).as_deref(), Some(&b"]"[..]));
    }

    #[test]
    fn preserves_shifted_bang_input() {
        let shifted_digit = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('1'),
            crossterm::event::KeyModifiers::SHIFT,
        );
        let shifted_bang = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('!'),
            crossterm::event::KeyModifiers::SHIFT,
        );

        assert_eq!(encode_key_event(shifted_digit).as_deref(), Some(&b"!"[..]));
        assert_eq!(encode_key_event(shifted_bang).as_deref(), Some(&b"!"[..]));
    }

    #[test]
    fn strips_terminal_status_reports() {
        assert_eq!(normalize_terminal_input(b"a\x1b[1;2Rb\x1b[0nc"), b"abc");
    }

    #[test]
    fn strips_terminal_queries_from_visible_output() {
        let shared = test_shared();

        let output = process_test_output(&shared, b"a\x1b[6nb\x1b[5nc\x1b[?6nd");

        assert_eq!(output, b"abcd");
    }

    #[test]
    fn strips_split_terminal_queries_from_visible_output() {
        let shared = test_shared();
        let mut filter = TerminalOutputFilter::default();

        assert_eq!(filter.process(&shared, b"a\x1b["), b"a");
        assert_eq!(filter.process(&shared, b"6"), b"");
        assert_eq!(filter.process(&shared, b"nb"), b"b");
        assert_eq!(filter.process(&shared, b"\x1b[?"), b"");
        assert_eq!(filter.process(&shared, b"6"), b"");
        assert_eq!(filter.process(&shared, b"nc"), b"c");
    }

    #[test]
    fn tracks_cursor_position_from_terminal_output() {
        let shared = test_shared();

        let output = process_test_output(&shared, b"\x1b[10;4H[]");

        assert_eq!(output, b"\x1b[10;4H[]");
        assert_eq!(cursor_position(&shared), (10, 6));
    }

    #[test]
    fn drops_transient_cursor_show_before_redraw_hide() {
        let mut filter = ForegroundOutputFilter::default();

        let output = filter
            .process(b"\x1b[18;3H\x1b[?25h\x1b[?2026h\x1b[0 q\x1b[?2026l\x1b[?25l\x1b[14;1Htext");

        assert!(!output.windows(CURSOR_SHOW.len()).any(|w| w == CURSOR_SHOW));
        assert!(output.windows(CURSOR_HIDE.len()).any(|w| w == CURSOR_HIDE));
        assert!(output.ends_with(b"text"));
    }

    #[test]
    fn preserves_final_cursor_show() {
        let mut filter = ForegroundOutputFilter::default();

        let output = filter.process(b"\x1b[18;3H\x1b[?25h");

        assert!(!output.ends_with(CURSOR_SHOW));
        assert!(filter.flush_pending().ends_with(CURSOR_SHOW));
    }

    #[test]
    fn detects_oom_output() {
        assert!(looks_like_oom_output(
            b"FATAL ERROR: Reached heap limit Allocation failed"
        ));
        assert!(looks_like_oom_output(b"JavaScript heap out of memory"));
    }

    #[test]
    fn detects_oom_exit_codes() {
        assert!(is_oom_exit_code(134));
        assert!(is_oom_exit_code(137));
        assert!(is_oom_exit_code(-1073741801));
        assert!(!is_oom_exit_code(0));
    }

    #[test]
    fn detects_missing_path_like_codex_command() {
        let missing = OsStr::new(r"C:\this\path\does-not-exist\codex.exe");
        assert!(!command_exists(missing));
    }

    #[test]
    fn derives_distinct_session_for_targeted_resume() {
        let args = vec![
            "resume".to_string(),
            "--cd".to_string(),
            ".".to_string(),
            "019e3fd6-0c53-7302-8c84-3f9d47d0370b".to_string(),
        ];

        assert_eq!(
            derived_session_name(&args).as_deref(),
            Some("resume-019e3fd6-0c53-7302-8c84-3f9d47d0370b")
        );
    }

    #[test]
    fn plain_resume_uses_default_session() {
        assert_eq!(derived_session_name(&["resume".to_string()]), None);
    }

    #[test]
    fn resume_with_options_but_no_target_is_not_default() {
        let args = vec!["resume".to_string(), "--last".to_string()];
        assert!(
            derived_session_name(&args)
                .as_deref()
                .is_some_and(|value| value.starts_with("resume-"))
        );
    }
}

fn send_resize(stream: &mut TcpStream) -> io::Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 30));
    let mut payload = Vec::with_capacity(4);
    payload.extend(cols.to_le_bytes());
    payload.extend(rows.to_le_bytes());
    write_frame(stream, FRAME_RESIZE, &payload)
}

fn write_frame<W: Write>(writer: &mut W, kind: u8, payload: &[u8]) -> io::Result<()> {
    if payload.len() > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "frame payload too large",
        ));
    }

    writer.write_all(&[kind])?;
    writer.write_all(&(payload.len() as u32).to_le_bytes())?;
    writer.write_all(payload)?;
    writer.flush()?;
    Ok(())
}

fn read_frame<R: Read>(reader: &mut R) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut header = [0u8; 5];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) if err.kind() == io::ErrorKind::ConnectionReset => return Ok(None),
        Err(err) if err.kind() == io::ErrorKind::ConnectionAborted => return Ok(None),
        Err(err) => return Err(err),
    }

    let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > 8 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame payload too large",
        ));
    }

    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    Ok(Some((header[0], payload)))
}

fn resolve_codex_command(codex_args: &[String]) -> ResolvedCodexCommand {
    if let Some(command) = env::var_os("CODER_CODEX") {
        return ResolvedCodexCommand {
            program: command,
            args: codex_args.iter().map(OsString::from).collect(),
        };
    }

    let node = PathBuf::from(r"C:\Programs\nodejs\node.exe");
    let codex_js = PathBuf::from(r"C:\Programs\nodejs\node_modules\@openai\codex\bin\codex.js");
    if node.exists() && codex_js.exists() {
        let mut args = vec![codex_js.into_os_string()];
        args.extend(codex_args.iter().map(OsString::from));
        return ResolvedCodexCommand {
            program: node.into_os_string(),
            args,
        };
    }

    ResolvedCodexCommand {
        program: OsString::from("codex"),
        args: codex_args.iter().map(OsString::from).collect(),
    }
}

fn ensure_codex_available(codex_args: &[String]) -> Result<ResolvedCodexCommand> {
    let resolved = resolve_codex_command(codex_args);
    if command_exists(&resolved.program) {
        Ok(resolved)
    } else {
        Err(format!(
            "Codex executable was not found: {}. Install the Codex CLI so `codex` is on PATH, or set CODER_CODEX to the Codex executable path.",
            display_command(&resolved.program)
        )
        .into())
    }
}

fn command_exists(program: &OsStr) -> bool {
    let path = Path::new(program);
    if is_path_like(path) {
        return path.is_file();
    }

    let Some(path_var) = env::var_os("PATH") else {
        return false;
    };

    for dir in env::split_paths(&path_var) {
        let candidate = dir.join(path);
        if candidate.is_file() {
            return true;
        }

        #[cfg(windows)]
        {
            if candidate.extension().is_none() {
                for extension in windows_path_extensions() {
                    let mut with_extension = candidate.as_os_str().to_os_string();
                    with_extension.push(&extension);
                    if PathBuf::from(with_extension).is_file() {
                        return true;
                    }
                }
            }
        }
    }

    false
}

fn is_path_like(path: &Path) -> bool {
    path.is_absolute() || path.components().count() > 1
}

#[cfg(windows)]
fn windows_path_extensions() -> Vec<OsString> {
    env::var_os("PATHEXT")
        .unwrap_or_else(|| OsString::from(".COM;.EXE;.BAT;.CMD"))
        .to_string_lossy()
        .split(';')
        .filter(|extension| !extension.is_empty())
        .map(OsString::from)
        .collect()
}

fn display_command(command: &OsStr) -> String {
    Path::new(command).display().to_string()
}

fn run_codex_direct(codex_args: &[String]) -> Result<i32> {
    let resolved = ensure_codex_available(codex_args)?;
    let status = Command::new(&resolved.program)
        .args(&resolved.args)
        .status()?;
    Ok(status.code().unwrap_or(1))
}

fn debug_log(message: impl AsRef<str>) {
    let Some(path) = env::var_os("CODER_DEBUG_LOG") else {
        return;
    };

    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(
            file,
            "{} pid={} {}",
            now_ms(),
            process::id(),
            message.as_ref()
        );
    }
}
