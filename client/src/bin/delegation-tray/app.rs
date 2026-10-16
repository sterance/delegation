//! The tray process: a winit event loop on the main thread owning the tray
//! icon and menu, and the session controller on a tokio runtime in its own
//! thread. Menu clicks, session status and form results all arrive as
//! winit user events through an `EventLoopProxy`.

use crate::form::FormResult;
use crate::menu::{self, MenuAction, TrayMenu};
use crate::store::Store;
use delegation_client::identity::Identity;
use delegation_client::session::{self, ClientInfo, ServerIdentity, SessionCmd, SessionStatus, Timings};
use delegation_client::{hardware, paths};
use log::{error, info, warn};
use protocol::config::Config;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tray_icon::menu::MenuEvent;
use tray_icon::{TrayIcon, TrayIconBuilder};
use winit::application::ApplicationHandler;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
use winit::window::WindowId;

pub enum UserEvent {
    Menu(MenuEvent),
    Session(String, SessionStatus),
    /// `None` when the form was closed without a result.
    Form(Option<FormResult>),
}

const UNKNOWN_CLIENT_NOTE: &str =
    "This server no longer recognizes this machine. Enter a new pairing code to pair it again.";

pub fn run() {
    if already_running() {
        return;
    }
    let _ = delegation_client::log::init_file(&paths::log_dir().join("tray.log"));
    info!(target: "tray", "starting, version {}", env!("CARGO_PKG_VERSION"));

    let config = Config::load(&[paths::config_dir().join("config.env")]);
    let timings = Timings::from_config(&config);
    let machine = hardware::spawn_collect();

    let event_loop = match EventLoop::<UserEvent>::with_user_event().build() {
        Ok(event_loop) => event_loop,
        Err(e) => {
            error!(target: "tray", "couldn't create event loop: {e}");
            return;
        }
    };
    let proxy = event_loop.create_proxy();

    // EventLoopProxy is Send but not Sync, and both handlers must be Sync.
    let menu_proxy = Mutex::new(proxy.clone());
    MenuEvent::set_event_handler(Some(move |event| {
        if let Ok(proxy) = menu_proxy.lock() {
            let _ = proxy.send_event(UserEvent::Menu(event));
        }
    }));

    let (cmd_tx, cmd_rx) = mpsc::channel(8);
    let status_proxy = Mutex::new(proxy.clone());
    let session_thread = std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                error!(target: "tray", "couldn't start tokio runtime: {e}");
                return;
            }
        };
        let status = move |url: &str, status: SessionStatus| {
            if let Ok(proxy) = status_proxy.lock() {
                let _ = proxy.send_event(UserEvent::Session(url.to_string(), status));
            }
        };
        runtime.block_on(session::controller(cmd_rx, status, timings, ClientInfo { machine }));
    });

    let store_path = Store::default_path();
    let mut app = App {
        store: Store::load(&store_path),
        store_path,
        connected: false,
        tray: None,
        menu: None,
        cmd_tx,
        proxy,
        form_open: false,
        session_thread: Some(session_thread),
    };
    if let Err(e) = event_loop.run_app(&mut app) {
        error!(target: "tray", "event loop failed: {e}");
    }
    info!(target: "tray", "exited");
}

struct App {
    store: Store,
    store_path: PathBuf,
    /// Live status of the active server's connection.
    connected: bool,
    tray: Option<TrayIcon>,
    menu: Option<TrayMenu>,
    cmd_tx: mpsc::Sender<SessionCmd>,
    proxy: EventLoopProxy<UserEvent>,
    form_open: bool,
    session_thread: Option<std::thread::JoinHandle<()>>,
}

impl ApplicationHandler<UserEvent> for App {
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        // The tray icon must be created once the event loop is running.
        if cause == StartCause::Init {
            self.start();
        }
    }

    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, _event: WindowEvent) {}

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Menu(event) => self.on_menu(event_loop, event),
            UserEvent::Session(url, status) => self.on_status(&url, status),
            UserEvent::Form(result) => self.on_form(result),
        }
    }
}

