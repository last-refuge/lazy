//! End-to-end tests live in `tests/`. They start a real server on an ephemeral port against a
//! throwaway Postgres database, run real client runtimes against it, and inject faults through a
//! TCP proxy. The Postgres from `scripts/dev-db.sh start` must be running.
