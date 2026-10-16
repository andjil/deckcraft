//! The `postMessage` bridge to a same-origin parent page (`?host=parent`), for pages that embed
//! DeckCraft in an `<iframe>`. The xrm fork's port of gridcraft-web's `host.rs`: the same shape and
//! the same messages, with the `deckcraft:` prefix. Messages are plain objects with a `type`:
//!
//! - `deckcraft:ready` (to the parent): `{ version, protocol }`, the app is listening; send a file now.
//! - `deckcraft:open` (from the parent): `{ name, bytes }`, bytes an `ArrayBuffer` or `Uint8Array`.
//!   It opens like a presentation picked with File › Open or dropped on the window (a picture,
//!   audio or video file is inserted on the current slide, as a drop would).
//! - `deckcraft:save` (to the parent): `{ name, bytes }` (a transferred `ArrayBuffer`) for every
//!   file DeckCraft would otherwise download: Save and Save As (.pptx or .deckcraft) and the
//!   exports. The parent stores it and reports failures itself; a presentation handed over counts
//!   as saved under that name.
//! - `deckcraft:dirty` (to the parent): `{ dirty }` whenever unsaved changes appear or go away.
//! - `deckcraft:failed` (to the parent): `{ error }` when the app couldn't start.
//! - `deckcraft:command` (from the parent): `{ id, command, params }` runs one of DeckCraft's own
//!   commands (`file.new`, `slide.new`, …) or a bridge command (`host.save` with `params.format`
//!   pptx or deckcraft, `host.state`); the answer is `deckcraft:result` `{ id, ok, result }` or
//!   `{ id, ok: false, error }`.
//! - `deckcraft:saved` (from the parent): `{ name }`, the file handed over was stored.
//! - `deckcraft:save-failed` (from the parent): `{ name, error }`, it was not: the presentation
//!   counts as unsaved again.
//!
//! Only the parent window on the page's own origin is heard or answered. In host mode the page
//! starts with an empty presentation instead of the sample deck, and the parent guards leaving
//! the page.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use deckcraft_ui_egui::{Services, SlideApp};
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;

/// Saving one of these hands the whole presentation to the parent, which stores it: it counts as
/// saved under that name, as it would after Save on the desktop.
const PRESENTATION_EXTS: &[&str] = &["pptx", "deckcraft"];

/// The protocol version announced in `deckcraft:ready`.
const PROTOCOL: u32 = 1;

/// A message from the parent that the frame handles (the app lives on the UI thread).
enum Inbound {
    /// `deckcraft:open` `{ name, bytes }`: open a presentation or insert a media file.
    Open { name: String, bytes: Vec<u8> },
    /// `deckcraft:command` `{ id, command, params }`: run a command, answer with `deckcraft:result`.
    Command { id: JsValue, command: String, params: serde_json::Value },
    /// `deckcraft:saved` `{ name }`: the parent stored the file it was handed.
    Saved { name: String },
    /// `deckcraft:save-failed` `{ name, error }`: the parent couldn't store it.
    SaveFailed { name: String, error: String },
}

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    origin: String,
    /// Messages from the parent waiting for the frame.
    inbound: Rc<RefCell<Vec<Inbound>>>,
    /// Names of the files handed to the parent since the last frame.
    saved: Rc<RefCell<Vec<String>>>,
    /// The unsaved-changes state last reported to the parent.
    reported: Rc<Cell<Option<bool>>>,
}

