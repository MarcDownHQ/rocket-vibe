//! The account's threads, in place of the room list: those I follow, those
//! with replies I have not read, "mark all as read". A thread picked opens in
//! the content pane, the list staying beside it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use rv_core::provider::Chat;
use rv_core::thread_inbox::{self, Entry, Filter, Participant, Scope};

use crate::i18n::{t, tn};
use crate::on_tokio;
use crate::rows::{label, person_photo, short_time};
use crate::widgets::{self, TileSize};

const AVATARS: usize = 3;
const REFRESH_DELAY: Duration = Duration::from_millis(400);
const POLL_SECONDS: u32 = 30;

type Handler<T> = RefCell<Option<Rc<dyn Fn(T)>>>;

pub struct ThreadInbox {
    pub root: gtk::Box,
    pub entry: gtk::Button,
    entry_badge: gtk::Label,
    unread: gtk::ToggleButton,
    mark_all: gtk::Button,
    stack: gtk::Stack,
    list: gtk::ListBox,
    more: gtk::Button,
    empty: adw::StatusPage,
    source: RefCell<Option<Chat>>,
    filter: Cell<Filter>,
    scope: Cell<Scope>,
    chips: [(gtk::ToggleButton, Scope); 4],
    entries: RefCell<Vec<Entry>>,
    next: RefCell<Option<String>>,
    selected: RefCell<Option<String>>,
    sync_cursor: RefCell<Option<String>>,
    read_further: Cell<bool>,
    extra_pages: Cell<u32>,
    account: Cell<u64>,
    polling: Cell<u64>,
    generation: Cell<u64>,
    refresh_generation: Cell<u64>,
    shown: Cell<bool>,
    on_open: Handler<Entry>,
    on_toggle: Handler<bool>,
    on_failed: Handler<String>,
}

