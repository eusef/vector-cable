//! The bridge's admin PIN (vector-bridge DOCS/web-api.md, "Admin PIN" and "How
//! a native app logs in"): every POST needs `Authorization: Bearer <token>`.
//!
//! A bridge from its roadmap step 1.8 on names itself with a `device_id` in
//! `GET /api/status`, and a log-in that names the app gives an app token that
//! lasts until a PIN change or a revoke. That token is kept in a [`TokenStore`]
//! under the `device_id`, and a token is only ever sent to the `device_id` that
//! issued it (every access point is 10.42.0.1). An older bridge gives a session
//! token, held in memory only.
//!
//! No PIN is kept, on disk or in memory, past its one log-in. A stored PIN sent
//! by itself could be collected by a host posing as the bridge.

use crate::http::{self, HttpOutcome};
use crate::runner::ListingState;
use std::fmt;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

/// The bridge takes 4 to 8 digits.
pub fn valid_pin(pin: &str) -> bool {
    (4..=8).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit())
}

/// 16 lowercase hex digits, as bridge 1.8 sends.
fn valid_device_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A token goes into a header, so a stored one may hold only letters, digits,
/// `-` and `_`.
fn valid_token(token: &str) -> bool {
    (1..=256).contains(&token.len())
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The name the bridge lists our app token under (`GET /api/tokens`).
pub fn app_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "VectorCable on Mac"
    } else if cfg!(windows) {
        "VectorCable on Windows"
    } else {
        "VectorCable on Linux"
    }
}

/// Where app tokens live between runs (the macOS Keychain), one per bridge
/// `device_id`.
pub trait TokenStore: Send + Sync + fmt::Debug {
    fn load(&self, device_id: &str) -> Option<String>;
    fn save(&self, device_id: &str, token: &str) -> Result<(), String>;
    fn remove(&self, device_id: &str);
}

/// The token held in memory, shared between the runner and its POST threads.
#[derive(Default)]
pub struct Auth {
    pub token: Option<String>,
    /// The bridge that issued `token`: its `device_id`, or `None` before 1.8.
    pub device_id: Option<String>,
    /// How often the bridge's name has moved to another address. A POST
    /// started before a move holds the old count and changes nothing here.
    pub moves: u64,
}

impl Auth {
    /// The bridge's name points somewhere else now: forget the token, so none
    /// goes to the new address until its `device_id` is read.
    pub fn bridge_moved(&mut self) {
        *self = Auth {
            moves: self.moves + 1,
            ..Auth::default()
        };
    }
}

impl fmt::Debug for Auth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Auth")
            .field("token", &self.token.as_ref().map(|_| "<set>"))
            .field("device_id", &self.device_id)
            .field("moves", &self.moves)
            .finish()
    }
}

/// The bridge's web server and what we log in to it with.
#[derive(Clone, Copy)]
pub struct Web<'a> {
    pub host: &'a str,
    pub port: u16,
    pub auth: &'a Mutex<Auth>,
    pub tokens: Option<&'a dyn TokenStore>,
    pub timeout: Duration,
    /// `Auth.moves` when this POST began.
    pub moves: u64,
}

/// The held token, unless the bridge has moved since this POST began.
fn current<'a>(web: &Web<'a>) -> Option<MutexGuard<'a, Auth>> {
    let a = web.auth.lock().unwrap();
    (a.moves == web.moves).then_some(a)
}

fn json(body: &str) -> Option<serde_json::Value> {
    serde_json::from_str(body).ok()
}

/// `Some(pin_set)` for 401 `pin_required`, else `None`.
fn pin_required(outcome: &HttpOutcome) -> Option<bool> {
    let HttpOutcome::Response { status: 401, body } = outcome else {
        return None;
    };
    let v = json(body)?;
    (v.get("error")?.as_str()? == "pin_required")
        .then(|| v.get("pin_set").and_then(|p| p.as_bool()).unwrap_or(true))
}