impl App {
    fn start(&mut self) {
        self.rebuild_menu();
        if self.store.servers.is_empty() {
            self.open_form(None, None);
            return;
        }
        if self.store.manually_disconnected {
            info!(target: "tray", "active server was disconnected by hand, not connecting");
            return;
        }
        if let Some((url, identity)) = self.active_identity() {
            self.connect(url, identity);
        }
    }

    fn on_menu(&mut self, event_loop: &ActiveEventLoop, event: MenuEvent) {
        let Some(action) = self.menu.as_ref().and_then(|m| m.action(&event.id)) else {
            return;
        };
        match action {
            MenuAction::Server(i) if i == self.store.active => self.toggle_active(),
            MenuAction::Server(i) => self.switch_to(i),
            MenuAction::AddServer => self.open_form(None, None),
            MenuAction::Quit => self.quit(event_loop),
        }
    }

    fn toggle_active(&mut self) {
        if self.store.manually_disconnected {
            self.store.manually_disconnected = false;
            self.save();
            match self.active_identity() {
                Some((url, identity)) => self.connect(url, identity),
                None => self.reopen_pairing(),
            }
        } else {
            self.store.manually_disconnected = true;
            self.save();
            self.send(SessionCmd::Disconnect);
            self.connected = false;
            self.refresh();
        }
    }

    fn switch_to(&mut self, index: usize) {
        self.store.active = index;
        self.store.manually_disconnected = false;
        self.save();
        match self.active_identity() {
            Some((url, identity)) => self.connect(url, identity),
            None => {
                // Self-healed earlier: no identity until it's paired again.
                self.send(SessionCmd::Disconnect);
                self.connected = false;
                self.refresh();
                self.reopen_pairing();
            }
        }
    }

    fn on_status(&mut self, url: &str, status: SessionStatus) {
        // Ignore stragglers from a session we've already switched away from.
        if self.store.active_server().map(|s| s.url.as_str()) != Some(url) {
            return;
        }
        match status {
            SessionStatus::Connected => self.connected = true,
            SessionStatus::UnknownClient => {
                warn!(target: "tray", "{url} no longer recognizes this machine, clearing its identity");
                self.connected = false;
                self.store.forget_identity(self.store.active);
                self.save();
                self.reopen_pairing();
            }
            SessionStatus::Connecting | SessionStatus::Disconnected { .. } => self.connected = false,
        }
        self.refresh();
    }

    fn on_form(&mut self, result: Option<FormResult>) {
        self.form_open = false;
        match result {
            None => {}
            Some(FormResult::Existing(url)) => match self.store.find(&url) {
                Some(i) if i == self.store.active && self.connected => {}
                Some(i) => self.switch_to(i),
                None => warn!(target: "tray", "form reported {url} as known, but it isn't in the store"),
            },
            Some(FormResult::Enrolled {
                url,
                client_id,
                signing_key_b64,
            }) => {
                let identity = match Identity::from_b64(client_id, &signing_key_b64) {
                    Ok(identity) => identity,
                    Err(e) => {
                        error!(target: "tray", "form returned an unusable identity: {e:#}");
                        return;
                    }
                };
                let count = self.store.servers.len();
                self.store.upsert(&url, &identity);
                self.save();
                info!(target: "tray", "paired with {url}");
                if self.store.servers.len() != count {
                    self.rebuild_menu();
                }
                self.connect(url, identity);
            }
        }
    }

