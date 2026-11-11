# Native threads (P11)

The Rust server and the mobile / desktop providers use the existing thread screens,
message lists, menus and composers. The Rocket.Chat provider
keeps its routes and its `tmid` model; Vibez announces `threads` and uses
`reply_to` / `root_id`. The qualification of the installed applications remains open.

## Roots and replies

`POST /api/v1/rooms/{room}/messages` accepts an optional `SendMessage.reply_to`.
Its absence keeps the sending of a root and the earlier idempotent fingerprint.
A reply keeps the same operation ID, text, quotes and root across
a retry or a restart. Changing the root under the same ID gives a conflict.

The root must belong to the same room, be a confirmed non-deleted message,
with no parent and no system activity. An unknown root / one from another room returns
`404`, a reply used as a root `422 invalid_thread_root`, and a deleted
root `410 thread_root_deleted`. The receipt of an already committed reply remains
replayable after its root is deleted, for a current member of the room.

`Message.reply_to` identifies replies. A root receives `Message.thread`
with `replies` as an exact decimal and `last_reply_at`. The count covers only
non-deleted replies. Creating and deleting a reply also publish
a new revision of the root without changing its creation position.
The room history and its main list contain only roots.

`GET /api/v1/messages/{root}/thread?before=…&limit=…` returns `ThreadPage`:
root, replies by descending position, `has_more` and personal state.
The limit is 50 by default, between 1 and 100. The transactional view and the
delivery barriers also cover the sources of quotes.
A deleted root remains consultable as a tombstone with its old replies.
The instance administration gives no implicit access to a private thread.
The reserved route `GET /messages/{root}/replies` returns this same page.
`POST /messages/{root}/replies` sends into the root in the path; a redundant
`reply_to` must designate it exactly. Both send routes share the same
fingerprint, the same rights and the same receipt, including after root deletion.

Snapshots keep at most 50 roots **and** 50 recent replies per room.
A flood of replies therefore does not push all the roots out of the window.
Opening a thread then completes its history by pages. The mobile and desktop
projections verify the room, the root, the exact positions and the membership
duration before the SQLite commit; a failure confirms no intent.

## Independent reads

`POST /api/v1/messages/{root}/thread/read` receives `{ position }`, captured from
a confirmed reply actually displayed. `ThreadReadState` contains
`root_id`, `room_id`, `membership_version`, `position`, `revision` and `unread`.
Positions, revisions and counters remain canonical strings, including
beyond `2^53`.

The read advances by maximum, without going back after an old receipt.
It clears neither the unread of the roots nor those of the other threads. Global
room reads now accept `reply_position`; a read of an effective
reply takes the maximum of this position and that of the thread. Room counters
and mentions use the same rule. Own sends, edits,
reactions and system activities add no unread.

The persistent caches keep one intent per thread. A receipt covering an
old observation does not erase a more recent observation. New
membership: positions initialized at the room maximum with no historical backlog.
Removal, change of generation or of membership duration: deletion of the
drafts, intents and private messages of the thread, then refusal of old callbacks.

## List, following and "also send to the room"

Three additive capabilities, each `false` on an older server
(`apps/server/src/threads.rs`, migration `0058_thread_follows.sql`).

**`thread_list`**: `GET /api/v1/rooms/{room}/threads?filter=all|following&before=…&limit=…`
returns `ThreadList`: `threads`, each a `ThreadEntry` (`root` with its `thread`
summary and the reader's `personal_follow`, `last_reply_position`, `following`,
and `unread`, the replies by others after the reader's thread read position),
and `next`, the following page's `before`, absent on the last page. A thread is
a non-deleted root with at least one non-deleted reply; the list is ordered by
its latest such reply's position, newest first, and pages by that position.
`filter` defaults to `all`; anything else answers `400`. The limit is 50 by
default, between 1 and 100. Members of the room only (`404` otherwise). Only
plaintext threads: an encrypted room's roots are opaque `e2ee_application_messages`
routing the server cannot summarize, so its list carries at most the plaintext
roots from before encryption, and clients do not offer it there.

**`thread_follow`**: `PUT /api/v1/messages/{id}/follow` takes `SetMark`
(`operation_id`, `present`), like stars and pins, and answers the root as the
caller reads it. A reply's id acts on its root, like Rocket.Chat's
`chat.followMessage`. The operation receipt lives in `message_actions`:
replaying the same operation answers again without a change, another body
under it is `409`. A deleted root answers `410 message_deleted`, a system
message `403`. The root's author (once, at the first reply, unless they already
chose) and every replier (each time, even after an unfollow) follow
automatically; an explicit unfollow is kept as a row, so other people's replies
do not bring it back. Each change writes a private `message_upsert` of the root
for that account only, carrying `personal_follow {present, revision}` (an
instance-sequence revision of its own, like `personal_star`): the account's
other sessions see it live, and a client keeps the newer revision over an older
copy. Every account-scoped read of a root with replies (history, snapshot,
thread page, list, message) carries `personal_follow`; no row reads as
`{present: false, revision: "0"}`.

