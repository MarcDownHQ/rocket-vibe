//! The chat of a call's room beside the call: its latest messages, kept up to
//! date from the store, and a composer that writes in the room.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::glib;
use rv_core::session::Session;
use tokio::sync::broadcast::error::RecvError;

use crate::composer::Composer;
use crate::message_list::MessageList;

/// How many of the room's latest messages the pane shows.
const SHOWN: i64 = 100;

pub fn pane(session: Arc<Session>, rid: &str) -> gtk::Box {
    let list = MessageList::new(Rc::new(RefCell::new(Some(session.clone()))));
    list.root.set_vexpand(true);
    list.set_rows(session.store.messages(rid, SHOWN));
    let composer = Composer::new();
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .width_request(340)
        .css_classes(["call-chat"])
        .build();
    root.append(&list.root);
    root.append(&composer.root);

    let (s, r, weak) = (session.clone(), rid.to_owned(), Rc::downgrade(&composer));
    composer.connect_submit(move |text| {
        if let Some(composer) = weak.upgrade() {
            composer.set_text("");
        }
        let (s, r) = (s.clone(), r.clone());
        glib::spawn_future_local(async move { crate::on_tokio(async move { s.send(&r, &text).await }).await });
    });

    // The store's changes to this room redraw the list, a burst of them once.
    let (tx, rx) = async_channel::bounded::<()>(1);
    let (mut changes, watched) = (session.store.changes(), rid.to_owned());
    let forward = crate::runtime().spawn(async move {
        loop {
            match changes.recv().await {
                Ok(change) if !change.rids.contains(&watched) => continue,
                Err(RecvError::Closed) => return,
                _ => {}
            }
            if let Err(async_channel::TrySendError::Closed(_)) = tx.try_send(()) {
                return;
            }
        }
    });
    let abort = forward.abort_handle();
    root.connect_destroy(move |_| abort.abort());
    let (weak, rid) = (Rc::downgrade(&list), rid.to_owned());
    glib::spawn_future_local(async move {
        while rx.recv().await.is_ok() {
            let Some(list) = weak.upgrade() else { return };
            list.set_rows(session.store.messages(&rid, SHOWN));
        }
    });
    // The list and the composer live as long as the pane shows them.
    let keep = RefCell::new(Some((list, composer)));
    root.connect_destroy(move |_| drop(keep.take()));
    root
}
