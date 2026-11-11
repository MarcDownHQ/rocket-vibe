//! Same-room roots and replies, with independent monotone thread reads, the
//! room's thread list and who follows each thread.
use crate::{
    App,
    auth::{Account, identifier, lock_active},
    error::{Error, Result},
    room_reads,
    store::{self, MESSAGE_SELECT, MessageRow},
};
use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use rv_protocol::{
    Change, MarkThreadRead, MarkThreadsRead, Message, PersonalFollow, ThreadEntry, ThreadInbox,
    ThreadInboxEntry, ThreadInboxTotals, ThreadList, ThreadPage, ThreadParticipant,
    ThreadReadState, ThreadsReadReceipt,
    parity::{QuoteReference, SetMark},
};
use sqlx::{Postgres, Transaction};

pub(crate) fn send_fingerprint(
    room: &str,
    text: &str,
    quotes: &[QuoteReference],
    root: Option<&str>,
) -> String {
    match root {
        None => store::quoted_send_fingerprint(room, text, quotes),
        Some(root) => {
            crate::auth::hash_token(&serde_json::json!([room, text, quotes, root]).to_string())
        }
    }
}

pub(crate) async fn validate_root(
    tx: &mut Transaction<'_, Postgres>,
    room: &str,
    root: &str,
) -> Result<()> {
    let row: Option<(Option<String>, bool, bool)> = sqlx::query_as(
        "SELECT reply_to,deleted,system IS NOT NULL FROM messages WHERE id=$1 AND room_id=$2",
    )
    .bind(root)
    .bind(room)
    .fetch_optional(&mut **tx)
    .await?;
    let (parent, deleted, system) = row.ok_or_else(Error::missing)?;
    if deleted {
        return Err(Error::new(StatusCode::GONE, "thread_root_deleted"));
    }
    if parent.is_some() || system {
        return Err(Error::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_thread_root",
        ));
    }
    Ok(())
}

pub(crate) async fn refresh(
    tx: &mut Transaction<'_, Postgres>,
    room: &str,
    root: &str,
) -> Result<()> {
    let revision = store::next_position(tx).await?;
    sqlx::query("UPDATE messages SET revision=$2 WHERE id=$1")
        .bind(root)
        .bind(revision)
        .execute(&mut **tx)
        .await?;
    let message = sqlx::query_as::<_, MessageRow>(&format!("{MESSAGE_SELECT} WHERE m.id=$1"))
        .bind(root)
        .fetch_one(&mut **tx)
        .await?
        .wire();
    store::event(tx, revision, room, None, Change::MessageUpsert(message)).await
}

pub(crate) async fn root_room(
    conn: &mut sqlx::PgConnection,
    user: &str,
    root: &str,
) -> Result<String> {
    if !identifier(root) {
        return Err(Error::invalid());
    }
    sqlx::query_scalar("SELECT m.room_id FROM messages m JOIN members a ON a.room_id=m.room_id WHERE m.id=$1 AND a.user_id=$2 AND m.reply_to IS NULL AND m.system IS NULL")
        .bind(root).bind(user).fetch_optional(conn).await?.ok_or_else(Error::missing)
}

async fn state(
    conn: &mut sqlx::PgConnection,
    user: &str,
    root: &str,
    room: &str,
) -> Result<ThreadReadState> {
    let (membership,position,revision,unread):(String,i64,i64,i64)=sqlx::query_as("SELECT s.membership_version,GREATEST(s.reply_position,COALESCE(t.position,0)),GREATEST(s.revision,COALESCE(t.revision,0)),(SELECT count(*) FROM messages m WHERE m.reply_to=$1 AND m.position>GREATEST(s.reply_position,COALESCE(t.position,0)) AND m.author_id<>s.user_id AND NOT m.deleted) FROM room_read_states s LEFT JOIN thread_read_states t ON t.root_id=$1 AND t.user_id=s.user_id WHERE s.room_id=$2 AND s.user_id=$3")
        .bind(root).bind(room).bind(user).fetch_one(conn).await?;
    Ok(ThreadReadState {
        root_id: root.into(),
        room_id: room.into(),
        membership_version: membership,
        position: position.to_string(),
        revision: revision.to_string(),
        unread: unread.to_string(),
    })
}

pub async fn page(
    app: &App,
    actor: &Account,
    root: &str,
    before: Option<i64>,
    limit: i64,
) -> Result<ThreadPage> {
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let room = root_room(&mut tx, &actor.id, root).await?;
    let parent = sqlx::query_as::<_, MessageRow>(&format!("{MESSAGE_SELECT} WHERE m.id=$1"))
        .bind(root)
        .fetch_one(&mut *tx)
        .await?
        .wire();
    let mut messages = sqlx::query_as::<_, MessageRow>(&format!(
        "{MESSAGE_SELECT} WHERE m.reply_to=$1 AND m.position<$2 ORDER BY m.position DESC LIMIT $3"
    ))
    .bind(root)
    .bind(before.unwrap_or(i64::MAX))
    .bind(limit + 1)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .map(MessageRow::wire)
    .collect::<Vec<_>>();
    let has_more = messages.len() > limit as usize;
    messages.truncate(limit as usize);
    // Resolve root and replies together from the same authorized view.
    messages.insert(0, parent);
    crate::marks::personalize(&mut tx, &actor.id, &mut messages).await?;
    crate::quotes::personalize(&mut tx, &actor.id, &mut messages).await?;
    let parent = messages.remove(0);
    let read_state = state(&mut tx, &actor.id, root, &room).await?;
    tx.commit().await?;
    Ok(ThreadPage {
        root: parent,
        messages,
        has_more,
        read_state,
    })
}

