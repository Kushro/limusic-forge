//! Playlist tools: reorder, move and copy between playlists, find duplicates, split, merge,
//! export, with an undo journal over all of them. The ideas (and the pure algorithms) come from
//! PlaylistForge; the I/O is this app's own InnerTube client and SQLite.
//!
//! Layout: pure logic in its own modules (`lis`), every edit through `rows` (account and local
//! playlists alike), and `journal` for undo.

pub mod build;
pub mod dedup;
pub mod export;
pub mod journal;
pub mod lis;
pub mod merge;
pub mod rows;
pub mod split;
pub mod transfer;
