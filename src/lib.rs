//! Installs, updates and starts World of Warcraft clients from Blizzard's CDN, without the Battle.net
//! launcher. Used by the `wowupd` command and the `wowupd-gtk` window.

mod casc;
mod cdn;
pub mod install;
mod jenkins;
pub mod launch;
pub mod report;
mod tact;