pub async fn mark(
    app: &App,
    actor: &Account,
    root: &str,
    input: MarkThreadRead,
) -> Result<ThreadReadState> {
    let position = room_reads::position(&input.position)?;
    let mut tx = app.pool.begin().await?;
    lock_active(&mut tx, actor).await?;
    let room = root_room(&mut tx, &actor.id, root).await?;
    room_reads::lock_room(&mut tx, &room).await?;
    store::require_member(&mut tx, &room, &actor.id).await?;
    let highest: i64 =
        sqlx::query_scalar("SELECT COALESCE(max(position),0) FROM messages WHERE reply_to=$1")
            .bind(root)
            .fetch_one(&mut *tx)
            .await?;
    if position > highest {
        return Err(Error::new(StatusCode::CONFLICT, "invalid_read_position"));
    }
    let current = state(&mut tx, &actor.id, root, &room).await?;
    if position > room_reads::position(&current.position)? {
        room_reads::admission(&mut tx, &actor.id).await?;
        let revision = store::next_position(&mut tx).await?;
        sqlx::query("INSERT INTO thread_read_states(root_id,room_id,user_id,position,revision) VALUES($1,$2,$3,$4,$5) ON CONFLICT(root_id,user_id) DO UPDATE SET position=GREATEST(thread_read_states.position,excluded.position),revision=excluded.revision")
            .bind(root).bind(&room).bind(&actor.id).bind(position).bind(revision).execute(&mut *tx).await?;
        sqlx::query("UPDATE room_read_states SET revision=$3 WHERE room_id=$1 AND user_id=$2")
            .bind(&room)
            .bind(&actor.id)
            .bind(revision)
            .execute(&mut *tx)
            .await?;
        room_reads::publish(&mut tx, &room, Some(&actor.id), revision).await?;
    }
    let result = state(&mut tx, &actor.id, root, &room).await?;
    tx.commit().await?;
    Ok(result)
}

/// The reader's follow state of these roots, beside `personal_star`. A root
/// with replies and no row reads as not followed at revision 0.
pub(crate) async fn personalize(
    conn: &mut sqlx::PgConnection,
    user: &str,
    messages: &mut [Message],
) -> Result<()> {
    let root = |m: &Message| m.reply_to.is_none() && m.system.is_none() && !m.deleted;
    let ids: Vec<_> = messages
        .iter()
        .filter(|m| root(m))
        .map(|m| m.id.clone())
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let rows: Vec<(String, bool, i64)> = sqlx::query_as(
        "SELECT root_id,present,revision FROM thread_follows WHERE user_id=$1 AND root_id=ANY($2)",
    )
    .bind(user)
    .bind(&ids)
    .fetch_all(conn)
    .await?;
    for message in messages.iter_mut().filter(|m| root(m)) {
        message.personal_follow = match rows.iter().find(|r| r.0 == message.id) {
            Some(row) => Some(Box::new(PersonalFollow {
                present: row.1,
                revision: row.2.to_string(),
            })),
            None if message.thread.is_some() => Some(Box::new(PersonalFollow {
                present: false,
                revision: "0".into(),
            })),
            None => None,
        };
    }
    Ok(())
}

/// Sets `user`'s follow of `root` and tells that account's sessions with a
/// private upsert of the root. Returns whether anything changed.
async fn set_follow(
    tx: &mut Transaction<'_, Postgres>,
    room: &str,
    root: &str,
    user: &str,
    present: bool,
) -> Result<bool> {
    let current: Option<bool> = sqlx::query_scalar(
        "SELECT present FROM thread_follows WHERE root_id=$1 AND user_id=$2 FOR UPDATE",
    )
    .bind(root)
    .bind(user)
    .fetch_optional(&mut **tx)
    .await?;
    // An unfollow before any reply is kept too: the first reply will not override it.
    if current == Some(present) {
        return Ok(false);
    }
    let revision = store::next_position(tx).await?;
    sqlx::query("INSERT INTO thread_follows(root_id,room_id,user_id,present,revision) VALUES($1,$2,$3,$4,$5) ON CONFLICT(root_id,user_id) DO UPDATE SET present=excluded.present,revision=excluded.revision")
        .bind(root).bind(room).bind(user).bind(present).bind(revision).execute(&mut **tx).await?;
    let mut message = sqlx::query_as::<_, MessageRow>(&format!("{MESSAGE_SELECT} WHERE m.id=$1"))
        .bind(root)
        .fetch_one(&mut **tx)
        .await?
        .wire();
    crate::marks::personalize(tx, user, std::slice::from_mut(&mut message)).await?;
    store::event(
        tx,
        revision,
        room,
        Some(user),
        Change::MessageUpsert(message),
    )
    .await?;
    Ok(true)
}

/// A reply makes its author follow the thread again (like Rocket.Chat), and
/// the root's author too the first time, unless they already chose.
pub(crate) async fn take_part(
    tx: &mut Transaction<'_, Postgres>,
    room: &str,
    root: &str,
    author: &str,
) -> Result<()> {
    set_follow(tx, room, root, author, true).await?;
    let first: Option<String> = sqlx::query_scalar(
        "SELECT m.author_id FROM messages m JOIN members a ON a.room_id=m.room_id AND a.user_id=m.author_id WHERE m.id=$1 AND m.author_id<>$2 AND NOT EXISTS(SELECT 1 FROM thread_follows f WHERE f.root_id=m.id AND f.user_id=m.author_id)",
    )
    .bind(root)
    .bind(author)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(first) = first {
        set_follow(tx, room, root, &first, true).await?;
    }
    Ok(())
}

/// `PUT /messages/{id}/follow`: a reply's id acts on its root. Idempotent per
/// operation, like stars. Returns the root's id.
pub async fn follow(app: &App, account: &Account, id: &str, input: SetMark) -> Result<String> {
    if !identifier(id) || !identifier(&input.operation_id) {
        return Err(Error::invalid());
    }
    let hash =
        crate::auth::hash_token(&serde_json::json!(["follow", id, input.present]).to_string());
    let mut tx = app.pool.begin().await?;
    lock_active(&mut tx, account).await?;
    let (room, parent): (String, Option<String>) =
        sqlx::query_as("SELECT room_id,reply_to FROM messages WHERE id=$1")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(Error::missing)?;
    let root = parent.unwrap_or_else(|| id.to_owned());
    store::require_member(&mut tx, &room, &account.id).await?;
    let receipt: Option<String> = sqlx::query_scalar(
        "SELECT command_hash FROM message_actions WHERE user_id=$1 AND operation_id=$2",
    )
    .bind(&account.id)
    .bind(&input.operation_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some(previous) = receipt {
        if previous != hash {
            return Err(Error::conflict());
        }
        tx.commit().await?;
        return Ok(root);
    }
    let used: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE author_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM room_creation_requests WHERE user_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM room_commands WHERE user_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM e2ee_application_messages WHERE user_id=$1 AND operation_id=$2)")
        .bind(&account.id).bind(&input.operation_id).fetch_one(&mut *tx).await?;
    if used {
        return Err(Error::conflict());
    }
    let (deleted, system): (bool, bool) =
        sqlx::query_as("SELECT deleted,system IS NOT NULL FROM messages WHERE id=$1")
            .bind(&root)
            .fetch_one(&mut *tx)
            .await?;
    if deleted {
        return Err(Error::new(StatusCode::GONE, "message_deleted"));
    }
    if system {
        return Err(Error::forbidden());
    }
    crate::limits::message_action(&mut tx, &account.id).await?;
    set_follow(&mut tx, &room, &root, &account.id, input.present).await?;
    sqlx::query("INSERT INTO message_actions(user_id,operation_id,command_hash,message_id) VALUES($1,$2,$3,$4)")
        .bind(&account.id).bind(&input.operation_id).bind(hash).bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(root)
}

