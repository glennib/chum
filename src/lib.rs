//! `chum` — a general-purpose ClickHouse schema migration tool.
//!
//! `chum` is modeled on sqlx-cli / golang-migrate but speaks ClickHouse over
//! the HTTP interface via the [`clickhouse`] crate. Migration files use the
//! `<version>_<name>.{up,down}.sql` convention. Multi-statement files are split
//! into individual statements with sqlparser's *tokenizer* (ClickHouse DDL
//! cannot be fully parsed into an AST, but statement boundaries can be found
//! robustly — see [`split`]).
//!
//! # Library usage
//!
//! Build a [`Migrator`] from a source and drive it against a client. This
//! example reads migrations from a directory at runtime:
//!
//! The library takes a ready-made [`clickhouse::Client`] — building a client
//! from a connection URL is the CLI's job, not the library's.
//!
//! ```no_run
//! use std::path::Path;
//!
//! use chum::Migrator;
//! use chum::backend::Bookkeeping;
//! use chum::source;
//!
//! # async fn run() -> chum::Result<()> {
//! let migrator = Migrator::new(source::from_path(
//!     Path::new("migrations"),
//! )?);
//! // The client is connected with no app database pinned as the session
//! // default, so migrations can create and fully-qualify their own databases.
//! let client = clickhouse::Client::default()
//!     .with_url("http://localhost:8123");
//! // Bookkeeping lives in its own database (default `_chum`), fully-qualified
//! // as `_chum._chum_migrations` — independent of any app database.
//! let bookkeeping = Bookkeeping::new(
//!     chum::DEFAULT_BOOKKEEPING_DATABASE,
//!     chum::DEFAULT_TABLE,
//! )?;
//! migrator.run(&client, &bookkeeping, None).await?;
//! # Ok(())
//! # }
//! ```
//!
//! To embed migrations in the binary at compile time instead, use
//! [`source::from_dir`] with an [`include_dir`]-embedded directory:
//!
//! ```ignore
//! use include_dir::{Dir, include_dir};
//! static MIGRATIONS: Dir = include_dir!("$CARGO_MANIFEST_DIR/migrations");
//! let migrator = chum::Migrator::new(chum::source::from_dir(&MIGRATIONS)?);
//! ```

pub mod backend;
pub mod error;
pub mod migration;
pub mod migrator;
pub mod source;
pub mod split;

/// The default name of the bookkeeping table recording applied migrations.
pub const DEFAULT_TABLE: &str = "_chum_migrations";

/// The default name of the dedicated database that holds the bookkeeping table.
///
/// Bookkeeping lives in its own database (fully-qualified as
/// `<database>.<table>`) so it never depends on the app database — the
/// connection's session default — existing. This lets a migration create and
/// fully-qualify its own database.
pub const DEFAULT_BOOKKEEPING_DATABASE: &str = "_chum";

pub use backend::Bookkeeping;
pub use error::ChumError;
pub use error::Result;
pub use migration::AppliedMigration;
pub use migration::Direction;
pub use migration::Migration;
pub use migrator::Applied;
pub use migrator::Migrator;
pub use migrator::Planned;
pub use migrator::Progress;
pub use migrator::State;
pub use migrator::Status;
