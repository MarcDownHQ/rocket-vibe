//! The account's threads, across its rooms: those I follow, those with
//! replies I have not read. Each provider fills the same page.

use std::collections::HashSet;

use crate::store::MessageRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    Followed,
    Unread,
}

/// The chips inside a tab, applied here over what was loaded: no server
/// narrows a list this way.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    All,
    Participated,
    Mentioned,
    Others,
}

impl Scope {
    /// Participated and Mentioned overlap; Others is neither.
    pub fn matches(self, entry: &Entry) -> bool {
        match self {
            Scope::All => true,
            Scope::Participated => entry.participated,
            Scope::Mentioned => entry.mentioned,
            Scope::Others => !entry.participated && !entry.mentioned,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    pub id: String,
    pub username: String,
    /// As the account names people; empty: the username.
    pub label: String,
    /// The photo's file id where people are served by it (Vibez).
    pub photo: Option<String>,
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
    /// I wrote the root or replied, or the thread is in a direct conversation.
    pub participated: bool,
    /// The provider says so, or a text the listing returned names me.
    pub mentioned: bool,
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

/// Rows that fill the panel: fewer through a chip, more pages are worth reading.
pub const FILL: usize = 12;
/// Pages read on top of the one asked for, per user action.
pub const EXTRA_PAGES: u32 = 4;

/// Whether a list that shows `shown` rows through its chip reads another page.
pub fn keep_fetching(shown: usize, has_next: bool, extra_pages: u32) -> bool {
    shown < FILL && has_next && extra_pages < EXTRA_PAGES
}

/// A thread that moves while the list is paged comes back on a later page:
/// its first place stands.
pub fn append(entries: &mut Vec<Entry>, page: Vec<Entry>) {
    let mut seen: HashSet<String> = entries.iter().map(|e| e.root.id.clone()).collect();
    entries.extend(page.into_iter().filter(|e| seen.insert(e.root.id.clone())));
}

pub fn participated<'a>(
    me: &str,
    author: &str,
    mut repliers: impl Iterator<Item = &'a str>,
    room_kind: Option<&str>,
) -> bool {
    author == me || room_kind == Some("d") || repliers.any(|id| id == me)
}

/// Whether `text` names `username` with an `@`, as a whole name in any case.
pub fn names(text: &str, username: &str) -> bool {
    if username.is_empty() {
        return false;
    }
    let part = |c: char| c.is_alphanumeric() || c == '_' || c == '-';
    let chars: Vec<char> = text.chars().collect();
    let wanted: Vec<char> = username.chars().flat_map(char::to_lowercase).collect();
    chars.iter().enumerate().filter(|(_, c)| **c == '@').any(|(at, _)| {
        let end = at + 1 + wanted.len();
        let Some(name) = chars.get(at + 1..end) else { return false };
        let same = name.iter().flat_map(|c| c.to_lowercase()).eq(wanted.iter().copied());
        let before = at.checked_sub(1).and_then(|i| chars.get(i)).is_some_and(|c| part(*c) || *c == '.');
        let after = match chars.get(end) {
            Some('.') => chars.get(end + 1).is_some_and(|c| part(*c)),
            Some(c) => part(*c),
            None => false,
        };
        same && !before && !after
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, participated: bool, mentioned: bool) -> Entry {
        Entry {
            root: MessageRow { id: id.to_owned(), ..Default::default() },
            room: String::new(),
            last_reply: 0,
            replies: 0,
            participants: Vec::new(),
            unread_replies: 0,
            unread_mentions: 0,
            participated,
            mentioned,
        }
    }

    #[test]
    fn participated_and_mentioned_overlap_and_others_is_neither() {
        let both = entry("both", true, true);
        let neither = entry("neither", false, false);
        let shown = |scope: Scope| {
            [&both, &entry("mine", true, false), &entry("named", false, true), &neither]
                .into_iter()
                .filter(|e| scope.matches(e))
                .map(|e| e.root.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(shown(Scope::All), ["both", "mine", "named", "neither"]);
        assert_eq!(shown(Scope::Participated), ["both", "mine"]);
        assert_eq!(shown(Scope::Mentioned), ["both", "named"]);
        assert_eq!(shown(Scope::Others), ["neither"]);
        assert_eq!(Scope::default(), Scope::All);
    }

    #[test]
    fn i_took_part_as_author_as_replier_or_by_the_conversation_being_direct() {
        let none = || std::iter::empty();
        assert!(participated("me", "me", none(), Some("c")));
        assert!(participated("me", "bob", ["ann", "me"].into_iter(), Some("c")));
        assert!(participated("me", "bob", none(), Some("d")));
        assert!(!participated("me", "bob", ["ann"].into_iter(), Some("p")));
        assert!(!participated("me", "bob", none(), None));
    }

    #[test]
    fn a_name_counts_whole_and_in_any_case() {
        assert!(names("hey @Bob, look", "bob"));
        assert!(names("@bob", "bob"));
        assert!(names("see @bob.", "bob"));
        assert!(names("(@bob)", "bob"));
        assert!(names("@jean.dupont hi", "jean.dupont"));
        assert!(!names("@bobby", "bob"));
        assert!(!names("@bob.smith", "bob"));
        assert!(!names("@bob-smith", "bob"));
        assert!(!names("bob@bob.example", "bob"));
        assert!(!names("bob", "bob"));
        assert!(!names("@", ""));
    }

    #[test]
    fn a_short_list_reads_on_but_only_so_far() {
        assert!(keep_fetching(0, true, 0));
        assert!(keep_fetching(FILL - 1, true, EXTRA_PAGES - 1));
        assert!(!keep_fetching(FILL, true, 0), "full");
        assert!(!keep_fetching(0, false, 0), "nothing left");
        assert!(!keep_fetching(0, true, EXTRA_PAGES), "the load-more button takes over");
    }

    #[test]
    fn a_thread_that_moved_keeps_its_first_place() {
        let mut entries = vec![entry("a", false, false), entry("b", false, false)];
        append(&mut entries, vec![entry("b", true, true), entry("c", false, false), entry("c", false, false)]);
        let ids: Vec<&str> = entries.iter().map(|e| e.root.id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert!(!entries[1].participated);
    }
}