/// `GET /rooms/{id}/threads`: the room's roots that have live replies, latest
/// reply first, paged by that reply's position. Plaintext threads only: an
/// encrypted room's roots are opaque routing the server does not list.
pub async fn list(
    app: &App,
    actor: &Account,
    room: &str,
    following: bool,
    before: Option<i64>,
    limit: i64,
) -> Result<ThreadList> {
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    store::require_member(&mut tx, room, &actor.id).await?;
    let mut rows: Vec<(String, i64)> = sqlx::query_as("SELECT r.reply_to,max(r.position) AS last FROM messages r JOIN messages m ON m.id=r.reply_to WHERE r.room_id=$1 AND r.reply_to IS NOT NULL AND NOT r.deleted AND NOT m.deleted AND (NOT $3 OR EXISTS(SELECT 1 FROM thread_follows f WHERE f.root_id=r.reply_to AND f.user_id=$4 AND f.present)) GROUP BY r.reply_to HAVING max(r.position)<$2 ORDER BY last DESC LIMIT $5")
        .bind(room)
        .bind(before.unwrap_or(i64::MAX))
        .bind(following)
        .bind(&actor.id)
        .bind(limit + 1)
        .fetch_all(&mut *tx)
        .await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let ids: Vec<_> = rows.iter().map(|r| r.0.clone()).collect();
    let mut roots: Vec<Message> =
        sqlx::query_as::<_, MessageRow>(&format!("{MESSAGE_SELECT} WHERE m.id=ANY($1)"))
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .map(MessageRow::wire)
            .collect();
    crate::marks::personalize(&mut tx, &actor.id, &mut roots).await?;
    crate::quotes::personalize(&mut tx, &actor.id, &mut roots).await?;
    let unread: Vec<(String, i64)> = sqlx::query_as("SELECT r.reply_to,count(*) FROM messages r JOIN room_read_states s ON s.room_id=r.room_id AND s.user_id=$2 LEFT JOIN thread_read_states t ON t.root_id=r.reply_to AND t.user_id=$2 WHERE r.reply_to=ANY($1) AND NOT r.deleted AND r.author_id<>$2 AND r.position>GREATEST(s.reply_position,COALESCE(t.position,0)) GROUP BY r.reply_to")
        .bind(&ids)
        .bind(&actor.id)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    let next = if has_more {
        rows.last().map(|r| r.1.to_string())
    } else {
        None
    };
    let mut threads = Vec::with_capacity(rows.len());
    for (id, last) in rows {
        let Some(index) = roots.iter().position(|m| m.id == id) else {
            continue;
        };
        let root = roots.swap_remove(index);
        threads.push(ThreadEntry {
            following: root.personal_follow.as_ref().is_some_and(|f| f.present),
            unread: unread
                .iter()
                .find(|u| u.0 == id)
                .map_or(0, |u| u.1)
                .to_string(),
            last_reply_position: last.to_string(),
            root,
        });
    }
    Ok(ThreadList { threads, next })
}

/// Followed threads of `$1` the reader may see: members only, live root, not an
/// encrypted room. `s` is the reader's room read state, `t` the thread's.
const INBOX_FROM: &str = "FROM thread_follows f JOIN members a ON a.room_id=f.room_id AND a.user_id=f.user_id JOIN messages m ON m.id=f.root_id AND NOT m.deleted JOIN room_read_states s ON s.room_id=f.room_id AND s.user_id=f.user_id LEFT JOIN thread_read_states t ON t.root_id=f.root_id AND t.user_id=f.user_id";
const INBOX_WHERE: &str = "WHERE f.user_id=$1 AND f.present AND NOT EXISTS(SELECT 1 FROM e2ee_groups g WHERE g.room_id=f.room_id)";
/// Replies `u` of `f.root_id` the reader has not read, by the same rule as
/// `state` and the room counters.
const INBOX_UNREAD: &str = "u.room_id=f.room_id AND u.reply_to=f.root_id AND NOT u.deleted AND u.author_id<>$1 AND u.position>GREATEST(s.reply_position,COALESCE(t.position,0))";
const INBOX_PARTICIPANTS: i64 = 5;

