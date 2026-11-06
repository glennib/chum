//! Resolving migrations from a source.
//!
//! Two front-ends produce the same [`Migration`] values:
//!
//! * [`from_path`] reads a directory at runtime — used by the `chum` CLI.
//! * [`from_dir`] reads a compile-time-embedded [`include_dir::Dir`] — used by
//!   library consumers that embed their own migrations in their binary.
//!
//! A file ending in `.up.sql` or `.down.sql` is a plain migration; one ending
//! in `.up.sql.j2` or `.down.sql.j2` (or, equally, `.up.j2.sql` or
//! `.down.j2.sql`) is a template, rendered with the supplied variables before
//! it runs. Without the `template` feature, a
//! source containing a template fails to load rather than skipping it.
//!
//! A future `migrate!` proc-macro would be a third front-end emitting
//! `&'static [Migration]` directly; the migrator already accepts borrowed
//! static data via [`Cow`], so adding it is not a breaking change to the core.
//!
//! [`Cow`]: std::borrow::Cow

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;

use include_dir::Dir;

use crate::error::ChumError;
use crate::error::Result;
use crate::migration::Direction;
use crate::migration::Migration;

/// How a template migration's extension is spelled. Both spellings load the
/// same way; the style only matters when chum writes or renames a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TemplateStyle {
    /// `.sql.j2`, e.g. `0001_x.up.sql.j2`.
    #[default]
    SqlJ2,
    /// `.j2.sql`, e.g. `0001_x.up.j2.sql`.
    J2Sql,
}

impl TemplateStyle {
    /// The file extension, with its leading dot: `.sql.j2` or `.j2.sql`.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            TemplateStyle::SqlJ2 => ".sql.j2",
            TemplateStyle::J2Sql => ".j2.sql",
        }
    }
}

/// The filename suffix after `<version>_<name>`: `.up.sql`, `.down.j2.sql`, …
fn suffix(direction: Direction, template: Option<TemplateStyle>) -> String {
    let extension = template.map_or(".sql", TemplateStyle::extension);
    format!(".{}{extension}", direction.as_str())
}

/// A migration filename, parsed.
#[derive(Debug, PartialEq, Eq)]
struct Parsed {
    version: i64,
    /// The `<name>` segment with its underscores intact.
    name: String,
    direction: Direction,
    template: Option<TemplateStyle>,
}

impl Parsed {
    /// A human description: `<name>` with underscores turned into spaces.
    fn description(&self) -> String {
        self.name.replace('_', " ")
    }
}

/// Parse a migration filename of the form
/// `<version>_<name>.{up,down}.{sql,sql.j2,j2.sql}`.
///
/// Returns `None` for any filename that does not match (such files are
/// ignored, matching sqlx / golang-migrate behavior).
fn parse(file_name: &str) -> Option<Parsed> {
    let (stem, template) = [TemplateStyle::SqlJ2, TemplateStyle::J2Sql]
        .into_iter()
        .find_map(|style| Some((file_name.strip_suffix(style.extension())?, Some(style))))
        .or_else(|| Some((file_name.strip_suffix(".sql")?, None)))?;
    let (rest, direction) = [Direction::Up, Direction::Down]
        .into_iter()
        .find_map(|d| Some((stem.strip_suffix(&format!(".{}", d.as_str()))?, d)))?;

    let (version_part, name) = rest.split_once('_')?;
    let version: i64 = version_part.parse().ok()?;

    Some(Parsed {
        version,
        name: name.to_owned(),
        direction,
        template,
    })
}

/// Build a migration from a parsed filename and its contents.
fn migration(parsed: &Parsed, file_name: &str, sql: String) -> Result<Migration> {
    if parsed.template.is_some() && !cfg!(feature = "template") {
        return Err(ChumError::TemplatingDisabled(file_name.to_owned()));
    }
    Ok(
        Migration::new(parsed.version, parsed.description(), parsed.direction, sql)
            .with_template(parsed.template.is_some())
            .with_file_name(file_name.to_owned()),
    )
}

/// Resolve all migrations from a directory, sorted by version then direction.
///
/// # Errors
///
/// Returns [`ChumError::Source`] if the directory cannot be read or a version
/// has both a plain and a template file for one direction,
/// [`ChumError::TemplatingDisabled`] for a template without the `template`
/// feature, or [`ChumError::Io`] if a migration file cannot be read.
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
        let Some(parsed) = parse(&file_name) else {
            continue;
        };
        let sql = std::fs::read_to_string(entry.path())?;
        migrations.push(migration(&parsed, &file_name, sql)?);
    }

    finish(migrations)
}

