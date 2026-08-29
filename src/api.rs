//! REST mode: the keys are driven over HTTP instead of a YAML file.
//!
//! Started with `rustydeck api`, the deck becomes a display another program
//! owns: it sets key images over HTTP and learns about presses from an event
//! stream. Nothing is executed on the deck's behalf in this mode — what a
//! press means is entirely up to the client.

use crate::config::Style;
use crate::device::{self, KeyEvent, Kind, StreamDeck};
use crate::icons::{self, IconRef};
use crate::render::Renderer;
use crate::signals;
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, StatusCode};

/// How many requests are served in parallel. Event streams get a thread of
/// their own on top of these, so a listener never occupies a worker.
const WORKERS: usize = 4;

pub struct Options {
    pub listen: String,
    /// When set, every request must carry it as `Authorization: Bearer …` or
    /// as a `token` query parameter.
    pub token: Option<String>,
    pub serial: Option<String>,
    pub brightness: u8,
}

/// What a key should show. The same shape is returned by `GET /api/keys`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeySpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// A named icon (`mdi:volume-high`) or a path on the machine running the
    /// service.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// A base64-encoded image (PNG, JPEG, GIF, BMP, WebP). Takes precedence
    /// over `icon`, and spares a remote client from needing files here.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon_color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_size: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub padding: Option<u32>,
}

impl KeySpec {
    fn style(&self) -> Style {
        Style {
            background: self.background.clone(),
            color: self.color.clone(),
            icon_color: self.icon_color.clone(),
            font: None,
            font_size: self.font_size,
            padding: self.padding,
            press_feedback: Some(false),
        }
    }

    /// Turn `image`/`icon` into something the renderer understands.
    fn icon_ref(&self) -> Result<Option<IconRef>> {
        if let Some(encoded) = &self.image {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded.trim())
                .context("`image` is not valid base64")?;
            return Ok(Some(IconRef::Data(Arc::new(bytes))));
        }
        Ok(self
            .icon
            .as_deref()
            .map(|raw| icons::parse(raw, crate::config::expand_tilde)))
    }
}

/// A key press, as sent to the event stream.
#[derive(Debug, Clone, Serialize)]
struct KeyEventMessage {
    key: u8,
    /// `down` or `up`.
    action: &'static str,
}

struct Api {
    /// Write handle. A second handle reads presses, so neither waits on the
    /// other.
    deck: Mutex<StreamDeck>,
    renderer: Renderer,
    kind: &'static Kind,
    serial: String,
    firmware: String,
    keys: Mutex<Vec<Option<KeySpec>>>,
    brightness: Mutex<u8>,
    listeners: Mutex<Vec<Sender<String>>>,
    token: Option<String>,
}

impl Api {
    /// Render a key and push it to the device.
    fn set_key(&self, index: u8, spec: &KeySpec) -> Result<()> {
        if index >= self.kind.keys {
            bail!("no such key {index} (device has {})", self.kind.keys);
        }
        let icon = spec.icon_ref()?;
        if let Some(IconRef::Unknown(name)) = &icon {
            let hints = icons::suggestions(name);
            if hints.is_empty() {
                bail!("unknown icon `mdi:{name}`");
            }
            bail!(
                "unknown icon `mdi:{name}` — did you mean {}?",
                hints.join(", ")
            );
        }

        let image =
            self.renderer
                .render(&spec.style(), spec.label.as_deref(), icon.as_ref(), false)?;
        self.deck.lock().unwrap().set_key_image(index, &image)?;
        self.keys.lock().unwrap()[index as usize] = Some(spec.clone());
        Ok(())
    }

    fn clear_key(&self, index: u8) -> Result<()> {
        if index >= self.kind.keys {
            bail!("no such key {index} (device has {})", self.kind.keys);
        }
        let blank = self.renderer.render_blank(&Style::default())?;
        self.deck.lock().unwrap().set_key_image(index, &blank)?;
        self.keys.lock().unwrap()[index as usize] = None;
        Ok(())
    }

