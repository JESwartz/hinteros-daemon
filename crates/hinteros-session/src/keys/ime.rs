//! Client of gamescope's private `gamescope_input_method` protocol (v3, used by Steam's
//! keyboard; `protocol/gamescope-input-method.xml`, copied from gamescope's source in nixpkgs):
//! bind the manager, `create_input_method(seat)`, wait for `done(serial)`, then
//! `set_string`/`set_action` + `commit(serial)`. gamescope makes a temporary keymap for each
//! commit and presses real keys through its seat, so X11 and Wayland clients both get the text.
//! It is private: pin gamescope and keep the VM test (spike 5) when updating it.

use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

use super::ImeAction;
use super::worker::ImeSink;

/// Generated from the protocol XML.
#[allow(missing_docs, clippy::all)]
pub mod protocol {
    use wayland_client;
    use wayland_client::protocol::*;

    pub mod __interfaces {
        use wayland_client::protocol::__interfaces::*;
        wayland_scanner::generate_interfaces!("protocol/gamescope-input-method.xml");
    }
    use self::__interfaces::*;

    wayland_scanner::generate_client_code!("protocol/gamescope-input-method.xml");
}

use protocol::gamescope_input_method::{self, GamescopeInputMethod};
use protocol::gamescope_input_method_manager::GamescopeInputMethodManager;

/// Where a Wayland display's socket is: an absolute path, or a name in `$XDG_RUNTIME_DIR`.
pub fn socket_path(display: &str) -> Result<PathBuf> {
    let path = PathBuf::from(display);
    if path.is_absolute() {
        return Ok(path);
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is not set")?;
    Ok(PathBuf::from(dir).join(display))
}

/// How long gamescope has to answer.
pub const ANSWER_TIMEOUT: Duration = Duration::from_secs(3);

/// Run `f`, blocking Wayland calls on `stream`, for at most `timeout`; then the socket is shut
/// down, so they fail. A read timeout on the socket does not help: wayland-client waits in
/// poll() with no timeout.
pub fn with_timeout<T>(
    stream: &UnixStream,
    timeout: Duration,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let fired = AtomicBool::new(false);
    let (done, wait) = mpsc::channel::<()>();
    let out = thread::scope(|s| {
        let fired = &fired;
        s.spawn(move || {
            if wait.recv_timeout(timeout) == Err(mpsc::RecvTimeoutError::Timeout) {
                fired.store(true, Ordering::SeqCst);
                let _ = stream.shutdown(Shutdown::Both);
            }
        });
        let out = f();
        drop(done);
        out
    });
    if fired.load(Ordering::SeqCst) {
        bail!("gamescope did not answer in {timeout:?}");
    }
    out
}

#[derive(Default)]
struct State {
    serial: u32,
    unavailable: bool,
}

pub struct GamescopeIme {
    queue: EventQueue<State>,
    state: State,
    im: GamescopeInputMethod,
    /// The connection's socket, to shut down when gamescope stops answering.
    stream: UnixStream,
    timeout: Duration,
}

impl GamescopeIme {
    /// Connect to gamescope's Wayland display (e.g. `gamescope-0`).
    pub fn connect(display: &str) -> Result<Self> {
        let path = socket_path(display)?;
        let stream = UnixStream::connect(&path)
            .with_context(|| format!("connecting to Wayland display {}", path.display()))?;
        Self::from_stream(stream)
    }

    pub fn from_stream(stream: UnixStream) -> Result<Self> {
        Self::from_stream_timeout(stream, ANSWER_TIMEOUT)
    }

    /// Like [`Self::from_stream`]; every wait for gamescope fails after `timeout` (the
    /// connection is then broken, and the caller connects again).
    fn from_stream_timeout(stream: UnixStream, timeout: Duration) -> Result<Self> {
        let watched = stream.try_clone()?;
        let (queue, state, im) = with_timeout(&watched, timeout, || {
            let conn = Connection::from_socket(stream)?;
            let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
            let qh = queue.handle();
            let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=1, ()).context("no wl_seat")?;
            let manager: GamescopeInputMethodManager = globals
                .bind(&qh, 1..=3, ())
                .context("no gamescope_input_method_manager (is this gamescope?)")?;
            let im = manager.create_input_method(&seat, &qh, ());
            let mut state = State::default();
            queue.roundtrip(&mut state)?;
            Ok((queue, state, im))
        })?;
        if state.unavailable {
            bail!("gamescope's input method is taken (Steam's keyboard?)");
        }
        Ok(Self {
            queue,
            state,
            im,
            stream: watched,
            timeout,
        })
    }

    fn commit(&mut self) -> Result<()> {
        self.im.commit(self.state.serial);
        let Self {
            queue,
            state,
            stream,
            timeout,
            ..
        } = self;
        with_timeout(stream, *timeout, || Ok(queue.roundtrip(state)?))?;
        if self.state.unavailable {
            bail!("gamescope's input method became unavailable");
        }
        Ok(())
    }
}

