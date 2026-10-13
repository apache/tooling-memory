//! The companion: a browser extension beside the launcher's page that fetches pages for
//! a session with the browser's own cookies (PLAN M19, third cut).
//!
//! A tool whose grant says `browser = true` is fetched in the owner's browser instead of
//! by the launcher or a tool server, so a page the owner is signed in to answers as it
//! would to them, and no credential is copied into a grant for it. The extension holds
//! one websocket to this server, [`WS_PATH`], paired with the key the launcher's page
//! handed it over the window. The launcher asks for a fetch at [`FETCH_PATH`] with the
//! token, which no page holds: a page's key cannot drive the browser's cookies anywhere.
//! The ask carries the URL, which the memory server has already decided by its policy,
//! and the hosts the session may reach, which the extension refuses to go outside; the
//! answer carries the URL that answered, which the memory server decides again before
//! anything is kept. The page shows the companion at [`STATUS_PATH`].
//!
//! The extension itself is `crates/web/companion`, loaded unpacked from there; one
//! browser connects at a time, the newest.

use crate::App;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// The extension's websocket.
pub const WS_PATH: &str = "/ws/companion";
/// `POST`, with the token: a fetch for the launcher.
pub const FETCH_PATH: &str = "/api/companion/fetch";
/// `GET`: whether a companion is connected, for the page.
pub const STATUS_PATH: &str = "/api/companion";

/// How long a fetch may take in the browser before the launcher is told it did not come.
pub const WAIT: Duration = Duration::from_secs(60);
/// How often the server speaks to a quiet extension, so its worker is not put to sleep
/// with the socket open: Chrome stops a worker idle for thirty seconds.
const PING: Duration = Duration::from_secs(20);

/// What the launcher asks for, as the extension reads it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Ask {
    /// The page, decided by the policy already.
    pub url: String,
    /// The hosts the session may reach at its label: the extension goes nowhere else,
    /// whatever answers.
    #[serde(default)]
    pub hosts: Vec<String>,
    /// How much of the body to read.
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
}

fn default_max_bytes() -> usize {
    2_000_000
}

/// Where a companion is: connected, and since when.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    /// Whether an extension holds the socket now.
    pub connected: bool,
    /// When it connected, RFC 3339.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// The extension's version, as it said hello.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Fetches asked of it since this server started, answered or not.
    pub fetches: u64,
}

/// Why an ask got no answer.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    /// No extension is connected.
    NotConnected,
    /// The extension went away before it answered.
    Gone,
    /// It did not answer within [`WAIT`].
    Timeout,
}

struct Link {
    tx: mpsc::UnboundedSender<String>,
    since: String,
    version: Option<String>,
    generation: u64,
}

#[derive(Default)]
struct Inner {
    link: Option<Link>,
    pending: HashMap<u64, oneshot::Sender<Value>>,
    next_id: u64,
    next_generation: u64,
    fetches: u64,
}

/// The one companion a server has, or none.
#[derive(Default)]
pub struct Companion {
    inner: Mutex<Inner>,
}

impl Companion {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Where the companion is.
    #[must_use]
    pub fn status(&self) -> Status {
        let i = self.inner();
        Status {
            connected: i.link.is_some(),
            since: i.link.as_ref().map(|l| l.since.clone()),
            version: i.link.as_ref().and_then(|l| l.version.clone()),
            fetches: i.fetches,
        }
    }

    /// An extension connected: its outbound queue. A companion already there is let go,
    /// its pending asks answered as gone; the newest browser is the one.
    fn attach(&self, tx: mpsc::UnboundedSender<String>) -> u64 {
        let mut i = self.inner();
        i.next_generation += 1;
        let generation = i.next_generation;
        i.pending.clear();
        i.link = Some(Link {
            tx,
            since: crate::now(),
            version: None,
            generation,
        });
        generation
    }

    /// The extension of `generation` went away; a newer one, if any, stays.
    fn detach(&self, generation: u64) {
        let mut i = self.inner();
        if i.link.as_ref().is_some_and(|l| l.generation == generation) {
            i.link = None;
            i.pending.clear();
        }
    }

    /// A message from the extension of `generation`: its hello, or an answer.
    fn incoming(&self, generation: u64, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let mut i = self.inner();
        if !i.link.as_ref().is_some_and(|l| l.generation == generation) {
            return;
        }
        if v["type"] == "hello" {
            if let Some(l) = i.link.as_mut() {
                l.version = v["version"].as_str().map(str::to_string);
            }
            return;
        }
        if let Some(id) = v["id"].as_u64() {
            if let Some(reply) = i.pending.remove(&id) {
                let _ = reply.send(v);
            }
        }
    }

    /// Ask the connected extension to fetch, and wait for what it says.
    ///
    /// # Errors
    ///
    /// No extension connected, one that went away, or no answer within [`WAIT`].
    pub async fn ask(&self, ask: &Ask) -> Result<Value, Refusal> {
        let (reply, answer) = oneshot::channel();
        {
            let mut i = self.inner();
            let Some(tx) = i.link.as_ref().map(|l| l.tx.clone()) else {
                return Err(Refusal::NotConnected);
            };
            i.next_id += 1;
            i.fetches += 1;
            let id = i.next_id;
            let mut message = serde_json::to_value(ask).unwrap_or_default();
            message["id"] = json!(id);
            message["type"] = json!("fetch");
            if tx.send(message.to_string()).is_err() {
                return Err(Refusal::Gone);
            }
            i.pending.insert(id, reply);
        }
        match tokio::time::timeout(WAIT, answer).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(Refusal::Gone),
            Err(_) => Err(Refusal::Timeout),
        }
    }
}

