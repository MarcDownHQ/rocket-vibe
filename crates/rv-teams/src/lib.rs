//! Microsoft Teams: an account signed in with a device code (`auth`), its
//! service calls (`client`), translation and the Trouter live connection.
pub mod auth;
pub mod calling;
pub mod client;
pub mod datachannel;
pub mod frames;
pub mod live;
pub mod notes;
pub mod sdp;
pub mod service;
pub mod translate;

pub use auth::{SESSION_REJECTED, Session, TeamsAccount};
