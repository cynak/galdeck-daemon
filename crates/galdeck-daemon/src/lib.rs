//! The galdeck daemon.
//!
//! Exposed as a library as well as a binary so the engine loop can be driven
//! from integration tests against a virtual deck, with no hardware attached.

pub mod actions;
pub mod assets;
pub mod audio;
pub mod backdrop;
pub mod base64;
pub mod clock;
pub mod controls;
pub mod countdown;
pub mod engine;
pub mod http;
pub mod icons;
pub mod input;
pub mod io;
pub mod ipc_server;
pub mod lighting;
pub mod logos;
pub mod pipewire;
pub mod plugins;
pub mod preview;
pub mod render;
pub mod ring;
pub mod states;
pub mod theme_editor;
pub mod uinput;
pub mod widgets;
