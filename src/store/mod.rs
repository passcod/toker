//! SQLite storage: the requests ledger plus state tables.
//!
//! Plan: "Storage" — `$XDG_DATA_HOME/toker/toker.db`, WAL mode. `requests` is
//! insert-only, one row per request, with cost in three explicit kinds never
//! conflated (billed / estimated / plan-equivalent); state (lanes, learned
//! models, allowances, pings, last-meters) lives in tables. No content is
//! ever stored (invariant 1).

/// Placeholder handle for the open `toker.db` connection.
#[allow(dead_code)]
#[derive(Debug)]
pub struct Store {
    /// Placeholder; grows into a rusqlite handle with migrations.
    pub placeholder: (),
}

#[cfg(test)]
mod tests {
    #[test]
    fn store_skeleton() {
        // Schema and round-trip tests land here.
        assert!(true);
    }
}