**`thread_also_in_room`**: `SendMessage.also_in_room` (with `reply_to` only,
else `400`) marks a reply its author also posted to the room's timeline, like
Rocket.Chat's `tshow`. It stays a reply: `Message.reply_to` is unchanged and
`Message.also_in_room` is `true`. It is part of the send fingerprint only when
set, so earlier receipts replay unchanged and the same operation with the other
value is `409`. `GET /api/v1/rooms/{room}/messages` returns such replies among
the roots only with `also_in_room=true`: an older client keeps a history of
roots only, so its keyset paging never lands on rows it hides. Snapshots and
the sync journal already carried every reply; they now also carry the flag,
which older clients ignore.

## Composers and qualification

Each thread has its own draft, distinct from the room's. GTK and SwiftUI
reuse their panels; mobile uses `/thread/[id]`. The room right and
the availability of the root control sending. A deleted root keeps
the draft and makes the thread consultable without accepting a new reply.
Quotes use the existing cards and controls, including in a thread.

The PostgreSQL tests cover pagination, monotonic / independent reads,
mentions, rights, deletion, replay and removal. The SQLite caches cover
on-disk restart, exact positions, rollback and old memberships.
The real mobile provider verifies a lost confirmation and a replay after
root deletion. The GTK journeys and Swift models use their existing interface
and keychain against PostgreSQL. Physical Android, installed Windows and
the macOS application remain explicitly open external validations.

## The account's threads

Additive capability **`thread_inbox`**, `false` on an older server
(`apps/server/src/threads.rs`, migration `0067_thread_inbox.sql`): the threads
the account follows in every room it is a member of.

`GET /api/v1/threads?filter=followed|unread&before=…&limit=…` returns
`ThreadInbox`: `threads`, each a `ThreadInboxEntry`, and `next`, the following
page's `before`, absent on the last page. `filter` defaults to `followed`;
anything else answers `400`. The limit is 50 by default, between 1 and 100.

- `root`: the root as the reader sees it, with `thread` and `personal_follow`;
- `room_id`, `last_reply_position`, `last_reply_at`, `replies`: the room, the
  latest non-deleted reply's position and time, and the non-deleted replies;
- `participants`: up to five distinct repliers, the most recent first, each
  `{id, username, display_name, avatar_file_id, deleted}`. `avatar_file_id` is
  served by `/api/v1/avatars/{id}`, and absent for a disabled or deleted account;
- `following`: always `true` here;
- `unread`: the replies by others after the reader's thread read position, the
  rule of `ThreadEntry.unread` and of the room counters;
- `mentions`: those of `unread` that name the reader (`direct`, like
  `ReadState.mentions`; `@all` and `@here` do not count).

`followed` is a `present` row of `thread_follows`; `unread` narrows it to threads
with `unread` above zero. A thread is a non-deleted root with at least one
non-deleted reply, so an explicit follow of a root nobody answered does not list
yet. The order is the latest reply's position, newest first, and the cursor is
that position: a thread that moves during paging can appear on two pages, or
none after a deletion, so a client deduplicates by root and reads the first page
again on a change.

**Plaintext threads only.** A room with an E2EE group never lists, not even its
roots from before encryption, and `POST /threads/read` does not touch it: the
server cannot summarize `e2ee_application_messages`, and clients open such a room
through their sealed path. Leaving a room removes its rows from `thread_follows`,
so it stops listing at once and a return starts unfollowed.

`GET /api/v1/threads/totals` returns `ThreadInboxTotals`: `unread_threads`, the
followed threads with unread replies, and `unread_mentions`, the sum of their
`mentions`. A separate route, because a badge has no use for the rows. It reads
every followed thread of the account: a client asks again when its sync cursor
moved, at most about once a second.

`POST /api/v1/threads/read` takes `MarkThreadsRead` (`operation_id`) and answers
`ThreadsReadReceipt`: `operation_id`, `threads`, the threads advanced, and
`applied_revision`, the highest revision published, `"0"` when nothing was
unread. Every followed thread with unread replies moves to its latest non-deleted
reply. It leaves room reads, unfollowed threads and encrypted rooms alone, and
shares the `room_read_limit` budget (one attempt per call).

It journals as a single-thread read does: one private `room_upsert` per room
touched, for this account only, its revision set to that room's new
`room_read_states.revision`. The account's other sessions therefore receive the
room with its lowered `read_state` on their sync; no event names the threads, so
a client holding a list or totals reads them again when its cursor moves. Nothing
is published when nothing was unread.

The receipt lives in `thread_read_all_commands`. Replaying an `operation_id`
answers the first receipt without moving anything, so a retry never reads replies
that arrived since; an `operation_id` already used by another command answers
`409`.
