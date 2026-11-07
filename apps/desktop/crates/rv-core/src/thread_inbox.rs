//! The account's threads, across its rooms: those I follow, those with
//! replies I have not read. Each provider fills the same page.

use crate::store::MessageRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Followed,
    Unread,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    pub id: String,
    pub username: String,
    /// As the account names people; empty: the username.
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub root: MessageRow,
    /// The room's shown name; empty when the account does not list it.
    pub room: String,
    pub last_reply: i64,
    pub replies: i64,
    pub participants: Vec<Participant>,
    pub unread_replies: i64,
    pub unread_mentions: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Totals {
    pub unread_threads: i64,
    pub unread_mentions: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Page {
    pub entries: Vec<Entry>,
    /// Hand it back for the next page; None on the last one.
    pub next: Option<String>,
}
