//! The two pages, served inline. Libraries come from a CDN in this first cut; vendoring
//! them into the binary is a later step so the page works offline.

/// The session list and the form that starts one.
pub const INDEX: &str = include_str!("../static/index.html");
/// A session: the terminal and the reading pane.
pub const SESSION: &str = include_str!("../static/session.html");