/// After a POST: the listing state, or `None` when it needs a log-in.
fn settled(outcome: &HttpOutcome) -> Option<ListingState> {
    match pin_required(outcome) {
        None => Some(ListingState::parse(outcome)),
        Some(false) => Some(ListingState::PinUnset),
        Some(true) => None,
    }
}

/// What a settings POST came to.
#[derive(Clone, Debug, PartialEq)]
pub enum Authed {
    Answered(HttpOutcome),
    /// 401 `pin_required`: the token is gone (or there was none), so the user
    /// logs in again with `p` and repeats the change. `pin_set: false` means
    /// the card has no PIN yet.
    PinRequired {
        pin_set: bool,
    },
}

/// The bridge's `device_id` from `GET /api/status` (also on its 503), `None`
/// from a bridge before 1.8, or the outcome when the bridge did not answer.
fn device_id(web: &Web) -> Result<Option<String>, HttpOutcome> {
    match http::get(web.host, web.port, "/api/status", web.timeout) {
        HttpOutcome::Response { body, .. } => Ok(json(&body)
            .and_then(|v| v.get("device_id")?.as_str().map(String::from))
            .filter(|d| valid_device_id(d))),
        other => Err(other),
    }
}

/// The token for the bridge with this `device_id`: the one held if that
/// bridge issued it, else the one stored for it. A token held for another
/// bridge is dropped, never sent.
fn token_for(web: &Web, device: Option<&str>) -> Option<String> {
    {
        let mut a = current(web)?;
        if a.device_id.as_deref() != device {
            a.token = None;
            a.device_id = device.map(String::from);
        }
        if a.token.is_some() {
            return a.token.clone();
        }
    }
    // The Keychain may ask the user first, so it is read without the lock.
    let stored = device
        .zip(web.tokens)
        .and_then(|(d, store)| store.load(d))
        .filter(|t| valid_token(t))?;
    let mut a = current(web)?;
    if a.device_id.as_deref() != device {
        return None;
    }
    Some(a.token.get_or_insert(stored).clone())
}

/// Forgets `sent` after the bridge refused it: from memory unless a log-in has
/// already replaced it, and from the store if it is the one stored there.
fn forget(web: &Web, device: Option<&str>, sent: Option<&str>) {
    if let Some(mut a) = current(web) {
        if a.token.as_deref() == sent {
            a.token = None;
        }
    }
    if let (Some(d), Some(store), Some(sent)) = (device, web.tokens, sent) {
        if store.load(d).as_deref() == Some(sent) {
            store.remove(d);
        }
    }
}

/// `POST path` with the token, never logging in: a settings change is not
/// retried behind the user's back.
pub fn post_authed(web: &Web, path: &str, body: &str) -> Authed {
    let device = match device_id(web) {
        Ok(d) => d,
        Err(outcome) => return Authed::Answered(outcome),
    };
    let token = token_for(web, device.as_deref());
    let outcome = http::post_json_auth(
        web.host,
        web.port,
        path,
        body,
        token.as_deref(),
        web.timeout,
    );
    if let Some(pin_set) = pin_required(&outcome) {
        forget(web, device.as_deref(), token.as_deref());
        return Authed::PinRequired { pin_set };
    }
    Authed::Answered(outcome)
}

