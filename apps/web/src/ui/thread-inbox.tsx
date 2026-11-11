import { useCallback, useEffect, useRef, useState } from "react";
import type { App } from "../app";
import type { ThreadInbox, ThreadInboxEntry } from "../protocol";
import { t } from "../i18n";
import { nt } from "../native-i18n";
import { toast } from "../dom";
import { personName, previewText } from "../presentation";
import {
  appendPage,
  countLabel,
  EXTRA_PAGES,
  INBOX_FILTERS,
  matchesFilter,
  refreshHead,
  refreshLimit,
  threadTraits,
  type InboxFilter,
  type InboxTab,
} from "../thread-inbox";
import { ActionButton, Avatar, IconButton, Symbol } from "./controls";
import { roomTime } from "./rooms";
import "../thread-inbox.css";

const SHOWN_PARTICIPANTS = 3;
const NEAR_END = 120;

const CHIP_LABELS = {
  all: "threadsAll",
  participated: "threadsParticipated",
  mentioned: "threadsMentioned",
  others: "threadsOthers",
} as const;

const CHIP_EMPTY = {
  participated: "threadsNoParticipated",
  mentioned: "threadsNoMentioned",
  others: "threadsNoOthers",
} as const;

const nearEnd = (node: HTMLElement) =>
  node.scrollHeight - node.scrollTop - node.clientHeight < NEAR_END;

export function ThreadsRow({ app }: { app: App }) {
  const totals = app.threadTotals;
  const unread = totals?.unread_threads ?? "0";
  return (
    <ActionButton
      className="threads-row"
      action={() => app.showThreads("followed")}
    >
      <Symbol name="threads" />
      <span className="threads-row-title">{t("threads")}</span>
      {unread !== "0" && (
        <span
          className={
            "badge " +
            (totals?.unread_mentions !== "0" ? "badge-mention" : "badge-unread")
          }
        >
          {countLabel(unread)}
        </span>
      )}
    </ActionButton>
  );
}

function ThreadRow({ app, entry }: { app: App; entry: ThreadInboxEntry }) {
  const room = app.model.rooms.get(entry.room_id);
  const replies = Number(entry.replies);
  return (
    <ActionButton
      className={"inbox-row" + (app.root === entry.root.id ? " selected" : "")}
      data-thread={entry.root.id}
      action={() => app.openInboxThread(entry.room_id, entry.root.id)}
    >
      <div className="inbox-top">
        <span className="inbox-author">{personName(entry.root.author)}</span>
        {room && <span className="inbox-room">{room.name}</span>}
        <span className="inbox-time">{roomTime(entry.last_reply_at)}</span>
      </div>
      <div className="inbox-preview">{previewText(entry.root)}</div>
      <div className="inbox-bottom">
        {entry.participants.slice(0, SHOWN_PARTICIPANTS).map((person) => (
          <Avatar
            key={person.id}
            app={app}
            name={person.display_name || person.username}
            user={person}
            size="inbox"
          />
        ))}
        <span className="inbox-replies">
          {replies === 1
            ? t("threadOneReply")
            : t("threadReplies").replace("{n}", entry.replies)}
        </span>
        {entry.unread !== "0" && (
          <span
            className={
              "badge " +
              (entry.mentions !== "0" ? "badge-mention" : "badge-unread")
            }
          >
            {countLabel(entry.unread)}
          </span>
        )}
      </div>
    </ActionButton>
  );
}