/// Resolve all migrations from a compile-time-embedded directory.
///
/// # Errors
///
/// Returns [`ChumError::Source`] if an embedded file's contents are not valid
/// UTF-8 or a version has both a plain and a template file for one direction,
/// or [`ChumError::TemplatingDisabled`] for a template without the `template`
/// feature.
pub fn from_dir(dir: &Dir<'static>) -> Result<Vec<Migration>> {
    let mut migrations = Vec::new();
    for file in dir.files() {
        let Some(file_name) = file.path().file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(parsed) = parse(file_name) else {
            continue;
        };
        let sql = file.contents_utf8().ok_or_else(|| {
            ChumError::Source(format!(
                "embedded migration {} is not valid UTF-8",
                file.path().display()
            ))
        })?;
        migrations.push(migration(&parsed, file_name, sql.to_owned())?);
    }

    finish(migrations)
}

/// Sort, and reject a version that has a plain and a template file for the
/// same direction: it would be ambiguous which one runs.
fn finish(mut migrations: Vec<Migration>) -> Result<Vec<Migration>> {
    sort(&mut migrations);
    let mut seen: BTreeMap<(i64, u8), &Migration> = BTreeMap::new();
    for m in &migrations {
        if let Some(other) = seen.insert((m.version, direction_rank(m.direction)), m)
            && (m.template || other.template)
        {
            return Err(ChumError::Source(format!(
                "version {} has two {} migrations: {} and {}",
                m.version,
                m.direction.as_str(),
                other.file_name,
                m.file_name
            )));
        }
    }
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
        let Some(Parsed { version, .. }) = parse(&file_name) else {
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
/// human description [`from_path`] produces), its direction, whether it is a
/// template, and full path.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MigrationFile {
    /// The numeric version parsed from the filename prefix.
    pub version: i64,
    /// The `<name>` segment, exactly as written (e.g. `add_users_table`).
    pub name: String,
    /// Which half of the pair this file is.
    pub direction: Direction,
    /// The extension style of a template; `None` for a plain `.sql` file.
    pub template: Option<TemplateStyle>,
    /// The file's path on disk.
    pub path: PathBuf,
}

impl MigrationFile {
    /// The filename suffix after `<version>_<name>`, spelled as on disk:
    /// `.up.sql`, `.down.sql.j2`, `.up.j2.sql`, …
    #[must_use]
    pub fn suffix(&self) -> String {
        suffix(self.direction, self.template)
    }
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
        let Some(parsed) = parse(&file_name) else {
            continue;
        };
        files.push(MigrationFile {
            version: parsed.version,
            name: parsed.name,
            direction: parsed.direction,
            template: parsed.template,
            path: entry.path(),
        });
    }

    files.sort_by_key(|f| (f.version, direction_rank(f.direction)));
    Ok(files)
}

/// Scaffold an empty `up`/`down` migration pair in `dir`, as templates spelled
/// in the given style when `template` is set.
///
/// `version` is the numeric prefix — either a UTC timestamp such as
/// `20260626111407` or a zero-padded sequential counter such as `0001`.
/// Returns the paths of the created files.
///
/// # Errors
///
/// Returns [`ChumError::Source`] if either file already exists, or
/// [`ChumError::Io`] on a write failure.
pub fn scaffold(
    dir: &Path,
    version: &str,
    name: &str,
    template: Option<TemplateStyle>,
) -> Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(dir)?;
    let file = |direction| format!("{version}_{name}{}", suffix(direction, template));
    let up = dir.join(file(Direction::Up));
    let down = dir.join(file(Direction::Down));
    for path in [&up, &down] {
        if path.exists() {
            return Err(ChumError::Source(format!(
                "migration file already exists: {}",
                path.display()
            )));
        }
    }
    // A `--` comment in a template is still rendered; `{# … #}` isn't.
    let comment = |direction: &str| {
        if template.is_some() {
            format!("{{#{version}_{name} ({direction}) #}}\n")
        } else {
            format!("-- {version}_{name} ({direction})\n")
        }
    };
    std::fs::write(&up, comment("up"))?;
    std::fs::write(&down, comment("down"))?;
    Ok((up, down))
}