impl ThreadInbox {
    pub fn new() -> Rc<Self> {
        let entry_badge = label("", &["badge", "badge-unread"]);
        entry_badge.set_visible(false);
        let entry_content = gtk::Box::builder().spacing(10).build();
        entry_content.append(&gtk::Image::from_icon_name("chat-message-new-symbolic"));
        let entry_title = label(t("threads.title"), &[]);
        entry_title.set_hexpand(true);
        entry_title.set_xalign(0.0);
        entry_content.append(&entry_title);
        entry_content.append(&entry_badge);
        let entry = gtk::Button::builder()
            .child(&entry_content)
            .css_classes(["flat", "thread-inbox-entry"])
            .visible(false)
            .build();

        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .css_classes(["flat"])
            .tooltip_text(t("thread_inbox.back"))
            .build();
        let title = label(t("threads.title"), &["heading"]);
        title.set_hexpand(true);
        title.set_xalign(0.0);
        let mark_all = gtk::Button::builder()
            .icon_name("object-select-symbolic")
            .css_classes(["flat"])
            .tooltip_text(t("thread_inbox.mark_all"))
            .build();
        let top = gtk::Box::builder().spacing(6).margin_start(8).margin_end(8).build();
        top.append(&back);
        top.append(&title);
        top.append(&mark_all);

        let followed =
            gtk::ToggleButton::builder().label(t("thread_inbox.followed")).active(true).hexpand(true).build();
        let unread =
            gtk::ToggleButton::builder().label(t("thread_inbox.unread")).group(&followed).hexpand(true).build();
        let tabs = gtk::Box::builder()
            .css_classes(["linked"])
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        tabs.append(&followed);
        tabs.append(&unread);

        let scopes = gtk::Box::builder()
            .spacing(4)
            .homogeneous(false)
            .css_classes(["thread-inbox-scopes"])
            .margin_start(12)
            .margin_end(12)
            .margin_bottom(6)
            .build();
        let chip = |key: &str, active: bool| {
            let chip = gtk::ToggleButton::builder()
                .label(t(key))
                .active(active)
                .hexpand(true)
                .css_classes(["flat", "caption"])
                .build();
            scopes.append(&chip);
            chip
        };
        let chip_all = chip("thread_inbox.scope_all", true);
        let chips = [
            (chip_all.clone(), Scope::All),
            (chip("thread_inbox.scope_participated", false), Scope::Participated),
            (chip("thread_inbox.scope_mentioned", false), Scope::Mentioned),
            (chip("thread_inbox.scope_others", false), Scope::Others),
        ];
        for (other, _) in &chips[1..] {
            other.set_group(Some(&chip_all));
        }

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["rooms", "thread-inbox"])
            .valign(gtk::Align::Start)
            .build();
        let more = gtk::Button::builder()
            .label(t("threads.more"))
            .css_classes(["pill"])
            .halign(gtk::Align::Center)
            .margin_top(10)
            .margin_bottom(10)
            .visible(false)
            .build();
        let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        column.append(&list);
        column.append(&more);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&column)
            .build();
        let empty = adw::StatusPage::builder().vexpand(true).css_classes(["compact"]).build();
        let retry = gtk::Button::builder()
            .label(t("native.retry"))
            .css_classes(["pill", "suggested-action"])
            .halign(gtk::Align::Center)
            .build();
        let failed = adw::StatusPage::builder()
            .icon_name("dialog-error-symbolic")
            .title(t("info.failed"))
            .css_classes(["compact"])
            .child(&retry)
            .build();
        let stack = gtk::Stack::builder().vexpand(true).build();
        stack.add_named(&adw::Spinner::builder().width_request(32).height_request(32).build(), Some("loading"));
        stack.add_named(&scroll, Some("list"));
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&failed, Some("failed"));

        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).margin_top(6).build();
        root.append(&top);
        root.append(&tabs);
        root.append(&scopes);
        root.append(&stack);

        let this = Rc::new(ThreadInbox {
            root,
            entry,
            entry_badge,
            unread,
            mark_all,
            stack,
            list,
            more,
            empty,
            source: RefCell::default(),
            filter: Cell::new(Filter::Followed),
            scope: Cell::new(Scope::All),
            chips: chips.clone(),
            entries: RefCell::default(),
            next: RefCell::default(),
            selected: RefCell::default(),
            sync_cursor: RefCell::default(),
            read_further: Cell::new(false),
            extra_pages: Cell::new(0),
            account: Cell::new(0),
            polling: Cell::new(0),
            generation: Cell::new(0),
            refresh_generation: Cell::new(0),
            shown: Cell::new(false),
            on_open: RefCell::default(),
            on_toggle: RefCell::default(),
            on_failed: RefCell::default(),
        });

        let weak = Rc::downgrade(&this);
        this.entry.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.toggle(true);
            }
        });
        let weak = Rc::downgrade(&this);
        back.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.toggle(false);
            }
        });
        let weak = Rc::downgrade(&this);
        this.unread.connect_toggled(move |unread| {
            if let Some(this) = weak.upgrade() {
                this.filter.set(if unread.is_active() { Filter::Unread } else { Filter::Followed });
                this.entries.borrow_mut().clear();
                this.next.replace(None);
                this.read_further.set(false);
                this.reload();
            }
        });
        for (chip, scope) in chips {
            let weak = Rc::downgrade(&this);
            chip.connect_toggled(move |chip| {
                if chip.is_active()
                    && let Some(this) = weak.upgrade()
                {
                    this.set_scope(scope);
                }
            });
        }
        let weak = Rc::downgrade(&this);
        retry.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.reload();
            }
        });
        let weak = Rc::downgrade(&this);
        this.more.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.load_more();
            }
        });
        let weak = Rc::downgrade(&this);
        this.mark_all.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.mark_all_read();
            }
        });
        let weak = Rc::downgrade(&this);
        this.list.connect_row_activated(move |_, row| {
            if let Some(this) = weak.upgrade() {
                this.pick(row.index());
            }
        });
        this
    }

    pub fn connect_open(&self, f: impl Fn(Entry) + 'static) {
        self.on_open.replace(Some(Rc::new(f)));
    }

    /// The panel opened (true) or went back to the room list (false).
    pub fn connect_toggle(&self, f: impl Fn(bool) + 'static) {
        self.on_toggle.replace(Some(Rc::new(f)));
    }

    pub fn connect_failed(&self, f: impl Fn(String) + 'static) {
        self.on_failed.replace(Some(Rc::new(f)));
    }

    /// The account on screen, whichever kind of session speaks for it.
    pub fn set_source(self: &Rc<Self>, source: Option<Chat>) {
        self.entry.set_visible(source.as_ref().is_some_and(Chat::thread_inbox_available));
        self.source.replace(source);
        self.sync_cursor.replace(None);
        self.account.set(self.account.get().wrapping_add(1));
        self.generation.set(self.generation.get().wrapping_add(1));
        self.entries.borrow_mut().clear();
        self.next.replace(None);
        self.selected.replace(None);
        self.read_further.set(false);
        self.chips[0].0.set_active(true);
        self.entry_badge.set_visible(false);
        self.toggle(false);
        self.refresh_totals();
    }

    fn toggle(self: &Rc<Self>, shown: bool) {
        if self.shown.replace(shown) == shown {
            return;
        }
        if let Some(f) = self.on_toggle.borrow().clone() {
            f(shown);
        }
        self.polling.set(self.polling.get().wrapping_add(1));
        if shown {
            self.reload();
            self.poll();
        }
    }

    /// Where nothing tells that the list changed, it is read again while on screen.
    fn poll(self: &Rc<Self>) {
        if !self.source.borrow().as_ref().is_some_and(Chat::thread_inbox_polled) {
            return;
        }
        let (weak, polling) = (Rc::downgrade(self), self.polling.get());
        glib::timeout_add_seconds_local(POLL_SECONDS, move || {
            let Some(this) = weak.upgrade() else { return glib::ControlFlow::Break };
            if this.polling.get() != polling {
                return glib::ControlFlow::Break;
            }
            this.schedule_refresh();
            glib::ControlFlow::Continue
        });
    }

    /// Back in the app: where no event told, what changed meanwhile is read.
    pub fn on_window_active(self: &Rc<Self>) {
        if self.source.borrow().as_ref().is_some_and(Chat::thread_inbox_polled) {
            self.schedule_refresh();
        }
    }

    fn set_scope(self: &Rc<Self>, scope: Scope) {
        if self.scope.replace(scope) == scope {
            return;
        }
        self.extra_pages.set(0);
        self.show_page();
    }

    fn visible_entries(&self) -> Vec<Entry> {
        let scope = self.scope.get();
        self.entries.borrow().iter().filter(|e| scope.matches(e)).cloned().collect()
    }

    fn reads_on(&self, shown: usize) -> bool {
        thread_inbox::keep_fetching(shown, self.next.borrow().is_some(), self.extra_pages.get())
    }

    /// Shows what the chip lets through, and reads on, within bounds, when that
    /// does not fill the list. Past the bound the load-more button stays, so
    /// nothing silently goes missing.
    fn show_page(self: &Rc<Self>) {
        self.render();
        let shown = self.visible_entries().len();
        if self.reads_on(shown) {
            self.extra_pages.set(self.extra_pages.get() + 1);
            self.more.set_sensitive(false);
            self.load(self.next.borrow().clone(), false);
        }
    }

    fn available_source(&self) -> Option<Chat> {
        self.source.borrow().clone().filter(Chat::thread_inbox_available)
    }

    /// A Vibez account learns what its server offers after it is set, and no
    /// event names the threads: they are read again when its sync cursor moves.
    pub fn on_sync(self: &Rc<Self>, cursor: Option<String>) {
        let available = self.available_source().is_some();
        let appeared = available && !self.entry.is_visible();
        self.entry.set_visible(available);
        if self.sync_cursor.replace(cursor.clone()) != cursor || appeared {
            self.schedule_refresh();
        }
    }

    /// Something changed on the server: a burst of events reads once.
    pub fn schedule_refresh(self: &Rc<Self>) {
        let generation = self.refresh_generation.get().wrapping_add(1);
        self.refresh_generation.set(generation);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(REFRESH_DELAY, move || {
            let Some(this) = weak.upgrade() else { return };
            if this.refresh_generation.get() != generation {
                return;
            }
            this.refresh_totals();
            if this.shown.get() && !this.read_further.get() {
                this.reload_quietly();
            }
        });
    }

    fn refresh_totals(self: &Rc<Self>) {
        let Some(source) = self.available_source() else { return };
        let (weak, expected) = (Rc::downgrade(self), self.account.get());
        glib::spawn_future_local(async move {
            let totals = on_tokio(async move { source.thread_totals().await }).await;
            let Some(this) = weak.upgrade() else { return };
            if this.account.get() != expected {
                return;
            }
            if let Ok(totals) = totals {
                let count = totals.unread_threads;
                this.entry_badge.set_label(&if count > 99 { "99+".to_owned() } else { count.to_string() });
                this.entry_badge.set_visible(count > 0);
                if totals.unread_mentions > 0 {
                    this.entry_badge.remove_css_class("badge-unread");
                    this.entry_badge.add_css_class("badge-mention");
                } else {
                    this.entry_badge.remove_css_class("badge-mention");
                    this.entry_badge.add_css_class("badge-unread");
                }
                this.mark_all.set_sensitive(count > 0);
            }
        });
    }

    fn reload(self: &Rc<Self>) {
        self.stack.set_visible_child_name("loading");
        self.reload_quietly();
    }

    fn reload_quietly(self: &Rc<Self>) {
        self.extra_pages.set(0);
        self.load(None, false);
    }

    fn load_more(self: &Rc<Self>) {
        let next = self.next.borrow().clone();
        if next.is_some() {
            self.extra_pages.set(0);
            self.more.set_sensitive(false);
            self.load(next, true);
        }
    }

    /// `asked`: the reader pressed load more. A list read further that way
    /// stays as it is on a refresh: reading it again would throw the reader
    /// back to the top.
    fn load(self: &Rc<Self>, cursor: Option<String>, asked: bool) {
        let Some(source) = self.available_source() else { return };
        let generation = self.generation.get().wrapping_add(1);
        self.generation.set(generation);
        let filter = self.filter.get();
        let weak = Rc::downgrade(self);
        let appending = cursor.is_some();
        glib::spawn_future_local(async move {
            let page = on_tokio(async move { source.thread_inbox(filter, cursor.as_deref()).await }).await;
            let Some(this) = weak.upgrade() else { return };
            if this.generation.get() != generation {
                return;
            }
            this.more.set_sensitive(true);
            let page = match page {
                Ok(page) => page,
                Err(_) if appending || !this.entries.borrow().is_empty() => {
                    if let Some(f) = this.on_failed.borrow().clone() {
                        f(t("info.failed").to_owned());
                    }
                    this.stack.set_visible_child_name("list");
                    return;
                }
                Err(_) => {
                    this.stack.set_visible_child_name("failed");
                    return;
                }
            };
            if !appending {
                this.entries.borrow_mut().clear();
                this.read_further.set(false);
            }
            if asked {
                this.read_further.set(true);
            }
            thread_inbox::append(&mut this.entries.borrow_mut(), page.entries);
            this.next.replace(page.next);
            this.show_page();
        });
    }

    fn render(self: &Rc<Self>) {
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        let source = self.source.borrow().clone();
        let selected = self.selected.borrow().clone();
        let visible = self.visible_entries();
        for entry in &visible {
            let row = entry_row(entry, source.as_ref());
            self.list.append(&row);
            if selected.as_deref() == Some(entry.root.id.as_str()) {
                self.list.select_row(Some(&row));
            }
        }
        self.more.set_visible(self.next.borrow().is_some());
        if !visible.is_empty() || (self.next.borrow().is_some() && !self.reads_on(0)) {
            self.stack.set_visible_child_name("list");
        } else if self.next.borrow().is_some() {
            self.stack.set_visible_child_name("loading");
        } else {
            let unread = self.scope.get() == Scope::All && self.filter.get() == Filter::Unread;
            self.empty.set_icon_name(Some(if unread { "object-select-symbolic" } else { "chat-message-new-symbolic" }));
            self.empty.set_title(t(empty_title(self.filter.get(), self.scope.get())));
            self.empty.set_description(unread.then(|| t("thread_inbox.caught_up")));
            self.stack.set_visible_child_name("empty");
        }
    }

    fn pick(self: &Rc<Self>, index: i32) {
        let Some(entry) = usize::try_from(index).ok().and_then(|i| self.visible_entries().get(i).cloned()) else {
            return;
        };
        self.selected.replace(Some(entry.root.id.clone()));
        if let Some(f) = self.on_open.borrow().clone() {
            f(entry);
        }
    }

    fn mark_all_read(self: &Rc<Self>) {
        let Some(source) = self.available_source() else { return };
        self.mark_all.set_sensitive(false);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let done = on_tokio(async move { source.mark_all_threads_read().await }).await;
            let Some(this) = weak.upgrade() else { return };
            this.mark_all.set_sensitive(true);
            if done.is_err()
                && let Some(f) = this.on_failed.borrow().clone()
            {
                f(t("info.failed").to_owned());
            }
            this.refresh_totals();
            if this.shown.get() {
                this.reload_quietly();
            }
        });
    }

    /// For the smoke checks.
    pub(crate) fn open_first_for_smoke(self: &Rc<Self>) -> usize {
        self.toggle(true);
        self.entries.borrow().len()
    }

    /// For the smoke checks.
    pub(crate) fn pick_first_for_smoke(self: &Rc<Self>) {
        self.pick(0);
    }

    /// For the smoke checks: rows on screen through this chip, and the empty
    /// page's title when it is the one shown.
    pub(crate) fn scope_for_smoke(self: &Rc<Self>, scope: Scope) -> (usize, Option<String>) {
        if let Some((chip, _)) = self.chips.iter().find(|(_, s)| *s == scope) {
            chip.set_active(true);
        }
        let rows = std::iter::successors(self.list.row_at_index(0), |row| row.next_sibling()?.downcast().ok()).count();
        let empty =
            (self.stack.visible_child_name().as_deref() == Some("empty")).then(|| self.empty.title().to_string());
        (rows, empty)
    }

    /// For the smoke checks: the unread tab, or the followed one.
    pub(crate) fn unread_for_smoke(&self, on: bool) {
        self.unread.set_active(on);
    }
}

