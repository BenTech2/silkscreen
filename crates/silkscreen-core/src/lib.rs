//! Silkscreen's shared core, used by the desktop app and the Android app:
//! device identity and pairing ([`secure`]), LAN discovery ([`discovery`]),
//! settings ([`ctx`]), live status ([`status`]), the in-app log ([`logbuf`]),
//! and the display's network service ([`service`], [`display`]).

pub mod ctx;
pub mod discovery;
pub mod display;
pub mod logbuf;
pub mod net;
pub mod secure;
pub mod service;
pub mod status;
