//! Runs the session against a fake bridge on 127.0.0.1 UDP sockets and asserts
//! the packets on the wire. The fake follows `vector-bridge/src/vector_bridge/service.py`:
//! one loop that reads the audio socket before the control socket, the peer
//! slot goes to the last address that sent `hello` or a TX packet, a
//! `monitor` from the peer is refused with `is_peer`, a third monitor gets
//! `monitors_full`, a `hello` from a monitor promotes it, and the peer and
//! monitors expire after 5 s of silence. `bye` exists only when advertised.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use vbcore::audio::NullAdapter;
use vbcore::pin::TokenStore;
use vbcore::protocol::{Direction, Packet, SAMPLES_PER_PACKET};
use vbcore::rigrelay::RelayState;
use vbcore::runner::{Command, CommandReply, Config, ListingState, Runner};
use vbcore::session::{Link, Mode};

const PEER_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_MONITORS: usize = 2;

#[derive(Default)]
struct Seen {
    /// (cmd, source port, time) in arrival order.
    control: Vec<(String, u16, Instant)>,
    /// The `client` string of every hello and monitor, in arrival order.
    clients: Vec<Option<String>>,
    /// Error replies sent: (error, destination port).
    errors: Vec<(String, u16)>,
    /// (packet, source port, time).
    tx: Vec<(Packet, u16, Instant)>,
    rx_sent_to: HashMap<u16, u64>,
    /// Every change of the peer slot: (time, port).
    peer_history: Vec<(Instant, Option<u16>)>,
}

#[derive(Default)]
struct State {
    peer: Option<SocketAddr>,
    peer_seen: Option<Instant>,
    monitors: HashMap<SocketAddr, Instant>,
    /// Sequence and clock restart from zero on a Pi restart.
    epoch: Option<Instant>,
    seq: u32,
}

impl State {
    fn current_peer(&mut self, seen: &mut Seen) -> Option<SocketAddr> {
        if let (Some(_), Some(at)) = (self.peer, self.peer_seen) {
            if at.elapsed() > PEER_TIMEOUT {
                self.peer = None;
                seen.peer_history.push((Instant::now(), None));
            }
        }
        self.peer
    }

    fn register_peer(&mut self, addr: SocketAddr, seen: &mut Seen) {
        if self.peer != Some(addr) {
            seen.peer_history.push((Instant::now(), Some(addr.port())));
        }
        self.peer = Some(addr);
        self.peer_seen = Some(Instant::now());
        self.monitors.remove(&addr); // a hello from a monitor makes it the peer
    }

    fn touch_peer(&mut self, addr: SocketAddr, seen: &mut Seen) {
        if self.peer == Some(addr) {
            self.peer_seen = Some(Instant::now());
        } else {
            self.register_peer(addr, seen);
        }
    }

    fn register_monitor(&mut self, addr: SocketAddr, seen: &mut Seen) -> Option<&'static str> {
        if self.current_peer(seen) == Some(addr) {
            return Some("is_peer");
        }
        self.expire_monitors();
        if !self.monitors.contains_key(&addr) && self.monitors.len() >= MAX_MONITORS {
            return Some("monitors_full");
        }
        self.monitors.insert(addr, Instant::now());
        None
    }

    fn expire_monitors(&mut self) {
        self.monitors
            .retain(|_, seen| seen.elapsed() <= PEER_TIMEOUT);
    }
}

struct FakeBridge {
    control_port: u16,
    audio_port: u16,
    seen: Arc<Mutex<Seen>>,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    restart: Arc<AtomicBool>,
    /// While set, the service reads nothing: datagrams queue in the sockets.
    pause: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeBridge {
    fn start(advertise_bye: bool) -> FakeBridge {
        let control = UdpSocket::bind("127.0.0.1:0").unwrap();
        let audio = UdpSocket::bind("127.0.0.1:0").unwrap();
        let control_port = control.local_addr().unwrap().port();
        let audio_port = audio.local_addr().unwrap().port();
        control.set_nonblocking(true).unwrap();
        audio.set_nonblocking(true).unwrap();
        let seen = Arc::new(Mutex::new(Seen::default()));
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let restart = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(false));

        let t_seen = seen.clone();
        let t_state = state.clone();
        let t_stop = stop.clone();
        let t_restart = restart.clone();
        let t_pause = pause.clone();
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let mut next = Instant::now();
            // One more pass after stop so a final bye is read.
            let mut stopping = false;
            while !stopping {
                stopping = t_stop.load(Ordering::SeqCst);
                if t_restart.swap(false, Ordering::SeqCst) {
                    // A service restart: the slot, the monitors, the clock and the sequence go.
                    let mut st = t_state.lock().unwrap();
                    let had_peer = st.peer.is_some();
                    *st = State::default();
                    if had_peer {
                        t_seen
                            .lock()
                            .unwrap()
                            .peer_history
                            .push((Instant::now(), None));
                    }
                }
                let paused = t_pause.load(Ordering::SeqCst);
                let mut read_any = false;
                // One datagram per socket per pass, audio socket first, like
                // service.py's select loop (service.py:493-507).
                let audio_in = if paused {
                    None
                } else {
                    audio.recv_from(&mut buf).ok()
                };
                if let Some((n, from)) = audio_in {
                    read_any = true;
                    if let Ok(p) = Packet::decode(&buf[..n]) {
                        if p.direction == Direction::Tx {
                            let mut st = t_state.lock().unwrap();
                            let mut seen = t_seen.lock().unwrap();
                            seen.tx.push((p.clone(), from.port(), Instant::now()));
                            st.expire_monitors();
                            // A monitor's TX is dropped and never registers.
                            if !st.monitors.contains_key(&from) {
                                st.touch_peer(from, &mut seen);
                            }
                        }
                    }
                }
                let control_in = if paused {
                    None
                } else {
                    control.recv_from(&mut buf).ok()
                };
                if let Some((n, from)) = control_in {
                    read_any = true;
                    let v: serde_json::Value =
                        serde_json::from_slice(&buf[..n]).unwrap_or(serde_json::Value::Null);
                    let cmd = v["cmd"].as_str().unwrap_or("").to_string();
                    let mut st = t_state.lock().unwrap();
                    let mut seen = t_seen.lock().unwrap();
                    seen.control
                        .push((cmd.clone(), from.port(), Instant::now()));
                    if cmd == "hello" || cmd == "monitor" {
                        seen.clients.push(v["client"].as_str().map(str::to_string));
                    }
                    let mut hello = serde_json::json!({"ok":true,"device":"VectorBridge","protocol":1,"sample_rate":48000,"channels":1,"audio_port":audio_port});
                    if advertise_bye {
                        hello["features"] = serde_json::json!(["monitor", "bye"]);
                    }
                    let reply = match cmd.as_str() {
                        "hello" => {
                            st.register_peer(from, &mut seen);
                            hello
                        }
                        "monitor" => match st.register_monitor(from, &mut seen) {
                            Some(err) => serde_json::json!({"ok":false,"error":err}),
                            None => {
                                hello["role"] = "monitor".into();
                                hello
                            }
                        },
                        "status" => {
                            let p = st.current_peer(&mut seen).map(|p| p.to_string());
                            st.expire_monitors();
                            let monitors: Vec<String> =
                                st.monitors.keys().map(|m| m.to_string()).collect();
                            let uptime = st.epoch.map(|e| e.elapsed().as_secs()).unwrap_or(0);
                            serde_json::json!({"ok":true,"rx_packets_sent":1,"peer":p,"monitors":monitors,"uptime_s":uptime,"tx_underruns":0})
                        }
                        "bye" if advertise_bye => {
                            if st.peer == Some(from) {
                                st.peer = None;
                                seen.peer_history.push((Instant::now(), None));
                            }
                            serde_json::json!({"ok":true})
                        }
                        _ => serde_json::json!({"ok":false,"error":"unknown_cmd"}),
                    };
                    if let Some(e) = reply["error"].as_str() {
                        seen.errors.push((e.to_string(), from.port()));
                    }
                    let _ = control.send_to(reply.to_string().as_bytes(), from);
                }
                if Instant::now() >= next {
                    let mut st = t_state.lock().unwrap();
                    let mut seen = t_seen.lock().unwrap();
                    let epoch = *st.epoch.get_or_insert_with(Instant::now);
                    let ts = epoch.elapsed().as_micros() as u64;
                    let samples: Vec<i16> = (0..SAMPLES_PER_PACKET)
                        .map(|i| ((i as f64 * 0.13).sin() * 8000.0) as i16)
                        .collect();
                    let packet = Packet {
                        direction: Direction::Rx,
                        sequence: st.seq,
                        timestamp_us: ts,
                        samples,
                    };
                    st.seq = st.seq.wrapping_add(1);
                    let bytes = packet.encode();
                    let mut targets: Vec<SocketAddr> =
                        st.current_peer(&mut seen).into_iter().collect();
                    st.expire_monitors();
                    targets.extend(st.monitors.keys().copied());
                    for t in targets {
                        let _ = audio.send_to(&bytes, t);
                        *seen.rx_sent_to.entry(t.port()).or_insert(0) += 1;
                    }
                    next += Duration::from_millis(10);
                }
                if !read_any {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        });

        FakeBridge {
            control_port,
            audio_port,
            seen,
            state,
            stop,
            restart,
            pause,
            thread: Some(thread),
        }
    }