export function ThreadsPanel({
  app,
  tab,
  filter,
}: {
  app: App;
  tab: InboxTab;
  filter: InboxFilter;
}) {
  const [rows, setRows] = useState<ThreadInboxEntry[]>([]);
  const [next, setNext] = useState<string | null>();
  const [status, setStatus] = useState<"loading" | "ready" | "failed">(
    "loading",
  );
  const [moreFailed, setMoreFailed] = useState(false);
  const [spent, setSpent] = useState(false);
  const budget = useRef(EXTRA_PAGES);
  const lastScroll = useRef(0);
  const list = useRef<HTMLDivElement>(null);
  const epoch = useRef(0);
  const loadingMore = useRef(false);
  const shown = useRef(0);
  shown.current = rows.length;
  const request = useCallback(
    (limit: number, before?: string | null) =>
      app.api.request<ThreadInbox>(
        "/api/v1/threads?filter=" +
          tab +
          "&limit=" +
          limit +
          (before ? "&before=" + encodeURIComponent(before) : ""),
      ),
    [app, tab],
  );
  const grantPages = useCallback(() => {
    budget.current = EXTRA_PAGES;
    setSpent(false);
  }, []);
  const first = useCallback(async () => {
    const current = ++epoch.current;
    loadingMore.current = true;
    grantPages();
    setStatus("loading");
    setRows([]);
    setNext(undefined);
    setMoreFailed(false);
    try {
      const page = await request(50);
      if (current !== epoch.current) return;
      loadingMore.current = false;
      setRows(page.threads);
      setNext(page.next);
      setStatus("ready");
    } catch {
      if (current !== epoch.current) return;
      loadingMore.current = false;
      setStatus("failed");
    }
  }, [grantPages, request]);
  const more = useCallback(async () => {
    if (loadingMore.current || !next || budget.current <= 0) return;
    budget.current--;
    const current = epoch.current;
    loadingMore.current = true;
    setMoreFailed(false);
    try {
      const page = await request(50, next);
      if (current !== epoch.current) return;
      setRows((rows) => appendPage(rows, page.threads));
      setNext(page.next);
      setSpent(budget.current <= 0);
    } catch {
      if (current === epoch.current) setMoreFailed(true);
    } finally {
      if (current === epoch.current) loadingMore.current = false;
    }
  }, [next, request]);
  const moreOnRequest = useCallback(() => {
    grantPages();
    return more();
  }, [grantPages, more]);
  useEffect(() => {
    void first();
    return () => {
      epoch.current++;
    };
  }, [first]);
  useEffect(() => {
    const refresh = () => {
      const current = epoch.current;
      void request(refreshLimit(shown.current))
        .then((page) => {
          if (current !== epoch.current) return;
          setRows((rows) => refreshHead(rows, page.threads, !page.next));
          setNext((known) => (page.next ? (known ?? page.next) : null));
          setStatus("ready");
        })
        .catch(() => {});
    };
    window.addEventListener("rv-thread-inbox", refresh);
    return () => window.removeEventListener("rv-thread-inbox", refresh);
  }, [request]);
  useEffect(() => {
    grantPages();
    lastScroll.current = 0;
    if (list.current) list.current.scrollTop = 0;
  }, [filter, grantPages]);
  useEffect(() => {
    const node = list.current;
    if (node && next && !moreFailed && nearEnd(node)) void more();
  }, [rows, filter, next, moreFailed, more]);
  const totals = app.threadTotals;
  const me = app.account?.session.user;
  const visible = me
    ? rows.filter((entry) =>
        matchesFilter(
          filter,
          threadTraits(entry, me, app.model.rooms.get(entry.room_id)?.kind),
        ),
      )
    : rows;
  return (
    <div className="threads-panel" data-tab={tab}>
      <header className="threads-header">
        <IconButton
          name="back"
          label={nt("social.back")}
          className="flat threads-back"
          action={() => app.showThreads(undefined)}
        />
        <span className="threads-title">{t("threads")}</span>
        <IconButton
          name="check"
          label={t("threadsMarkAll")}
          className="flat threads-mark-all"
          disabled={!totals || totals.unread_threads === "0"}
          action={() => app.markAllThreadsRead().catch(toast)}
        />
      </header>
      <div className="tabs threads-tabs" role="tablist">
        {(
          [
            ["followed", t("threadsFollowed")],
            ["unread", t("unread")],
          ] as const
        ).map(([value, label]) => (
          <ActionButton
            key={value}
            role="tab"
            aria-selected={tab === value}
            action={() => {
              if (tab !== value) app.showThreads(value);
            }}
          >
            {label}
          </ActionButton>
        ))}
      </div>
      <div
        className="threads-filter"
        role="group"
        aria-label={t("threadsFilter")}
      >
        {INBOX_FILTERS.map((value) => (
          <ActionButton
            key={value}
            className="threads-chip"
            aria-pressed={filter === value}
            action={() => {
              if (filter !== value) app.showThreads(tab, value);
            }}
          >
            {t(CHIP_LABELS[value])}
          </ActionButton>
        ))}
      </div>
      <div
        className="inbox-list"
        ref={list}
        onScroll={(event) => {
          const node = event.currentTarget;
          const down = node.scrollTop > lastScroll.current;
          lastScroll.current = node.scrollTop;
          if (down && nearEnd(node)) void moreOnRequest();
        }}
      >
        {visible.map((entry) => (
          <ThreadRow key={entry.root.id} app={app} entry={entry} />
        ))}
        {status === "failed" && (
          <div className="inbox-empty">
            <p>{t("threadsFailed")}</p>
            <ActionButton className="inbox-retry" action={first}>
              {t("retry")}
            </ActionButton>
          </div>
        )}
        {status === "ready" &&
          !visible.length &&
          !moreFailed &&
          (!next || spent) && (
            <div className="inbox-empty">
              <Symbol name="threads" />
              <p>
                {t(
                  filter !== "all"
                    ? CHIP_EMPTY[filter]
                    : tab === "unread"
                      ? "threadsNoUnread"
                      : "threadsNoFollowed",
                )}
              </p>
              {next ? (
                <p className="dim">{t("threadsMayMatch")}</p>
              ) : (
                filter === "all" &&
                tab === "unread" && (
                  <p className="dim">{t("threadsCaughtUp")}</p>
                )
              )}
            </div>
          )}
        {moreFailed && (
          <ActionButton className="inbox-retry" action={moreOnRequest}>
            {t("retry")}
          </ActionButton>
        )}
        {next && spent && !moreFailed && (
          <ActionButton className="inbox-more" action={moreOnRequest}>
            {t("loadMore")}
          </ActionButton>
        )}
      </div>
    </div>
  );
}
