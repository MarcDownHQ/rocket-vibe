//! Rooms and people: their details, my own profile, search, calls, the
//! pinned and starred messages of a room, its threads, its own
//! notifications, and forwarding a message to another room.

use rv_core::media::{self, AvatarTarget};
use rv_core::session::two_factor_code;
use rv_core::thread_inbox;
use serde_json::json;

use crate::model::{MessageItem, Presence, Room, RvError};
use crate::{Chat, on_tokio};

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct RoomDetails {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub topic: Option<String>,
    pub announcement: Option<String>,
    pub description: Option<String>,
    pub members: Option<i64>,
    pub read_only: bool,
    pub encrypted: bool,
    pub archived: bool,
    pub default: bool,
}

impl From<rv_core::info::RoomInfo> for RoomDetails {
    fn from(r: rv_core::info::RoomInfo) -> Self {
        RoomDetails {
            id: r.id,
            name: r.name,
            kind: r.kind,
            topic: r.topic,
            announcement: r.announcement,
            description: r.description,
            members: r.members,
            read_only: r.read_only,
            encrypted: r.encrypted,
            archived: r.archived,
            default: r.default,
        }
    }
}

/// One thread of a room's list: its root, when it was last answered, whether I follow it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ThreadEntry {
    pub root: MessageItem,
    pub last_reply: Option<i64>,
    pub following: bool,
}

/// One page of a room's threads, and how many there are in all.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ThreadsPage {
    pub threads: Vec<ThreadEntry>,
    pub total: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ThreadInboxParticipant {
    pub username: String,
    /// As the account names them.
    pub name: String,
    pub avatar: String,
}

/// One thread of the account's list.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ThreadInboxEntry {
    pub root: String,
    pub rid: String,
    pub author: String,
    /// What the root says, or that it is a file or locked.
    pub preview: String,
    /// The room's shown name; empty when the account does not list it.
    pub room: String,
    pub last_reply: i64,
    pub replies: i64,
    pub participants: Vec<ThreadInboxParticipant>,
    pub unread_replies: i64,
    pub unread_mentions: i64,
    /// I wrote the root or replied, or the thread is in a direct conversation.
    pub participated: bool,
    /// The provider says so, or a text the listing returned names me.
    pub mentioned: bool,
}

/// The chips inside a tab. Not persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum ThreadInboxScope {
    All,
    Participated,
    Mentioned,
    Others,
}

impl From<ThreadInboxScope> for thread_inbox::Scope {
    fn from(scope: ThreadInboxScope) -> Self {
        match scope {
            ThreadInboxScope::All => thread_inbox::Scope::All,
            ThreadInboxScope::Participated => thread_inbox::Scope::Participated,
            ThreadInboxScope::Mentioned => thread_inbox::Scope::Mentioned,
            ThreadInboxScope::Others => thread_inbox::Scope::Others,
        }
    }
}

/// Whether the entry shows through the chip; the rule of `thread_inbox::Scope`.
#[uniffi::export]
pub fn thread_inbox_scope_matches(scope: ThreadInboxScope, participated: bool, mentioned: bool) -> bool {
    thread_inbox::Scope::from(scope).allows(participated, mentioned)
}