    fn peer_port(&self) -> Option<u16> {
        let mut st = self.state.lock().unwrap();
        let mut seen = self.seen.lock().unwrap();
        st.current_peer(&mut seen).map(|p| p.port())
    }

    fn monitor_ports(&self) -> Vec<u16> {
        let mut st = self.state.lock().unwrap();
        st.expire_monitors();
        st.monitors.keys().map(|m| m.port()).collect()
    }

    /// The service restarts: everything it knew is gone, the ports stay.
    fn restart(&self) {
        self.restart.store(true, Ordering::SeqCst);
    }

    fn stop(mut self) -> Seen {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
        std::mem::take(&mut *self.seen.lock().unwrap())
    }
}

/// Another client (the iPad) that holds the peer slot with a hello and keepalives.
struct OtherClient {
    port: u16,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl OtherClient {
    fn start(bridge: &FakeBridge) -> OtherClient {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = socket.local_addr().unwrap().port();
        let control: SocketAddr = format!("127.0.0.1:{}", bridge.control_port)
            .parse()
            .unwrap();
        let audio: SocketAddr = format!("127.0.0.1:{}", bridge.audio_port).parse().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let t_stop = stop.clone();
        socket
            .send_to(br#"{"client":"vectorcomm","cmd":"hello"}"#, control)
            .unwrap();
        let thread = std::thread::spawn(move || {
            let mut i = 0u32;
            while !t_stop.load(Ordering::SeqCst) {
                let _ = socket.send_to(&Packet::keepalive(i, 0).encode(), audio);
                i += 1;
                std::thread::sleep(Duration::from_millis(500));
            }
        });
        OtherClient {
            port,
            stop,
            thread: Some(thread),
        }
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

/// The bridge's web server, enough for `POST /api/apps`: records every body
/// and answers `listed:true`; everything else is 404. With a PIN it keeps the
/// admin PIN rules (DOCS/web-api.md): `/api/apps` needs the token that
/// `POST /api/login` gives for the right PIN. As bridge 1.8 it also names
/// itself in `GET /api/status` and a log-in naming the app gets an app token.
struct FakeWeb {
    port: u16,
    bodies: Arc<Mutex<Vec<String>>>,
    /// Every `POST /api/login` body.
    logins: Arc<Mutex<Vec<String>>>,
    /// Accepted settings POSTs: (path, body).
    settings: Arc<Mutex<Vec<(String, String)>>>,
    /// The `GET /api/rigctl` body, 404 while `None`.
    rigctl: Arc<Mutex<Option<String>>>,
    /// The app token was revoked on the bridge's page: it is refused from now on.
    revoked: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl FakeWeb {
    fn start() -> FakeWeb {
        FakeWeb::start_with_pin(None)
    }

    fn start_with_pin(pin: Option<&'static str>) -> FakeWeb {
        FakeWeb::start_with(pin, true)
    }

    /// `settings_routes: false` is a bridge older than `/api/gain` and
    /// `/api/tx_audio_cap` (404).
    fn start_with(pin: Option<&'static str>, settings_routes: bool) -> FakeWeb {
        FakeWeb::start_full(pin, settings_routes, None)
    }

    /// Bridge 1.8 with this `device_id`.
    fn start_1_8(pin: &'static str, device_id: &'static str) -> FakeWeb {
        FakeWeb::start_full(Some(pin), true, Some(device_id))
    }

    fn start_full(
        pin: Option<&'static str>,
        settings_routes: bool,
        device_id: Option<&'static str>,
    ) -> FakeWeb {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let t_bodies = bodies.clone();
        let settings = Arc::new(Mutex::new(Vec::new()));
        let t_settings = settings.clone();
        let logins = Arc::new(Mutex::new(Vec::new()));
        let t_logins = logins.clone();
        let rigctl: Arc<Mutex<Option<String>>> = Arc::default();
        let t_rigctl = rigctl.clone();
        let revoked = Arc::new(AtomicBool::new(false));
        let t_revoked = revoked.clone();
        let t_stop = stop.clone();
        let thread = std::thread::spawn(move || {
            while !t_stop.load(Ordering::SeqCst) {
                let Ok((mut stream, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                // On macOS an accepted socket inherits the listener's non-blocking
                // flag; a read before the request landed would then fail with
                // WouldBlock, answer 404 and close with unread data (a TCP reset).
                stream.set_nonblocking(false).unwrap();
                let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
                let mut raw = Vec::new();
                let mut chunk = [0u8; 4096];
                while let Ok(n) = stream.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    raw.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&raw);
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if body.len() >= len {
                            break;
                        }
                    }
                }
                let text = String::from_utf8_lossy(&raw).to_string();
                let body = text.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
                let authorised = text.contains("\r\nAuthorization: Bearer fake-token\r\n")
                    || (device_id.is_some()
                        && !t_revoked.load(Ordering::SeqCst)
                        && text.contains("\r\nAuthorization: Bearer app-token\r\n"));
                let setting = ["/api/gain", "/api/tx_audio_cap"]
                    .into_iter()
                    .find(|p| settings_routes && text.starts_with(&format!("POST {p} ")));
                if text.starts_with("POST /api/login ") {
                    t_logins.lock().unwrap().push(body.to_string());
                }
                let login: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
                let rigctl_body = t_rigctl.lock().unwrap().clone();
                let reply = if let (true, Some(b)) =
                    (text.starts_with("GET /api/rigctl "), rigctl_body)
                {
                    format!("HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{b}")
                } else if text.starts_with("GET /api/status ") && device_id.is_some() {
                    format!(
                        "HTTP/1.0 200 OK\r\n\r\n{{\"ok\":true,\"device_id\":\"{}\"}}",
                        device_id.unwrap()
                    )
                } else if text.starts_with("POST /api/login ")
                    && device_id.is_some()
                    && login["pin"] == pin.unwrap_or_default()
                    && login["app"].is_string()
                {
                    format!(
                        "HTTP/1.0 200 OK\r\n\r\n{{\"ok\":true,\"token\":\"app-token\",\"app\":{},\"device_id\":\"{}\"}}",
                        login["app"],
                        device_id.unwrap()
                    )
                } else if text.starts_with("POST /api/login ") && pin.is_some() {
                    if body == format!("{{\"pin\":\"{}\"}}", pin.unwrap()) {
                        "HTTP/1.0 200 OK\r\n\r\n{\"ok\":true,\"token\":\"fake-token\"}".to_string()
                    } else {
                        "HTTP/1.0 401 Unauthorized\r\n\r\n{\"ok\":false,\"error\":\"bad_pin\"}"
                            .to_string()
                    }
                } else if (text.starts_with("POST /api/apps ") || setting.is_some())
                    && pin.is_some()
                    && !authorised
                {
                    "HTTP/1.0 401 Unauthorized\r\n\r\n{\"ok\":false,\"error\":\"pin_required\",\"pin_set\":true}".to_string()
                } else if text.starts_with("POST /api/apps ") {
                    t_bodies.lock().unwrap().push(body.to_string());
                    "HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"ok\":true,\"listed\":true}".to_string()
                } else if let Some(path) = setting {
                    t_settings
                        .lock()
                        .unwrap()
                        .push((path.to_string(), body.to_string()));
                    // The bridge echoes what it applied: `tx`/`rx`, or `tx_audio_cap_s`.
                    let v: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
                    let mut applied = serde_json::json!({"ok": true});
                    for k in ["tx", "rx"] {
                        if let Some(x) = v.get(k) {
                            applied[k] = x.clone();
                        }
                    }
                    if let Some(x) = v.get("seconds") {
                        applied["tx_audio_cap_s"] = x.clone();
                    }
                    format!("HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{applied}")
                } else {
                    "HTTP/1.0 404 Not Found\r\nContent-Type: application/json\r\n\r\n{\"ok\":false,\"error\":\"not_found\"}".to_string()
                };
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        FakeWeb {
            port,
            bodies,
            logins,
            settings,
            rigctl,
            revoked,
            stop,
            thread: Some(thread),
        }
    }

    fn revoke_app_token(&self) {
        self.revoked.store(true, Ordering::SeqCst);
    }

    fn set_rigctl(&self, body: String) {
        *self.rigctl.lock().unwrap() = Some(body);
    }

    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }

    fn settings(&self) -> Vec<(String, String)> {
        self.settings.lock().unwrap().clone()
    }

    fn logins(&self) -> Vec<String> {
        self.logins.lock().unwrap().clone()
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

fn config(bridge: &FakeBridge, mode: Mode) -> Config {
    let mut c = Config::new("127.0.0.1");
    c.control_port = bridge.control_port;
    c.audio_port = bridge.audio_port;
    c.http_port = None;
    c.mode = mode;
    c.bind = "127.0.0.1".parse().unwrap();
    c.client_name = "vectorcable-test".into();
    c
}

fn tone(seconds: f64) -> Vec<i16> {
    (0..(seconds * 48_000.0) as usize)
        .map(|i| ((i as f64 * 0.2).sin() * 6000.0) as i16)
        .collect()
}

/// Polls until `f` is true or `timeout` passes.
fn wait_for(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if f() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    f()
}

fn cmds<'a>(seen: &'a Seen, cmd: &'a str) -> impl Iterator<Item = &'a (String, u16, Instant)> {
    seen.control.iter().filter(move |(c, _, _)| c == cmd)
}

#[test]
fn listen_only_on_the_wire_sends_monitor_from_the_rx_socket_and_no_tx_packet() {
    let bridge = FakeBridge::start(false);
    let mut adapter = NullAdapter::new();
    adapter.queue_tx(&tone(4.0)); // the app plays audio; a monitor must still never send it
    let runner = Runner::spawn(config(&bridge, Mode::ListenOnly), Box::new(adapter)).unwrap();
    std::thread::sleep(Duration::from_millis(3500));
    let snap = runner.snapshot();
    assert_eq!(snap.mode, Mode::ListenOnly);
    assert_eq!(snap.link, Link::Connected);
    assert!(snap.rx.received > 200, "rx received {:?}", snap.rx);
    assert!(
        snap.playback.written_chunks > 100,
        "playback {:?}",
        snap.playback
    );
    assert!(snap.status.is_some(), "status polled");
    let local_audio_port = snap.local_audio_port;
    assert_eq!(bridge.monitor_ports(), vec![local_audio_port]);
    assert_eq!(bridge.peer_port(), None);
    let final_snap = runner.stop();
    assert!(final_snap.finished);
    let seen = bridge.stop();

    assert!(
        seen.tx.is_empty(),
        "a monitor sent TX packets: {}",
        seen.tx.len()
    );
    let monitors: Vec<&(String, u16, Instant)> = cmds(&seen, "monitor").collect();
    assert!(monitors.len() >= 3, "monitor re-sent: {}", monitors.len());
    assert!(
        monitors
            .iter()
            .all(|(_, port, _)| *port == local_audio_port),
        "monitor from the socket that receives RX"
    );
    for w in monitors.windows(2) {
        assert!(w[1].2.duration_since(w[0].2) <= Duration::from_millis(2100));
    }
    assert_eq!(cmds(&seen, "hello").count(), 0);
    assert_eq!(
        cmds(&seen, "bye").count(),
        0,
        "no bye advertised, none sent"
    );
    assert!(
        cmds(&seen, "status").all(|(_, port, _)| *port != local_audio_port),
        "status from the control socket"
    );
    assert!(cmds(&seen, "status").count() >= 1);
    assert!(seen.rx_sent_to.get(&local_audio_port).copied().unwrap_or(0) > 200);
}

#[test]
fn transmit_on_the_wire_hello_then_gated_audio_then_keepalives_then_bye() {
    let bridge = FakeBridge::start(true);
    let mut adapter = NullAdapter::new();
    adapter.queue_tx(&vec![0i16; 48_000 / 2]); // 0.5 s of silence first
    adapter.queue_tx(&tone(1.0)); // then a 1 s transmission
    let runner = Runner::spawn(config(&bridge, Mode::Transmit), Box::new(adapter)).unwrap();
    std::thread::sleep(Duration::from_millis(4000));
    let snap = runner.snapshot();
    assert_eq!(snap.mode, Mode::Transmit);
    assert_eq!(snap.link, Link::Connected);
    assert!(snap.hello.as_ref().is_some_and(|h| h.supports("bye")));
    assert!(!snap.streaming, "the gate closed after the tone");
    let local_audio_port = snap.local_audio_port;
    assert_eq!(bridge.peer_port(), Some(local_audio_port));
    let _ = runner.stop();
    let seen = bridge.stop();

    // Everything that registers comes from the audio socket; status from the control socket.
    for (cmd, port, _) in &seen.control {
        match cmd.as_str() {
            "status" => assert_ne!(*port, local_audio_port, "status from the control socket"),
            _ => assert_eq!(*port, local_audio_port, "{cmd} from the audio socket"),
        }
    }
    let hellos: Vec<&(String, u16, Instant)> = cmds(&seen, "hello").collect();
    assert_eq!(hellos.len(), 1, "one hello: {:?}", seen.control);
    assert_eq!(cmds(&seen, "monitor").count(), 0);
    let hello_at = hellos[0].2;
    // The bridge saw only one peer, our audio socket, from the hello onwards.
    assert_eq!(
        seen.peer_history
            .iter()
            .map(|(_, p)| *p)
            .collect::<Vec<_>>(),
        vec![Some(local_audio_port), None],
        "peer: us from the hello, then freed by bye"
    );

    // A keepalive from the audio socket right after hello, before any audio.
    assert!(!seen.tx.is_empty());
    assert!(
        seen.tx.iter().all(|(_, port, _)| *port == local_audio_port),
        "all TX from the audio socket"
    );
    assert!(seen.tx[0].0.is_keepalive());
    // (The bridge reads the audio socket before the control socket, so that keepalive can
    // be stamped before the hello. Both come from the same address, so the slot never
    // moves; the peer history above is the assertion that matters.)
    let first_audio_at = seen
        .tx
        .iter()
        .find(|(p, _, _)| !p.is_keepalive())
        .expect("audio packets")
        .2;
    assert!(first_audio_at > hello_at + Duration::from_millis(300));

    // About 100 audio packets (1 s) plus 20 for the gate's 200 ms close hold, sequence contiguous.
    let audio: Vec<&Packet> = seen
        .tx
        .iter()
        .filter(|(p, _, _)| !p.is_keepalive())
        .map(|(p, _, _)| p)
        .collect();
    assert!(
        audio.len() >= 115 && audio.len() <= 125,
        "audio packets {}",
        audio.len()
    );
    for w in audio.windows(2) {
        assert_eq!(w[1].sequence, w[0].sequence.wrapping_add(1));
    }
    assert!(audio.iter().all(|p| p.samples.len() == SAMPLES_PER_PACKET));

    // Keepalives never more than 2 s apart while idle, and none during the transmission.
    let keepalives: Vec<Instant> = seen
        .tx
        .iter()
        .filter(|(p, _, _)| p.is_keepalive())
        .map(|(_, _, t)| *t)
        .collect();
    assert!(keepalives.len() >= 3);
    let last_audio = seen
        .tx
        .iter()
        .filter(|(p, _, _)| !p.is_keepalive())
        .next_back()
        .unwrap()
        .2;
    assert!(!keepalives
        .iter()
        .any(|t| *t > first_audio_at && *t < last_audio));
    for w in keepalives.windows(2) {
        if w[0] > last_audio || w[1] < first_audio_at {
            assert!(w[1].duration_since(w[0]) <= Duration::from_millis(2100));
        }
    }
    assert!(
        keepalives.last().unwrap().duration_since(last_audio) <= Duration::from_millis(2100),
        "a keepalive within 2 s of the stream ending; (keepalive, sent ms, received ms after hello): {:?}",
        seen.tx
            .iter()
            .filter(|(p, _, _)| p.is_keepalive() || p.sequence % 20 == 0)
            .map(|(p, _, t)| (
                p.is_keepalive(),
                p.timestamp_us / 1000,
                t.duration_since(hello_at).as_millis()
            ))
            .collect::<Vec<_>>()
    );

    // Quit sent bye from the audio socket (the peer address), after the last TX packet.
    let byes: Vec<&(String, u16, Instant)> = cmds(&seen, "bye").collect();
    assert_eq!(byes.len(), 1);
    assert_eq!(byes[0].1, local_audio_port);
    assert!(byes[0].2 >= seen.tx.last().unwrap().2);
    assert!(
        seen.rx_sent_to.get(&local_audio_port).copied().unwrap_or(0) > 200,
        "RX reached the audio socket"
    );
}

/// Listen Only, Transmit, Listen Only. With `bye` the same socket frees the
/// slot and becomes a monitor; without it a fresh socket becomes the monitor
/// while the old peer expires. Either way no TX packet after the switch and we
/// never end up as the peer.
fn listen_transmit_listen(advertise_bye: bool) {
    let bridge = FakeBridge::start(advertise_bye);
    let mut adapter = NullAdapter::new();
    adapter.queue_tx(&vec![0i16; 48_000 * 2]); // silence while listening
    adapter.queue_tx(&tone(1.0)); // a transmission once in Transmit
    adapter.queue_tx(&tone(10.0)); // and audio that must not go out once back in Listen Only
    let runner = Runner::spawn(config(&bridge, Mode::ListenOnly), Box::new(adapter)).unwrap();
    assert!(wait_for(Duration::from_secs(3), || {
        runner.snapshot().link == Link::Connected
    }));
    let first_port = runner.snapshot().local_audio_port;
    assert_eq!(bridge.monitor_ports(), vec![first_port]);

    runner.command(Command::SetMode(Mode::Transmit));
    assert_eq!(
        runner.replies.recv_timeout(Duration::from_secs(2)).unwrap(),
        CommandReply::ModeSet(Mode::Transmit)
    );
    assert!(wait_for(Duration::from_secs(3), || {
        bridge.peer_port() == Some(first_port)
    }));
    assert!(
        !bridge.monitor_ports().contains(&first_port),
        "a hello from a monitor promotes it"
    );
    assert!(
        wait_for(Duration::from_secs(5), || {
            runner.snapshot().tx.audio_packets >= 50
        }),
        "the transmission goes out: {:?}",
        runner.snapshot().tx
    );

    runner.command(Command::SetMode(Mode::ListenOnly));
    assert_eq!(
        runner.replies.recv_timeout(Duration::from_secs(2)).unwrap(),
        CommandReply::ModeSet(Mode::ListenOnly)
    );
    let switched_at = Instant::now();
    let registered = wait_for(Duration::from_secs(3), || {
        let snap = runner.snapshot();
        !bridge.monitor_ports().is_empty() && snap.mode == Mode::ListenOnly
    });
    if !registered {
        let seen = bridge.seen.lock().unwrap();
        panic!(
            "not a monitor again: mode {:?} port {} peer {:?} monitors {:?}\ncontrol {:?}\npeers {:?}",
            runner.snapshot().mode,
            runner.snapshot().local_audio_port,
            bridge.state.lock().unwrap().peer,
            bridge.state.lock().unwrap().monitors,
            seen.control.iter().map(|(c, p, t)| (c.clone(), *p, t.duration_since(switched_at - Duration::from_secs(3)).as_millis())).collect::<Vec<_>>(),
            seen.peer_history.iter().map(|(t, p)| (t.duration_since(switched_at - Duration::from_secs(3)).as_millis(), *p)).collect::<Vec<_>>()
        );
    }
    let second_port = runner.snapshot().local_audio_port;
    if advertise_bye {
        assert_eq!(second_port, first_port, "same socket after bye");
        assert_eq!(bridge.peer_port(), None, "bye freed the slot");
    } else {
        assert_ne!(second_port, first_port, "a fresh socket without bye");
    }
    assert_eq!(bridge.monitor_ports(), vec![second_port]);
    // RX keeps arriving on the monitor socket.
    assert!(wait_for(Duration::from_secs(3), || {
        runner.snapshot().link == Link::Connected
    }));
    if !advertise_bye {
        assert!(
            wait_for(Duration::from_millis(6000), || bridge.peer_port().is_none()),
            "the old peer expires"
        );
    }
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(bridge.peer_port(), None, "we are not the peer");
    assert_eq!(bridge.monitor_ports(), vec![second_port]);
    let _ = runner.stop();
    let seen = bridge.stop();

    assert!(
        !seen
            .tx
            .iter()
            .any(|(_, _, t)| *t > switched_at + Duration::from_millis(50)),
        "no TX packet after the switch (keepalives included)"
    );
    assert!(
        seen.tx.iter().any(|(p, _, _)| !p.is_keepalive()),
        "the transmission went out"
    );
    assert_eq!(cmds(&seen, "bye").count(), usize::from(advertise_bye));
    for (cmd, port, _) in &seen.control {
        if cmd != "status" {
            assert!(
                *port == first_port || *port == second_port,
                "{cmd} from port {port}"
            );
        }
    }
    // The peer slot only ever held our audio socket.
    assert!(seen
        .peer_history
        .iter()
        .all(|(_, p)| p.is_none() || *p == Some(first_port)));
    assert!(seen.rx_sent_to.get(&second_port).copied().unwrap_or(0) > 50);
}

#[test]
fn listen_to_transmit_to_listen_with_bye_frees_the_slot_and_monitors_again() {
    listen_transmit_listen(true);
}

#[test]
fn listen_to_transmit_to_listen_without_bye_monitors_from_a_fresh_socket() {
    listen_transmit_listen(false);
}

#[test]
fn reconnects_after_a_pi_restart_without_user_action() {
    let bridge = FakeBridge::start(false);
    let runner = Runner::spawn(
        config(&bridge, Mode::Transmit),
        Box::new(NullAdapter::new()),
    )
    .unwrap();
    assert!(wait_for(Duration::from_secs(4), || {
        runner.snapshot().link == Link::Connected
    }));
    let port = runner.snapshot().local_audio_port;
    assert_eq!(bridge.peer_port(), Some(port));
    bridge.restart();
    // The restarted service knows nobody; the next keepalive (at most 1 s away) makes us
    // the peer again and RX resumes.
    assert!(wait_for(Duration::from_secs(3), || {
        bridge.peer_port() == Some(port)
    }));
    let rx_before = runner.snapshot().rx.received;
    assert!(wait_for(Duration::from_secs(3), || {
        let snap = runner.snapshot();
        snap.rx.received > rx_before + 50 && snap.link == Link::Connected
    }));
    assert_eq!(runner.snapshot().mode, Mode::Transmit);
    let _ = runner.stop();
    let seen = bridge.stop();
    assert!(seen.tx.iter().all(|(_, p, _)| *p == port));
    assert!(cmds(&seen, "hello").all(|(_, p, _)| *p == port));
}

/// Where `vectorbridge.test` points: TEST-NET-1 (nothing answers) until the
/// bridge "moves" to the fake on 127.0.0.1, as a new DHCP lease would move it.
static MOVED: AtomicBool = AtomicBool::new(false);

fn moving_name(host: &str) -> std::io::Result<Vec<IpAddr>> {
    assert_eq!(host, "vectorbridge.test");
    let ip: [u8; 4] = if MOVED.load(Ordering::SeqCst) {
        [127, 0, 0, 1]
    } else {
        [192, 0, 2, 1]
    };
    Ok(vec![IpAddr::from(ip)])
}

#[test]
fn a_bridge_name_that_moves_is_looked_up_again_while_the_link_is_down() {
    let bridge = FakeBridge::start(false);
    let mut c = config(&bridge, Mode::ListenOnly);
    c.host = "vectorbridge.test".into();
    c.bind = IpAddr::from([0, 0, 0, 0]);
    c.resolve = moving_name;
    let runner = Runner::spawn(c, Box::new(NullAdapter::new())).unwrap();
    assert_eq!(runner.snapshot().bridge.ip(), IpAddr::from([192, 0, 2, 1]));
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(runner.snapshot().link, Link::Searching);

    MOVED.store(true, Ordering::SeqCst);
    assert!(
        wait_for(Duration::from_secs(8), || {
            runner.snapshot().link == Link::Connected
        }),
        "link {:?}",
        runner.snapshot().link
    );
    assert_eq!(runner.snapshot().bridge.ip(), IpAddr::from([127, 0, 0, 1]));
    let mut said = Vec::new();
    assert!(
        announced(
            &runner,
            &mut said,
            "vectorbridge.test now points at 127.0.0.1"
        ),
        "{said:?}"
    );
    let port = runner.snapshot().local_audio_port;
    assert_eq!(bridge.monitor_ports(), vec![port]);
    let _ = runner.stop();
    let _ = bridge.stop();
}

/// Where `steady.test` points: the fake on 127.0.0.1 until `STRAYED`.
static STRAYED: AtomicBool = AtomicBool::new(false);

fn straying_name(_: &str) -> std::io::Result<Vec<IpAddr>> {
    let ip: [u8; 4] = if STRAYED.load(Ordering::SeqCst) {
        [192, 0, 2, 1]
    } else {
        [127, 0, 0, 1]
    };
    Ok(vec![IpAddr::from(ip)])
}

#[test]
fn a_connected_link_stays_put_whatever_the_name_says() {
    let bridge = FakeBridge::start(false);
    let mut c = config(&bridge, Mode::ListenOnly);
    c.host = "steady.test".into();
    c.bind = IpAddr::from([0, 0, 0, 0]);
    c.resolve = straying_name;
    let runner = Runner::spawn(c, Box::new(NullAdapter::new())).unwrap();
    assert!(wait_for(Duration::from_secs(4), || {
        runner.snapshot().link == Link::Connected
    }));
    STRAYED.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(3000));
    let snap = runner.snapshot();
    assert_eq!(snap.link, Link::Connected);
    assert_eq!(snap.bridge.ip(), IpAddr::from([127, 0, 0, 1]));
    let _ = runner.stop();
    let _ = bridge.stop();
}

#[test]
fn status_polling_never_changes_the_peer() {
    let bridge = FakeBridge::start(false);
    let other = OtherClient::start(&bridge);
    assert!(wait_for(Duration::from_secs(2), || {
        bridge.peer_port() == Some(other.port)
    }));
    let runner = Runner::spawn(
        config(&bridge, Mode::ListenOnly),
        Box::new(NullAdapter::new()),
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(5000));
    let snap = runner.snapshot();
    assert_eq!(snap.link, Link::Connected);
    assert!(snap.status.is_some());
    assert_eq!(
        bridge.peer_port(),
        Some(other.port),
        "the iPad still holds the slot"
    );
    assert_eq!(bridge.monitor_ports(), vec![snap.local_audio_port]);
    let _ = runner.stop();
    let other_port = other.port;
    other.stop();
    let seen = bridge.stop();
    assert!(cmds(&seen, "status").count() >= 2);
    assert_eq!(
        seen.peer_history
            .iter()
            .map(|(_, p)| *p)
            .collect::<Vec<_>>(),
        vec![Some(other_port)],
        "the peer never changed"
    );
}

#[test]
fn starting_in_transmit_starts_in_listen_only_when_another_peer_holds_the_slot() {
    let bridge = FakeBridge::start(false);
    let other = OtherClient::start(&bridge);
    assert!(wait_for(Duration::from_secs(2), || {
        bridge.peer_port() == Some(other.port)
    }));
    let runner = Runner::spawn(
        config(&bridge, Mode::Transmit),
        Box::new(NullAdapter::new()),
    )
    .unwrap();
    let reply = runner.replies.recv_timeout(Duration::from_secs(3)).unwrap();
    let CommandReply::Refused(why) = reply else {
        panic!("expected a refusal, got {reply:?}");
    };
    assert!(
        why.contains(&other.port.to_string()) && why.contains("Listen Only"),
        "{why}"
    );
    let port = runner.snapshot().local_audio_port;
    assert!(wait_for(Duration::from_secs(2), || {
        bridge.monitor_ports() == vec![port]
    }));
    assert_eq!(runner.snapshot().mode, Mode::ListenOnly);
    assert_eq!(
        bridge.peer_port(),
        Some(other.port),
        "the other peer keeps the slot"
    );
    let _ = runner.stop();
    let other_port = other.port;
    other.stop();
    let seen = bridge.stop();
    let ours: Vec<&(String, u16, Instant)> = seen
        .control
        .iter()
        .filter(|(_, p, _)| *p != other_port)
        .collect();
    assert_eq!(ours[0].0, "status", "status first: {:?}", ours);
    assert!(
        cmds(&seen, "hello").all(|(_, p, _)| *p == other_port),
        "no hello from us"
    );
    assert!(
        !seen.tx.iter().any(|(_, p, _)| *p == port),
        "no TX packet from us"
    );
}

/// With `bye`, the switch sends `bye` then `monitor` from the audio socket. A
/// TX packet still queued in the bridge's audio socket between the two makes
/// that socket the peer again (the audio socket is read first each pass), so
/// the `monitor` is refused as `is_peer`; the session then monitors from a
/// fresh socket.
#[test]
fn queued_tx_packet_after_the_switch_is_refused_as_is_peer_then_a_fresh_socket_monitors() {
    let bridge = FakeBridge::start(true);
    let mut adapter = NullAdapter::new();
    adapter.queue_tx(&tone(30.0));
    let mut cfg = config(&bridge, Mode::Transmit);
    cfg.gated = false; // audio flows the whole time, so packets are always queued
    let runner = Runner::spawn(cfg, Box::new(adapter)).unwrap();
    assert_eq!(
        runner.replies.recv_timeout(Duration::from_secs(3)).unwrap(),
        CommandReply::ModeSet(Mode::Transmit)
    );
    let first_port = runner.snapshot().local_audio_port;
    assert!(wait_for(Duration::from_secs(3), || {
        bridge.peer_port() == Some(first_port) && runner.snapshot().tx.audio_packets >= 50
    }));

    // A busy bridge: several TX packets queue ahead of the monitor the switch sends.
    // Reading one datagram per socket per pass, it re-registers the old socket
    // from a queued TX packet after it has read the monitor's predecessor.
    let p = bridge.pause.clone();
    p.store(true, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(30));
    runner.command(Command::SetMode(Mode::ListenOnly));
    assert_eq!(
        runner.replies.recv_timeout(Duration::from_secs(2)).unwrap(),
        CommandReply::ModeSet(Mode::ListenOnly)
    );
    std::thread::sleep(Duration::from_millis(30));
    p.store(false, Ordering::SeqCst);

    assert!(wait_for(Duration::from_secs(3), || {
        bridge.monitor_ports().len() == 1 && bridge.monitor_ports()[0] != first_port
    }));
    let listening_port = bridge.monitor_ports()[0];
    assert!(wait_for(Duration::from_secs(3), || {
        bridge
            .seen
            .lock()
            .unwrap()
            .rx_sent_to
            .get(&listening_port)
            .copied()
            .unwrap_or(0)
            > 20
    }));
    assert_eq!(runner.snapshot().local_audio_port, listening_port);
    let mut runner_announcements = Vec::new();
    while let Ok(a) = runner.announcements.try_recv() {
        runner_announcements.push(a);
    }
    let _ = runner.stop();
    let seen = bridge.stop();
    let is_peer: Vec<&(String, u16)> = seen.errors.iter().filter(|(e, _)| e == "is_peer").collect();
    assert!(
        !is_peer.is_empty(),
        "the queued TX packet re-registered us: {:?}",
        seen.errors
    );
    assert!(is_peer.iter().all(|(_, p)| *p == first_port), "{is_peer:?}");
    let announced: Vec<String> = runner_announcements;
    assert_eq!(
        cmds(&seen, "bye").count(),
        1,
        "{announced:?} {:?}",
        seen.errors
    );
    assert!(seen.rx_sent_to.get(&listening_port).copied().unwrap_or(0) > 20);
}

/// The app listing: `client` on every hello and monitor, `POST /api/apps` with
/// the VectorCable description after registering and again after a new session.
#[test]
fn app_listing_is_posted_after_registering_and_again_after_a_new_session() {
    let bridge = FakeBridge::start(false);
    let web = FakeWeb::start();
    let mut cfg = config(&bridge, Mode::ListenOnly);
    cfg.http_port = Some(web.port);
    cfg.client_name = "vectorcable".into();
    let runner = Runner::spawn(cfg, Box::new(NullAdapter::new())).unwrap();
    assert!(wait_for(Duration::from_secs(3), || {
        runner.snapshot().listing == Some(ListingState::Listed)
    }));
    assert_eq!(web.bodies().len(), 1);
    let body: serde_json::Value = serde_json::from_str(&web.bodies()[0]).unwrap();
    assert_eq!(body["app_id"], "vectorcable");
    assert_eq!(body["name"], "VectorCable");
    assert_eq!(body["note"], "Audio cable for WSJT-X, JS8Call and FLDIGI");
    assert_eq!(body["launch_url"], "http://127.0.0.1/downloads/");
    assert!(body.get("icon_png").is_none());

    // A bridge restart forgets the description: the uptime drop re-posts it.
    // Long enough that the uptime before the restart exceeds any uptime seen
    // in the first status poll after it (polls are 2 s apart).
    std::thread::sleep(Duration::from_millis(4500));
    assert_eq!(web.bodies().len(), 1, "no re-post inside the 60 s cycle");
    bridge.restart();
    assert!(wait_for(Duration::from_secs(5), || web.bodies().len() >= 2));
    let _ = runner.stop();
    let seen = bridge.stop();
    web.stop();
    assert!(seen.control.iter().all(|(c, _, _)| c != "hello"));
    assert!(!seen.clients.is_empty());
    assert!(
        seen.clients
            .iter()
            .all(|c| c.as_deref() == Some("vectorcable")),
        "client on every monitor: {:?}",
        seen.clients
    );
}

/// The admin PIN: the listing asks for one; a wrong one is refused; the right
/// one lists the app. Nothing is stored, so the next run asks again.
#[test]
fn listing_on_a_pin_bridge_asks_for_the_pin_then_logs_in_and_keeps_nothing() {
    let bridge = FakeBridge::start(false);
    let web = FakeWeb::start_with_pin(Some("2468"));
    let mut cfg = config(&bridge, Mode::ListenOnly);
    cfg.http_port = Some(web.port);
    let runner = Runner::spawn(cfg.clone(), Box::new(NullAdapter::new())).unwrap();
    assert!(wait_for(Duration::from_secs(3), || {
        runner.snapshot().listing == Some(ListingState::PinNeeded)
    }));
    runner.command(Command::SetPin("1111".into()));
    assert!(wait_for(Duration::from_secs(3), || {
        runner.snapshot().listing == Some(ListingState::BadPin)
    }));
    runner.command(Command::SetPin("2468".into()));
    assert!(wait_for(Duration::from_secs(3), || {
        runner.snapshot().listing == Some(ListingState::Listed)
    }));
    assert_eq!(web.bodies().len(), 1);
    let body: serde_json::Value = serde_json::from_str(&web.bodies()[0]).unwrap();
    assert_eq!(
        body["app_id"], "vectorcable",
        "the retry posts the listing: {body}"
    );
    let _ = runner.stop();

    // A new run has no PIN and no token, so it asks again.
    let runner = Runner::spawn(cfg, Box::new(NullAdapter::new())).unwrap();
    assert!(wait_for(Duration::from_secs(3), || {
        runner.snapshot().listing == Some(ListingState::PinNeeded)
    }));
    let _ = runner.stop();
    bridge.stop();
    web.stop();
}

fn announced(runner: &Runner, seen: &mut Vec<String>, part: &str) -> bool {
    while let Ok(a) = runner.announcements.try_recv() {
        seen.push(a);
    }
    seen.iter().any(|a| a.contains(part))
}

/// Gain and the TX cap go over HTTP with the session token, not UDP (the
/// bridge refuses UDP settings from off the Pi since its step 1.2). Before the
/// log-in a change is refused and the user is told to press p.
#[test]
fn settings_go_over_http_with_the_token_and_never_over_udp() {
    let bridge = FakeBridge::start(false);
    let web = FakeWeb::start_with_pin(Some("2468"));
    let mut cfg = config(&bridge, Mode::ListenOnly);
    cfg.http_port = Some(web.port);
    let runner = Runner::spawn(cfg, Box::new(NullAdapter::new())).unwrap();
    let mut said = Vec::new();
    assert!(wait_for(Duration::from_secs(3), || {
        runner.snapshot().listing == Some(ListingState::PinNeeded)
    }));
    runner.command(Command::SetGain {
        tx: Some(55),
        rx: None,
    });
    assert!(wait_for(Duration::from_secs(3), || {
        announced(&runner, &mut said, "needs its PIN for that change")
    }));
    assert!(web.settings().is_empty());

    runner.command(Command::SetPin("2468".into()));
    assert!(wait_for(Duration::from_secs(3), || {
        runner.snapshot().listing == Some(ListingState::Listed)
    }));
    runner.command(Command::SetGain {
        tx: Some(55),
        rx: None,
    });
    runner.command(Command::SetTxAudioCap { seconds: 120 });
    assert!(
        wait_for(Duration::from_secs(3), || {
            announced(&runner, &mut said, "Bridge set TX gain 55 %")
                && announced(&runner, &mut said, "TX audio cap 120 s")
        }),
        "announced: {said:?}"
    );
    let posted = web.settings();
    assert!(posted.contains(&("/api/gain".to_string(), "{\"tx\":55}".to_string())));
    assert!(posted.contains(&(
        "/api/tx_audio_cap".to_string(),
        "{\"seconds\":120}".to_string()
    )));
    let _ = runner.stop();
    {
        let seen = bridge.seen.lock().unwrap();
        assert_eq!(cmds(&seen, "set_gain").count(), 0);
        assert_eq!(cmds(&seen, "set_tx_audio_cap").count(), 0);
    }
    bridge.stop();
    web.stop();
}

/// A token store in memory, standing in for the Keychain.
#[derive(Debug, Default)]
struct MemTokens(Mutex<HashMap<String, String>>);

impl TokenStore for MemTokens {
    fn load(&self, device_id: &str) -> Option<String> {
        self.0.lock().unwrap().get(device_id).cloned()
    }
    fn save(&self, device_id: &str, token: &str) -> Result<(), String> {
        self.0
            .lock()
            .unwrap()
            .insert(device_id.into(), token.into());
        Ok(())
    }
    fn remove(&self, device_id: &str) {
        self.0.lock().unwrap().remove(device_id);
    }
}

/// Bridge 1.8: the log-in names the app and its token is stored under the
/// bridge's `device_id`, so the next run lists and changes a setting without
/// the PIN.
#[test]
fn an_app_token_is_stored_so_the_next_run_needs_no_pin() {
    const DEVICE: &str = "fedcba9876543210";
    let bridge = FakeBridge::start(false);
    let web = FakeWeb::start_1_8("2468", DEVICE);
    let tokens = Arc::new(MemTokens::default());
    let mut cfg = config(&bridge, Mode::ListenOnly);
    cfg.http_port = Some(web.port);
    cfg.tokens = Some(tokens.clone());
    let runner = Runner::spawn(cfg.clone(), Box::new(NullAdapter::new())).unwrap();
    assert!(wait_for(Duration::from_secs(3), || {
        runner.snapshot().listing == Some(ListingState::PinNeeded)
    }));
    runner.command(Command::SetPin("2468".into()));
    assert!(wait_for(Duration::from_secs(3), || {
        runner.snapshot().listing == Some(ListingState::Listed)
    }));
    let _ = runner.stop();
    assert_eq!(tokens.load(DEVICE).as_deref(), Some("app-token"));
    let login: serde_json::Value = serde_json::from_str(&web.logins()[0]).unwrap();
    assert_eq!(login["app"], vbcore::pin::app_label());

    // The next run: no PIN, and a setting goes through.
    let runner = Runner::spawn(cfg, Box::new(NullAdapter::new())).unwrap();
    let mut said = Vec::new();
    assert!(wait_for(Duration::from_secs(3), || {
        runner.snapshot().listing == Some(ListingState::Listed)
    }));
    runner.command(Command::SetGain {
        tx: Some(55),
        rx: None,
    });
    assert!(
        wait_for(Duration::from_secs(3), || {
            announced(&runner, &mut said, "Bridge set TX gain 55 %")
        }),
        "announced: {said:?}"
    );
    let _ = runner.stop();
    assert_eq!(web.logins().len(), 1, "one log-in, in the first run");
    assert_eq!(web.bodies().len(), 2);
    bridge.stop();
    web.stop();
}

/// The bridge's 4532: records each line, answers `\vb_auth` with `RPRT 0` and
/// `f` with a frequency.
fn fake_rig() -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let lines = Arc::new(Mutex::new(Vec::new()));
    let t_lines = lines.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let lines = t_lines.clone();
            std::thread::spawn(move || {
                let mut reader = std::io::BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                while std::io::BufRead::read_line(&mut reader, &mut line).unwrap_or(0) > 0 {
                    let l = line.trim_end().to_string();
                    let answer: &[u8] = if l.starts_with("\\vb_auth ") {
                        b"RPRT 0\n"
                    } else if l == "f" {
                        b"14074000\n"
                    } else {
                        b"RPRT -1\n"
                    };
                    lines.lock().unwrap().push(l);
                    let _ = stream.write_all(answer);
                    line.clear();
                }
            });
        }
    });
    (port, lines)
}

/// Runs VectorCable with a stored 1.8 app token and the relay on, against a
/// bridge whose `/api/rigctl` has `vb_auth` as given (absent for `None`); an app asks the relay for the
/// frequency. Returns the app's answer and the lines the bridge's 4532 saw.
fn relay_run(vb_auth: Option<bool>) -> (Option<String>, Vec<String>) {
    const DEVICE: &str = "fedcba9876543210";
    let bridge = FakeBridge::start(false);
    let web = FakeWeb::start_1_8("2468", DEVICE);
    let (rig_port, rig_lines) = fake_rig();
    let mut body = serde_json::json!({"ok": true, "access": "all", "port": rig_port});
    if let Some(v) = vb_auth {
        body["vb_auth"] = v.into();
    }
    web.set_rigctl(body.to_string());
    let tokens = Arc::new(MemTokens::default());
    tokens.save(DEVICE, "app-token").unwrap();
    let mut cfg = config(&bridge, Mode::ListenOnly);
    cfg.http_port = Some(web.port);
    cfg.tokens = Some(tokens);
    cfg.rig_relay = Some("127.0.0.1:0".parse().unwrap());
    let runner = Runner::spawn(cfg, Box::new(NullAdapter::new())).unwrap();
    // The stored token is loaded by the listing POST; the port and the
    // marker come with `/api/rigctl`.
    assert!(wait_for(Duration::from_secs(5), || {
        let s = runner.snapshot();
        s.listing == Some(ListingState::Listed) && s.rigctl.is_some()
    }));
    let Some(RelayState::Listening(relay)) = runner.snapshot().rig_relay else {
        panic!("relay: {:?}", runner.snapshot().rig_relay);
    };
    let mut app = std::net::TcpStream::connect(relay).unwrap();
    app.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    app.write_all(b"f\n").unwrap();
    let mut answer = String::new();
    let got = std::io::BufRead::read_line(&mut std::io::BufReader::new(&app), &mut answer)
        .ok()
        .filter(|n| *n > 0)
        .map(|_| answer.trim_end().to_string());
    let _ = runner.stop();
    bridge.stop();
    web.stop();
    let lines = rig_lines.lock().unwrap().clone();
    (got, lines)
}

/// Bridge 1.10 with the marker: the relay logs in with the app token first,
/// keeps the `RPRT 0`, and the app gets its answer.
#[test]
fn the_rig_relay_logs_in_with_the_app_token_where_the_bridge_says_vb_auth() {
    let (answer, lines) = relay_run(Some(true));
    assert_eq!(answer.as_deref(), Some("14074000"));
    assert_eq!(lines, ["\\vb_auth app-token", "f"]);
}

/// Without a literal `"vb_auth": true` the token line is never sent: an older
/// proxy would hand it to rigctld, which runs its letters as commands.
#[test]
fn the_rig_relay_never_sends_the_token_without_the_marker() {
    for marker in [None, Some(false)] {
        let (answer, lines) = relay_run(marker);
        assert_eq!(answer.as_deref(), Some("14074000"), "{marker:?}");
        assert_eq!(lines, ["f"], "{marker:?}");
    }
}

/// The bridge's 4532 after a revoke: every `\vb_auth` gets `RPRT -9` and a close.
fn refusing_rig() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut line = String::new();
            let _ = std::io::BufRead::read_line(
                &mut std::io::BufReader::new(stream.try_clone().unwrap()),
                &mut line,
            );
            let _ = stream.write_all(b"RPRT -9\n");
        }
    });
    port
}

