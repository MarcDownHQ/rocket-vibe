//! A call carried natively by the voice sidecar (rv-call's signalling for
//! Jitsi Meet or a Slack huddle's Chime, libwebrtc's media), in a window built from the Vibez voice
//! page's parts: the room's name and the call's state, a tile per person with
//! the speaking ring, and the same control bar (microphone and its device menu,
//! deafen, camera and screen where the provider takes them, hang up): a
//! camera fills its person's tile, a shared screen takes the window's stage
//! above the people. A call the sidecar cannot join falls back to
//! the meeting's page; a call lost on the way shows "Reconnecting…" while the
//! sidecar joins again, and one that ends stays on screen with the reason.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use adw::prelude::*;
use gtk::glib;
use rv_core::session::Session;
use rv_core::teams_call::Target;
use rv_core::voice::{ConnectionState, Ended, LOCAL_PREVIEW, NativeCall, Snapshot, VideoSource, VoiceController};

use crate::chat::voice::{Occupant, avatar_card, avatar_frame, card, controls, following_view, name_tag, video_view};
use crate::i18n::{call_duration, t, tf};
use crate::on_tokio;
use crate::tile_grid::TileGrid;
use crate::widgets::{self, TileSize};

type Lookup = Rc<dyn Fn(&str) -> Option<String>>;

/// Names the people of a call whose provider sends only their ids (a Slack
/// huddle): `lookup` from what is known, `fetch` to learn the others.
#[derive(Clone)]
pub struct Names {
    lookup: Lookup,
    fetch: Rc<dyn Fn(Vec<String>)>,
    asked: Rc<RefCell<std::collections::HashSet<String>>>,
}

impl Names {
    pub fn none() -> Self {
        Self::new(|_| None, |_| {})
    }

    pub fn new(lookup: impl Fn(&str) -> Option<String> + 'static, fetch: impl Fn(Vec<String>) + 'static) -> Self {
        Names { lookup: Rc::new(lookup), fetch: Rc::new(fetch), asked: Rc::default() }
    }

    fn name(&self, identity: &str) -> Option<String> {
        let name = (self.lookup)(identity);
        if name.is_none() && self.asked.borrow_mut().insert(identity.to_owned()) {
            (self.fetch)(vec![identity.to_owned()]);
        }
        name
    }
}

/// What the window joins.
pub enum Call {
    /// The sidecar speaks the provider's signalling itself.
    Sidecar(NativeCall),
    /// This account speaks it and the sidecar carries the media (Teams).
    Account(std::sync::Arc<Session>, Target),
    /// The WhatsApp engine speaks it and carries the media; the sidecar its audio.
    WhatsApp(std::sync::Arc<rv_core::whatsapp_session::WaSession>, rv_core::whatsapp_session::WaTarget),
}

/// The window of a call opened, and the session it shows.
pub struct Opened {
    pub window: adw::Window,
    pub voice: VoiceController,
    header: adw::HeaderBar,
    side: gtk::Box,
}

impl Opened {
    /// The chat of the call's room beside the call, behind a header button.
    pub fn chat(&self, session: std::sync::Arc<Session>, rid: &str) {
        let toggle = gtk::ToggleButton::builder()
            .icon_name("chat-message-new-symbolic")
            .tooltip_text(t("voice_session.open_chat"))
            .build();
        self.header.pack_end(&toggle);
        let (side, window, rid) = (self.side.clone(), self.window.downgrade(), rid.to_owned());
        toggle.connect_toggled(move |toggle| {
            let on = toggle.is_active();
            if on && side.first_child().is_none() {
                side.append(&gtk::Separator::new(gtk::Orientation::Vertical));
                side.append(&crate::call_chat::pane(session.clone(), &rid));
            }
            side.set_visible(on);
            if let Some(window) = window.upgrade() {
                let width = window.width() + if on { 360 } else { -360 };
                window.set_default_size(width.max(480), window.height());
            }
        });
    }
}