/// `GET /threads`: the threads this account follows in all its rooms, latest
/// reply first, paged by that reply's position.
pub async fn inbox(
    app: &App,
    actor: &Account,
    unread_only: bool,
    before: Option<i64>,
    limit: i64,
) -> Result<ThreadInbox> {
    let mut tx = app.pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await?;
    let mut rows: Vec<(String, i64, DateTime<Utc>)> = sqlx::query_as(&format!(
        "SELECT f.root_id,l.position,l.created_at {INBOX_FROM} CROSS JOIN LATERAL (SELECT r.position,r.created_at FROM messages r WHERE r.room_id=f.room_id AND r.reply_to=f.root_id AND NOT r.deleted ORDER BY r.position DESC LIMIT 1) l {INBOX_WHERE} AND l.position<$2 AND (NOT $3 OR EXISTS(SELECT 1 FROM messages u WHERE {INBOX_UNREAD})) ORDER BY l.position DESC LIMIT $4"
    ))
    .bind(&actor.id)
    .bind(before.unwrap_or(i64::MAX))
    .bind(unread_only)
    .bind(limit + 1)
    .fetch_all(&mut *tx)
    .await?;
    let has_more = rows.len() > limit as usize;
    rows.truncate(limit as usize);
    let ids: Vec<_> = rows.iter().map(|r| r.0.clone()).collect();
    let mut roots: Vec<Message> =
        sqlx::query_as::<_, MessageRow>(&format!("{MESSAGE_SELECT} WHERE m.id=ANY($1)"))
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await?
            .into_iter()
            .map(MessageRow::wire)
            .collect();
    crate::marks::personalize(&mut tx, &actor.id, &mut roots).await?;
    crate::quotes::personalize(&mut tx, &actor.id, &mut roots).await?;
    let counts: Vec<(String, i64, i64)> = sqlx::query_as(&format!(
        "SELECT f.root_id,count(u.id),count(u.id) FILTER(WHERE EXISTS(SELECT 1 FROM message_mentions p WHERE p.message_id=u.id AND p.user_id=$1 AND p.kind='direct')) {INBOX_FROM} JOIN messages u ON {INBOX_UNREAD} {INBOX_WHERE} AND f.root_id=ANY($2) GROUP BY f.root_id"
    ))
    .bind(&actor.id)
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await?;
    let people: Vec<(String, String, String, String, Option<String>, bool)> = sqlx::query_as(
        "SELECT root.id,u.id,u.username,u.display_name,CASE WHEN u.disabled OR u.deleted THEN NULL ELSE u.avatar_file_id END,u.deleted FROM unnest($1::text[]) AS root(id) CROSS JOIN LATERAL (SELECT r.author_id,max(r.position) AS last FROM messages r WHERE r.reply_to=root.id AND NOT r.deleted GROUP BY r.author_id ORDER BY last DESC LIMIT $2) l JOIN users u ON u.id=l.author_id ORDER BY root.id,l.last DESC",
    )
    .bind(&ids)
    .bind(INBOX_PARTICIPANTS)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    let next = if has_more {
        rows.last().map(|r| r.1.to_string())
    } else {
        None
    };
    let mut threads = Vec::with_capacity(rows.len());
    for (id, last, last_at) in rows {
        let Some(index) = roots.iter().position(|m| m.id == id) else {
            continue;
        };
        let root = roots.swap_remove(index);
        let (unread, mentions) = counts
            .iter()
            .find(|c| c.0 == id)
            .map_or((0, 0), |c| (c.1, c.2));
        threads.push(ThreadInboxEntry {
            room_id: root.room_id.clone(),
            last_reply_position: last.to_string(),
            last_reply_at: last_at.to_rfc3339(),
            replies: root
                .thread
                .as_ref()
                .map_or_else(|| "0".to_owned(), |t| t.replies.clone()),
            participants: people
                .iter()
                .filter(|p| p.0 == id)
                .map(|p| ThreadParticipant {
                    id: p.1.clone(),
                    username: p.2.clone(),
                    display_name: p.3.clone(),
                    avatar_file_id: p.4.clone(),
                    deleted: p.5,
                })
                .collect(),
            following: true,
            unread: unread.to_string(),
            mentions: mentions.to_string(),
            root,
        });
    }
    Ok(ThreadInbox { threads, next })
}

/// `GET /threads/totals`: what the Threads badge shows.
pub async fn inbox_totals(app: &App, actor: &Account) -> Result<ThreadInboxTotals> {
    let (threads, mentions): (i64, i64) = sqlx::query_as(&format!(
        "SELECT count(*) FILTER(WHERE c.unread>0),COALESCE(sum(c.mentions),0)::bigint {INBOX_FROM} CROSS JOIN LATERAL (SELECT count(*) AS unread,count(*) FILTER(WHERE EXISTS(SELECT 1 FROM message_mentions p WHERE p.message_id=u.id AND p.user_id=$1 AND p.kind='direct')) AS mentions FROM messages u WHERE {INBOX_UNREAD}) c {INBOX_WHERE}"
    ))
    .bind(&actor.id)
    .fetch_one(&app.pool)
    .await?;
    Ok(ThreadInboxTotals {
        unread_threads: threads.to_string(),
        unread_mentions: mentions.to_string(),
    })
}