    fn clear_all(&self) -> Result<()> {
        let blank = self.renderer.render_blank(&Style::default())?;
        let deck = self.deck.lock().unwrap();
        for index in 0..self.kind.keys {
            deck.set_key_image(index, &blank)?;
        }
        drop(deck);
        self.keys.lock().unwrap().iter_mut().for_each(|k| *k = None);
        Ok(())
    }

    fn set_brightness(&self, value: u8) -> Result<()> {
        let value = value.min(100);
        self.deck.lock().unwrap().set_brightness(value)?;
        *self.brightness.lock().unwrap() = value;
        Ok(())
    }

    /// Send an event to every listener, dropping those that went away.
    fn broadcast(&self, message: &str) {
        let mut listeners = self.listeners.lock().unwrap();
        listeners.retain(|tx| tx.send(message.to_string()).is_ok());
    }

    fn subscribe(&self) -> Receiver<String> {
        let (tx, rx) = channel();
        self.listeners.lock().unwrap().push(tx);
        rx
    }
}

pub fn run(opts: Options) -> Result<()> {
    signals::install();

    let (info, kind) = device::find_devices(opts.serial.as_deref())?
        .into_iter()
        .next()
        .context("no Stream Deck found")?;

    // Two handles on the same node: one writes images, one reads presses.
    let deck = StreamDeck::open(&info, kind)?;
    let mut input = StreamDeck::open(&info, kind)?;

    let firmware = deck.firmware_version().unwrap_or_else(|_| "?".into());
    log::info!(
        "{} connected (serial {}, firmware {firmware})",
        kind.name,
        deck.serial()
    );

    deck.reset()?;
    deck.set_brightness(opts.brightness.min(100))?;

    let api = Arc::new(Api {
        renderer: Renderer::new(kind.image_size, kind.rotate180, None),
        kind,
        serial: deck.serial().to_string(),
        firmware,
        keys: Mutex::new(vec![None; kind.keys as usize]),
        brightness: Mutex::new(opts.brightness.min(100)),
        listeners: Mutex::new(Vec::new()),
        token: opts.token,
        deck: Mutex::new(deck),
    });
    api.clear_all()?;

    let server = tiny_http::Server::http(&opts.listen)
        .map_err(|e| anyhow::anyhow!("cannot listen on {}: {e}", opts.listen))?;
    let server = Arc::new(server);
    log::info!("REST API on http://{}", opts.listen);
    if api.token.is_none() {
        log::info!("no token set — every client that can reach the port may drive the deck");
    }

    for _ in 0..WORKERS {
        let server = Arc::clone(&server);
        let api = Arc::clone(&api);
        std::thread::spawn(move || {
            while let Ok(request) = server.recv() {
                handle(&api, request);
            }
        });
    }

    // Presses are read here and handed to the listeners.
    loop {
        if signals::shutdown_requested() {
            log::info!("shutting down");
            let _ = api.deck.lock().unwrap().reset();
            return Ok(());
        }

        match input.poll_events(200) {
            Ok(events) => {
                for event in events {
                    let (key, action) = match event {
                        KeyEvent::Down(key) => (key, "down"),
                        KeyEvent::Up(key) => (key, "up"),
                    };
                    log::debug!("key {key} {action}");
                    let payload =
                        serde_json::to_string(&KeyEventMessage { key, action }).unwrap_or_default();
                    api.broadcast(&format!("event: key\ndata: {payload}\n\n"));
                }
            }
            Err(e) => {
                log::warn!("connection lost: {e:#}");
                return Ok(());
            }
        }
    }
}