/// Opens the call; `fallback` opens the meeting's page instead when the
/// sidecar is missing or could not join.
pub fn open(call: Call, title: &str, names: Names, fallback: impl Fn() + 'static) -> Option<Opened> {
    if !rv_core::voice::available() {
        fallback();
        return None;
    }
    let voice = VoiceController::new();
    let window = adw::Window::builder()
        .title(tf("call.native_title", &[("room", title)]))
        .default_width(720)
        .default_height(600)
        .build();
    let room = gtk::Label::builder()
        .label(title)
        .css_classes(["room-title"])
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    let heading = gtk::Box::builder().spacing(8).halign(gtk::Align::Center).build();
    heading.append(&gtk::Image::from_icon_name("call-start-symbolic"));
    heading.append(&room);
    let status = gtk::Label::builder().label(t("call.connecting")).css_classes(["voice-status"]).build();
    let cards = TileGrid::new();
    cards.set_vexpand(true);
    // The shared screen, when someone shares: the people go below it, smaller.
    let stage = gtk::Box::builder().vexpand(true).visible(false).css_classes(["voice-stage"]).build();
    let bar = controls(&["circular", "voice-control"]);
    for button in [&bar.camera, &bar.screen, &bar.reaction] {
        button.set_visible(false);
    }
    bar.row.set_halign(gtk::Align::Center);
    bar.row.set_spacing(8);
    bar.menu.set_popover(Some(&devices_menu(&voice)));
    let close = gtk::Button::builder()
        .label(t("call.close"))
        .css_classes(["pill"])
        .halign(gtk::Align::Center)
        .visible(false)
        .build();
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .margin_start(18)
        .margin_end(18)
        .margin_top(12)
        .margin_bottom(18)
        .css_classes(["voice-page"])
        .build();
    column.append(&heading);
    column.append(&status);
    column.append(&stage);
    column.append(&cards);
    column.append(&bar.row);
    column.append(&close);
    column.set_hexpand(true);
    let side = gtk::Box::builder().visible(false).build();
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.append(&column);
    body.append(&side);
    let header = adw::HeaderBar::new();
    let page = adw::ToolbarView::new();
    page.add_top_bar(&header);
    page.set_content(Some(&body));
    window.set_content(Some(&page));

    let state = Rc::new(State {
        connected_at: Cell::new(None),
        ever_connected: Cell::new(false),
        closed: Cell::new(false),
        tiles: RefCell::new(Vec::new()),
        shown: RefCell::new(None),
        sharer: Rc::new(RefCell::new(String::new())),
    });
    let fallback: Rc<dyn Fn()> = Rc::new(fallback);
    let bar = Rc::new(bar);

    let v = voice.clone();
    bar.mic.connect_clicked(move |_| {
        let (v, on) = (v.clone(), !v.snapshot().microphone);
        glib::spawn_future_local(async move { on_tokio(async move { v.set_microphone(on).await }).await });
    });
    let v = voice.clone();
    bar.deafen.connect_clicked(move |_| {
        let (v, on) = (v.clone(), !v.snapshot().deafened);
        glib::spawn_future_local(async move { on_tokio(async move { v.set_deafened(on).await }).await });
    });
    let v = voice.clone();
    bar.camera.connect_clicked(move |_| {
        let (v, on) = (v.clone(), !v.snapshot().camera);
        glib::spawn_future_local(async move { on_tokio(async move { v.set_camera(on).await }).await });
    });
    // The system's picker chooses where it has one (Wayland's portal), else the first screen.
    let v = voice.clone();
    bar.screen.connect_clicked(move |_| {
        let (v, sharing) = (v.clone(), v.snapshot().sharing);
        glib::spawn_future_local(async move {
            on_tokio(async move {
                if sharing { v.stop_screen_share().await } else { v.start_screen_share(None, None).await }
            })
            .await
        });
    });
    for button in [&bar.leave, &close] {
        let w = window.downgrade();
        button.connect_clicked(move |_| {
            if let Some(w) = w.upgrade() {
                w.close();
            }
        });
    }
    let (v, s) = (voice.clone(), state.clone());
    window.connect_close_request(move |_| {
        s.closed.set(true);
        let v = v.clone();
        glib::spawn_future_local(async move { on_tokio(async move { v.disconnect().await }).await });
        glib::Propagation::Proceed
    });

    // The snapshot drives the window; the call's length ticks every second.
    let mut changes = voice.changes();
    let (v, w, s, fb) = (voice.clone(), window.downgrade(), state.clone(), fallback.clone());
    let (cards_ref, status_ref, bar_ref, close_ref) = (cards.clone(), status.clone(), bar.clone(), close.clone());
    let (stage_ref, voice_ref) = (stage.clone(), voice.clone());
    glib::spawn_future_local(async move {
        loop {
            let snapshot = v.snapshot();
            let Some(window) = w.upgrade() else { return };
            if let Some(ended) = &snapshot.ended {
                if !s.closed.get() {
                    if !s.ever_connected.get() && ended.is_join_failure() {
                        // Never joined: the meeting's page instead.
                        fb();
                        s.closed.set(true);
                        window.close();
                    } else {
                        // Ended on the way: the reason stays on screen until closed.
                        status_ref.set_label(&tf("call.ended", &[("reason", &end_reason(ended))]));
                        status_ref.remove_css_class("connected");
                        cards_ref.remove_all();
                        s.tiles.borrow_mut().clear();
                        stage_ref.set_visible(false);
                        bar_ref.row.set_visible(false);
                        close_ref.set_visible(true);
                    }
                }
                return;
            }
            if snapshot.state == ConnectionState::Connected && !s.ever_connected.get() {
                s.ever_connected.set(true);
                s.connected_at.set(Some(Instant::now()));
            }
            bar_ref.sync(&snapshot);
            let (camera, screen) = snapshot.call_video.unwrap_or((false, false));
            bar_ref.camera.set_visible(camera);
            bar_ref.screen.set_visible(screen);
            let people = occupants(&snapshot, &names);
            if s.shown.borrow().as_ref() != Some(&people) {
                render(&voice_ref, &cards_ref, &stage_ref, &s, &people);
                s.shown.replace(Some(people));
            }
            light(&s, &snapshot);
            status_ref.set_label(&status_text(&s, &snapshot));
            if snapshot.state == ConnectionState::Connected {
                status_ref.add_css_class("connected");
            } else {
                status_ref.remove_css_class("connected");
            }
            drop(window);
            let _ = glib::future_with_timeout(std::time::Duration::from_secs(1), changes.recv()).await;
        }
    });

    let v = voice.clone();
    let fb = fallback.clone();
    let label = title.to_owned();
    glib::spawn_future_local(async move {
        let joined = on_tokio(async move {
            match call {
                Call::Sidecar(call) => v.connect_native(&label, &call).await.map_err(|e| e.to_string()),
                Call::Account(session, target) => {
                    session.place_teams_call(&v, target, &label).await.map_err(|e| e.message)
                }
                Call::WhatsApp(session, target) => session.carry_call(&v, target, &label).await,
            }
        })
        .await;
        if let Err(e) = joined {
            eprintln!("Native call not joined ({e}), opening the meeting's page");
            fb();
        }
    });
    window.present();
    Some(Opened { window, voice, header, side })
}