/// `POST path` with the token. On 401 `pin_required` it logs in once with
/// `pin`, the one the user typed for this POST if any, and retries once. The
/// PIN is dropped when this returns, whatever the answer, so a wrong one is
/// never tried again and the 60 s re-post cannot run the bridge into its
/// lockout. A 1.8 bridge's app token is stored; a failure to store it comes
/// back as a warning for the user.
pub fn post_listing(
    web: &Web,
    path: &str,
    body: &str,
    pin: Option<String>,
) -> (ListingState, Option<String>) {
    let device = match device_id(web) {
        Ok(d) => d,
        Err(outcome) => return (ListingState::parse(&outcome), None),
    };
    let token = token_for(web, device.as_deref());
    let first = http::post_json_auth(
        web.host,
        web.port,
        path,
        body,
        token.as_deref(),
        web.timeout,
    );
    if let Some(state) = settled(&first) {
        return (state, None);
    }
    forget(web, device.as_deref(), token.as_deref());
    let Some(pin) = pin else {
        return (ListingState::PinNeeded, None);
    };
    let mut login_body = serde_json::json!({ "pin": pin });
    if device.is_some() {
        login_body["app"] = app_label().into();
    }
    let login = http::post_json(
        web.host,
        web.port,
        "/api/login",
        &login_body.to_string(),
        web.timeout,
    );
    let state = match &login {
        HttpOutcome::Response {
            status: 200,
            body: reply,
        } => {
            let reply = json(reply);
            let Some(fresh) = reply
                .as_ref()
                .and_then(|v| v.get("token")?.as_str().map(String::from))
                .filter(|t| valid_token(t))
            else {
                return (
                    ListingState::Failed("login: no usable token in the reply".into()),
                    None,
                );
            };
            let issuer = reply
                .as_ref()
                .and_then(|v| v.get("device_id")?.as_str().map(String::from))
                .filter(|d| valid_device_id(d));
            // After a move the token is dropped, kept neither here nor in the store.
            let held = current(web).map(|mut a| {
                a.token = Some(fresh.clone());
                // A reply without `device_id` came from the bridge we just asked.
                a.device_id = issuer.clone().or_else(|| device.clone());
            });
            let warning = match (issuer.as_deref(), web.tokens) {
                (Some(d), Some(store)) if issuer == device && held.is_some() => {
                    store.save(d, &fresh).err().map(|e| {
                        format!(
                            "The Keychain did not keep the bridge token ({e}), so the next \
                             run asks for the PIN again."
                        )
                    })
                }
                _ => None,
            };
            let retry =
                http::post_json_auth(web.host, web.port, path, body, Some(&fresh), web.timeout);
            return (settled(&retry).unwrap_or(ListingState::PinNeeded), warning);
        }
        HttpOutcome::Response { status: 401, .. } => ListingState::BadPin,
        HttpOutcome::Response { status: 429, body } => ListingState::Locked(
            json(body)
                .and_then(|v| v.get("retry_s").and_then(|r| r.as_u64()))
                .unwrap_or(0),
        ),
        HttpOutcome::Response { status: 409, .. } => ListingState::PinUnset,
        other => match ListingState::parse(other) {
            ListingState::Failed(e) => ListingState::Failed(format!("login: {e}")),
            state => state,
        },
    };
    (state, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Answers each connection with the next reply and records the requests.
    /// After the last reply it waits 200 ms and records any further connection
    /// as "unexpected".
    fn serve(replies: Vec<&'static str>) -> (u16, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for reply in replies {
                let (mut stream, _) = listener.accept().unwrap();
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
                seen.push(String::from_utf8_lossy(&raw).to_string());
                let _ = stream.write_all(reply.as_bytes());
            }
            listener.set_nonblocking(true).unwrap();
            let until = std::time::Instant::now() + Duration::from_millis(200);
            while std::time::Instant::now() < until {
                if listener.accept().is_ok() {
                    seen.push("unexpected".into());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            seen
        });
        (port, server)
    }

    const DEVICE: &str = "fedcba9876543210";
    const OTHER_DEVICE: &str = "0123456789abcdef";
    /// `GET /api/status` from a bridge before 1.8: no `device_id`.
    const OLD_STATUS: &str = "HTTP/1.0 200 OK\r\n\r\n{\"ok\":true}";
    const STATUS: &str = "HTTP/1.0 200 OK\r\n\r\n{\"ok\":true,\"device_id\":\"fedcba9876543210\"}";
    const PIN_REQUIRED: &str = "HTTP/1.0 401 Unauthorized\r\n\r\n{\"ok\":false,\"error\":\"pin_required\",\"pin_set\":true}";
    const LISTED: &str = "HTTP/1.0 200 OK\r\n\r\n{\"ok\":true,\"listed\":true}";
    const LOGGED_IN: &str = "HTTP/1.0 200 OK\r\n\r\n{\"ok\":true,\"token\":\"abc123\"}";
    const APP_LOGGED_IN: &str = "HTTP/1.0 200 OK\r\n\r\n{\"ok\":true,\"token\":\"0f0e0d0c0b0a09080706050403020100\",\"app\":\"VectorCable on Mac\",\"device_id\":\"fedcba9876543210\"}";

    const LISTING: &str = "{\"name\":\"VectorCable\"}";

    /// A token store in memory.
    #[derive(Debug, Default)]
    struct Mem(Mutex<HashMap<String, String>>);

    impl Mem {
        fn with(device_id: &str, token: &str) -> Mem {
            let m = Mem::default();
            m.save(device_id, token).unwrap();
            m
        }

        fn get(&self, device_id: &str) -> Option<String> {
            self.load(device_id)
        }
    }

    impl TokenStore for Mem {
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

    fn auth(token: Option<&str>) -> Mutex<Auth> {
        Mutex::new(Auth {
            token: token.map(String::from),
            device_id: None,
            moves: 0,
        })
    }

    fn web<'a>(port: u16, auth: &'a Mutex<Auth>, tokens: Option<&'a Mem>) -> Web<'a> {
        Web {
            host: "127.0.0.1",
            port,
            auth,
            tokens: tokens.map(|t| t as &dyn TokenStore),
            timeout: Duration::from_secs(2),
            moves: 0,
        }
    }

    fn run_with(
        port: u16,
        auth: &Mutex<Auth>,
        tokens: Option<&Mem>,
        pin: Option<&str>,
    ) -> ListingState {
        post_listing(
            &web(port, auth, tokens),
            "/api/apps",
            LISTING,
            pin.map(String::from),
        )
        .0
    }

    fn run(port: u16, auth: &Mutex<Auth>, pin: Option<&str>) -> ListingState {
        run_with(port, auth, None, pin)
    }

    fn setting(port: u16, auth: &Mutex<Auth>, tokens: Option<&Mem>) -> Authed {
        post_authed(&web(port, auth, tokens), "/api/gain", "{\"tx\":55}")
    }

    #[test]
    fn valid_pin_is_4_to_8_digits() {
        assert!(valid_pin("1234") && valid_pin("12345678"));
        assert!(!valid_pin("123") && !valid_pin("123456789") && !valid_pin("12a4"));
    }

    #[test]
    fn a_log_in_token_unfit_for_a_header_is_not_used() {
        let (port, server) = serve(vec![
            OLD_STATUS,
            PIN_REQUIRED,
            "HTTP/1.0 200 OK\r\n\r\n{\"ok\":true,\"token\":\"abc\\r\\nX-Evil: 1\"}",
        ]);
        let a = auth(None);
        assert_eq!(
            run(port, &a, Some("4321")),
            ListingState::Failed("login: no usable token in the reply".into())
        );
        assert_eq!(server.join().unwrap().len(), 3);
        assert!(a.lock().unwrap().token.is_none());
    }

    #[test]
    fn a_stored_token_must_be_fit_for_a_header() {
        assert!(valid_token("0f0e0d0c0b0a09080706050403020100") && valid_token("fake-token"));
        assert!(!valid_token("") && !valid_token("abc\r\nX-Evil: 1") && !valid_token("a b"));
        assert!(valid_device_id(DEVICE));
        assert!(!valid_device_id("FEDCBA9876543210") && !valid_device_id("fedcba98"));
    }

    /// A bridge before 1.8 logs in as before: no `app`, nothing stored.
    #[test]
    fn pin_required_logs_in_once_and_retries_the_listing_with_the_token() {
        let (port, server) = serve(vec![OLD_STATUS, PIN_REQUIRED, LOGGED_IN, LISTED]);
        let a = auth(None);
        let store = Mem::default();
        assert_eq!(
            run_with(port, &a, Some(&store), Some("4321")),
            ListingState::Listed
        );
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 4, "{seen:?}");
        assert!(seen[0].starts_with("GET /api/status "));
        assert!(seen[1].starts_with("POST /api/apps ") && !seen[1].contains("Authorization"));
        assert!(seen[1].ends_with(LISTING));
        assert!(seen[2].starts_with("POST /api/login ") && seen[2].ends_with("{\"pin\":\"4321\"}"));
        assert!(seen[3].starts_with("POST /api/apps "));
        assert!(seen[3].contains("\r\nAuthorization: Bearer abc123\r\n"));
        assert!(
            seen[3].ends_with(LISTING),
            "the retry posts the listing: {}",
            seen[3]
        );
        assert_eq!(a.lock().unwrap().token.as_deref(), Some("abc123"));
        assert!(store.0.lock().unwrap().is_empty());
    }

    #[test]
    fn a_held_token_is_sent_first_and_needs_no_log_in() {
        let (port, server) = serve(vec![OLD_STATUS, LISTED]);
        let a = auth(Some("abc123"));
        assert_eq!(run(port, &a, None), ListingState::Listed);
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[1].contains("\r\nAuthorization: Bearer abc123\r\n"));
    }

    #[test]
    fn a_stored_token_for_this_bridge_is_sent_and_needs_no_log_in() {
        let (port, server) = serve(vec![STATUS, LISTED]);
        let a = auth(None);
        let store = Mem::with(DEVICE, "storedtoken");
        assert_eq!(run_with(port, &a, Some(&store), None), ListingState::Listed);
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(seen[0].starts_with("GET /api/status "));
        assert!(seen[1].contains("\r\nAuthorization: Bearer storedtoken\r\n"));
        // A setting uses it too.
        let (port, server) = serve(vec![
            STATUS,
            "HTTP/1.0 200 OK\r\n\r\n{\"ok\":true,\"tx\":55}",
        ]);
        let got = setting(port, &auth(None), Some(&store));
        assert!(matches!(
            got,
            Authed::Answered(HttpOutcome::Response { status: 200, .. })
        ));
        assert!(server.join().unwrap()[1].contains("\r\nAuthorization: Bearer storedtoken\r\n"));
    }

    #[test]
    fn a_token_from_another_bridge_is_never_sent() {
        let (port, server) = serve(vec![STATUS, PIN_REQUIRED]);
        let a = Mutex::new(Auth {
            token: Some("othertoken".into()),
            device_id: Some(OTHER_DEVICE.into()),
            moves: 0,
        });
        let store = Mem::with(OTHER_DEVICE, "othertoken");
        assert_eq!(
            run_with(port, &a, Some(&store), None),
            ListingState::PinNeeded
        );
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert!(!seen[1].contains("Authorization"), "{}", seen[1]);
        assert!(a.lock().unwrap().token.is_none());
        assert_eq!(a.lock().unwrap().device_id.as_deref(), Some(DEVICE));
        // The other bridge's token stays stored for that bridge.
        assert_eq!(store.get(OTHER_DEVICE).as_deref(), Some("othertoken"));
    }

    #[test]
    fn a_move_forgets_the_token_and_a_post_begun_before_it_keeps_none() {
        let a = Mutex::new(Auth {
            token: Some("oldbridgetoken".into()),
            device_id: Some(DEVICE.into()),
            moves: 0,
        });
        a.lock().unwrap().bridge_moved();
        assert!(a.lock().unwrap().token.is_none());
        assert_eq!(a.lock().unwrap().device_id, None);
        assert_eq!(a.lock().unwrap().moves, 1);

        // A POST that began before the move (count 0) logs in to the old
        // bridge: the token it gets must not be held for the new address.
        let (port, server) = serve(vec![STATUS, PIN_REQUIRED, APP_LOGGED_IN, LISTED]);
        let store = Mem::default();
        let (state, _) = post_listing(
            &web(port, &a, Some(&store)),
            "/api/apps",
            LISTING,
            Some("4321".into()),
        );
        assert_eq!(state, ListingState::Listed);
        assert_eq!(server.join().unwrap().len(), 4);
        let held = a.lock().unwrap();
        assert!(held.token.is_none() && held.device_id.is_none());
        assert!(store.get(DEVICE).is_none(), "nor in the store");
    }

    #[test]
    fn a_log_in_on_a_1_8_bridge_names_the_app_and_stores_the_token() {
        let (port, server) = serve(vec![STATUS, PIN_REQUIRED, APP_LOGGED_IN, LISTED]);
        let a = auth(None);
        let store = Mem::default();
        assert_eq!(
            run_with(port, &a, Some(&store), Some("4321")),
            ListingState::Listed
        );
        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 4, "{seen:?}");
        assert!(seen[2].starts_with("POST /api/login "));
        let login: serde_json::Value =
            serde_json::from_str(seen[2].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            login,
            serde_json::json!({"pin": "4321", "app": app_label()})
        );
        assert!(seen[3].contains("\r\nAuthorization: Bearer 0f0e0d0c0b0a09080706050403020100\r\n"));
        assert_eq!(
            store.get(DEVICE).as_deref(),
            Some("0f0e0d0c0b0a09080706050403020100")
        );
        assert_eq!(a.lock().unwrap().device_id.as_deref(), Some(DEVICE));
        // Only the token is kept, never the PIN.
        let kept = store.0.lock().unwrap();
        assert_eq!(kept.len(), 1);
        assert!(kept.values().all(|t| !t.contains("4321")));
    }

    #[test]
    fn a_refused_stored_token_is_removed_from_the_store() {
        let (port, server) = serve(vec![STATUS, PIN_REQUIRED]);
        let a = auth(None);
        let store = Mem::with(DEVICE, "revokedtoken");
        assert_eq!(
            setting(port, &a, Some(&store)),
            Authed::PinRequired { pin_set: true }
        );
        assert!(server.join().unwrap()[1].contains("Bearer revokedtoken"));
        assert_eq!(store.get(DEVICE), None);
        assert!(a.lock().unwrap().token.is_none());
        // The listing drops it the same way.
        let (port, server) = serve(vec![STATUS, PIN_REQUIRED]);
        let store = Mem::with(DEVICE, "revokedtoken");
        assert_eq!(
            run_with(port, &auth(None), Some(&store), None),
            ListingState::PinNeeded
        );
        server.join().unwrap();
        assert_eq!(store.get(DEVICE), None);
    }

    #[test]
    fn no_answer_to_the_status_get_posts_nothing() {
        let (port, server) = serve(vec![""]);
        let store = Mem::with(DEVICE, "storedtoken");
        assert_eq!(
            run_with(port, &auth(None), Some(&store), Some("4321")),
            ListingState::Failed("connection dropped".into())
        );
        assert_eq!(server.join().unwrap().len(), 1);
        let (port, server) = serve(vec![""]);
        assert_eq!(
            setting(port, &auth(Some("abc123")), Some(&store)),
            Authed::Answered(HttpOutcome::ConnectionDropped)
        );
        assert_eq!(server.join().unwrap().len(), 1);
        assert_eq!(store.get(DEVICE).as_deref(), Some("storedtoken"));
    }

    #[test]
    fn a_refused_token_with_no_typed_pin_asks_for_one_without_logging_in() {
        let (port, server) = serve(vec![OLD_STATUS, PIN_REQUIRED]);
        let a = auth(Some("expired"));
        assert_eq!(run(port, &a, None), ListingState::PinNeeded);
        assert_eq!(server.join().unwrap().len(), 2);
        assert!(a.lock().unwrap().token.is_none());
    }

    #[test]
    fn bad_pin_reports_and_keeps_no_token() {
        let (port, server) = serve(vec![
            OLD_STATUS,
            PIN_REQUIRED,
            "HTTP/1.0 401 Unauthorized\r\n\r\n{\"ok\":false,\"error\":\"bad_pin\"}",
        ]);
        let a = auth(None);
        assert_eq!(run(port, &a, Some("0000")), ListingState::BadPin);
        assert_eq!(server.join().unwrap().len(), 3);
        assert!(a.lock().unwrap().token.is_none());
    }

    #[test]
    fn locked_reports_the_wait() {
        let (port, server) = serve(vec![
            OLD_STATUS,
            PIN_REQUIRED,
            "HTTP/1.0 429 Too Many Requests\r\n\r\n{\"ok\":false,\"error\":\"locked\",\"retry_s\":120}",
        ]);
        assert_eq!(
            run(port, &auth(None), Some("4321")),
            ListingState::Locked(120)
        );
        server.join().unwrap();
    }

    #[test]
    fn a_card_with_no_pin_says_so_without_logging_in() {
        let (port, server) = serve(vec![
            OLD_STATUS,
            "HTTP/1.0 401 Unauthorized\r\n\r\n{\"ok\":false,\"error\":\"pin_required\",\"pin_set\":false}",
        ]);
        assert_eq!(run(port, &auth(None), Some("4321")), ListingState::PinUnset);
        assert_eq!(server.join().unwrap().len(), 2);
        // 409 pin_unset from the log-in (the PIN was reset between the two) says the same.
        let (port, server) = serve(vec![
            OLD_STATUS,
            PIN_REQUIRED,
            "HTTP/1.0 409 Conflict\r\n\r\n{\"ok\":false,\"error\":\"pin_unset\"}",
        ]);
        assert_eq!(run(port, &auth(None), Some("4321")), ListingState::PinUnset);
        server.join().unwrap();
    }

    #[test]
    fn a_setting_goes_with_the_token_and_returns_the_answer() {
        let reply = "HTTP/1.0 200 OK\r\n\r\n{\"ok\":true,\"tx\":55}";
        let (port, server) = serve(vec![OLD_STATUS, reply]);
        let a = auth(Some("abc123"));
        let got = setting(port, &a, None);
        let seen = server.join().unwrap();
        assert!(seen[1].starts_with("POST /api/gain "));
        assert!(seen[1].contains("\r\nAuthorization: Bearer abc123\r\n"));
        assert!(seen[1].ends_with("{\"tx\":55}"));
        assert!(matches!(
            got,
            Authed::Answered(HttpOutcome::Response { status: 200, .. })
        ));
    }

    #[test]
    fn a_refusal_keeps_a_token_a_log_in_stored_meanwhile() {
        let a = auth(Some("fresh"));
        let w = web(0, &a, None);
        forget(&w, None, Some("stale"));
        assert_eq!(a.lock().unwrap().token.as_deref(), Some("fresh"));
        forget(&w, None, Some("fresh"));
        assert!(a.lock().unwrap().token.is_none());
        // The store keeps a token other than the one refused.
        let store = Mem::with(DEVICE, "fresh");
        forget(&web(0, &a, Some(&store)), Some(DEVICE), Some("stale"));
        assert_eq!(store.get(DEVICE).as_deref(), Some("fresh"));
    }

    #[test]
    fn a_setting_refused_for_the_pin_drops_the_token_and_does_not_log_in() {
        let (port, server) = serve(vec![OLD_STATUS, PIN_REQUIRED]);
        let a = auth(Some("expired"));
        assert_eq!(
            setting(port, &a, None),
            Authed::PinRequired { pin_set: true }
        );
        assert_eq!(server.join().unwrap().len(), 2);
        assert!(a.lock().unwrap().token.is_none());
    }

    #[test]
    fn open_wifi_refuses_the_log_in_as_it_refuses_the_listing() {
        let (port, server) = serve(vec![
            OLD_STATUS,
            PIN_REQUIRED,
            "HTTP/1.0 403 Forbidden\r\n\r\n{\"ok\":false,\"error\":\"wifi_open\"}",
        ]);
        assert_eq!(run(port, &auth(None), Some("4321")), ListingState::WifiOpen);
        server.join().unwrap();
    }
}