/// `POST /threads/read`: every followed thread read up to its latest reply. One
/// private room event per room touched, as `mark` publishes for one thread; a
/// replay answers the first receipt and reads nothing that arrived since.
pub async fn mark_inbox_read(
    app: &App,
    actor: &Account,
    input: MarkThreadsRead,
) -> Result<ThreadsReadReceipt> {
    if !identifier(&input.operation_id) {
        return Err(Error::invalid());
    }
    let mut tx = app.pool.begin().await?;
    lock_active(&mut tx, actor).await?;
    let old: Option<(i64, i64)> = sqlx::query_as(
        "SELECT threads,applied_revision FROM thread_read_all_commands WHERE user_id=$1 AND operation_id=$2",
    )
    .bind(&actor.id)
    .bind(&input.operation_id)
    .fetch_optional(&mut *tx)
    .await?;
    if let Some((threads, applied)) = old {
        tx.commit().await?;
        return Ok(ThreadsReadReceipt {
            operation_id: input.operation_id,
            threads: threads.to_string(),
            applied_revision: applied.to_string(),
        });
    }
    let used: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE author_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM room_creation_requests WHERE user_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM room_commands WHERE user_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM message_actions WHERE user_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM e2ee_application_messages WHERE user_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM e2ee_message_cancellations WHERE user_id=$1 AND operation_id=$2)")
        .bind(&actor.id)
        .bind(&input.operation_id)
        .fetch_one(&mut *tx)
        .await?;
    if used {
        return Err(Error::conflict());
    }
    room_reads::admission(&mut tx, &actor.id).await?;
    let unread_threads = format!(
        "{INBOX_FROM} {INBOX_WHERE} AND EXISTS(SELECT 1 FROM messages u WHERE {INBOX_UNREAD})"
    );
    let rooms: Vec<String> =
        sqlx::query_scalar(&format!("SELECT DISTINCT f.room_id {unread_threads}"))
            .bind(&actor.id)
            .fetch_all(&mut *tx)
            .await?;
    // In one order, so two of these cannot deadlock over the rooms they share.
    let locked: Vec<String> =
        sqlx::query_scalar("SELECT id FROM rooms WHERE id=ANY($1) ORDER BY id FOR UPDATE")
            .bind(&rooms)
            .fetch_all(&mut *tx)
            .await?;
    let targets: Vec<(String, String, i64)> = sqlx::query_as(&format!(
        "SELECT f.room_id,f.root_id,(SELECT max(r.position) FROM messages r WHERE r.room_id=f.room_id AND r.reply_to=f.root_id AND NOT r.deleted) {unread_threads} AND f.room_id=ANY($2) ORDER BY f.room_id"
    ))
    .bind(&actor.id)
    .bind(&locked)
    .fetch_all(&mut *tx)
    .await?;
    let mut applied = 0;
    for chunk in targets.chunk_by(|a, b| a.0 == b.0) {
        let room = &chunk[0].0;
        let roots: Vec<_> = chunk.iter().map(|t| t.1.clone()).collect();
        let positions: Vec<_> = chunk.iter().map(|t| t.2).collect();
        let revision = store::next_position(&mut tx).await?;
        sqlx::query("INSERT INTO thread_read_states(root_id,room_id,user_id,position,revision) SELECT root,$1,$2,position,$5 FROM unnest($3::text[],$4::bigint[]) AS target(root,position) ON CONFLICT(root_id,user_id) DO UPDATE SET position=GREATEST(thread_read_states.position,excluded.position),revision=excluded.revision")
            .bind(room)
            .bind(&actor.id)
            .bind(&roots)
            .bind(&positions)
            .bind(revision)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE room_read_states SET revision=$3 WHERE room_id=$1 AND user_id=$2")
            .bind(room)
            .bind(&actor.id)
            .bind(revision)
            .execute(&mut *tx)
            .await?;
        room_reads::publish(&mut tx, room, Some(&actor.id), revision).await?;
        applied = revision;
    }
    sqlx::query("INSERT INTO thread_read_all_commands(user_id,operation_id,threads,applied_revision) VALUES($1,$2,$3,$4)")
        .bind(&actor.id)
        .bind(&input.operation_id)
        .bind(targets.len() as i64)
        .bind(applied)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(ThreadsReadReceipt {
        operation_id: input.operation_id,
        threads: targets.len().to_string(),
        applied_revision: applied.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth;
    use rv_protocol::{CreateRoom, SendMessage};
    use sqlx::PgPool;

    const PASSWORD: &str = "disposable-threads-test-password";

    async fn account(app: &App, name: &str) -> Account {
        auth::create_user(app, name, PASSWORD.into(), false)
            .await
            .unwrap();
        let session = auth::login(app, name.into(), PASSWORD.into())
            .await
            .unwrap();
        auth::authenticate(app, &auth::hash_token(&session.token))
            .await
            .unwrap()
    }

    fn say(id: &str, root: Option<&str>, also_in_room: bool) -> SendMessage {
        SendMessage {
            also_in_room,
            operation_id: id.into(),
            text: format!("text of {id}"),
            reply_to: root.map(Into::into),
            quotes: vec![],
            cards: vec![],
            files: vec![],
        }
    }

    fn mark(id: &str, present: bool) -> SetMark {
        SetMark {
            operation_id: id.into(),
            present,
        }
    }

    async fn room(app: &App, owner: &Account, members: &[&Account]) -> String {
        let room = store::create_room(
            app,
            owner,
            CreateRoom {
                name: "Threads".into(),
                private: true,
                operation_id: None,
                voice: false,
            },
        )
        .await
        .unwrap();
        for member in members {
            store::membership(app, owner, &room.id, &member.id, false)
                .await
                .unwrap();
        }
        room.id
    }

    async fn ids(app: &App, who: &Account, room: &str, following: bool) -> Vec<String> {
        list(app, who, room, following, None, 50)
            .await
            .unwrap()
            .threads
            .into_iter()
            .map(|t| t.root.id)
            .collect()
    }

    /// Private root upserts in the journal for `user`, carrying their follow state.
    async fn private_follow_events(app: &App, user: &str, root: &str) -> Vec<bool> {
        let rows: Vec<sqlx::types::Json<Change>> = sqlx::query_scalar(
            "SELECT change FROM journal WHERE recipient_id=$1 AND change->>'type'='message_upsert' AND change#>>'{data,id}'=$2 ORDER BY position",
        )
        .bind(user)
        .bind(root)
        .fetch_all(&app.pool)
        .await
        .unwrap();
        rows.into_iter()
            .map(|change| match change.0 {
                Change::MessageUpsert(message) => message.personal_follow.unwrap().present,
                _ => unreachable!(),
            })
            .collect()
    }

    fn say_to(id: &str, root: Option<&str>, text: &str) -> SendMessage {
        SendMessage {
            text: text.into(),
            ..say(id, root, false)
        }
    }

    async fn inbox_ids(app: &App, who: &Account, unread: bool) -> Vec<String> {
        inbox(app, who, unread, None, 50)
            .await
            .unwrap()
            .threads
            .into_iter()
            .map(|t| t.root.id)
            .collect()
    }

    /// Private room events in the journal for `user`, as (room, recipient) pairs.
    async fn private_room_events(app: &App, user: &str) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT room_id FROM journal WHERE recipient_id=$1 AND change->>'type'='room_upsert' ORDER BY position",
        )
        .bind(user)
        .fetch_all(&app.pool)
        .await
        .unwrap()
    }

    #[sqlx::test]
    async fn the_inbox_spans_my_rooms_latest_reply_first_and_pages(pool: PgPool) {
        let app = App::from_pool(pool).await.unwrap();
        let alice = account(&app, "inbox-alice").await;
        let bob = account(&app, "inbox-bob").await;
        let carol = account(&app, "inbox-carol").await;
        let one = room(&app, &alice, &[&bob, &carol]).await;
        let two = room(&app, &bob, &[&alice]).await;
        store::send(&app, &alice, &one, say("a-root", None, false))
            .await
            .unwrap();
        store::send(&app, &bob, &two, say("b-root", None, false))
            .await
            .unwrap();
        store::send(&app, &bob, &one, say("no-replies", None, false))
            .await
            .unwrap();
        follow(&app, &alice, "no-replies", mark("op-no-replies", true))
            .await
            .unwrap();
        store::send(&app, &bob, &one, say("a-1", Some("a-root"), false))
            .await
            .unwrap();
        store::send(&app, &alice, &two, say("b-1", Some("b-root"), false))
            .await
            .unwrap();
        store::send(
            &app,
            &bob,
            &two,
            say_to("b-2", Some("b-root"), "@inbox-alice look"),
        )
        .await
        .unwrap();
        store::send(&app, &carol, &one, say("a-2", Some("a-root"), false))
            .await
            .unwrap();

        // Followed across both rooms; a root with no reply is not a thread.
        assert_eq!(inbox_ids(&app, &alice, false).await, ["a-root", "b-root"]);
        let first = inbox(&app, &alice, false, None, 1).await.unwrap();
        assert_eq!(first.threads.len(), 1);
        let entry = &first.threads[0];
        assert_eq!(entry.root.id, "a-root");
        assert_eq!(entry.room_id, one);
        assert_eq!(entry.replies, "2");
        assert_eq!(entry.unread, "2");
        assert_eq!(entry.mentions, "0");
        assert!(entry.following);
        assert_eq!(
            Some(entry.last_reply_at.as_str()),
            entry
                .root
                .thread
                .as_ref()
                .and_then(|t| t.last_reply_at.as_deref())
        );
        assert_eq!(first.next.as_deref(), Some(&*entry.last_reply_position));
        // Repliers only, the most recent first.
        assert_eq!(
            entry
                .participants
                .iter()
                .map(|p| p.username.as_str())
                .collect::<Vec<_>>(),
            ["inbox-carol", "inbox-bob"]
        );
        let before = room_reads::position(first.next.as_ref().unwrap()).unwrap();
        let second = inbox(&app, &alice, false, Some(before), 1).await.unwrap();
        assert_eq!(second.threads[0].root.id, "b-root");
        assert_eq!(second.threads[0].unread, "1");
        assert_eq!(second.threads[0].mentions, "1");
        assert!(second.next.is_none());

        assert_eq!(
            inbox_totals(&app, &alice).await.unwrap(),
            ThreadInboxTotals {
                unread_threads: "2".into(),
                unread_mentions: "1".into(),
            }
        );
        // Reading one thread leaves the other; only others' replies count.
        let read = room_reads::position(&second.threads[0].last_reply_position).unwrap();
        super::mark(
            &app,
            &alice,
            "b-root",
            MarkThreadRead {
                position: read.to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(inbox_ids(&app, &alice, true).await, ["a-root"]);
        assert_eq!(inbox_ids(&app, &alice, false).await, ["a-root", "b-root"]);
        assert_eq!(
            inbox_totals(&app, &alice).await.unwrap(),
            ThreadInboxTotals {
                unread_threads: "1".into(),
                unread_mentions: "0".into(),
            }
        );
        // An own reply is no unread for its author.
        assert!(
            inbox_ids(&app, &carol, true)
                .await
                .contains(&"a-root".to_owned())
        );
        store::send(&app, &carol, &one, say("a-3", Some("a-root"), false))
            .await
            .unwrap();
        let carols = inbox(&app, &carol, false, None, 50).await.unwrap();
        assert_eq!(carols.threads[0].unread, "1");
        // Unfollowing removes it from the list and from the totals.
        follow(&app, &alice, "a-root", mark("op-off", false))
            .await
            .unwrap();
        assert_eq!(inbox_ids(&app, &alice, false).await, ["b-root"]);
        assert_eq!(
            inbox_totals(&app, &alice).await.unwrap().unread_threads,
            "0"
        );
        // A root deleted, or all its replies, leaves the list.
        sqlx::query("UPDATE messages SET deleted=true WHERE id='b-root'")
            .execute(&app.pool)
            .await
            .unwrap();
        assert!(inbox_ids(&app, &alice, false).await.is_empty());
    }

    #[sqlx::test]
    async fn the_inbox_never_shows_a_room_i_am_not_in(pool: PgPool) {
        let app = App::from_pool(pool).await.unwrap();
        let alice = account(&app, "leak-alice").await;
        let bob = account(&app, "leak-bob").await;
        let eve = account(&app, "leak-eve").await;
        let room = room(&app, &alice, &[&bob]).await;
        store::send(&app, &alice, &room, say("root", None, false))
            .await
            .unwrap();
        store::send(&app, &bob, &room, say("re-1", Some("root"), false))
            .await
            .unwrap();
        store::send(&app, &alice, &room, say("re-2", Some("root"), false))
            .await
            .unwrap();
        assert!(inbox_ids(&app, &eve, false).await.is_empty());
        assert_eq!(inbox_ids(&app, &bob, true).await, ["root"]);
        store::membership(&app, &alice, &room, &bob.id, true)
            .await
            .unwrap();
        assert!(inbox_ids(&app, &bob, false).await.is_empty());
        assert!(inbox_ids(&app, &bob, true).await.is_empty());
        assert_eq!(inbox_totals(&app, &bob).await.unwrap().unread_threads, "0");
        store::send(&app, &alice, &room, say("re-3", Some("root"), false))
            .await
            .unwrap();
        // Marking all touches no room I left.
        let before = private_room_events(&app, &bob.id).await.len();
        let receipt = mark_inbox_read(
            &app,
            &bob,
            MarkThreadsRead {
                operation_id: "op-gone".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(receipt.threads, "0");
        assert_eq!(receipt.applied_revision, "0");
        assert_eq!(private_room_events(&app, &bob.id).await.len(), before);
        // Coming back does not bring the old follow or a backlog.
        store::membership(&app, &alice, &room, &bob.id, false)
            .await
            .unwrap();
        assert!(inbox_ids(&app, &bob, false).await.is_empty());
    }

    #[sqlx::test]
    async fn the_inbox_leaves_out_an_encrypted_room(pool: PgPool) {
        let app = App::from_pool(pool).await.unwrap();
        let alice = account(&app, "sealed-alice").await;
        let bob = account(&app, "sealed-bob").await;
        let room = room(&app, &alice, &[&bob]).await;
        store::send(&app, &alice, &room, say("root", None, false))
            .await
            .unwrap();
        store::send(&app, &bob, &room, say("re-1", Some("root"), false))
            .await
            .unwrap();
        assert_eq!(inbox_ids(&app, &alice, true).await, ["root"]);
        sqlx::query("INSERT INTO e2ee_groups(room_id,data_epoch,incarnation,revision,epoch,fingerprint,transition,tree,receipt) VALUES($1,'e','i',1,0,'f','','','{}')")
            .bind(&room)
            .execute(&app.pool)
            .await
            .unwrap();
        assert!(inbox_ids(&app, &alice, false).await.is_empty());
        assert_eq!(
            inbox_totals(&app, &alice).await.unwrap().unread_threads,
            "0"
        );
        let receipt = mark_inbox_read(
            &app,
            &alice,
            MarkThreadsRead {
                operation_id: "op-sealed".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(receipt.threads, "0");
    }

    #[sqlx::test]
    async fn marking_all_reads_followed_threads_once_and_tells_only_me(pool: PgPool) {
        let app = App::from_pool(pool).await.unwrap();
        let alice = account(&app, "all-alice").await;
        let bob = account(&app, "all-bob").await;
        let carol = account(&app, "all-carol").await;
        let one = room(&app, &alice, &[&bob, &carol]).await;
        let two = room(&app, &alice, &[&bob]).await;
        for (id, room) in [("r-1", &one), ("r-2", &one), ("r-3", &two), ("quiet", &one)] {
            store::send(&app, &alice, room, say(id, None, false))
                .await
                .unwrap();
        }
        for (id, root, room) in [
            ("re-1", "r-1", &one),
            ("re-2", "r-2", &one),
            ("re-3", "r-3", &two),
        ] {
            store::send(&app, &bob, room, say(id, Some(root), false))
                .await
                .unwrap();
        }
        // Not followed by Alice: a reply there stays unread for the room.
        follow(&app, &alice, "r-2", mark("op-off", false))
            .await
            .unwrap();
        let unfollowed = room_reads::read(&app, &alice, &one).await.unwrap();
        assert_eq!(unfollowed.unread_replies, "2");
        assert_eq!(
            inbox_totals(&app, &alice).await.unwrap().unread_threads,
            "2"
        );
        let bob_events = private_room_events(&app, &bob.id).await.len();
        let before = private_room_events(&app, &alice.id).await.len();

        let input = || MarkThreadsRead {
            operation_id: "op-all".into(),
        };
        let receipt = mark_inbox_read(&app, &alice, input()).await.unwrap();
        assert_eq!(receipt.threads, "2");
        assert_ne!(receipt.applied_revision, "0");
        assert!(inbox_ids(&app, &alice, true).await.is_empty());
        assert_eq!(
            inbox_totals(&app, &alice).await.unwrap(),
            ThreadInboxTotals {
                unread_threads: "0".into(),
                unread_mentions: "0".into(),
            }
        );
        // The room counters follow, the unfollowed thread stays unread, and no
        // root read moved.
        let read = room_reads::read(&app, &alice, &one).await.unwrap();
        assert_eq!(read.unread_replies, "1");
        // One private event per room, for my sessions alone.
        let mut events = private_room_events(&app, &alice.id).await[before..].to_vec();
        events.sort();
        let mut rooms = vec![one.clone(), two.clone()];
        rooms.sort();
        assert_eq!(events, rooms);
        assert_eq!(private_room_events(&app, &bob.id).await.len(), bob_events);
        assert_eq!(inbox_ids(&app, &bob, true).await, Vec::<String>::new());

        // A replay answers the first receipt and reads nothing newer.
        store::send(&app, &bob, &two, say("re-4", Some("r-3"), false))
            .await
            .unwrap();
        let replay = mark_inbox_read(&app, &alice, input()).await.unwrap();
        assert_eq!(replay, receipt);
        assert_eq!(inbox_ids(&app, &alice, true).await, ["r-3"]);
        let after = private_room_events(&app, &alice.id).await.len();
        assert_eq!(after, before + 2);
        // A new operation does, and with nothing unread publishes nothing.
        let second = mark_inbox_read(
            &app,
            &alice,
            MarkThreadsRead {
                operation_id: "op-all-2".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(second.threads, "1");
        assert!(inbox_ids(&app, &alice, true).await.is_empty());
        let idle = mark_inbox_read(
            &app,
            &alice,
            MarkThreadsRead {
                operation_id: "op-all-3".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(idle.threads, "0");
        assert_eq!(idle.applied_revision, "0");
        assert_eq!(private_room_events(&app, &alice.id).await.len(), after + 1);
        // An operation id already used elsewhere conflicts.
        store::send(&app, &alice, &one, say("op-taken", None, false))
            .await
            .unwrap();
        assert!(
            mark_inbox_read(
                &app,
                &alice,
                MarkThreadsRead {
                    operation_id: "op-taken".into(),
                },
            )
            .await
            .is_err()
        );
    }

    #[sqlx::test]
    async fn the_list_puts_the_latest_reply_first_and_pages(pool: PgPool) {
        let app = App::from_pool(pool).await.unwrap();
        let alice = account(&app, "threads-alice").await;
        let bob = account(&app, "threads-bob").await;
        let carol = account(&app, "threads-carol").await;
        let room = room(&app, &alice, &[&bob, &carol]).await;
        for root in ["root-1", "root-2", "root-3"] {
            store::send(&app, &alice, &room, say(root, None, false))
                .await
                .unwrap();
        }
        // root-3 has no reply: not a thread. root-1 gets the latest reply.
        for (reply, root) in [("re-1", "root-1"), ("re-2", "root-2"), ("re-3", "root-1")] {
            store::send(&app, &bob, &room, say(reply, Some(root), false))
                .await
                .unwrap();
        }
        assert_eq!(ids(&app, &carol, &room, false).await, ["root-1", "root-2"]);
        let first = list(&app, &carol, &room, false, None, 1).await.unwrap();
        assert_eq!(first.threads.len(), 1);
        assert_eq!(first.threads[0].root.id, "root-1");
        assert_eq!(first.threads[0].root.thread.as_ref().unwrap().replies, "2");
        assert_eq!(first.threads[0].unread, "2");
        assert!(!first.threads[0].following);
        let before = room_reads::position(first.next.as_ref().unwrap()).unwrap();
        let second = list(&app, &carol, &room, false, Some(before), 1)
            .await
            .unwrap();
        assert_eq!(second.threads[0].root.id, "root-2");
        assert!(second.next.is_none());
        // The replier and the root's author follow both; a bystander none.
        assert_eq!(ids(&app, &bob, &room, true).await, ["root-1", "root-2"]);
        assert_eq!(ids(&app, &alice, &room, true).await, ["root-1", "root-2"]);
        assert!(ids(&app, &carol, &room, true).await.is_empty());
        // A deleted reply leaves the order; a root whose replies are all gone leaves the list.
        sqlx::query("UPDATE messages SET deleted=true WHERE id='re-2'")
            .execute(&app.pool)
            .await
            .unwrap();
        assert_eq!(ids(&app, &carol, &room, false).await, ["root-1"]);
        // Members only.
        let dave = account(&app, "threads-dave").await;
        assert!(list(&app, &dave, &room, false, None, 50).await.is_err());
    }

    #[sqlx::test]
    async fn following_is_idempotent_private_and_comes_back_with_a_reply(pool: PgPool) {
        let app = App::from_pool(pool).await.unwrap();
        let alice = account(&app, "follow-alice").await;
        let bob = account(&app, "follow-bob").await;
        let carol = account(&app, "follow-carol").await;
        let room = room(&app, &alice, &[&bob, &carol]).await;
        store::send(&app, &alice, &room, say("root", None, false))
            .await
            .unwrap();
        store::send(&app, &bob, &room, say("reply-1", Some("root"), false))
            .await
            .unwrap();
        // The replier and the root's author were told, each privately.
        assert_eq!(private_follow_events(&app, &bob.id, "root").await, [true]);
        assert_eq!(private_follow_events(&app, &alice.id, "root").await, [true]);
        // Carol follows through a reply's id: it acts on the root.
        assert_eq!(
            follow(&app, &carol, "reply-1", mark("op-follow", true))
                .await
                .unwrap(),
            "root"
        );
        // The same operation again changes nothing; another body under it conflicts.
        follow(&app, &carol, "reply-1", mark("op-follow", true))
            .await
            .unwrap();
        assert!(
            follow(&app, &carol, "reply-1", mark("op-follow", false))
                .await
                .is_err()
        );
        // A new operation to the same state publishes nothing more.
        follow(&app, &carol, "root", mark("op-follow-2", true))
            .await
            .unwrap();
        assert_eq!(private_follow_events(&app, &carol.id, "root").await, [true]);
        assert_eq!(ids(&app, &carol, &room, true).await, ["root"]);
        // Reads of the root carry the reader's own state, and only theirs.
        let page = page(&app, &carol, "root", None, 50).await.unwrap();
        assert!(page.root.personal_follow.unwrap().present);
        // Bob unfollows: a reply by someone else does not bring it back...
        follow(&app, &bob, "root", mark("op-unfollow", false))
            .await
            .unwrap();
        assert!(ids(&app, &bob, &room, true).await.is_empty());
        store::send(&app, &alice, &room, say("reply-2", Some("root"), false))
            .await
            .unwrap();
        assert!(ids(&app, &bob, &room, true).await.is_empty());
        // ...his own reply does, like Rocket.Chat.
        store::send(&app, &bob, &room, say("reply-3", Some("root"), false))
            .await
            .unwrap();
        assert_eq!(ids(&app, &bob, &room, true).await, ["root"]);
        assert_eq!(
            private_follow_events(&app, &bob.id, "root").await,
            [true, false, true]
        );
        // An unfollow by the root's author before anyone replies holds.
        store::send(&app, &alice, &room, say("quiet", None, false))
            .await
            .unwrap();
        follow(&app, &alice, "quiet", mark("op-quiet", false))
            .await
            .unwrap();
        store::send(&app, &bob, &room, say("reply-q", Some("quiet"), false))
            .await
            .unwrap();
        assert_eq!(ids(&app, &alice, &room, true).await, ["root"]);
        // Outsiders and unknown messages: missing.
        let dave = account(&app, "follow-dave").await;
        assert!(
            follow(&app, &dave, "root", mark("op-dave", true))
                .await
                .is_err()
        );
        assert!(
            follow(&app, &carol, "nothing", mark("op-none", true))
                .await
                .is_err()
        );
    }

    #[sqlx::test]
    async fn a_reply_also_sent_to_the_room_shows_in_its_history_on_request(pool: PgPool) {
        let app = App::from_pool(pool).await.unwrap();
        let alice = account(&app, "shown-alice").await;
        let room = room(&app, &alice, &[]).await;
        store::send(&app, &alice, &room, say("root", None, false))
            .await
            .unwrap();
        // Only a reply can be shown in the room.
        assert!(
            store::send(&app, &alice, &room, say("lonely", None, true))
                .await
                .is_err()
        );
        let shown = store::send(&app, &alice, &room, say("shown", Some("root"), true))
            .await
            .unwrap();
        assert!(shown.also_in_room);
        assert_eq!(shown.reply_to.as_deref(), Some("root"));
        store::send(&app, &alice, &room, say("hidden", Some("root"), false))
            .await
            .unwrap();
        // A replay keeps it; the same operation without the flag conflicts.
        assert!(
            store::send(&app, &alice, &room, say("shown", Some("root"), true))
                .await
                .unwrap()
                .also_in_room
        );
        assert!(
            store::send(&app, &alice, &room, say("shown", Some("root"), false))
                .await
                .is_err()
        );
        let history = |also| {
            let (app, alice, room) = (&app, &alice, &room);
            async move {
                store::history(app, alice, room, None, 50, also)
                    .await
                    .unwrap()
                    .messages
                    .into_iter()
                    .filter(|m| m.system.is_none())
                    .map(|m| m.id)
                    .collect::<Vec<_>>()
            }
        };
        // Older clients keep a timeline of roots only.
        assert_eq!(history(false).await, ["root"]);
        assert_eq!(history(true).await, ["shown", "root"]);
        // It stays a reply of its thread.
        let thread = page(&app, &alice, "root", None, 50).await.unwrap();
        assert_eq!(
            thread
                .messages
                .iter()
                .map(|m| m.id.as_str())
                .collect::<Vec<_>>(),
            ["hidden", "shown"]
        );
    }
}