struct State {
    connected_at: Cell<Option<Instant>>,
    ever_connected: Cell<bool>,
    closed: Cell<bool>,
    /// Each person's tile, lit while they speak.
    tiles: RefCell<Vec<(String, gtk::Overlay)>>,
    shown: RefCell<Option<Vec<Occupant>>>,
    /// Whose screen the stage shows.
    sharer: Rc<RefCell<String>>,
}

fn status_text(state: &State, snapshot: &Snapshot) -> String {
    match snapshot.state {
        ConnectionState::Reconnecting => return t("voice_session.reconnecting").to_owned(),
        ConnectionState::Connected => {}
        _ => return t("call.connecting").to_owned(),
    }
    let Some(at) = state.connected_at.get() else { return t("call.connecting").to_owned() };
    let alone = snapshot.participants.iter().all(|p| p.local);
    if alone { t("call.waiting").to_owned() } else { call_duration(at.elapsed().as_secs()) }
}

/// Why the call ended, in words.
fn end_reason(ended: &Ended) -> String {
    match ended.explained() {
        (key, Some(reason)) => tf(key, &[("reason", reason)]),
        (key, None) => t(key).to_owned(),
    }
}

fn occupants(snapshot: &Snapshot, names: &Names) -> Vec<Occupant> {
    snapshot
        .participants
        .iter()
        .map(|p| Occupant {
            uid: p.identity.clone(),
            name: if p.local {
                t("call.you").to_owned()
            } else {
                p.name.clone().or_else(|| names.name(&p.identity)).unwrap_or_else(|| p.identity.clone())
            },
            muted: p.muted,
            deafened: p.deafened,
            local: p.local,
            camera: p.camera,
            screen: p.screen,
            muted_here: false,
        })
        .collect()
}

/// The frames of someone's camera or screen: this side's previews go under
/// their own identity whatever the provider calls this side.
fn video_identity(o: &Occupant) -> String {
    if o.local { LOCAL_PREVIEW.to_owned() } else { o.uid.clone() }
}