/// Whether a list showing `shown` rows through its chip reads another page,
/// `extra_pages` having been read on top of the one asked for by this user action.
#[uniffi::export]
pub fn thread_inbox_keep_fetching(shown: u32, has_next: bool, extra_pages: u32) -> bool {
    thread_inbox::keep_fetching(shown as usize, has_next, extra_pages)
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct ThreadInboxPage {
    pub entries: Vec<ThreadInboxEntry>,
    /// Hand it back for the next page; None on the last one.
    pub next: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct ThreadInboxTotals {
    pub unread_threads: i64,
    pub unread_mentions: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Person {
    pub id: String,
    pub username: String,
    pub name: Option<String>,
    pub presence: Option<Presence>,
    pub status_text: Option<String>,
    pub roles: Vec<String>,
    /// Their time now and their zone: `14:05 (UTC+2)`.
    pub local_time: Option<String>,
    pub bio: Option<String>,
    pub avatar: String,
    /// A Vibez bot account; false on Rocket.Chat.
    pub bot: bool,
    /// The username of the person who owns this bot.
    pub bot_owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Me {
    pub username: String,
    pub name: String,
    pub email: String,
    /// `online`, `away`, `busy` or `offline`: the status I chose.
    pub status: String,
    pub status_text: String,
    pub bio: String,
    pub avatar: String,
    /// `default`, `all`, `mention` or `nothing`.
    pub desktop_notifications: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct SearchHit {
    pub id: String,
    pub author: String,
    pub ts: i64,
    pub body: Vec<crate::markup::BodyBlock>,
}

fn me_record(m: rv_core::account::Me) -> Me {
    Me {
        avatar: media::avatar_path(AvatarTarget::User(&m.username), m.avatar_etag.as_deref()),
        username: m.username,
        name: m.name,
        email: m.email,
        status: m.status,
        status_text: m.status_text,
        bio: m.bio,
        desktop_notifications: m.desktop_notifications,
    }
}

#[uniffi::export]
impl Chat {
    /// A DM's `rooms.info` has no name: it keeps the one the list shows.
    pub async fn room_details(&self, rid: String) -> Result<RoomDetails, RvError> {
        let s = self.session.clone();
        let mut details: RoomDetails = on_tokio({
            let rid = rid.clone();
            async move { s.room_info(&rid).await }
        })
        .await?
        .into();
        if details.name.is_empty()
            && let Some((name, _)) = self.session.store.room_name(&rid)
        {
            details.name = name;
        }
        Ok(details)
    }

    /// A channel by its name (a `#channel` link): its details, id included.
    pub async fn room_named(&self, name: String) -> Result<RoomDetails, RvError> {
        let s = self.session.clone();
        Ok(on_tokio(async move { s.room_by_name(&name).await }).await?.into())
    }

    /// Someone, by username, or by user id when `by_id`.
    pub async fn person(&self, key: String, by_id: bool) -> Result<Person, RvError> {
        let s = self.session.clone();
        let p = on_tokio(async move { s.profile(&key, by_id).await }).await?;
        Ok(Person {
            avatar: media::avatar_path(AvatarTarget::User(&p.username), p.avatar_etag.as_deref()),
            local_time: p.utc_offset.map(|o| rv_core::info::local_time(o, chrono::Utc::now())),
            presence: p
                .shows_presence()
                .then(|| self.session.presence(&p.id).or(p.presence))
                .flatten()
                .map(Presence::from),
            id: p.id,
            username: p.username,
            name: p.name,
            status_text: p.status_text,
            roles: p.roles,
            bio: p.bio,
            bot: p.bot,
            bot_owner: p.bot_owner,
        })
    }

    /// Someone's live presence, by user id: None until the server told.
    pub fn presence_of(&self, uid: String) -> Option<Presence> {
        self.session.presence(&uid).map(Presence::from)
    }

    pub async fn me(&self) -> Result<Me, RvError> {
        let s = self.session.clone();
        Ok(me_record(on_tokio(async move { s.me().await }).await?))
    }

    /// Saves what changed between `before` and `after` (name, username, email,
    /// bio). Username and email also need my password, and the server may ask
    /// a 2FA code (an `RvError::Server` with `two_factor`), then given back
    /// with its `method`.
    pub async fn update_profile(
        &self,
        before: Me,
        after: Me,
        password: Option<String>,
        method: Option<String>,
        code: Option<String>,
    ) -> Result<(), RvError> {
        let core = |m: &Me| rv_core::account::Me {
            username: m.username.clone(),
            name: m.name.clone(),
            email: m.email.clone(),
            status: m.status.clone(),
            status_text: m.status_text.clone(),
            bio: m.bio.clone(),
            avatar_etag: None,
            desktop_notifications: m.desktop_notifications.clone(),
        };
        let changes = rv_core::account::basic_info_changes(&core(&before), &core(&after));
        if changes.is_empty() {
            return Ok(());
        }
        if rv_core::account::needs_password(&changes) && password.as_deref().unwrap_or_default().is_empty() {
            return Err(RvError::local("password-needed"));
        }
        let two_factor = method.zip(code).map(|(m, c)| two_factor_code(&m, &c));
        let s = self.session.clone();
        Ok(on_tokio(async move { s.update_basic_info(changes, password.as_deref(), two_factor).await }).await?)
    }

    pub async fn set_avatar(&self, path: String, mime: String) -> Result<(), RvError> {
        let s = self.session.clone();
        Ok(on_tokio(async move { s.set_avatar(std::path::Path::new(&path), &mime).await }).await?)
    }

    pub async fn reset_avatar(&self) -> Result<(), RvError> {
        let s = self.session.clone();
        Ok(on_tokio(async move { s.reset_avatar().await }).await?)
    }

    /// `default`, `all`, `mention` or `nothing`: which messages notify.
    pub async fn set_desktop_notifications(&self, value: String) -> Result<(), RvError> {
        let s = self.session.clone();
        Ok(on_tokio(async move { s.set_preference("desktopNotifications", json!(value)).await }).await?)
    }

    /// `chat.search` in a room, newest first.
    pub async fn search(&self, rid: String, text: String) -> Result<Vec<SearchHit>, RvError> {
        let s = self.session.clone();
        let found = on_tokio(async move { s.search(&rid, &text).await }).await?;
        let me = self.session.info.username.clone();
        let ctx = rv_core::markdown::Context { me: &me, names: None };
        Ok(found
            .into_iter()
            .map(|m| SearchHit {
                body: crate::markup::blocks(rv_core::markdown::render(m.md.as_deref(), m.text.as_deref(), &ctx)),
                author: m.author_name.unwrap_or_default(),
                id: m.id,
                ts: m.ts,
            })
            .collect())
    }

    /// Whether a call can be started in the room: the server offers calls, and
    /// on Teams the room is a chat, not a channel.
    pub async fn call_available(&self, rid: String) -> bool {
        let s = self.session.clone();
        on_tokio(async move { s.room_callable(&rid).await }).await
    }

    /// Starts a call in the room: its link, to open in the browser.
    pub async fn start_call(&self, rid: String) -> Result<String, RvError> {
        let s = self.session.clone();
        Ok(on_tokio(async move { s.start_call(&rid).await }).await?)
    }

    /// The room's pinned messages, or the ones I starred there.
    pub async fn marked(&self, rid: String, starred: bool) -> Result<Vec<MessageItem>, RvError> {
        let s = self.session.clone();
        let rows = on_tokio(async move { s.marked(&rid, starred).await }).await?;
        Ok(rows.into_iter().map(|r| self.listed(r)).collect())
    }

    /// Whether the server lists a room's threads and lets me follow one (Rocket.Chat).
    pub fn threads_available(&self) -> bool {
        self.session.threads_available()
    }

    /// The room has a Notes tab (a Slack conversation's canvas).
    pub fn notes_available(&self, rid: String) -> bool {
        self.session.notes_available(&rid)
    }

    pub async fn room_notes(&self, rid: String) -> Result<crate::native_rooms::RoomNotes, RvError> {
        let s = self.session.clone();
        let me = self.session.info.username.clone();
        let notes = on_tokio(async move { s.room_notes(&rid).await }).await?;
        Ok(crate::native_rooms::RoomNotes::new(notes, &me))
    }

    /// A save over a newer version fails with the server error `revision_conflict`.
    pub async fn save_room_notes(
        &self,
        rid: String,
        markdown: String,
        revision: String,
    ) -> Result<crate::native_rooms::RoomNotes, RvError> {
        let s = self.session.clone();
        let me = self.session.info.username.clone();
        let notes = on_tokio(async move { s.save_room_notes(&rid, &markdown, &revision).await }).await?;
        Ok(crate::native_rooms::RoomNotes::new(notes, &me))
    }

    /// One page of the room's threads, every one or those I follow, the
    /// most recently answered first.
    pub async fn threads(&self, rid: String, following: bool, offset: u32, count: u32) -> Result<ThreadsPage, RvError> {
        let s = self.session.clone();
        let (rows, total) = on_tokio(async move { s.threads(&rid, following, offset, count).await }).await?;
        let me = self.session.info.user_id.clone();
        let threads = rows
            .into_iter()
            .map(|r| ThreadEntry { last_reply: r.thread_last, following: r.followed_by(&me), root: self.listed(r) })
            .collect();
        Ok(ThreadsPage { threads, total })
    }

    /// Follows a thread (`on`) or stops following it.
    pub async fn follow_thread(&self, root: String, on: bool) -> Result<(), RvError> {
        let s = self.session.clone();
        Ok(on_tokio(async move { s.follow_thread(&root, on).await }).await?)
    }

    /// Whether the account lists its threads across its rooms.
    pub fn thread_inbox_available(&self) -> bool {
        self.session.thread_inbox_available()
    }

    /// Whether nothing tells when the list changes: it is read again on
    /// returning to the app and every 30 s while it is on screen (Slack).
    pub fn thread_inbox_polled(&self) -> bool {
        self.session.thread_inbox_polled()
    }

    /// One page of the account's threads: those I follow, or (`unread`) those
    /// with replies I have not read.
    pub async fn thread_inbox(&self, unread: bool, cursor: Option<String>) -> Result<ThreadInboxPage, RvError> {
        let filter = if unread { thread_inbox::Filter::Unread } else { thread_inbox::Filter::Followed };
        let s = self.session.clone();
        let page = on_tokio(async move { s.thread_inbox(filter, cursor.as_deref()).await }).await?;
        Ok(ThreadInboxPage {
            entries: page.entries.into_iter().map(|e| self.thread_inbox_entry(e)).collect(),
            next: page.next,
        })
    }

    pub async fn thread_totals(&self) -> Result<ThreadInboxTotals, RvError> {
        let s = self.session.clone();
        let totals = on_tokio(async move { s.thread_totals().await }).await?;
        Ok(ThreadInboxTotals { unread_threads: totals.unread_threads, unread_mentions: totals.unread_mentions })
    }

    /// I read the thread up to now; its room stays as it is.
    pub async fn mark_thread_read(&self, rid: String, root: String) -> Result<(), RvError> {
        let s = self.session.clone();
        Ok(on_tokio(async move { s.mark_thread_read(&rid, &root).await }).await?)
    }

    pub async fn mark_all_threads_read(&self) -> Result<(), RvError> {
        let s = self.session.clone();
        Ok(on_tokio(async move { s.mark_all_threads_read().await }).await?)
    }

    /// Whether I can follow a thread and see that I do, from the thread itself.
    pub fn thread_follow_available(&self) -> bool {
        self.session.thread_follow_available()
    }

    /// Whether I follow it, by the server's answer where the root does not say.
    pub async fn fetch_thread_following(&self, rid: String, root: String) -> Result<bool, RvError> {
        let s = self.session.clone();
        Ok(on_tokio(async move { s.thread_following(&rid, &root).await }).await?)
    }

    /// Whether the room list can mark a room unread or read (Rocket.Chat).
    pub fn unread_marks_available(&self) -> bool {
        self.session.unread_marks_available()
    }

    /// The room list's "Mark as unread" (`unread`) or "Mark as read" (threads
    /// included). A room with no message to mark unread fails with
    /// `Local("nothing-unread")`. Leave the room first when it is the open one.
    pub async fn mark_room(&self, rid: String, unread: bool) -> Result<(), RvError> {
        let s = self.session.clone();
        let done =
            on_tokio(async move { if unread { s.mark_unread(&rid).await } else { s.mark_room_read(&rid).await } })
                .await;
        match done {
            Err(e) if e.error_type.as_deref() == Some(rv_core::actions::NOTHING_TO_UNREAD) => {
                Err(RvError::local("nothing-unread"))
            }
            other => Ok(other?),
        }
    }

    /// No room open any more: its deletions and typing are no longer heard.
    pub fn close_room(&self) {
        self.session.close_room();
    }

    /// Whether a room can have notifications of its own (Rocket.Chat).
    pub fn room_notifications_available(&self) -> bool {
        self.session.room_notifications_available()
    }

    /// The room's own notifications as stored: `default` (the account's),
    /// `all`, `mentions` or `nothing`; a room another client silenced says `nothing`.
    pub fn room_notifications(&self, rid: String) -> String {
        match self.session.store.room_notifications(&rid) {
            (_, true) => "nothing".to_owned(),
            (own, false) => own.unwrap_or_else(|| "default".to_owned()),
        }
    }

    /// Chooses the room's own notifications, desktop and push together.
    pub async fn set_room_notifications(&self, rid: String, level: String) -> Result<(), RvError> {
        let s = self.session.clone();
        Ok(on_tokio(async move { s.room_notifications(&rid, &level).await }).await?)
    }

    /// Whether the message can be forwarded to another room (Rocket.Chat):
    /// where it can be quoted, an ordinary message on the server.
    pub fn forwardable(&self, rid: String, message_id: String, in_thread: bool) -> bool {
        self.session.forwarding_available()
            && self.possible_actions(&rid, &message_id, in_thread).is_some_and(|(row, possible)| {
                rv_core::actions::forwardable(&possible, row.system_type.as_deref(), row.outbox_status.is_some())
            })
    }

    /// The rooms a message can go to, matching `query` by name, latest activity first.
    pub fn forward_targets(&self, query: String) -> Vec<Room> {
        let rows = self.session.store.rooms();
        rv_core::rooms::forward_targets(&rows, &query).into_iter().map(|r| crate::model::room(r, None, None)).collect()
    }

    /// Forwards a message of the room (`kind`, `slug`, `rid`) to `target`: its
    /// permalink as a quote, through the outbox.
    pub async fn forward(
        &self,
        kind: String,
        slug: Option<String>,
        rid: String,
        message_id: String,
        target: String,
    ) -> Result<(), RvError> {
        let s = self.session.clone();
        Ok(on_tokio(async move { s.forward(&kind, slug.as_deref(), &rid, &message_id, &target).await }).await?)
    }

    /// Whether I follow the thread, by the stored copy of its root (false while unknown).
    pub fn thread_following(&self, root: String) -> bool {
        self.session
            .store
            .messages_by_id(std::slice::from_ref(&root))
            .first()
            .is_some_and(|r| r.followed_by(&self.session.info.user_id))
    }
}

fn thread_preview(row: &rv_core::store::MessageRow) -> String {
    match row.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        Some(text) => text.to_owned(),
        None if row.system_type.as_deref() == Some(rv_core::normalize::ENCRYPTED_TYPE) => {
            rv_core::i18n::t("message.encrypted").to_owned()
        }
        None => rv_core::i18n::t("marked.attachment").to_owned(),
    }
}

/// One row for the Swift app; `author` and `avatar` depend on the account's server.
pub(crate) fn thread_inbox_row(
    entry: thread_inbox::Entry,
    author: Option<String>,
    avatar: impl Fn(&thread_inbox::Participant) -> String,
) -> ThreadInboxEntry {
    ThreadInboxEntry {
        author: author.or_else(|| entry.root.author.clone()).unwrap_or_default(),
        preview: thread_preview(&entry.root),
        root: entry.root.id,
        rid: entry.root.rid,
        room: entry.room,
        last_reply: entry.last_reply,
        replies: entry.replies,
        participants: entry
            .participants
            .into_iter()
            .map(|person| ThreadInboxParticipant {
                avatar: avatar(&person),
                name: if person.label.is_empty() { person.username.clone() } else { person.label },
                username: person.username,
            })
            .collect(),
        unread_replies: entry.unread_replies,
        unread_mentions: entry.unread_mentions,
        participated: entry.participated,
        mentioned: entry.mentioned,
    }
}

impl Chat {
    fn thread_inbox_entry(&self, entry: thread_inbox::Entry) -> ThreadInboxEntry {
        let author = self.session.person_label(&entry.root.author_id);
        thread_inbox_row(entry, author, |person| media::avatar_path(AvatarTarget::User(&person.username), None))
    }

    /// A message of a list beside the room (pinned, starred, threads), opened when encrypted.
    pub(crate) fn listed(&self, row: rv_core::store::MessageRow) -> MessageItem {
        let info = &self.session.info;
        let d = rv_core::timeline::Display {
            row: self.session.open_row(row),
            show_header: true,
            show_day: false,
            gutter_time: false,
            new_marker: false,
            seen: None,
        };
        let reactor = self.session.reactor_names();
        crate::model::message_named(d, &info.user_id, &info.username, None, &reactor)
    }
}

#[cfg(test)]
mod thread_inbox_tests {
    use super::*;

    #[test]
    fn the_chips_keep_the_core_rule() {
        use ThreadInboxScope::{All, Mentioned, Others, Participated};
        let shown = |scope, participated, mentioned| thread_inbox_scope_matches(scope, participated, mentioned);
        assert!(shown(All, false, false));
        assert!(shown(Participated, true, true) && !shown(Participated, false, true));
        assert!(shown(Mentioned, true, true) && !shown(Mentioned, true, false));
        assert!(shown(Others, false, false) && !shown(Others, true, false) && !shown(Others, false, true));
    }

    #[test]
    fn a_short_list_reads_four_more_pages_at_most() {
        assert!(thread_inbox_keep_fetching(0, true, 3));
        assert!(!thread_inbox_keep_fetching(0, true, 4));
        assert!(!thread_inbox_keep_fetching(0, false, 0));
    }
}
