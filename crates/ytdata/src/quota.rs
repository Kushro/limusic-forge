//! Quota accounting hook for [`crate::client::YouTubeClient`].
//!
//! An injectable [`QuotaSink`] rather than a return value: `videos_list` chunks into several
//! `videos.list` calls, and recording at the moment each call succeeds keeps the units of call 1
//! even if call 2 fails. The app supplies a sink that writes its ledger; callers that do not care
//! never call `with_quota_sink`, and every record becomes a no-op.

/// Receives every successful, billable call. `endpoint` is one of [`endpoint`]'s constants and
/// `units` the cost from [`cost`]; the sink records them as-is.
pub trait QuotaSink: Send + Sync {
    fn record(&self, endpoint: &'static str, units: u32, account_id: Option<&str>);
}

/// Per-endpoint unit costs (Google's Quota Calculator: list = 1, write = 50).
pub mod cost {
    pub const PLAYLISTS_LIST: u32 = 1;
    pub const PLAYLIST_ITEMS_LIST: u32 = 1;
    pub const VIDEOS_LIST: u32 = 1;
    pub const CHANNELS_LIST: u32 = 1;

    pub const PLAYLISTS_INSERT: u32 = 50;
    pub const PLAYLISTS_UPDATE: u32 = 50;
    pub const PLAYLISTS_DELETE: u32 = 50;
    pub const PLAYLIST_ITEMS_INSERT: u32 = 50;
    pub const PLAYLIST_ITEMS_UPDATE: u32 = 50;
    pub const PLAYLIST_ITEMS_DELETE: u32 = 50;
}

/// Endpoint names, identical to PlaylistForge's ledger strings so imported history lines up.
pub mod endpoint {
    pub const PLAYLISTS_LIST: &str = "playlists.list";
    pub const PLAYLIST_ITEMS_LIST: &str = "playlistItems.list";
    pub const VIDEOS_LIST: &str = "videos.list";
    pub const CHANNELS_LIST: &str = "channels.list";

    pub const PLAYLISTS_INSERT: &str = "playlists.insert";
    pub const PLAYLISTS_UPDATE: &str = "playlists.update";
    pub const PLAYLISTS_DELETE: &str = "playlists.delete";
    pub const PLAYLIST_ITEMS_INSERT: &str = "playlistItems.insert";
    pub const PLAYLIST_ITEMS_UPDATE: &str = "playlistItems.update";
    pub const PLAYLIST_ITEMS_DELETE: &str = "playlistItems.delete";
}

/// Cost of an endpoint by name. Unknown names cost `0` rather than panicking: an endpoint added
/// without updating this table must never crash or inflate the ledger with a made-up number.
pub fn unit_cost(endpoint_name: &str) -> u32 {
    use endpoint::*;
    match endpoint_name {
        PLAYLISTS_LIST => cost::PLAYLISTS_LIST,
        PLAYLIST_ITEMS_LIST => cost::PLAYLIST_ITEMS_LIST,
        VIDEOS_LIST => cost::VIDEOS_LIST,
        CHANNELS_LIST => cost::CHANNELS_LIST,
        PLAYLISTS_INSERT => cost::PLAYLISTS_INSERT,
        PLAYLISTS_UPDATE => cost::PLAYLISTS_UPDATE,
        PLAYLISTS_DELETE => cost::PLAYLISTS_DELETE,
        PLAYLIST_ITEMS_INSERT => cost::PLAYLIST_ITEMS_INSERT,
        PLAYLIST_ITEMS_UPDATE => cost::PLAYLIST_ITEMS_UPDATE,
        PLAYLIST_ITEMS_DELETE => cost::PLAYLIST_ITEMS_DELETE,
        _ => 0,
    }
}

/// Whether an endpoint is a write (50 units, never retried blindly by job runners).
pub fn is_write(endpoint_name: &str) -> bool {
    unit_cost(endpoint_name) >= 50
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from pf-core/src/quota.rs `unit_cost_matches_doc_02_table`.
    #[test]
    fn unit_cost_matches_doc_02_table() {
        assert_eq!(unit_cost(endpoint::PLAYLISTS_LIST), 1);
        assert_eq!(unit_cost(endpoint::PLAYLIST_ITEMS_LIST), 1);
        assert_eq!(unit_cost(endpoint::VIDEOS_LIST), 1);
        assert_eq!(unit_cost(endpoint::CHANNELS_LIST), 1);
        assert_eq!(unit_cost(endpoint::PLAYLISTS_INSERT), 50);
        assert_eq!(unit_cost(endpoint::PLAYLIST_ITEMS_DELETE), 50);
        assert_eq!(unit_cost("some.unknown.endpoint"), 0);
    }

    #[test]
    fn every_write_costs_fifty_and_every_read_costs_one() {
        let reads = [
            endpoint::PLAYLISTS_LIST,
            endpoint::PLAYLIST_ITEMS_LIST,
            endpoint::VIDEOS_LIST,
            endpoint::CHANNELS_LIST,
        ];
        let writes = [
            endpoint::PLAYLISTS_INSERT,
            endpoint::PLAYLISTS_UPDATE,
            endpoint::PLAYLISTS_DELETE,
            endpoint::PLAYLIST_ITEMS_INSERT,
            endpoint::PLAYLIST_ITEMS_UPDATE,
            endpoint::PLAYLIST_ITEMS_DELETE,
        ];
        for name in reads {
            assert_eq!(unit_cost(name), 1, "{name}");
            assert!(!is_write(name), "{name}");
        }
        for name in writes {
            assert_eq!(unit_cost(name), 50, "{name}");
            assert!(is_write(name), "{name}");
        }
    }
}
