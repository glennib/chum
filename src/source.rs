//! Resolving migrations from a source.
//!
//! Two front-ends produce the same [`Migration`] values:
//!
//! * [`from_path`] reads a directory at runtime — used by the `chum` CLI.
//! * [`from_dir`] reads a compile-time-embedded [`include_dir::Dir`] — used by
//!   library consumers that embed their own migrations in their binary.
//!
//! A future `migrate!` proc-macro would be a third front-end emitting
//! `&'static [Migration]` directly; the migrator already accepts borrowed
//! static data via [`Cow`], so adding it is not a breaking change to the core.
//!
//! [`Cow`]: std::borrow::Cow

use std::path::Path;
use std::path::PathBuf;

use include_dir::Dir;

use crate::error::ChumError;
use crate::error::Result;
use crate::migration::Direction;
use crate::migration::Migration;

/// Parse a migration filename of the form `<version>_<name>.{up,down}.sql`,
/// returning the version, a human description (`<name>` with underscores turned
/// into spaces), and the direction.
///
/// Returns `None` for any filename that does not match (such files are
/// ignored, matching sqlx / golang-migrate behavior).
fn parse_filename(file_name: &str) -> Option<(i64, String, Direction)> {
    let (version, name, direction) = parse_raw(file_name)?;
    Some((version, name.replace('_', " "), direction))
}

/// Like [`parse_filename`] but returns the raw `<name>` segment with its
/// underscores intact — what a rename needs in order to reconstruct the
/// filename under a new version prefix.
fn parse_raw(file_name: &str) -> Option<(i64, String, Direction)> {
    let direction = if file_name.ends_with(".up.sql") {
        Direction::Up
    } else if file_name.ends_with(".down.sql") {
        Direction::Down
    } else {
        return None;
    };

    let (version_part, rest) = file_name.split_once('_')?;
    let version: i64 = version_part.parse().ok()?;

    let name = rest.trim_end_matches(direction.suffix()).to_owned();

    Some((version, name, direction))
}

/// Resolve all migrations from a directory, sorted by version then direction.
///
/// # Errors
///
/// Returns [`ChumError::Source`] if the directory cannot be read, or
/// [`ChumError::Io`] if a migration file cannot be read.
pub fn from_path(dir: &Path) -> Result<Vec<Migration>> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        ChumError::Source(format!(
            "reading migration directory {}: {e}",
            dir.display()
        ))
    })?;

    let mut migrations = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some((version, description, direction)) = parse_filename(&file_name) else {
            continue;
        };
        let sql = std::fs::read_to_string(entry.path())?;
        migrations.push(Migration::new(version, description, direction, sql));
    }

    sort(&mut migrations);
    Ok(migrations)
}

/// Resolve all migrations from a compile-time-embedded directory.
///
/// # Errors
///
/// Returns [`ChumError::Source`] if an embedded file's contents are not valid
/// UTF-8.
pub fn from_dir(dir: &Dir<'static>) -> Result<Vec<Migration>> {
    let mut migrations = Vec::new();
    for file in dir.files() {
        let Some(file_name) = file.path().file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some((version, description, direction)) = parse_filename(file_name) else {
            continue;
        };
        let sql = file.contents_utf8().ok_or_else(|| {
            ChumError::Source(format!(
                "embedded migration {} is not valid UTF-8",
                file.path().display()
            ))
        })?;
        migrations.push(Migration::new(
            version,
            description,
            direction,
            sql.to_owned(),
        ));
    }

    sort(&mut migrations);
    Ok(migrations)
}

/// The highest version present in `dir`, paired with the character width of its
/// zero-padded numeric prefix, or `None` when the directory holds no migrations
/// (including when it does not exist yet).
///
/// Used by the `add` command to continue a sequential numbering scheme. Unlike
/// [`from_path`] it only inspects filenames — it never reads file bodies — and
/// treats a missing directory as "no migrations" rather than an error, since
/// `add` scaffolds into a directory that may not exist yet.
///
/// # Errors
///
/// Returns [`ChumError::Source`] if the directory exists but cannot be read, or
/// [`ChumError::Io`] if a directory entry cannot be inspected.
pub fn max_version(dir: &Path) -> Result<Option<(i64, usize)>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(ChumError::Source(format!(
                "reading migration directory {}: {e}",
                dir.display()
            )));
        }
    };

    let mut max: Option<(i64, usize)> = None;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some((version, _, _)) = parse_filename(&file_name) else {
            continue;
        };
        // Width of the version prefix exactly as written on disk, so a `0001`
        // scheme keeps its padding when continued.
        let width = file_name.split_once('_').map_or(0, |(v, _)| v.len());
        if max.is_none_or(|(m, _)| version > m) {
            max = Some((version, width));
        }
    }
    Ok(max)
}

