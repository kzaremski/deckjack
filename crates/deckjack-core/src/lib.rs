//! Deckjack core.
//!
//! One browser-based workspace served by machines I own. Every interface is
//! real web UI; nothing streams pixels. See docs/DESIGN.md.

pub mod agentbridge;
pub mod apphost;
pub mod approvals;
pub mod apps;
pub mod archive;
pub mod audit;
pub mod auth;
pub mod command;
pub mod config;
pub mod content;
pub mod debugmode;
pub mod editor;
pub mod events;
pub mod fetchgate;
pub mod files;
pub mod grants;
pub mod http;
pub mod install;
pub mod lock;
pub mod lockdown;
pub mod mcp;
pub mod mcpserve;
pub mod node;
pub mod passkey;
pub mod passkeyhttp;
pub mod passkeypage;
pub mod proxy;
pub mod registry;
pub mod sandbox;
pub mod seat;
pub mod shell;
pub mod space;
pub mod store;
pub mod sys;
pub mod terminal;
pub mod thumbnail;
pub mod ticket;
pub mod transfer;
pub mod trash;
pub mod window;
pub mod wiring;
pub mod ws;