/// The token revoked on the bridge: its refusal on 4532 has the token checked,
/// which forgets it and asks for the PIN, without the user doing anything.
#[test]
fn a_token_refused_on_4532_is_checked_and_the_pin_asked_for() {
    const DEVICE: &str = "fedcba9876543210";
    let bridge = FakeBridge::start(false);
    let web = FakeWeb::start_1_8("2468", DEVICE);
    let body =
        serde_json::json!({"ok": true, "access": "wifi", "port": refusing_rig(), "vb_auth": true});
    web.set_rigctl(body.to_string());
    let tokens = Arc::new(MemTokens::default());
    tokens.save(DEVICE, "app-token").unwrap();
    let mut cfg = config(&bridge, Mode::ListenOnly);
    cfg.http_port = Some(web.port);
    cfg.tokens = Some(tokens.clone());
    cfg.rig_relay = Some("127.0.0.1:0".parse().unwrap());
    let runner = Runner::spawn(cfg, Box::new(NullAdapter::new())).unwrap();
    assert!(wait_for(Duration::from_secs(5), || {
        let s = runner.snapshot();
        s.listing == Some(ListingState::Listed) && s.rigctl.is_some()
    }));
    let Some(RelayState::Listening(relay)) = runner.snapshot().rig_relay else {
        panic!("relay: {:?}", runner.snapshot().rig_relay);
    };
    web.revoke_app_token();
    let mut app = std::net::TcpStream::connect(relay).unwrap();
    app.write_all(b"f\n").unwrap();
    assert!(wait_for(Duration::from_secs(5), || {
        runner.snapshot().listing == Some(ListingState::PinNeeded)
    }));
    assert_eq!(tokens.load(DEVICE), None, "the revoked token is forgotten");
    let _ = runner.stop();
    bridge.stop();
    web.stop();
}