impl ImeSink for GamescopeIme {
    fn text(&mut self, text: &str) -> Result<()> {
        self.im.set_string(text.into());
        self.commit()
    }

    fn action(&mut self, action: ImeAction) -> Result<()> {
        use gamescope_input_method::Action;
        self.im.set_action(match action {
            ImeAction::Submit => Action::Submit,
            ImeAction::DeleteLeft => Action::DeleteLeft,
            ImeAction::DeleteRight => Action::DeleteRight,
            ImeAction::MoveLeft => Action::MoveLeft,
            ImeAction::MoveRight => Action::MoveRight,
            ImeAction::MoveUp => Action::MoveUp,
            ImeAction::MoveDown => Action::MoveDown,
        });
        self.commit()
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<GamescopeInputMethodManager, ()> for State {
    fn event(
        _: &mut Self,
        _: &GamescopeInputMethodManager,
        _: protocol::gamescope_input_method_manager::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<GamescopeInputMethod, ()> for State {
    fn event(
        state: &mut Self,
        _: &GamescopeInputMethod,
        event: gamescope_input_method::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            gamescope_input_method::Event::Done { serial } => state.serial = serial,
            gamescope_input_method::Event::Unavailable => state.unavailable = true,
        }
    }
}

#[cfg(test)]
mod tests {
    //! Against a fake compositor (wayland-server) that offers wl_seat and the input method
    //! manager and records what it is asked to do.

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use wayland_server::backend::{ClientData, ClientId, DisconnectReason};
    use wayland_server::protocol::wl_seat::WlSeat;
    use wayland_server::{Client, DataInit, Dispatch, Display, DisplayHandle, GlobalDispatch, New};

    use super::*;

    #[allow(missing_docs, clippy::all)]
    mod server_protocol {
        use wayland_server;
        use wayland_server::protocol::*;

        pub mod __interfaces {
            use wayland_server::protocol::__interfaces::*;
            wayland_scanner::generate_interfaces!("protocol/gamescope-input-method.xml");
        }
        use self::__interfaces::*;

        wayland_scanner::generate_server_code!("protocol/gamescope-input-method.xml");
    }

    use server_protocol::gamescope_input_method::{self as sim, GamescopeInputMethod as ServerIm};
    use server_protocol::gamescope_input_method_manager::{
        self as smgr, GamescopeInputMethodManager as ServerManager,
    };

    #[derive(Default)]
    struct Fake {
        log: Arc<Mutex<Vec<String>>>,
        /// Answer create_input_method with `unavailable` (the seat already has one).
        taken: bool,
        pending: Vec<String>,
    }

    struct NoData;
    impl ClientData for NoData {
        fn initialized(&self, _: ClientId) {}
        fn disconnected(&self, _: ClientId, _: DisconnectReason) {}
    }

    impl GlobalDispatch<WlSeat, ()> for Fake {
        fn bind(
            _: &mut Self,
            _: &DisplayHandle,
            _: &Client,
            resource: New<WlSeat>,
            _: &(),
            init: &mut DataInit<'_, Self>,
        ) {
            init.init(resource, ());
        }
    }

    impl Dispatch<WlSeat, ()> for Fake {
        fn request(
            _: &mut Self,
            _: &Client,
            _: &WlSeat,
            _: wayland_server::protocol::wl_seat::Request,
            _: &(),
            _: &DisplayHandle,
            _: &mut DataInit<'_, Self>,
        ) {
        }
    }

    impl GlobalDispatch<ServerManager, ()> for Fake {
        fn bind(
            _: &mut Self,
            _: &DisplayHandle,
            _: &Client,
            resource: New<ServerManager>,
            _: &(),
            init: &mut DataInit<'_, Self>,
        ) {
            init.init(resource, ());
        }
    }

    impl Dispatch<ServerManager, ()> for Fake {
        fn request(
            state: &mut Self,
            _: &Client,
            _: &ServerManager,
            request: smgr::Request,
            _: &(),
            _: &DisplayHandle,
            init: &mut DataInit<'_, Self>,
        ) {
            if let smgr::Request::CreateInputMethod { input_method, .. } = request {
                let im = init.init(input_method, ());
                if state.taken {
                    im.unavailable();
                } else {
                    im.done(7);
                }
            }
        }
    }

    impl Dispatch<ServerIm, ()> for Fake {
        fn request(
            state: &mut Self,
            _: &Client,
            im: &ServerIm,
            request: sim::Request,
            _: &(),
            _: &DisplayHandle,
            _: &mut DataInit<'_, Self>,
        ) {
            match request {
                sim::Request::SetString { text } => state.pending.push(format!("string {text}")),
                sim::Request::SetAction { action } => {
                    let n = match action {
                        wayland_server::WEnum::Value(a) => a as u32,
                        wayland_server::WEnum::Unknown(n) => n,
                    };
                    state.pending.push(format!("action {n}"));
                }
                sim::Request::Commit { serial } => {
                    let mut log = state.log.lock().unwrap();
                    for p in state.pending.drain(..) {
                        log.push(format!("{p} @{serial}"));
                    }
                    im.done(serial + 1);
                }
                _ => {}
            }
        }
    }

    /// Run a fake compositor on one end of a socket pair until the guard is dropped.
    struct Server {
        stop: Arc<AtomicBool>,
        /// Stop answering (gamescope hangs with the socket open).
        hang: Arc<AtomicBool>,
        thread: Option<thread::JoinHandle<()>>,
        log: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        fn start(taken: bool) -> (Self, UnixStream) {
            let (ours, theirs) = UnixStream::pair().unwrap();
            let stop = Arc::new(AtomicBool::new(false));
            let hang = Arc::new(AtomicBool::new(false));
            let log = Arc::new(Mutex::new(Vec::new()));
            let (s, h, l) = (stop.clone(), hang.clone(), log.clone());
            let thread = thread::spawn(move || {
                let mut display = Display::<Fake>::new().unwrap();
                let dh = display.handle();
                dh.create_global::<Fake, WlSeat, ()>(1, ());
                dh.create_global::<Fake, ServerManager, ()>(3, ());
                display
                    .handle()
                    .insert_client(theirs, Arc::new(NoData))
                    .unwrap();
                let mut state = Fake {
                    log: l,
                    taken,
                    ..Fake::default()
                };
                while !s.load(Ordering::SeqCst) {
                    if h.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    let _ = display.dispatch_clients(&mut state);
                    let _ = display.flush_clients();
                    thread::sleep(Duration::from_millis(1));
                }
            });
            (
                Self {
                    stop,
                    hang,
                    thread: Some(thread),
                    log,
                },
                ours,
            )
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }

    #[test]
    fn commits_strings_and_actions_with_the_latest_serial() {
        let (server, stream) = Server::start(false);
        let mut ime = GamescopeIme::from_stream(stream).unwrap();
        ime.text("é").unwrap();
        ime.text("日").unwrap();
        ime.action(ImeAction::DeleteLeft).unwrap();
        ime.action(ImeAction::Submit).unwrap();
        assert_eq!(
            *server.log.lock().unwrap(),
            ["string é @7", "string 日 @8", "action 2 @9", "action 1 @10"]
        );
    }

    #[test]
    fn reports_a_taken_input_method() {
        let (_server, stream) = Server::start(true);
        let err = GamescopeIme::from_stream(stream).err().unwrap();
        assert!(format!("{err:#}").contains("taken"), "{err:#}");
    }

    #[test]
    fn gives_up_when_gamescope_does_not_answer() {
        let ms = Duration::from_millis;
        // Nobody reads the other end.
        let (ours, _theirs) = UnixStream::pair().unwrap();
        let err = GamescopeIme::from_stream_timeout(ours, ms(100))
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("did not answer"), "{err:#}");
        // It answers at first, then hangs with the socket open.
        let (server, stream) = Server::start(false);
        let mut ime = GamescopeIme::from_stream_timeout(stream, ms(100)).unwrap();
        ime.text("a").unwrap();
        server.hang.store(true, Ordering::SeqCst);
        let err = ime.text("b").unwrap_err();
        assert!(format!("{err:#}").contains("did not answer"), "{err:#}");
    }

    #[test]
    fn socket_paths() {
        assert_eq!(
            socket_path("/run/x/wayland-1").unwrap(),
            PathBuf::from("/run/x/wayland-1")
        );
    }
}