impl Bridge {
    /// `None` when the page isn't framed (there is no parent to talk to).
    pub fn new() -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let origin = window.location().origin().ok()?;
        Some(Self { window, parent, origin, inbound: Rc::default(), saved: Rc::default(), reported: Rc::default() })
    }

    /// Send saved and exported files to the parent instead of downloading them, and hear files
    /// and commands from it.
    pub fn connect(&self, services: &mut Services, ctx: egui::Context) {
        self.listen(ctx);
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| {
            if let Err(e) = bridge.post_save(name, bytes) {
                log::error!("couldn't hand {name} to the page: {e}");
            }
        }));
        let bridge = self.clone();
        services.write = Some(Box::new(move |name: &str, bytes: &[u8]| bridge.post_save(name, bytes)));
    }

    pub fn post_ready(&self) {
        self.send(&[("type", "deckcraft:ready".into()), ("version", env!("CARGO_PKG_VERSION").into()), ("protocol", PROTOCOL.into())]);
    }

    pub fn post_failed(&self, error: &str) {
        self.send(&[("type", "deckcraft:failed".into()), ("error", error.into())]);
    }

    /// After each frame: the parent's files, commands and confirmations are applied; a
    /// presentation handed to the parent this frame counts as saved under the name it was saved
    /// as; and the parent hears when unsaved changes appear or go away.
    pub fn sync(&self, app: &mut SlideApp) {
        self.run_inbound(app);
        let saved = std::mem::take(&mut *self.saved.borrow_mut());
        if let Some(name) = saved.iter().rev().find(|name| is_presentation(name))
            && let Some(doc) = app.session.active_mut()
        {
            doc.saved_doc = doc.doc.clone();
            if doc.path.is_none()
                && let Some(stem) = std::path::Path::new(name).file_stem()
            {
                doc.title = stem.to_string_lossy().to_string();
            }
        }
        let dirty = app.session.documents().iter().any(|d| d.is_dirty());
        if self.reported.get() != Some(dirty) && self.send(&[("type", "deckcraft:dirty".into()), ("dirty", dirty.into())]) {
            self.reported.set(Some(dirty));
        }
    }

    fn post_save(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let message = message(&[("type", "deckcraft:save".into()), ("name", name.as_str().into()), ("bytes", buffer.clone().into())])
            .ok_or_else(|| "couldn't build the message".to_string())?;
        self.parent
            .post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer))
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))?;
        self.saved.borrow_mut().push(name);
        Ok(())
    }

    fn listen(&self, ctx: egui::Context) {
        let parent = self.parent.clone();
        let origin = self.origin.clone();
        let inbound = self.inbound.clone();
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            if event.origin() != origin || !event.source().is_some_and(|s| js_sys::Object::is(&s, &parent)) {
                return;
            }
            if let Some(message) = Inbound::parse(&event.data()) {
                inbound.borrow_mut().push(message);
                ctx.request_repaint();
            }
        });
        if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
            // The listener lives as long as the page.
            on_message.forget();
        }
    }

    /// The parent's messages since the last frame: files open, commands run against the app and
    /// are answered, a failed store makes the presentation unsaved again.
    fn run_inbound(&self, app: &mut SlideApp) {
        let queue = std::mem::take(&mut *self.inbound.borrow_mut());
        for message in queue {
            match message {
                // The same path as a dropped file: presentations open, media lands on the slide.
                Inbound::Open { name, bytes } => app.open_or_insert_bytes(&name, &bytes),
                Inbound::Command { id, command, params } => {
                    let result = self.run_command(app, &command, params);
                    self.post_result(&id, result);
                }
                Inbound::Saved { name } => log::debug!("the page stored {name}"),
                Inbound::SaveFailed { name, error } => {
                    log::error!("the page couldn't store {name}: {error}");
                    app.set_status(format!("Couldn't store {name}: {error}"));
                    // Counted as saved when handed over (`sync`); a fresh copy as the saved state
                    // makes it dirty again without touching its content.
                    if let Some(doc) = app.session.active_mut() {
                        doc.saved_doc = Arc::new((*doc.doc).clone());
                    }
                }
            }
        }
    }

    /// `host.*` commands are the bridge's own; anything else is one of DeckCraft's commands
    /// (`crates/engine/src/cmd/*`, the UI commands), as the control channel runs them.
    fn run_command(&self, app: &mut SlideApp, command: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        match command {
            // Save As without a picker hands the presentation to the parent (`post_save`) in the
            // asked format, pptx unless `params.format` says deckcraft.
            "host.save" => {
                let format = params.get("format").and_then(serde_json::Value::as_str).unwrap_or("pptx");
                let title = app.session.active().map(|d| d.title()).unwrap_or_else(|| "Presentation".into());
                app.save_as_dialog(format);
                Ok(serde_json::json!({"saved": format!("{title}.{}", if format == "pptx" { "pptx" } else { "deckcraft" })}))
            }
            "host.state" => Ok(serde_json::json!({
                "dirty": app.session.documents().iter().any(|d| d.is_dirty()),
                "title": app.session.active().map(|d| d.title()),
                "protocol": PROTOCOL,
            })),
            other => app.run(other, params),
        }
    }

    fn post_result(&self, id: &JsValue, result: Result<serde_json::Value, String>) {
        let fields: Vec<(&str, JsValue)> = match result {
            Ok(value) => vec![("type", "deckcraft:result".into()), ("id", id.clone()), ("ok", true.into()), ("result", to_js(&value))],
            Err(error) => vec![("type", "deckcraft:result".into()), ("id", id.clone()), ("ok", false.into()), ("error", error.as_str().into())],
        };
        self.send(&fields);
    }

    /// Post a message to the parent; false if it couldn't be sent.
    fn send(&self, fields: &[(&str, JsValue)]) -> bool {
        message(fields).is_some_and(|m| self.parent.post_message(&m, &self.origin).is_ok())
    }
}

impl Inbound {
    fn parse(data: &JsValue) -> Option<Self> {
        match string(data, "type")?.as_str() {
            "deckcraft:open" => {
                let name = string(data, "name").filter(|n| !n.trim().is_empty()).unwrap_or_else(|| "Presentation.pptx".to_string());
                Some(Self::Open { name, bytes: bytes(data)? })
            }
            "deckcraft:command" => {
                let command = string(data, "command")?;
                let id = js_sys::Reflect::get(data, &JsValue::from_str("id")).unwrap_or(JsValue::NULL);
                let params = json_field(data, "params").unwrap_or_else(|| serde_json::json!({}));
                Some(Self::Command { id, command, params })
            }
            "deckcraft:saved" => Some(Self::Saved { name: string(data, "name").unwrap_or_default() }),
            "deckcraft:save-failed" => {
                Some(Self::SaveFailed { name: string(data, "name").unwrap_or_default(), error: string(data, "error").unwrap_or_default() })
            }
            _ => None,
        }
    }
}

fn is_presentation(name: &str) -> bool {
    name.rsplit_once('.').is_some_and(|(_, ext)| PRESENTATION_EXTS.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
    let m = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
    }
    Some(m)
}

fn string(data: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
}

/// `bytes` as an `ArrayBuffer` or `Uint8Array`.
fn bytes(data: &JsValue) -> Option<Vec<u8>> {
    let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        Some(array.to_vec())
    } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
        Some(js_sys::Uint8Array::new(&bytes).to_vec())
    } else {
        None
    }
}

/// A JSON field of a message as serde JSON (through `JSON.stringify`; absent or null → `None`).
fn json_field(data: &JsValue, key: &str) -> Option<serde_json::Value> {
    let value = js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?;
    if value.is_undefined() || value.is_null() {
        return None;
    }
    let text: String = js_sys::JSON::stringify(&value).ok()?.into();
    serde_json::from_str(&text).ok()
}

/// serde JSON as a JavaScript value (`null` when it can't be converted).
fn to_js(value: &serde_json::Value) -> JsValue {
    js_sys::JSON::parse(&value.to_string()).unwrap_or(JsValue::NULL)
}