/// With the relay's port taken, VectorCable runs on and says why.
#[test]
fn a_taken_relay_port_is_shown_and_the_runner_carries_on() {
    let bridge = FakeBridge::start(false);
    let web = FakeWeb::start();
    let taken = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut cfg = config(&bridge, Mode::ListenOnly);
    cfg.http_port = Some(web.port);
    cfg.rig_relay = Some(taken.local_addr().unwrap());
    let runner = Runner::spawn(cfg, Box::new(NullAdapter::new())).unwrap();
    assert!(matches!(
        runner.snapshot().rig_relay,
        Some(RelayState::Failed { .. })
    ));
    assert!(wait_for(Duration::from_secs(3), || {
        runner.snapshot().listing == Some(ListingState::Listed)
    }));
    let _ = runner.stop();
    bridge.stop();
    web.stop();
}

/// A bridge older than the settings endpoints answers 404: fall back to UDP.
#[test]
fn a_bridge_without_the_settings_endpoints_gets_them_over_udp() {
    let bridge = FakeBridge::start(false);
    let web = FakeWeb::start_with(None, false);
    let mut cfg = config(&bridge, Mode::ListenOnly);
    cfg.http_port = Some(web.port);
    let runner = Runner::spawn(cfg, Box::new(NullAdapter::new())).unwrap();
    runner.command(Command::SetTxAudioCap { seconds: 120 });
    assert!(wait_for(Duration::from_secs(3), || {
        cmds(&bridge.seen.lock().unwrap(), "set_tx_audio_cap").count() == 1
    }));
    let _ = runner.stop();
    bridge.stop();
    web.stop();
}
