//! A WhatsApp account for the SwiftUI app, as GTK's `chat_whatsapp.rs` and
//! `whatsapp_link.rs` hold one: linking a new device by QR or by code, its
//! conversations and timeline, the message actions WhatsApp allows, its photos
//! and attachments, and its calls carried natively, audio only.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rv_core::session::{Connection, SessionInfo};
use rv_core::whatsapp::{self, Connector};
use rv_core::whatsapp_session::{KeySource, Notice, WaSession, WaTarget};
use tokio::sync::broadcast::error::RecvError;

use crate::accounts::{self, Dirs};
use crate::model::{self, MessageItem, RoomGroup, RvError};
use crate::{
    Client, ConnectionState, Event, Incoming, Listener, MediaData, MessageAction, RingingCall, blocking, on_tokio,
    runtime,
};

/// The prefix of a person's or a group's photo, by JID, in a media path.
const PHOTO_LINK: &str = "wa-photo:";
/// WhatsApp's own windows: recipients ignore later edits and revocations.
const EDIT_WINDOW_MS: i64 = 15 * 60 * 1000;
const DELETE_WINDOW_MS: i64 = 48 * 60 * 60 * 1000;

/// A photo's media path; it moves when the photo does, so a changed one is fetched again.
fn photo_link(jid: &str, photo: &[u8]) -> String {
    format!("{PHOTO_LINK}{jid}?{}:{:p}", photo.len(), photo.as_ptr())
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// The key that wraps a WhatsApp state, under its own Keychain item, where the
/// GTK app keeps it on macOS. Made on first use; a state already on disk
/// without its key fails closed.
async fn state_key(entry: String, has_state: bool) -> Result<String, String> {
    blocking(move || {
        let item = keyring::Entry::new(accounts::KEYCHAIN_SERVICE, &entry)
            .map_err(|_| "secure_storage_unavailable".to_owned())?;
        match item.get_password() {
            Ok(key) => return Ok(key),
            Err(keyring::Error::NoEntry) => {}
            Err(_) => return Err("secure_storage_unavailable".to_owned()),
        }
        if has_state {
            return Err("secure_storage_unavailable".into());
        }
        let key = whatsapp::storage_key()?;
        item.set_password(&key).map_err(|_| "secure_storage_unavailable".to_owned())?;
        Ok(key)
    })
    .await
}

pub(crate) fn keys() -> KeySource {
    Arc::new(|entry: String, has_state: bool| Box::pin(state_key(entry, has_state)))
}

/// Forgets a WhatsApp state: its wrapping key and its sealed file. Blocking.
pub(crate) fn forget(data: &Path, info: &SessionInfo) {
    let Some(state) = whatsapp::state_id(&info.base_url) else { return };
    let (path, entry) = whatsapp::state_location(data, state);
    if let Ok(item) = keyring::Entry::new(accounts::KEYCHAIN_SERVICE, &entry) {
        let _ = item.delete_credential();
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Starts a WhatsApp account's session, for the supervisor.
pub(crate) fn start(dirs: &Dirs, info: &SessionInfo) -> Arc<WaSession> {
    let _ = std::fs::create_dir_all(&dirs.data);
    WaSession::start(info.clone(), whatsapp::Runner::from_env(), dirs.data.clone(), keys())
}

/// What the linking sheet shows.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct WhatsAppLinkState {
    /// The code to draw as a QR, when one is up.
    pub qr: Option<String>,
    /// The code to type on the phone after asking by number, as WhatsApp
    /// shows it (two groups of four).
    pub pairing_code: Option<String>,
    /// Where the linking is, in words.
    pub status: String,
    /// The QR expired or the connection failed: it may be tried again.
    pub retry: bool,
    /// Asking for a code by number is possible (again).
    pub can_ask: bool,
    /// The phone linked the device: `finish` saves the account.
    pub linked: bool,
    /// The helper is gone: nothing more will come.
    pub over: bool,
}

/// A device being linked to a phone, on a fresh sealed state.
#[derive(uniffi::Object)]
pub struct WhatsAppLink {
    state: String,
    data: PathBuf,
    connector: Mutex<Option<Arc<Connector>>>,
    asked: Mutex<bool>,
    failed: Mutex<bool>,
    done: Mutex<bool>,
}

#[uniffi::export]
impl Client {
    /// Starts linking a new WhatsApp device; its QR comes through `state`.
    pub async fn whatsapp_link(&self) -> Result<Arc<WhatsAppLink>, RvError> {
        let state = whatsapp::new_state_id().map_err(RvError::local)?;
        let link = Arc::new(WhatsAppLink {
            state: state.clone(),
            data: self.dirs.data.clone(),
            connector: Mutex::default(),
            asked: Mutex::new(false),
            failed: Mutex::new(false),
            done: Mutex::new(false),
        });
        let data = self.dirs.data.clone();
        let started = on_tokio(async move {
            let _ = std::fs::create_dir_all(&data);
            let (path, entry) = whatsapp::state_location(&data, &state);
            let key = state_key(entry, false).await?;
            Connector::start(&whatsapp::Runner::from_env(), &path, key).await
        })
        .await;
        match started {
            Ok(connector) => *link.connector.lock().unwrap() = Some(connector),
            Err(_) => *link.failed.lock().unwrap() = true,
        }
        Ok(link)
    }

    /// The linked device made an account, saved and opened.
    pub async fn whatsapp_finish(&self, link: Arc<WhatsAppLink>) -> Result<Arc<WhatsAppChat>, RvError> {
        let connector = link.connector().ok_or_else(|| RvError::local("wa.start_failed"))?;
        let s = connector.snapshot();
        let Some((jid, name)) = s.account.filter(|_| s.state == "connected") else {
            return Err(RvError::local("wa.connecting"));
        };
        let info = whatsapp::account_info(&link.state, &jid, &name);
        let (dirs, saved) = (self.dirs.clone(), info.clone());
        blocking(move || accounts::save(&dirs, &saved, None)).await.map_err(RvError::local)?;
        *link.done.lock().unwrap() = true;
        rv_core::whatsapp_session::handoff(&link.state, connector);
        accounts::set_active(&self.dirs, &info);
        self.start_whatsapp(info)
    }

    /// Opens a WhatsApp account already linked on this machine.
    pub async fn whatsapp_resume(&self, key: String) -> Result<Arc<WhatsAppChat>, RvError> {
        let dirs = self.dirs.clone();
        let info = blocking(move || accounts::load_all(&dirs))
            .await
            .into_iter()
            .find(|i| accounts::key(i) == key)
            .ok_or_else(|| RvError::local("unknown account"))?;
        accounts::set_active(&self.dirs, &info);
        self.start_whatsapp(info)
    }
}

impl Client {
    fn start_whatsapp(&self, info: SessionInfo) -> Result<Arc<WhatsAppChat>, RvError> {
        let (session, opening) = match self.accounts.open_counted(&info).map_err(RvError::local)? {
            (rv_core::provider::Chat::WhatsApp(session), opening) => (session, opening),
            _ => return Err(RvError::local("not a WhatsApp account")),
        };
        Ok(Arc::new(WhatsAppChat {
            session,
            dirs: self.dirs.clone(),
            accounts: self.accounts.clone(),
            opening,
            forward: Mutex::default(),
        }))
    }
}

impl WhatsAppLink {
    fn connector(&self) -> Option<Arc<Connector>> {
        self.connector.lock().unwrap().clone()
    }
}

#[uniffi::export]
impl WhatsAppLink {
    pub fn state(&self) -> WhatsAppLinkState {
        let Some(connector) = self.connector() else {
            return WhatsAppLinkState {
                qr: None,
                pairing_code: None,
                status: rv_core::i18n::t("wa.start_failed").to_owned(),
                retry: false,
                can_ask: false,
                linked: false,
                over: true,
            };
        };
        let s = connector.snapshot();
        let refused = matches!(s.error.as_deref(), Some("phone_invalid" | "pairing_code_failed"));
        if refused {
            *self.asked.lock().unwrap() = false;
        }
        let status = match s.error.as_deref() {
            Some("client_outdated") => rv_core::i18n::t("wa.client_outdated").to_owned(),
            Some(code @ ("phone_invalid" | "pairing_code_failed")) => {
                rv_core::i18n::t(&format!("wa.{code}")).to_owned()
            }
            Some(_) if s.state == "error" => rv_core::i18n::t("wa.connection_failed").to_owned(),
            _ => rv_core::i18n::t(&format!("wa.{}", s.state)).to_owned(),
        };
        WhatsAppLinkState {
            qr: s.qr.map(|(code, _)| code),
            pairing_code: s
                .pairing_code
                .map(|code| if code.len() == 8 { format!("{}-{}", &code[..4], &code[4..]) } else { code }),
            status,
            retry: matches!(s.state.as_str(), "qr_expired" | "error" | "offline"),
            can_ask: !*self.asked.lock().unwrap(),
            linked: s.account.is_some() && s.state == "connected",
            over: s.exited,
        }
    }

    /// Waits for the next change, at most `limit_ms`.
    pub async fn changed(&self, limit_ms: u64) {
        let Some(connector) = self.connector() else { return };
        let mut changes = connector.subscribe();
        on_tokio(async move {
            let _ = tokio::time::timeout(Duration::from_millis(limit_ms), changes.changed()).await;
        })
        .await
    }

    /// Asks for a code to type on the phone instead of scanning; false when
    /// the number holds no digit or the helper refused it at once.
    pub fn pair_phone(&self, number: String) -> bool {
        let digits: String = number.chars().filter(char::is_ascii_digit).collect();
        let Some(connector) = self.connector() else { return false };
        if digits.is_empty() || connector.pair_phone(digits).is_err() {
            return false;
        }
        *self.asked.lock().unwrap() = true;
        true
    }

    pub fn reconnect(&self) {
        if let Some(connector) = self.connector() {
            let _ = connector.reconnect();
        }
    }

    /// Abandoned: the helper stops and the half-made state goes.
    pub async fn cancel(&self) {
        if *self.done.lock().unwrap() {
            return;
        }
        let connector = self.connector.lock().unwrap().take();
        let info = whatsapp::account_info(&self.state, "", "");
        let data = self.data.clone();
        on_tokio(async move {
            if let Some(connector) = connector {
                connector.stop();
                let mut changes = connector.subscribe();
                let _ = tokio::time::timeout(Duration::from_secs(8), async {
                    while !connector.exited() {
                        if changes.changed().await.is_err() {
                            return;
                        }
                    }
                })
                .await;
            }
            let _ = tokio::task::spawn_blocking(move || forget(&data, &info)).await;
        })
        .await
    }
}

/// Someone or a group to start a conversation with.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct WhatsAppPerson {
    pub jid: String,
    pub name: String,
}

#[derive(uniffi::Object)]
pub struct WhatsAppChat {
    session: Arc<WaSession>,
    dirs: Arc<Dirs>,
    accounts: rv_core::supervisor::Supervisor,
    opening: u64,
    forward: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for WhatsAppChat {
    fn drop(&mut self) {
        if let Some(f) = self.forward.lock().unwrap().take() {
            f.abort();
        }
    }
}

fn connection(c: Connection) -> ConnectionState {
    match c {
        Connection::Offline => ConnectionState::Offline,
        Connection::Connecting => ConnectionState::Connecting,
        Connection::Online => ConnectionState::Online,
    }
}

/// What the account has to say after a change, in words: a send given back,
/// one whose outcome is unknown, an action WhatsApp refused.
fn sayings(session: &WaSession, last_error: &mut Option<String>) -> Vec<Event> {
    let mut said = Vec::new();
    for (chat, text) in session.returned_drafts() {
        if session.draft(&chat).trim().is_empty() {
            session.set_draft(&chat, &text);
        }
        said.push(Event::Returned { rid: chat, text, notice: rv_core::i18n::t("wa.send_failed").to_owned() });
    }
    if session.take_uncertain() > 0 {
        said.push(Event::Notice { text: rv_core::i18n::t("wa.send_uncertain").to_owned() });
    }
    match session.snapshot().error.filter(|_| session.connection() == Connection::Online) {
        Some(code)
            if matches!(code.as_str(), "action_failed" | "edit_expired" | "delete_expired" | "logout_failed") =>
        {
            if last_error.as_deref() != Some(code.as_str()) {
                said.push(Event::Notice { text: rv_core::i18n::t(&format!("wa.{code}")).to_owned() });
                *last_error = Some(code);
            }
        }
        Some(_) => {}
        None => *last_error = None,
    }
    said
}

#[uniffi::export]
impl WhatsAppChat {
    /// The account leaves the screen; it stays linked in the background.
    pub fn shutdown(&self) {
        if let Some(task) = self.forward.lock().unwrap().take() {
            task.abort();
        }
        self.session.set_available(false);
        self.accounts.leave(self.opening);
    }

    pub fn account(&self) -> crate::model::Account {
        crate::account(&self.session.info)
    }

    pub fn connection(&self) -> ConnectionState {
        connection(self.session.connection())
    }

    /// Replaces the previous listener. An unlinked device is forgotten here,
    /// before the listener hears of it.
    pub fn set_listener(&self, listener: Arc<dyn Listener>) {
        let mut changes = self.session.subscribe();
        let mut notices = self.session.notices();
        let session = Arc::downgrade(&self.session);
        let (dirs, supervisor) = (self.dirs.clone(), self.accounts.clone());
        let task = runtime().spawn(async move {
            let mut last = None;
            let mut last_error = None;
            // A typing WhatsApp never ends by itself fades: look again then.
            let mut fade = tokio::time::interval(Duration::from_secs(5));
            loop {
                let events = tokio::select! {
                    c = changes.changed() => {
                        if c.is_err() { return }
                        let Some(s) = session.upgrade() else { return };
                        let now = s.connection();
                        let mut events = Vec::new();
                        if last != Some(now) {
                            last = Some(now);
                            events.push(Event::Connection { state: connection(now) });
                        }
                        events.push(Event::Resync);
                        events.extend(sayings(&s, &mut last_error));
                        events
                    }
                    _ = fade.tick() => {
                        let Some(s) = session.upgrade() else { return };
                        s.snapshot().typing.keys().map(|rid| Event::Typing { rid: rid.clone() }).collect()
                    }
                    n = notices.recv() => match n {
                        Ok(Notice::Incoming(i)) => vec![Event::Incoming {
                            incoming: Incoming {
                                rid: i.rid,
                                id: i.id,
                                author: i.author,
                                room_name: i.room_name,
                                direct: i.direct,
                                body: i.body,
                                mentions_me: i.mentions_me,
                            },
                        }],
                        Ok(Notice::Ring(..) | Notice::RingEnded(_)) => vec![Event::CallRing],
                        Ok(Notice::Expired) => {
                            let Some(s) = session.upgrade() else { return };
                            let info = s.info.clone();
                            supervisor.remove(&accounts::key(&info));
                            s.shutdown();
                            let dirs = dirs.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                accounts::remove(&dirs, &info);
                                forget(&dirs.data, &info);
                            })
                            .await;
                            vec![Event::Expired]
                        }
                        Err(RecvError::Lagged(_)) => vec![Event::Resync],
                        Err(RecvError::Closed) => return,
                    },
                };
                for e in events {
                    listener.on_event(e);
                }
            }
        });
        if let Some(old) = self.forward.lock().unwrap().replace(task) {
            old.abort();
        }
    }

    /// Online while this account is in front in the focused window.
    pub fn set_available(&self, available: bool) {
        self.session.set_available(available);
    }

    /// The conversations, by section, as the room list draws them.
    pub fn rooms(&self) -> Vec<RoomGroup> {
        let rows = self.session.room_rows();
        let photos = self.session.snapshot().avatars;
        rv_core::rooms::sections(&rows)
            .into_iter()
            .map(|(section, members)| {
                let rooms = members
                    .into_iter()
                    .map(|r| {
                        let photo = photos.get(&r.rid).map(|p| photo_link(&r.rid, p));
                        let mut room = model::room(r, None, None);
                        room.avatar = photo;
                        room
                    })
                    .collect();
                RoomGroup::new(&section, rooms)
            })
            .collect()
    }

    pub fn attention(&self) -> i64 {
        rv_core::rooms::attention(&self.session.room_rows())
    }

    /// A chat's messages, oldest first, grouped.
    pub fn messages(&self, rid: String) -> Vec<MessageItem> {
        let photos = self.session.snapshot().avatars;
        let me = self.session.info.user_id.clone();
        self.session
            .timeline(&rid)
            .into_iter()
            .map(|d| {
                let mut item = model::message(d, "me", "me");
                let jid = if item.mine { me.as_str() } else { item.author_id.as_str() };
                item.avatar = photos.get(jid).map(|p| photo_link(jid, p)).unwrap_or_default();
                item
            })
            .collect()
    }

    /// The photo path of a person or group, or of my account (`me`); empty without one.
    pub fn photo_path(&self, jid: String) -> String {
        let jid = if jid == "me" { self.session.info.user_id.clone() } else { jid };
        self.session.photo(&jid).map(|p| photo_link(&jid, &p)).unwrap_or_default()
    }

    /// The open chat is read while it is on screen in the focused window.
    pub fn mark_read(&self, rid: String) {
        self.session.mark_read(&rid);
    }

    pub fn typing(&self, rid: String) -> Vec<String> {
        self.session.typing(&rid)
    }

    pub fn set_typing(&self, rid: String, active: bool) {
        self.session.set_typing(&rid, active);
    }

    pub fn draft(&self, rid: String) -> String {
        self.session.draft(&rid)
    }

    pub fn set_draft(&self, rid: String, text: String) {
        self.session.set_draft(&rid, &text);
    }

    /// One send in flight per chat: its composer waits meanwhile.
    pub fn sending(&self, rid: String) -> bool {
        self.session.sending(&rid)
    }

    /// Sends a text, answering `reply` (a message id) when given.
    pub fn send(&self, rid: String, text: String, reply: Option<String>) -> Result<(), RvError> {
        self.session.set_typing(&rid, false);
        self.session.send(&rid, &text, reply).map_err(|_| RvError::local(rv_core::i18n::t("wa.send_failed")))?;
        self.session.set_draft(&rid, "");
        Ok(())
    }

    /// Sends a file, with its caption.
    pub fn send_file(
        &self,
        rid: String,
        path: String,
        name: String,
        mime: String,
        caption: String,
        reply: Option<String>,
    ) -> Result<(), RvError> {
        let file = whatsapp::OutgoingFile { chat: rid, path, name, mime, caption, reply, voice: false };
        self.session.send_file(file).map_err(|_| RvError::local(rv_core::i18n::t("wa.send_failed")))
    }

    /// What a message allows: reactions, reply, copy, and on my own messages
    /// edit (15 minutes) and delete for everyone (48 hours).
    pub fn actions(&self, message_id: String) -> Vec<MessageAction> {
        let s = self.session.snapshot();
        let Some(m) = s.messages.get(&message_id) else { return Vec::new() };
        let connected = s.state == "connected";
        let mut actions = Vec::new();
        if m.deleted {
            return actions;
        }
        actions.push(MessageAction::Copy);
        if connected {
            actions.push(MessageAction::React);
            actions.push(MessageAction::Reply);
        }
        if !m.kind.is_empty() {
            actions.push(MessageAction::Download);
        }
        let age = now_ms() - m.ts;
        let settled = m.mine && m.status.is_empty() && connected;
        if settled && age < EDIT_WINDOW_MS {
            actions.push(MessageAction::Edit);
        }
        if settled && age < DELETE_WINDOW_MS {
            actions.push(MessageAction::Delete);
        }
        actions
    }

    /// My reaction's glyph on a message, if any.
    pub fn my_reaction(&self, message_id: String) -> Option<String> {
        self.session.snapshot().reactions.get(&message_id).and_then(|by| by.get("me")).cloned()
    }

    /// Reacts with `shortcode`, or takes my reaction back.
    pub fn react(&self, message_id: String, shortcode: String, add: bool) -> Result<(), RvError> {
        let glyph = if add { whatsapp::reaction_glyph(&shortcode) } else { String::new() };
        self.session.react(&message_id, glyph).map_err(|_| RvError::local(rv_core::i18n::t("wa.action_failed")))
    }

    pub fn edit(&self, message_id: String, text: String) -> Result<(), RvError> {
        let text = text.trim().to_owned();
        if text.is_empty() {
            return Ok(());
        }
        self.session.edit(&message_id, text).map_err(|_| RvError::local(rv_core::i18n::t("wa.action_failed")))
    }

    /// Deletes my message for everyone.
    pub fn delete(&self, message_id: String) -> Result<(), RvError> {
        self.session.delete(&message_id).map_err(|_| RvError::local(rv_core::i18n::t("wa.action_failed")))
    }

    /// An attachment (`wa-media:<id>`) or a photo (`wa-photo:<jid>`).
    pub async fn media(&self, path: String) -> Result<MediaData, RvError> {
        if let Some(link) = path.strip_prefix(PHOTO_LINK) {
            let jid = link.split_once('?').map_or(link, |(jid, _)| jid);
            let bytes = self.session.photo(jid).ok_or_else(|| RvError::local("no photo"))?;
            return Ok(MediaData { bytes: bytes.to_vec(), content_type: "image/jpeg".into(), placeholder: false });
        }
        let file = self.file(path).await?;
        let bytes = tokio::fs::read(&file).await.map_err(|e| RvError::local(e.to_string()))?;
        Ok(MediaData { bytes, content_type: String::new(), placeholder: false })
    }

    /// An attachment saved at `destination`.
    pub async fn download(&self, path: String, destination: String) -> Result<(), RvError> {
        let file = self.file(path).await?;
        on_tokio(async move { tokio::fs::copy(file, destination).await.map(|_| ()) })
            .await
            .map_err(|e| RvError::local(e.to_string()))
    }

    /// An attachment as the local file the helper keeps.
    pub async fn local_file(&self, path: String) -> Result<String, RvError> {
        Ok(self.file(path).await?.to_string_lossy().into_owned())
    }

    /// People and groups to start a conversation with.
    pub async fn search(&self, text: String) -> Result<Vec<WhatsAppPerson>, RvError> {
        let session = self.session.clone();
        let found = on_tokio(async move { session.search(&text).await }).await.map_err(RvError::local)?;
        Ok(found.into_iter().map(|p| WhatsAppPerson { jid: p.jid, name: p.name }).collect())
    }

    /// A conversation opened before its first message: listed until it has one.
    pub fn start_chat(&self, jid: String, name: String) {
        self.session.start_chat(&jid, &name);
    }

    /// The call ringing here, if any.
    pub fn incoming_call(&self) -> Option<RingingCall> {
        let s = self.session.snapshot();
        let call = s.calls.values().find(|c| c.state == "ringing")?;
        Some(RingingCall {
            rid: if call.group.is_empty() { call.from.clone() } else { call.group.clone() },
            call_id: call.id.clone(),
            caller_id: call.from.clone(),
            caller_name: Some(self.session.chat_name(&call.from)),
        })
    }

    /// A ringing call this app can carry (a person's, audio only); the others
    /// are answered on the phone.
    pub fn call_answerable(&self, call_id: String) -> bool {
        rv_core::voice::available()
            && self.session.snapshot().calls.get(&call_id).is_some_and(|c| !c.video && c.group.is_empty())
    }

    /// A person's chat can be called, audio only; a group's not yet.
    pub fn call_possible(&self, rid: String) -> bool {
        rv_core::voice::available() && !rid.ends_with("@g.us") && self.session.connection() == Connection::Online
    }

    pub async fn answer_call(
        &self,
        call_id: String,
        title: String,
    ) -> Option<Arc<crate::jitsi_call::JitsiCallSession>> {
        crate::jitsi_call::carry_whatsapp(self.session.clone(), WaTarget::Incoming(call_id), title).await
    }

    pub async fn start_call(&self, rid: String, title: String) -> Option<Arc<crate::jitsi_call::JitsiCallSession>> {
        crate::jitsi_call::carry_whatsapp(self.session.clone(), WaTarget::Outgoing(rid), title).await
    }

    pub fn decline_call(&self, call_id: String) {
        self.session.reject_call(&call_id);
    }

    /// Unlinks this device from the phone, then forgets the account here; a
    /// failure keeps it.
    pub async fn unlink(&self) -> Result<(), RvError> {
        let session = self.session.clone();
        on_tokio(async move { session.unlink().await }).await.map_err(|code| {
            RvError::local(rv_core::i18n::t(if code == "logout_failed" {
                "wa.logout_failed"
            } else {
                "wa.action_failed"
            }))
        })?;
        let info = self.session.info.clone();
        self.accounts.remove(&accounts::key(&info));
        self.session.shutdown();
        let dirs = self.dirs.clone();
        blocking(move || {
            accounts::remove(&dirs, &info);
            forget(&dirs.data, &info);
        })
        .await;
        Ok(())
    }
}

impl WhatsAppChat {
    async fn file(&self, path: String) -> Result<PathBuf, RvError> {
        let id =
            path.strip_prefix(whatsapp::MEDIA_LINK).ok_or_else(|| RvError::local("not a WhatsApp file"))?.to_owned();
        let session = self.session.clone();
        on_tokio(async move { session.download(&id).await }).await.map_err(RvError::local)
    }
}