fn empty_title(filter: Filter, scope: Scope) -> &'static str {
    match (scope, filter) {
        (Scope::All, Filter::Followed) => "thread_inbox.none_followed",
        (Scope::All, Filter::Unread) => "thread_inbox.none_unread",
        (Scope::Participated, _) => "thread_inbox.none_participated",
        (Scope::Mentioned, _) => "thread_inbox.none_mentioned",
        (Scope::Others, _) => "thread_inbox.none_others",
    }
}

fn participant_tile(person: &Participant, source: Option<&Chat>) -> gtk::Widget {
    let name = if person.label.is_empty() { &person.username } else { &person.label };
    let tile = widgets::tile(&person.username, &widgets::initial(name), TileSize::Mini, false);
    match source {
        Some(source) => person_photo(tile, source, &person.username, person.photo.clone()),
        None => tile,
    }
}

fn entry_row(entry: &Entry, source: Option<&Chat>) -> gtk::ListBoxRow {
    let author = source
        .and_then(Chat::legacy)
        .and_then(|s| s.person_label(&entry.root.author_id))
        .or_else(|| entry.root.author.clone())
        .unwrap_or_default();
    let name = label(&author, &["thread-inbox-author"]);
    name.set_ellipsize(gtk::pango::EllipsizeMode::End);
    let head = gtk::Box::builder().spacing(6).build();
    head.append(&name);
    if !entry.room.is_empty() {
        let room = label(&entry.room, &["thread-inbox-room"]);
        room.set_ellipsize(gtk::pango::EllipsizeMode::End);
        head.append(&room);
    }
    let time = label(&short_time(entry.last_reply), &["dim-label", "caption"]);
    time.set_hexpand(true);
    time.set_xalign(1.0);
    head.append(&time);

    let preview = label(&crate::threads::preview(&entry.root), &[]);
    preview.set_xalign(0.0);
    preview.set_wrap(true);
    preview.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    preview.set_lines(2);
    preview.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let foot = gtk::Box::builder().spacing(4).build();
    for person in entry.participants.iter().take(AVATARS) {
        foot.append(&participant_tile(person, source));
    }
    let replies = label(&tn("message.replies", entry.replies), &["dim-label", "caption"]);
    replies.set_margin_start(4);
    foot.append(&replies);
    if entry.unread_replies > 0 {
        let unread = label(
            &entry.unread_replies.to_string(),
            &["badge", if entry.unread_mentions > 0 { "badge-mention" } else { "badge-unread" }],
        );
        unread.set_hexpand(true);
        unread.set_halign(gtk::Align::End);
        unread.set_valign(gtk::Align::Center);
        foot.append(&unread);
    }

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_start(10)
        .margin_end(10)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    column.append(&head);
    column.append(&preview);
    column.append(&foot);
    let row = gtk::ListBoxRow::builder().child(&column).build();
    if entry.unread_replies > 0 {
        row.add_css_class("thread-inbox-unread");
    }
    row
}