fn direction_rank(direction: Direction) -> u8 {
    match direction {
        Direction::Up => 0,
        Direction::Down => 1,
    }
}

/// Sort by version ascending, with `up` before `down` within a version.
fn sort(migrations: &mut [Migration]) {
    migrations.sort_by_key(|m| (m.version, direction_rank(m.direction)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_up_and_down() {
        let p = parse("20260626111407_initial_schema.up.sql").unwrap();
        assert_eq!(p.version, 20_260_626_111_407);
        assert_eq!(p.description(), "initial schema");
        assert_eq!(p.direction, Direction::Up);
        assert_eq!(p.template, None);

        let p = parse("0001_foo.down.sql").unwrap();
        assert_eq!(p.direction, Direction::Down);
    }

    #[test]
    fn parses_templates() {
        let p = parse("20261106120000_events_hourly_mv.up.sql.j2").unwrap();
        assert_eq!(p.version, 20_261_106_120_000);
        assert_eq!(p.name, "events_hourly_mv");
        assert_eq!(p.direction, Direction::Up);
        assert_eq!(p.template, Some(TemplateStyle::SqlJ2));

        let p = parse("0001_foo.down.sql.j2").unwrap();
        assert_eq!(p.direction, Direction::Down);
        assert_eq!(p.template, Some(TemplateStyle::SqlJ2));
    }

    #[test]
    fn parses_j2_sql_templates() {
        let p = parse("0001_foo.up.j2.sql").unwrap();
        assert_eq!(p.version, 1);
        assert_eq!(p.name, "foo");
        assert_eq!(p.direction, Direction::Up);
        assert_eq!(p.template, Some(TemplateStyle::J2Sql));

        let p = parse("0001_foo.down.j2.sql").unwrap();
        assert_eq!(p.direction, Direction::Down);
        assert_eq!(p.template, Some(TemplateStyle::J2Sql));
    }

    #[test]
    fn suffix_round_trips_the_spelling() {
        for name in [
            "0001_foo.up.sql",
            "0001_foo.down.sql.j2",
            "0001_foo.up.j2.sql",
        ] {
            let p = parse(name).unwrap();
            assert_eq!(format!("0001_foo{}", suffix(p.direction, p.template)), name);
        }
    }

    #[test]
    fn rejects_non_migrations() {
        assert!(parse("README.md").is_none());
        assert!(parse("notanumber_foo.up.sql").is_none());
        assert!(parse("0001_foo.sql").is_none());
        assert!(parse("0001_foo.sql.j2").is_none());
        assert!(parse("0001_foo.j2.sql").is_none());
        assert!(parse("0001_foo.up.j2").is_none());
    }

    fn write_dir(name: &str, files: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("chum-source-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for f in files {
            std::fs::write(dir.join(f), "SELECT 1\n").unwrap();
        }
        dir
    }

    #[cfg(feature = "template")]
    #[test]
    fn a_plain_and_a_template_file_for_one_direction_are_rejected() {
        let dir = write_dir("dup", &["0001_x.up.sql", "0001_x.up.sql.j2"]);
        let err = from_path(&dir).unwrap_err();
        assert!(
            matches!(err, ChumError::Source(ref m) if m.contains("two up")),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "template")]
    #[test]
    fn both_template_spellings_for_one_direction_are_rejected() {
        let dir = write_dir("dup-spelling", &["0001_x.up.sql.j2", "0001_x.up.j2.sql"]);
        let err = from_path(&dir).unwrap_err();
        assert!(
            matches!(err, ChumError::Source(ref m) if m.contains("two up")),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "template")]
    #[test]
    fn templates_load_with_the_feature() {
        let dir = write_dir("tpl", &["0001_x.up.sql.j2", "0001_x.down.j2.sql"]);
        let migrations = from_path(&dir).unwrap();
        assert!(migrations[0].template);
        assert_eq!(migrations[0].file_name, "0001_x.up.sql.j2");
        assert_eq!(migrations[0].description, "x");
        assert!(migrations[1].template);
        assert_eq!(migrations[1].file_name, "0001_x.down.j2.sql");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(feature = "template"))]
    #[test]
    fn templates_fail_to_load_without_the_feature() {
        let dir = write_dir("notpl", &["0001_x.up.sql.j2"]);
        let err = from_path(&dir).unwrap_err();
        assert!(matches!(err, ChumError::TemplatingDisabled(_)), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
