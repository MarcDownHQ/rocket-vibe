//! The account's threads on a Vibez server. Read from the server each time and
//! not cached: an old root stored here would look like a page of the room's
//! history. The server leaves encrypted rooms out.

use std::collections::{HashMap, HashSet};

use rv_protocol::parity::SetMark;
use rv_protocol::{MarkThreadsRead, RoomKind, ThreadInboxEntry};

use super::{Error, NativeSession};
use crate::store::MessageRow;
use crate::thread_inbox::{Entry, Filter, Page, Participant, Totals, names, participated};

fn count(value: &str) -> i64 {
    value.parse().unwrap_or(0)
}

fn millis(value: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(value).map(|t| t.timestamp_millis()).unwrap_or(0)
}

fn operation() -> String {
    format!("{:032x}", fastrand::u128(..))
}

fn shown_name(display: &str, username: &str) -> String {
    if display.is_empty() { username } else { display }.to_owned()
}

fn root_row(entry: &ThreadInboxEntry) -> MessageRow {
    let root = &entry.root;
    MessageRow {
        id: root.id.clone(),
        rid: entry.room_id.clone(),
        ts: millis(&root.created_at),
        text: Some(root.text.clone()).filter(|t| !t.is_empty()),
        author: Some(shown_name(&root.author.display_name, &root.author.username)),
        author_id: root.author.id.clone(),
        thread_count: count(&entry.replies),
        thread_last: Some(millis(&entry.last_reply_at)),
        ..Default::default()
    }
}

/// `rooms`: each room's shown name and kind.
fn entry(me: &str, username: &str, rooms: &HashMap<String, (String, RoomKind)>, listed: ThreadInboxEntry) -> Entry {
    let root = root_row(&listed);
    let room = rooms.get(&listed.room_id);
    let kind = room.map(|(_, kind)| if *kind == RoomKind::Direct { "d" } else { "c" });
    let repliers = listed.participants.iter().map(|person| person.id.as_str());
    let took_part = participated(me, &root.author_id, repliers, kind);
    let unread_mentions = count(&listed.mentions);
    let named = listed.root.personal_mention == Some(true) || names(&listed.root.text, username);
    Entry {
        room: room.map(|(name, _)| name.clone()).unwrap_or_default(),
        last_reply: millis(&listed.last_reply_at),
        replies: count(&listed.replies),
        participants: listed
            .participants
            .into_iter()
            .map(|person| Participant {
                label: person.display_name,
                id: person.id,
                username: person.username,
                photo: person.avatar_file_id,
            })
            .collect(),
        unread_replies: count(&listed.unread),
        unread_mentions,
        participated: took_part,
        mentioned: unread_mentions > 0 || named,
        root,
    }
}

impl NativeSession {
    fn supports(&self, feature: &str) -> bool {
        self.supported_features().iter().any(|f| f == feature)
    }

    pub fn thread_inbox_available(&self) -> bool {
        self.supports("thread_inbox")
    }

    pub fn thread_follow_available(&self) -> bool {
        self.supports("thread_follow")
    }

    fn room_titles(&self) -> Result<HashMap<String, (String, RoomKind)>, Error> {
        let rooms = self.store.rooms()?;
        let mut titles = HashMap::with_capacity(rooms.len());
        for room in rooms {
            let peer = if room.kind == RoomKind::Direct { self.store.direct_peer(&room.id)? } else { None };
            let name = peer.map_or(room.name, |p| shown_name(&p.user.display_name, &p.user.username));
            titles.insert(room.id, (name, room.kind));
        }
        Ok(titles)
    }

    pub async fn thread_inbox(&self, filter: Filter, cursor: Option<&str>) -> Result<Page, Error> {
        self.ready()?;
        if !self.thread_inbox_available() {
            return Err(Error::Protocol("unsupported_feature"));
        }
        let page = self.client.thread_inbox(filter == Filter::Unread, cursor).await?;
        self.ready()?;
        let rooms = self.room_titles()?;
        let mut seen = HashSet::new();
        let entries = page
            .threads
            .into_iter()
            .filter(|listed| seen.insert(listed.root.id.clone()))
            .map(|listed| entry(&self.info.user_id, &self.info.username, &rooms, listed))
            .collect();
        Ok(Page { entries, next: page.next })
    }

    pub async fn thread_totals(&self) -> Result<Totals, Error> {
        self.ready()?;
        if !self.thread_inbox_available() {
            return Ok(Totals::default());
        }
        let totals = self.client.thread_totals().await?;
        Ok(Totals { unread_threads: count(&totals.unread_threads), unread_mentions: count(&totals.unread_mentions) })
    }

    pub async fn mark_all_threads_read(&self) -> Result<(), Error> {
        self.ready()?;
        if !self.thread_inbox_available() {
            return Err(Error::Protocol("unsupported_feature"));
        }
        self.client.mark_all_threads_read(&MarkThreadsRead { operation_id: operation() }).await?;
        self.wake.notify_one();
        Ok(())
    }

    pub async fn follow_thread(&self, root: &str, on: bool) -> Result<(), Error> {
        self.ready()?;
        if !self.thread_follow_available() {
            return Err(Error::Protocol("unsupported_feature"));
        }
        let projection = self.store.projection_token();
        let answered = self.client.follow_thread(root, &SetMark { operation_id: operation(), present: on }).await?;
        self.ready()?;
        if !self.store.ingest_at(std::slice::from_ref(&answered), projection)? {
            return Err(Error::Protocol("delivery_revalidate"));
        }
        Ok(())
    }

    /// Whether I follow it, as the root stored here says.
    pub fn thread_following(&self, root: &str) -> Result<bool, Error> {
        Ok(self.store.thread_followed(root)?)
    }
}