fn render(voice: &VoiceController, cards: &TileGrid, stage: &gtk::Box, state: &State, people: &[Occupant]) {
    cards.remove_all();
    let mut tiles = state.tiles.borrow_mut();
    tiles.clear();
    // Someone else's screen before this side's own, which it sees anyway.
    let sharer = people.iter().find(|o| o.screen && !o.local).or_else(|| people.iter().find(|o| o.screen));
    match sharer {
        Some(sharer) => {
            let identity = video_identity(sharer);
            if stage.first_child().is_none() || *state.sharer.borrow() != identity {
                state.sharer.replace(identity);
                while let Some(child) = stage.first_child() {
                    stage.remove(&child);
                }
                let picture =
                    following_view(voice, state.sharer.clone(), VideoSource::Screen, gtk::ContentFit::Contain);
                picture.set_hexpand(true);
                stage.append(&picture);
            }
            stage.set_visible(true);
            cards.set_vexpand(false);
            cards.set_height_request(140);
        }
        None => {
            stage.set_visible(false);
            while let Some(child) = stage.first_child() {
                stage.remove(&child);
            }
            cards.set_vexpand(true);
            cards.set_height_request(-1);
        }
    }
    for o in people {
        let tile = if o.camera {
            let tile = card();
            tile.add_overlay(&video_view(voice, &video_identity(o), VideoSource::Camera, gtk::ContentFit::Cover));
            tile
        } else {
            let avatar = widgets::tile(&o.uid, &widgets::initial(&o.name), TileSize::Profile, false);
            avatar_card(&avatar_frame(&avatar, "large"))
        };
        tile.add_overlay(&name_tag(&Occupant { local: false, ..o.clone() }));
        cards.append(&tile);
        tiles.push((o.uid.clone(), tile));
    }
}

/// Who speaks: classes toggled, nothing rebuilt.
fn light(state: &State, snapshot: &Snapshot) {
    for (uid, tile) in state.tiles.borrow().iter() {
        let speaking = snapshot.participants.iter().any(|p| &p.identity == uid && p.speaking);
        if speaking { tile.add_css_class("speaking") } else { tile.remove_css_class("speaking") }
    }
}

/// The microphone button's menu: the input and output devices.
fn devices_menu(voice: &VoiceController) -> gtk::Popover {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .width_request(260)
        .css_classes(["voice-menu"])
        .build();
    let inputs = gtk::DropDown::from_strings(&[t("voice_settings.loading")]);
    let outputs = gtk::DropDown::from_strings(&[t("voice_settings.loading")]);
    for (key, dropdown) in [("call.microphone", &inputs), ("call.speaker", &outputs)] {
        column.append(&gtk::Label::builder().label(t(key)).xalign(0.0).css_classes(["heading"]).build());
        dropdown.set_sensitive(false);
        column.append(dropdown);
    }
    let popover = gtk::Popover::builder().child(&column).build();
    let (v, weak_in, weak_out) = (voice.clone(), inputs.downgrade(), outputs.downgrade());
    let loaded = Rc::new(Cell::new(false));
    popover.connect_show(move |_| {
        if loaded.replace(true) {
            return;
        }
        let (v, weak_in, weak_out) = (v.clone(), weak_in.clone(), weak_out.clone());
        glib::spawn_future_local(async move {
            let probe = v.clone();
            let Ok((ins, outs)) = on_tokio(async move { probe.devices().await }).await else { return };
            let (chosen_in, chosen_out) = v.selected_devices();
            for (weak, list, chosen, input) in [(&weak_in, ins, chosen_in, true), (&weak_out, outs, chosen_out, false)]
            {
                let Some(dropdown) = weak.upgrade() else { continue };
                let names: Vec<&str> = list.iter().map(|d| d.name.as_str()).collect();
                dropdown.set_model(Some(&gtk::StringList::new(&names)));
                let at = list
                    .iter()
                    .position(|d| chosen.as_deref() == Some(d.id.as_str()))
                    .or_else(|| list.iter().position(|d| d.default));
                if let Some(at) = at {
                    dropdown.set_selected(at as u32);
                }
                dropdown.set_sensitive(!list.is_empty());
                let v = v.clone();
                dropdown.connect_selected_notify(move |d| {
                    let Some(device) = list.get(d.selected() as usize) else { return };
                    let (v, id) = (v.clone(), device.id.clone());
                    glib::spawn_future_local(async move {
                        on_tokio(
                            async move { if input { v.select_input(&id).await } else { v.select_output(&id).await } },
                        )
                        .await
                    });
                });
            }
        });
    });
    popover
}