    fn quit(&mut self, event_loop: &ActiveEventLoop) {
        info!(target: "tray", "quitting");
        self.send(SessionCmd::Shutdown);
        // Give the session a moment to send its close frame.
        if let Some(thread) = self.session_thread.take() {
            let deadline = Instant::now() + Duration::from_secs(3);
            while !thread.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        self.tray = None;
        event_loop.exit();
    }

    // ---- helpers ----

    fn active_identity(&self) -> Option<(String, Identity)> {
        let server = self.store.active_server()?;
        Some((server.url.clone(), server.identity()?))
    }

    fn connect(&mut self, url: String, identity: Identity) {
        self.connected = false;
        self.refresh();
        self.send(SessionCmd::Connect(ServerIdentity { url, identity }));
    }

    fn send(&self, cmd: SessionCmd) {
        if self.cmd_tx.try_send(cmd).is_err() {
            error!(target: "tray", "session controller isn't accepting commands");
        }
    }

    fn save(&self) {
        if let Err(e) = self.store.save(&self.store_path) {
            error!(target: "tray", "couldn't save {}: {e:#}", self.store_path.display());
        }
    }

    fn reopen_pairing(&mut self) {
        let url = self.store.active_server().map(|s| s.url.clone());
        self.open_form(url, Some(UNKNOWN_CLIENT_NOTE.to_string()));
    }

    fn rebuild_menu(&mut self) {
        let menu = TrayMenu::build(&self.store, self.connected);
        match &self.tray {
            Some(tray) => tray.set_menu(Some(Box::new(menu.menu.clone()))),
            None => {
                let built = TrayIconBuilder::new()
                    .with_icon(menu::tray_icon())
                    .with_menu(Box::new(menu.menu.clone()))
                    .with_menu_on_left_click(true)
                    .with_tooltip("Delegation")
                    .build();
                match built {
                    Ok(tray) => self.tray = Some(tray),
                    Err(e) => error!(target: "tray", "couldn't create tray icon: {e}"),
                }
            }
        }
        self.menu = Some(menu);
        self.refresh();
    }

    /// Updates the active row's dot and the tooltip in place.
    fn refresh(&self) {
        if let Some(menu) = &self.menu {
            menu.set_status(self.store.active, self.connected);
        }
        if let Some(tray) = &self.tray {
            let tooltip = match (self.store.active_server(), self.connected) {
                (Some(server), true) => format!("Delegation: connected to {}", server.url),
                (Some(_), false) => "Delegation: disconnected".to_string(),
                (None, _) => "Delegation: no server".to_string(),
            };
            let _ = tray.set_tooltip(Some(tooltip));
        }
    }

    /// Starts the form as a child process and reports its result back as a
    /// user event. Only one can be open at a time.
    fn open_form(&mut self, url: Option<String>, note: Option<String>) {
        if self.form_open {
            return;
        }
        let exe = match std::env::current_exe() {
            Ok(exe) => exe,
            Err(e) => {
                error!(target: "tray", "can't find own executable to open the form: {e}");
                return;
            }
        };
        let mut cmd = Command::new(exe);
        cmd.arg("--add-server-form")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if let Some(url) = &url {
            cmd.args(["--url", url]);
        }
        if let Some(note) = &note {
            cmd.args(["--note", note]);
        }
        self.form_open = true;
        let proxy = self.proxy.clone();
        std::thread::spawn(move || {
            let result = cmd.output().ok().and_then(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .rev()
                    .find_map(|line| serde_json::from_str::<FormResult>(line.trim()).ok())
            });
            let _ = proxy.send_event(UserEvent::Form(result));
        });
    }
}

/// True if another tray is already running for this user. The mutex is
/// held for the rest of the process's life (the handle is never closed).
fn already_running() -> bool {
    use windows::core::w;
    use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
    use windows::Win32::System::Threading::CreateMutexW;
    // SAFETY: plain Win32 calls; GetLastError is read straight after the
    // call that set it.
    unsafe {
        match CreateMutexW(None, false, w!("Local\\DelegationTray")) {
            Ok(_handle) => GetLastError() == ERROR_ALREADY_EXISTS,
            Err(_) => false,
        }
    }
}
