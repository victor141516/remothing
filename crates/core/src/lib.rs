//! Platform-independent core: tile diff + motion classification, lossless/video
//! policy, LZ4, ACK flow control, wire protocol, HTTP/WebSocket transport and
//! the admin UI/API. OS specifics come in through the traits in [`platform`]
//! and [`encode`].

pub mod admin;
pub mod audio;
pub mod app;
pub mod assets;
pub mod capture;
pub mod config;
pub mod encode;
pub mod geom;
pub mod mic;
pub mod pixels;
pub mod pacer;
pub mod platform;
pub mod proto;
pub mod recovery;
pub mod rtc;
pub mod shared;
pub mod stream;
pub mod telemetry;
pub mod tls;
pub mod wt;