fn handle(api: &Arc<Api>, request: Request) {
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let path = path.trim_end_matches('/');

    if !authorised(api, &request, query) {
        let _ = request.respond(json_response(
            401,
            &serde_json::json!({"error": "missing or wrong token"}),
        ));
        return;
    }

    // An event stream holds its connection open, so it must not sit on a worker.
    if path == "/api/events" && request.method() == &Method::Get {
        let api = Arc::clone(api);
        std::thread::spawn(move || stream_events(&api, request));
        return;
    }

    let response = route(api, path, request);
    if let Err(e) = response {
        log::debug!("request failed: {e:#}");
    }
}

fn route(api: &Arc<Api>, path: &str, mut request: Request) -> std::io::Result<()> {
    let method = request.method().clone();
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();

    let response = match (&method, segments.as_slice()) {
        (Method::Get, []) => json_response(
            200,
            &serde_json::json!({
                "name": "rustydeck",
                "version": env!("CARGO_PKG_VERSION"),
                "endpoints": [
                    "GET /api/device",
                    "GET /api/keys", "PUT /api/keys", "DELETE /api/keys",
                    "GET /api/keys/{index}", "PUT /api/keys/{index}", "DELETE /api/keys/{index}",
                    "PUT /api/brightness",
                    "GET /api/events",
                ],
            }),
        ),

        (Method::Get, ["api", "device"]) => json_response(
            200,
            &serde_json::json!({
                "model": api.kind.name,
                "serial": api.serial,
                "firmware": api.firmware,
                "keys": api.kind.keys,
                "columns": api.kind.cols,
                "rows": api.kind.rows,
                "image_size": api.kind.image_size,
                "brightness": *api.brightness.lock().unwrap(),
            }),
        ),

        (Method::Get, ["api", "keys"]) => {
            let keys = api.keys.lock().unwrap();
            let listed: BTreeMap<String, &KeySpec> = keys
                .iter()
                .enumerate()
                .filter_map(|(i, spec)| spec.as_ref().map(|s| (i.to_string(), s)))
                .collect();
            json_response(200, &serde_json::json!({ "keys": listed }))
        }

        // Set several keys at once: {"0": {...}, "3": {...}}
        (Method::Put, ["api", "keys"]) => match read_json::<BTreeMap<u8, KeySpec>>(&mut request) {
            Ok(specs) => {
                let mut failures = BTreeMap::new();
                for (index, spec) in &specs {
                    if let Err(e) = api.set_key(*index, spec) {
                        failures.insert(index.to_string(), format!("{e:#}"));
                    }
                }
                if failures.is_empty() {
                    json_response(200, &serde_json::json!({"updated": specs.len()}))
                } else {
                    json_response(400, &serde_json::json!({"errors": failures}))
                }
            }
            Err(e) => bad_request(&e),
        },

        (Method::Delete, ["api", "keys"]) => match api.clear_all() {
            Ok(()) => json_response(200, &serde_json::json!({"cleared": api.kind.keys})),
            Err(e) => bad_request(&format!("{e:#}")),
        },

        (Method::Get, ["api", "keys", index]) => match index.parse::<u8>() {
            Ok(index) if index < api.kind.keys => {
                let keys = api.keys.lock().unwrap();
                json_response(
                    200,
                    &serde_json::json!({ "key": index, "spec": keys[index as usize] }),
                )
            }
            _ => bad_request("no such key"),
        },

        (Method::Put, ["api", "keys", index]) => match index.parse::<u8>() {
            Ok(index) => match read_json::<KeySpec>(&mut request) {
                Ok(spec) => match api.set_key(index, &spec) {
                    Ok(()) => json_response(200, &serde_json::json!({"key": index})),
                    Err(e) => bad_request(&format!("{e:#}")),
                },
                Err(e) => bad_request(&e),
            },
            Err(_) => bad_request("key index must be a number"),
        },

        (Method::Delete, ["api", "keys", index]) => match index.parse::<u8>() {
            Ok(index) => match api.clear_key(index) {
                Ok(()) => json_response(200, &serde_json::json!({"key": index, "cleared": true})),
                Err(e) => bad_request(&format!("{e:#}")),
            },
            Err(_) => bad_request("key index must be a number"),
        },

        (Method::Get, ["api", "brightness"]) => json_response(
            200,
            &serde_json::json!({"brightness": *api.brightness.lock().unwrap()}),
        ),

        // {"value": 60} sets it, {"delta": -10} shifts it.
        (Method::Put, ["api", "brightness"]) => {
            match read_json::<serde_json::Value>(&mut request) {
                Ok(body) => {
                    let current = *api.brightness.lock().unwrap() as i64;
                    let wanted = match (body.get("value"), body.get("delta")) {
                        (Some(v), _) => v.as_i64(),
                        (None, Some(d)) => d.as_i64().map(|d| current + d),
                        _ => None,
                    };
                    match wanted {
                        Some(value) => {
                            let value = value.clamp(0, 100) as u8;
                            match api.set_brightness(value) {
                                Ok(()) => {
                                    json_response(200, &serde_json::json!({"brightness": value}))
                                }
                                Err(e) => bad_request(&format!("{e:#}")),
                            }
                        }
                        None => bad_request("expected `value` or `delta` as a number"),
                    }
                }
                Err(e) => bad_request(&e),
            }
        }

        _ => json_response(404, &serde_json::json!({"error": "no such endpoint"})),
    };

    request.respond(response)
}

