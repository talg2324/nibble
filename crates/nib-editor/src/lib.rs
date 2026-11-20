//! UI-agnostic editor model. Nothing in here knows about terminals, so a GUI client can reuse it.

pub mod command;
pub mod config;
pub mod document;
pub mod keymap;

pub use command::Command;
pub use config::{Config, Profile, SshHost, Target};
pub use document::Document;
pub use keymap::{KeyChord, Keymap};