/// A migration file resolved from a directory, retaining what a rename needs:
/// the parsed version, the raw `<name>` segment (underscores intact, unlike the
/// human description [`from_path`] produces), its direction, and full path.
#[derive(Debug, Clone)]
pub struct MigrationFile {
    /// The numeric version parsed from the filename prefix.
    pub version: i64,
    /// The `<name>` segment, exactly as written (e.g. `add_users_table`).
    pub name: String,
    /// Which half of the pair this file is.
    pub direction: Direction,
    /// The file's path on disk.
    pub path: PathBuf,
}

/// List every migration file in `dir` (both directions), sorted by version then
/// direction.
///
/// Unlike [`from_path`] this never reads file bodies — only the metadata a
/// rename needs — and unlike [`max_version`] it returns the full set rather
/// than just the maximum. Non-migration files are ignored.
///
/// # Errors
///
/// Returns [`ChumError::Source`] if the directory cannot be read, or
/// [`ChumError::Io`] if a directory entry cannot be inspected.
pub fn migration_files(dir: &Path) -> Result<Vec<MigrationFile>> {
    let entries = std::fs::read_dir(dir).map_err(|e| {
        ChumError::Source(format!(
            "reading migration directory {}: {e}",
            dir.display()
        ))
    })?;

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some((version, name, direction)) = parse_raw(&file_name) else {
            continue;
        };
        files.push(MigrationFile {
            version,
            name,
            direction,
            path: entry.path(),
        });
    }

    files.sort_by_key(|f| {
        (
            f.version,
            match f.direction {
                Direction::Up => 0,
                Direction::Down => 1,
            },
        )
    });
    Ok(files)
}

/// Scaffold an empty `up`/`down` migration pair in `dir`.
///
/// `version` is the numeric prefix — either a UTC timestamp such as
/// `20260626111407` or a zero-padded sequential counter such as `0001`.
/// Returns the paths of the created files.
///
/// # Errors
///
/// Returns [`ChumError::Source`] if either file already exists, or
/// [`ChumError::Io`] on a write failure.
pub fn scaffold(dir: &Path, version: &str, name: &str) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let up = dir.join(format!("{version}_{name}.up.sql"));
    let down = dir.join(format!("{version}_{name}.down.sql"));
    for path in [&up, &down] {
        if path.exists() {
            return Err(ChumError::Source(format!(
                "migration file already exists: {}",
                path.display()
            )));
        }
    }
    std::fs::write(&up, format!("-- {version}_{name} (up)\n"))?;
    std::fs::write(&down, format!("-- {version}_{name} (down)\n"))?;
    Ok((up, down))
}

/// Sort by version ascending, with `up` before `down` within a version.
fn sort(migrations: &mut [Migration]) {
    migrations.sort_by_key(|m| {
        (
            m.version,
            match m.direction {
                Direction::Up => 0,
                Direction::Down => 1,
            },
        )
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_up_and_down() {
        let (v, d, dir) = parse_filename("20260626111407_initial_schema.up.sql").unwrap();
        assert_eq!(v, 20_260_626_111_407);
        assert_eq!(d, "initial schema");
        assert_eq!(dir, Direction::Up);

        let (_, _, dir) = parse_filename("0001_foo.down.sql").unwrap();
        assert_eq!(dir, Direction::Down);
    }

    #[test]
    fn rejects_non_migrations() {
        assert!(parse_filename("README.md").is_none());
        assert!(parse_filename("notanumber_foo.up.sql").is_none());
        assert!(parse_filename("0001_foo.sql").is_none());
    }
}