/// Server-sent events: one line per key press, plus a comment now and then so
/// idle connections stay open through proxies.
fn stream_events(api: &Arc<Api>, request: Request) {
    struct EventReader {
        events: Receiver<String>,
        buffer: Vec<u8>,
        position: usize,
    }

    impl Read for EventReader {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            while self.position >= self.buffer.len() {
                match self.events.recv_timeout(Duration::from_secs(15)) {
                    Ok(message) => {
                        self.buffer = message.into_bytes();
                        self.position = 0;
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        self.buffer = b": keep-alive\n\n".to_vec();
                        self.position = 0;
                    }
                    Err(RecvTimeoutError::Disconnected) => return Ok(0),
                }
            }
            let take = out.len().min(self.buffer.len() - self.position);
            out[..take].copy_from_slice(&self.buffer[self.position..self.position + take]);
            self.position += take;
            Ok(take)
        }
    }

    let reader = EventReader {
        events: api.subscribe(),
        buffer: b": connected\n\n".to_vec(),
        position: 0,
    };

    let headers = vec![
        header("Content-Type", "text/event-stream"),
        header("Cache-Control", "no-cache"),
        header("Connection", "keep-alive"),
    ];
    // Length is unknown, so tiny_http streams it chunked until the client leaves.
    let _ = request.respond(Response::new(StatusCode(200), headers, reader, None, None));
}

fn authorised(api: &Api, request: &Request, query: &str) -> bool {
    let Some(expected) = &api.token else {
        return true;
    };

    let from_header = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Authorization"))
        .and_then(|h| h.value.as_str().strip_prefix("Bearer ").map(str::to_string));

    // EventSource cannot set headers, so a query parameter is allowed too.
    let from_query = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("token=").map(str::to_string));

    matches!(from_header.or(from_query), Some(given) if given == *expected)
}

fn read_json<T: serde::de::DeserializeOwned>(request: &mut Request) -> Result<T, String> {
    let mut body = String::new();
    request
        .as_reader()
        .read_to_string(&mut body)
        .map_err(|e| format!("could not read the request body: {e}"))?;
    serde_json::from_str(&body).map_err(|e| format!("invalid JSON: {e}"))
}

fn header(field: &str, value: &str) -> Header {
    Header::from_bytes(field.as_bytes(), value.as_bytes()).expect("static header is valid")
}

fn json_response(status: u16, body: &serde_json::Value) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_string(body.to_string())
        .with_status_code(StatusCode(status))
        .with_header(header("Content-Type", "application/json"))
}

fn bad_request(message: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    json_response(400, &serde_json::json!({ "error": message }))
}