/// `GET /ws/companion`: the extension's socket. It offers its key as a page does, as a
/// second subprotocol; the answer names the first.
pub(crate) async fn ws(State(app): State<App>, up: WebSocketUpgrade) -> Response {
    up.protocols([crate::access::PROTOCOL])
        .on_upgrade(move |socket| serve(app.companion, socket))
}

async fn serve(c: std::sync::Arc<Companion>, socket: WebSocket) {
    let (mut tx, mut rx) = socket.split();
    let (out, mut queue) = mpsc::unbounded_channel::<String>();
    let generation = c.attach(out);
    let sender = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING);
        ping.tick().await;
        loop {
            let text = tokio::select! {
                m = queue.recv() => match m {
                    Some(m) => m,
                    None => break,
                },
                _ = ping.tick() => json!({"type": "ping"}).to_string(),
            };
            if tx.send(Message::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    while let Some(Ok(msg)) = rx.next().await {
        match msg {
            Message::Text(t) => c.incoming(generation, &t),
            Message::Close(_) => break,
            _ => {}
        }
    }
    sender.abort();
    c.detach(generation);
}

/// `GET /api/companion`: where the companion is.
pub(crate) async fn status(State(app): State<App>) -> Json<Status> {
    Json(app.companion.status())
}

/// `POST /api/companion/fetch`, with the token: what the extension says for the ask.
pub(crate) async fn fetch(State(app): State<App>, Json(ask): Json<Ask>) -> Response {
    match app.companion.ask(&ask).await {
        Ok(v) => Json(v).into_response(),
        Err(Refusal::NotConnected) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "no companion is connected: install the extension from crates/web/companion and open the amem page in that browser",
        )
            .into_response(),
        Err(Refusal::Gone) => (
            StatusCode::BAD_GATEWAY,
            "the companion went away before it answered",
        )
            .into_response(),
        Err(Refusal::Timeout) => (
            StatusCode::GATEWAY_TIMEOUT,
            "the companion did not answer within a minute",
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_ask_waits_for_the_connected_extension_and_no_other() {
        let c = Companion::default();
        let ask = Ask {
            url: "https://a.example/p".into(),
            hosts: vec!["a.example".into()],
            max_bytes: 10,
        };
        assert_eq!(c.ask(&ask).await, Err(Refusal::NotConnected));
        assert!(!c.status().connected);

        let (tx, mut rx) = mpsc::unbounded_channel();
        let first = c.attach(tx);
        c.incoming(first, r#"{"type":"hello","version":"0.1.0"}"#);
        let status = c.status();
        assert!(status.connected && status.version.as_deref() == Some("0.1.0"));
        let asking = tokio::spawn(async move { c.ask(&ask).await });
        // what the extension gets: the ask, numbered and typed
        let sent: Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        assert_eq!(sent["type"], "fetch");
        assert_eq!(sent["url"], "https://a.example/p");
        assert_eq!(sent["hosts"][0], "a.example");
        assert_eq!(sent["max_bytes"], 10);
        asking.abort();
        let _ = asking.await;

        // an answer from a generation that is gone changes nothing; the live one's lands
        let c = Companion::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let gen = c.attach(tx);
        let c = std::sync::Arc::new(c);
        let c2 = c.clone();
        let ask2 = Ask {
            url: "https://a.example/q".into(),
            hosts: vec![],
            max_bytes: 1,
        };
        let asking = tokio::spawn(async move { c2.ask(&ask2).await });
        let sent: Value = serde_json::from_str(&rx.recv().await.unwrap()).unwrap();
        let id2 = sent["id"].as_u64().unwrap();
        c.incoming(gen + 1, &json!({"id": id2, "status": 200}).to_string());
        c.incoming(gen, &json!({"id": id2 + 7, "status": 500}).to_string());
        c.incoming(gen, "not json");
        c.incoming(
            gen,
            &json!({"id": id2, "status": 200, "body": "hi"}).to_string(),
        );
        let got = asking.await.unwrap().unwrap();
        assert_eq!(got["body"], "hi");
        assert_eq!(c.status().fetches, 1);

        // the extension going away answers what it owed as gone
        let (tx, mut rx) = mpsc::unbounded_channel();
        let gen = c.attach(tx);
        let c2 = c.clone();
        let ask3 = Ask {
            url: "https://a.example/r".into(),
            hosts: vec![],
            max_bytes: 1,
        };
        let asking = tokio::spawn(async move { c2.ask(&ask3).await });
        rx.recv().await.unwrap();
        c.detach(gen);
        assert_eq!(asking.await.unwrap(), Err(Refusal::Gone));
        assert!(!c.status().connected);
        // an older generation detaching does not take the newer one down
        let (tx, _rx) = mpsc::unbounded_channel();
        let newer = c.attach(tx);
        c.detach(newer - 1);
        assert!(c.status().connected);
    }
}
