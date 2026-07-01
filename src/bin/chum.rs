//! `chum` — command-line ClickHouse migrator.
//!
//! The CLI owns the connection story: it maps a single connection URL (plus
//! optional overrides) to a [`clickhouse::Client`]. The library is given the
//! ready-made client and knows nothing about URLs.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io::IsTerminal as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use chrono::SecondsFormat;
use chum::Migrator;
use chum::Progress;
use chum::State;
use chum::source;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;
use indicatif::ProgressBar;
use indicatif::ProgressStyle;
use miette::Context as _;
use miette::IntoDiagnostic as _;
use miette::bail;
use miette::miette;
use percent_encoding::percent_decode_str;
use serde::Serialize;
use tabled::builder::Builder as TableBuilder;
use tabled::settings::Style as TableStyle;
use url::Url;

/// Visual theme — the single source of truth for color across the CLI.
///
/// Four independent ANSI systems run side by side: `anstyle` (clap's help),
/// `console` (our runtime output and tables), `indicatif` (spinners), and
/// `miette` (error reports; see `install_error_renderer`). Each decides
/// *independently* whether to emit color at all (tty / `NO_COLOR` /
/// `CLICOLOR`). To stay uniform we (a) keep one palette here, mapped to both
/// `anstyle` and `console`, and (b) route **all** table/cell color through
/// `console`-styled strings rather than tabled's native coloring, so a single
/// detector (console) turns color on or off for everything printed to stdout.
/// When color is suppressed, the styled strings degrade to plain text and the
/// tables degrade with them.
mod theme {
    use clap::builder::styling::AnsiColor;
    use clap::builder::styling::Effects;
    use clap::builder::styling::Style;
    use clap::builder::styling::Styles;
    use console::Style as ConsoleStyle;

    // clap help styling. The stock cargo palette (green headers, cyan literals)
    // is retinted to chum's own: magenta for structure (headers/usage) and blue
    // for the literals you type (subcommands, flags, valid values). Green is
    // reserved for success at runtime.
    const HEADER: Style = AnsiColor::Magenta.on_default().effects(Effects::BOLD);
    const USAGE: Style = AnsiColor::Magenta.on_default().effects(Effects::BOLD);
    const LITERAL: Style = AnsiColor::Blue.on_default().effects(Effects::BOLD);
    const PLACEHOLDER: Style = AnsiColor::Blue.on_default();
    const ERROR: Style = AnsiColor::Red.on_default().effects(Effects::BOLD);
    const VALID: Style = AnsiColor::Blue.on_default().effects(Effects::BOLD);
    const INVALID: Style = AnsiColor::Yellow.on_default().effects(Effects::BOLD);

    pub(super) const CLAP_STYLES: Styles = Styles::styled()
        .header(HEADER)
        .usage(USAGE)
        .literal(LITERAL)
        .placeholder(PLACEHOLDER)
        .error(ERROR)
        .valid(VALID)
        .invalid(INVALID);

    // Runtime styling. The semantic colors line up with the clap palette
    // above: magenta = structure/headings, blue = identifiers, green =
    // good/applied, yellow = needs attention/pending, red = error/dirty. Using
    // the 16 named ANSI colors (rather than fixed RGB) lets each terminal's own
    // theme tune them, so the palette stays legible on dark and light
    // backgrounds alike.

    /// A table header cell, or a success heading.
    pub(super) fn heading() -> ConsoleStyle {
        ConsoleStyle::new().magenta().bold()
    }
    /// A migration version number.
    pub(super) fn version() -> ConsoleStyle {
        ConsoleStyle::new().blue()
    }
    /// An applied migration / success.
    pub(super) fn applied() -> ConsoleStyle {
        ConsoleStyle::new().green()
    }
    /// A pending migration.
    pub(super) fn pending() -> ConsoleStyle {
        ConsoleStyle::new().yellow()
    }
    /// A dirty migration / error.
    pub(super) fn dirty() -> ConsoleStyle {
        ConsoleStyle::new().red().bold()
    }
    /// Secondary, de-emphasised text (summaries, timings, "nothing to do").
    pub(super) fn muted() -> ConsoleStyle {
        ConsoleStyle::new().dim()
    }
}

/// A green check glyph for success lines (plain `+` when color is off).
fn check() -> console::StyledObject<&'static str> {
    theme::applied().apply_to("✓")
}

/// Build a steady-ticking spinner for a database operation. Draws to stderr and
/// auto-hides when stderr is not a terminal, so piped/CI output stays clean.
fn spinner(message: &str) -> ProgressBar {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::with_template("{spinner:.magenta} {msg}")
            .expect("static spinner template is valid")
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏", "✓"]),
    );
    pb.enable_steady_tick(Duration::from_millis(90));
    pb.set_message(message.to_owned());
    pb
}

/// Style a set of column labels as a bold table header row.
fn header_row<const N: usize>(labels: [&str; N]) -> [String; N] {
    labels.map(|label| theme::heading().apply_to(label).to_string())
}

/// Render a built table with the shared rounded style. Cell color is already
/// baked into the strings by `console`; tabled's `ansi` feature measures their
/// width correctly so columns stay aligned.
fn print_table(builder: TableBuilder) {
    println!("{}", builder.build().with(TableStyle::rounded()));
}

/// Pluralise a count for a summary line: `1 migration`, `3 migrations`.
fn count(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// How output is rendered. `Auto` picks a human table on a terminal and
/// machine-readable TSV when stdout is redirected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    /// Pretty table on a terminal; TSV when piped/redirected.
    Auto,
    /// Always a pretty table.
    Pretty,
    /// JSON (pretty-printed).
    Json,
    /// Tab-separated values with a header row.
    Tsv,
}

/// The format actually used after resolving [`Format::Auto`] against the
/// terminal and the `--json` shorthand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolved {
    Pretty,
    Json,
    Tsv,
}

impl Format {
    /// Resolve to a concrete renderer. `--json` (the `json` flag) wins over
    /// `--format`; otherwise `Auto` is a table on an attended stdout and TSV
    /// when redirected.
    fn resolve(self, json: bool) -> Resolved {
        if json {
            return Resolved::Json;
        }
        match self {
            Format::Pretty => Resolved::Pretty,
            Format::Json => Resolved::Json,
            Format::Tsv => Resolved::Tsv,
            Format::Auto => {
                if std::io::stdout().is_terminal() {
                    Resolved::Pretty
                } else {
                    Resolved::Tsv
                }
            }
        }
    }
}

/// When to colorize output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum ColorChoice {
    /// Color on a terminal, plain when redirected (honors `NO_COLOR`).
    Auto,
    /// Always colorize.
    Always,
    /// Never colorize.
    Never,
}

impl ColorChoice {
    /// Apply the choice to `console`'s global color state, which governs every
    /// `theme`-styled string and the spinner. `Auto` leaves `console`'s own
    /// detection (tty + `NO_COLOR` / `CLICOLOR`) in place.
    fn apply(self) {
        let enabled = match self {
            ColorChoice::Always => Some(true),
            ColorChoice::Never => Some(false),
            ColorChoice::Auto => None,
        };
        if let Some(enabled) = enabled {
            console::set_colors_enabled(enabled);
            console::set_colors_enabled_stderr(enabled);
        }
    }
}

/// A migration numbering scheme — the target of the `convert` command and the
/// thing `add` auto-detects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Scheme {
    /// Zero-padded sequential counter (`0001`, `0002`, …).
    Sequential,
    /// `YYYYMMDDHHMMSS` UTC timestamp.
    Timestamp,
}

impl Scheme {
    /// The lowercase name used in messages and machine output.
    fn label(self) -> &'static str {
        match self {
            Scheme::Sequential => "sequential",
            Scheme::Timestamp => "timestamp",
        }
    }
}

/// The scheme a single version belongs to, by the same [`TIMESTAMP_FLOOR`]
/// boundary the `add` command uses to auto-detect.
fn scheme_of(version: i64) -> Scheme {
    if version >= TIMESTAMP_FLOOR {
        Scheme::Timestamp
    } else {
        Scheme::Sequential
    }
}

/// Print a value as pretty JSON to stdout.
fn print_json<T: Serialize>(value: &T) -> miette::Result<()> {
    println!("{}", serde_json::to_string_pretty(value).into_diagnostic()?);
    Ok(())
}

/// Print a header row and data rows as tab-separated values. Descriptions are
/// derived from filenames and never contain tabs or newlines, so no quoting is
/// needed.
fn print_tsv(header: &[&str], rows: &[Vec<String>]) {
    println!("{}", header.join("\t"));
    for row in rows {
        println!("{}", row.join("\t"));
    }
}

/// Format a timestamp for machine output (RFC 3339, UTC, second precision).
fn rfc3339(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Format a timestamp for the human table (no subsecond/zone noise).
fn human_time(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Ask the user to confirm a destructive action. Returns `Ok(true)` when
/// `assume_yes` is set. Otherwise prompts on stderr — but only when the session
/// is genuinely interactive (stdin and stderr are terminals) and rendering a
/// pretty table; in any non-interactive or machine-output mode it refuses
/// rather than block on stdin, so a piped `revert` fails fast instead of
/// hanging.
fn confirm(prompt: &str, assume_yes: bool, fmt: Resolved) -> miette::Result<bool> {
    if assume_yes {
        return Ok(true);
    }
    let interactive = fmt == Resolved::Pretty
        && std::io::stdin().is_terminal()
        && std::io::stderr().is_terminal();
    if !interactive {
        bail!("refusing to proceed without --yes in non-interactive mode");
    }
    eprint!("{prompt} {} ", theme::muted().apply_to("[y/N]"));
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).into_diagnostic()?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// A general-purpose ClickHouse schema migration tool.
#[derive(Debug, Parser)]
#[command(name = "chum", version, about, styles = theme::CLAP_STYLES)]
struct Cli {
    /// ClickHouse connection URL. Either a bare HTTP endpoint
    /// (`http://host:8123`) or a full DSN
    /// (`clickhouse+https://user:pass@host:8123/database?secure=true`).
    /// Credentials, database, and `secure` are read from it; `--user`,
    /// `--password`, and `--database` override the URL.
    ///
    /// **Not** read from the environment. chum reads no `CLICKHOUSE_*` env vars
    /// at all — the connection target must be passed explicitly, so a migrator
    /// never connects to whatever ambient cluster the environment happens to
    /// point at. Defaults to `http://localhost:8123` when omitted.
    #[arg(long, default_value = "http://localhost:8123")]
    url: String,

    /// Session default database for *unqualified* names in your migration SQL.
    ///
    /// This is NOT where chum keeps its bookkeeping — that lives in its own
    /// database (see `--bookkeeping-database`), always fully-qualified, so chum
    /// itself never needs any app database to exist. Leave this unset (the
    /// default) and fully-qualify object names in your migrations
    /// (`CREATE DATABASE app; CREATE TABLE app.t …`) so a migration can
    /// bootstrap its own database. Set it only if your migrations use
    /// unqualified names and rely on a session default — in which case the
    /// database **must already exist**, since ClickHouse rejects any
    /// request whose session default database is absent.
    ///
    /// This is **not** read from the environment — only an explicit
    /// `--database` or a database in the `--url` DSN (path /
    /// `database`/`db` query param) takes effect. chum reads no
    /// `CLICKHOUSE_*` env vars at all, so it never silently assembles a
    /// connection target from ambient env partials. This matters here
    /// specifically because chum commonly shares an `.env` with the
    /// application it migrates, which legitimately sets
    /// `CLICKHOUSE_DATABASE=<appdb>`; inheriting it would pin chum's session to
    /// an app database that may not exist yet, breaking the
    /// create-your-own-db flow above even though bookkeeping lives in
    /// `--bookkeeping-database`.
    #[arg(long)]
    database: Option<String>,

    /// Override the user from the `--url` DSN.
    ///
    /// **Not** read from the environment — supply credentials via the `--url`
    /// DSN userinfo (`clickhouse://user:pass@host…`) or this explicit flag.
    /// chum reads no `CLICKHOUSE_*` env vars, so it never silently
    /// assembles an identity from ambient env partials and ends up pointed
    /// at the wrong cluster.
    #[arg(long)]
    user: Option<String>,

    /// Override the password from the `--url` DSN.
    ///
    /// **Not** read from the environment (see `--user`). Supply it via the
    /// `--url` DSN userinfo or this explicit flag.
    #[arg(long)]
    password: Option<String>,

    /// Name of the bookkeeping table recording applied migrations.
    #[arg(long, env = "CHUM_TABLE", default_value = chum::DEFAULT_TABLE)]
    table: String,

    /// Dedicated database that holds the bookkeeping table.
    ///
    /// chum creates it (`CREATE DATABASE IF NOT EXISTS`) and fully-qualifies
    /// all bookkeeping SQL as `<bookkeeping-database>.<table>`, so
    /// bookkeeping never depends on any app database existing — a migration
    /// is free to create its own. Kept separate from `--database` for
    /// exactly this reason.
    #[arg(
        long,
        env = "CHUM_BOOKKEEPING_DATABASE",
        default_value = chum::DEFAULT_BOOKKEEPING_DATABASE
    )]
    bookkeeping_database: String,

    /// Directory containing `<version>_<name>.{up,down}.sql` migration files.
    #[arg(long, env = "CHUM_SOURCE", default_value = "migrations", global = true)]
    source: PathBuf,

    /// Output format. `auto` is a table on a terminal and TSV when redirected.
    #[arg(long, value_enum, default_value = "auto", global = true)]
    format: Format,

    /// Shorthand for `--format json`. Wins over `--format` if both are given.
    #[arg(long, global = true)]
    json: bool,

    /// When to colorize output.
    #[arg(long, value_enum, default_value = "auto", global = true)]
    color: ColorChoice,

    /// Apply the cross-replica consistency bundle (see `build_client`).
    ///
    /// Off by default: the `insert_quorum` settings it enables were found to
    /// hang every bookkeeping `INSERT` against the managed Aiven cluster (the
    /// `INSERT` blocks until `insert_quorum_timeout`), so the default keeps
    /// chum's writes best-effort and unblocked. Enable only on a cluster where
    /// the bundle is known to work and the linearizability guarantee is needed.
    #[arg(long, env = "CHUM_STRICT_CONSISTENCY", global = true)]
    strict_consistency: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Apply all pending migrations (optionally up to a target version).
    Run {
        /// Stop after applying this version.
        #[arg(long)]
        target: Option<i64>,
        /// Show the migrations that would be applied, without applying them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Revert applied migrations. With no flag, reverts the most recently
    /// applied migration.
    Revert {
        /// Revert every migration with a version greater than this.
        #[arg(long, conflicts_with = "steps")]
        target: Option<i64>,
        /// Revert this many of the most recently applied migrations
        /// (default: 1).
        #[arg(long)]
        steps: Option<usize>,
        /// Show the migrations that would be reverted, without reverting them.
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt. Required to revert non-interactively.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Show the state of every migration.
    Info,
    /// Scaffold a new `up`/`down` migration pair in the source directory.
    Add {
        /// Short name (e.g. `add_users_table`).
        name: String,
        /// Use a zero-padded sequential version (`0001`, `0002`, …) instead of
        /// a UTC timestamp. Only governs a fresh migration directory — when
        /// migrations already exist, chum continues whichever scheme they use,
        /// and this flag errors if asked to graft sequential numbers onto an
        /// existing timestamp scheme.
        #[arg(long)]
        sequential: bool,
    },
    /// Renumber every migration in the source directory to a numbering scheme.
    ///
    /// Filesystem-only: it renames files and never touches the database, so a
    /// database that already applied these migrations is left keyed by the old
    /// versions. Reset it (drop + re-migrate) or `chum force` the new versions
    /// afterward.
    Convert {
        /// Target scheme.
        #[arg(value_enum)]
        to: Scheme,
        /// Show the renames without performing them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Mark a version as applied without running it (clears a dirty state).
    Force {
        /// The version to force.
        version: i64,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cli = Cli::parse();
    cli.color.apply();
    install_error_renderer(cli.color);

    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(report) => {
            // `Report`'s `Debug` impl renders through the handler installed
            // above: the error chain, the `chum::*` code, and the `help` line.
            eprintln!("{report:?}");
            ExitCode::FAILURE
        }
    }
}

/// Install miette's graphical error renderer, honoring `--color`.
///
/// This is the fourth ANSI system in play (after `anstyle`, `console`, and
/// `indicatif`; see [`theme`]). Unlike the others it colorizes *error* output,
/// so we wire `--color` straight into its handler: `always`/`never` force the
/// setting, `auto` leaves miette's own tty/`NO_COLOR` detection in place. The
/// hook can only be installed once; a failure means one already exists, which
/// we ignore.
fn install_error_renderer(color: ColorChoice) {
    let _ = miette::set_hook(Box::new(move |_| {
        let opts = miette::MietteHandlerOpts::new();
        let opts = match color {
            ColorChoice::Always => opts.color(true),
            ColorChoice::Never => opts.color(false),
            ColorChoice::Auto => opts,
        };
        Box::new(opts.build())
    }));
}

async fn run(cli: Cli) -> miette::Result<()> {
    let fmt = cli.format.resolve(cli.json);

    // `add` and `convert` are purely local and need no database connection.
    if let Command::Add { name, sequential } = &cli.command {
        let version = next_version(&cli.source, *sequential)?;
        let (up, down) = source::scaffold(&cli.source, &version, name)?;
        return render_add(&up, &down, fmt);
    }
    if let Command::Convert { to, dry_run } = &cli.command {
        return run_convert(&cli.source, *to, *dry_run, fmt);
    }

    let migrator = Migrator::new(source::from_path(&cli.source)?);
    let client = build_client(&cli)?;
    let bookkeeping = chum::Bookkeeping::new(cli.bookkeeping_database.clone(), cli.table.clone())?;
    let bookkeeping = &bookkeeping;

    match cli.command {
        Command::Run { target, dry_run } => {
            if dry_run {
                let planned = migrator.plan(&client, bookkeeping, target).await?;
                render_plan(&planned, fmt)?;
            } else {
                let pb = spinner("Applying pending migrations…");
                let applied = migrator
                    .run_with(&client, bookkeeping, target, |event| {
                        if let Progress::ApplyStarted {
                            version,
                            description,
                        } = event
                        {
                            pb.set_message(format!("Applying {version} {description}…"));
                        }
                    })
                    .await?;
                pb.finish_and_clear();
                render_applied(&applied, fmt)?;
            }
        }
        Command::Revert {
            target,
            steps,
            dry_run,
            yes,
        } => {
            let target =
                resolve_revert_target(&migrator, &client, bookkeeping, target, steps).await?;
            let versions = migrator.revert_plan(&client, bookkeeping, target).await?;
            if versions.is_empty() {
                render_revert(&[], fmt, RevertPhase::Done)?;
            } else if dry_run {
                render_revert(&versions, fmt, RevertPhase::Plan)?;
            } else {
                // Show the plan before prompting, when interactive.
                if !yes && fmt == Resolved::Pretty {
                    render_revert(&versions, fmt, RevertPhase::Plan)?;
                }
                if confirm(
                    &format!("Revert {}?", count(versions.len(), "migration")),
                    yes,
                    fmt,
                )? {
                    let pb = spinner("Reverting migrations…");
                    let reverted = migrator
                        .undo_with(&client, bookkeeping, target, |event| {
                            if let Progress::RevertStarted { version } = event {
                                pb.set_message(format!("Reverting {version}…"));
                            }
                        })
                        .await?;
                    pb.finish_and_clear();
                    render_revert(&reverted, fmt, RevertPhase::Done)?;
                } else if fmt == Resolved::Pretty {
                    println!("{}", theme::muted().apply_to("aborted"));
                }
            }
        }
        Command::Info => {
            let pb = spinner("Reading migration state…");
            let statuses = migrator.info(&client, bookkeeping).await?;
            pb.finish_and_clear();
            render_info(&statuses, fmt)?;
        }
        Command::Force { version } => {
            let pb = spinner(&format!("Forcing {version} as applied…"));
            migrator.force(&client, bookkeeping, version).await?;
            pb.finish_and_clear();
            render_force(version, fmt)?;
        }
        Command::Add { .. } | Command::Convert { .. } => unreachable!("handled above"),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Per-command renderers. Each takes the resolved [`Format`] and the data the
// library returned. Pretty output uses the `theme` palette; JSON and TSV use
// the CLI-side `*Out` mapping structs so the library stays serde-free and the
// machine-output field names are a stable CLI contract.
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct StatusOut {
    version: i64,
    description: String,
    state: &'static str,
    applied_at: Option<String>,
    execution_ms: Option<u64>,
}

#[derive(Serialize)]
struct AppliedOut {
    version: i64,
    description: String,
    execution_ms: u128,
}

#[derive(Serialize)]
struct PlanOut {
    version: i64,
    description: String,
}

#[derive(Serialize)]
struct VersionOut {
    version: i64,
}

#[derive(Serialize)]
struct AddOut {
    up: String,
    down: String,
}

#[derive(Serialize)]
struct ForceOut {
    version: i64,
    forced: bool,
}

#[derive(Serialize)]
struct ConvertOut {
    from: String,
    to: String,
}

/// Whether a revert version list is a plan ("would revert") or a result
/// ("reverted"). Only affects pretty output.
#[derive(Clone, Copy, PartialEq, Eq)]
enum RevertPhase {
    Plan,
    Done,
}

/// The machine-output name for a state.
fn state_str(state: State) -> &'static str {
    match state {
        State::Applied => "applied",
        State::Pending => "pending",
        State::Dirty => "dirty",
    }
}

fn render_info(statuses: &[chum::Status], fmt: Resolved) -> miette::Result<()> {
    match fmt {
        Resolved::Pretty => {
            if statuses.is_empty() {
                println!("{}", theme::muted().apply_to("no migrations found"));
                return Ok(());
            }
            let mut builder = TableBuilder::new();
            builder.push_record(header_row([
                "Version",
                "Description",
                "State",
                "Applied At",
                "Took",
            ]));
            let (mut applied, mut pending, mut dirty) = (0u32, 0u32, 0u32);
            for s in statuses {
                let state = match s.state {
                    State::Applied => {
                        applied += 1;
                        theme::applied().apply_to("applied")
                    }
                    State::Pending => {
                        pending += 1;
                        theme::pending().apply_to("pending")
                    }
                    State::Dirty => {
                        dirty += 1;
                        theme::dirty().apply_to("DIRTY")
                    }
                };
                let when = s.applied_at.map(human_time).unwrap_or_default();
                let took = s
                    .execution_ms
                    .map_or_else(String::new, |ms| format!("{ms} ms"));
                builder.push_record([
                    theme::version().apply_to(s.version).to_string(),
                    s.description.clone(),
                    state.to_string(),
                    theme::muted().apply_to(when).to_string(),
                    theme::muted().apply_to(took).to_string(),
                ]);
            }
            print_table(builder);
            println!(
                "{}",
                theme::muted().apply_to(format!(
                    "{applied} applied · {pending} pending · {dirty} dirty"
                ))
            );
        }
        Resolved::Json => {
            let out: Vec<StatusOut> = statuses
                .iter()
                .map(|s| StatusOut {
                    version: s.version,
                    description: s.description.clone(),
                    state: state_str(s.state),
                    applied_at: s.applied_at.map(rfc3339),
                    execution_ms: s.execution_ms,
                })
                .collect();
            print_json(&out)?;
        }
        Resolved::Tsv => {
            let rows: Vec<Vec<String>> = statuses
                .iter()
                .map(|s| {
                    vec![
                        s.version.to_string(),
                        s.description.clone(),
                        state_str(s.state).to_string(),
                        s.applied_at.map(rfc3339).unwrap_or_default(),
                        s.execution_ms.map(|ms| ms.to_string()).unwrap_or_default(),
                    ]
                })
                .collect();
            print_tsv(
                &[
                    "version",
                    "description",
                    "state",
                    "applied_at",
                    "execution_ms",
                ],
                &rows,
            );
        }
    }
    Ok(())
}

fn render_applied(applied: &[chum::Applied], fmt: Resolved) -> miette::Result<()> {
    match fmt {
        Resolved::Pretty => {
            if applied.is_empty() {
                println!("{} no pending migrations; database is up to date", check());
                return Ok(());
            }
            let mut builder = TableBuilder::new();
            builder.push_record(header_row(["Version", "Description", "Took"]));
            let mut total_ms = 0u128;
            for a in applied {
                total_ms += a.elapsed.as_millis();
                builder.push_record([
                    theme::version().apply_to(a.version).to_string(),
                    theme::applied().apply_to(&a.description).to_string(),
                    theme::muted()
                        .apply_to(format!("{} ms", a.elapsed.as_millis()))
                        .to_string(),
                ]);
            }
            print_table(builder);
            println!(
                "{} applied {} in {}",
                check(),
                theme::applied().apply_to(count(applied.len(), "migration")),
                theme::muted().apply_to(format!("{total_ms} ms")),
            );
        }
        Resolved::Json => {
            let out: Vec<AppliedOut> = applied
                .iter()
                .map(|a| AppliedOut {
                    version: a.version,
                    description: a.description.clone(),
                    execution_ms: a.elapsed.as_millis(),
                })
                .collect();
            print_json(&out)?;
        }
        Resolved::Tsv => {
            let rows: Vec<Vec<String>> = applied
                .iter()
                .map(|a| {
                    vec![
                        a.version.to_string(),
                        a.description.clone(),
                        a.elapsed.as_millis().to_string(),
                    ]
                })
                .collect();
            print_tsv(&["version", "description", "execution_ms"], &rows);
        }
    }
    Ok(())
}

fn render_plan(planned: &[chum::Planned], fmt: Resolved) -> miette::Result<()> {
    match fmt {
        Resolved::Pretty => {
            if planned.is_empty() {
                println!("{} no pending migrations; database is up to date", check());
                return Ok(());
            }
            let mut builder = TableBuilder::new();
            builder.push_record(header_row(["Version", "Description"]));
            for p in planned {
                builder.push_record([
                    theme::version().apply_to(p.version).to_string(),
                    theme::pending().apply_to(&p.description).to_string(),
                ]);
            }
            print_table(builder);
            println!(
                "{}",
                theme::muted().apply_to(format!(
                    "dry run — would apply {}",
                    count(planned.len(), "migration")
                ))
            );
        }
        Resolved::Json => {
            let out: Vec<PlanOut> = planned
                .iter()
                .map(|p| PlanOut {
                    version: p.version,
                    description: p.description.clone(),
                })
                .collect();
            print_json(&out)?;
        }
        Resolved::Tsv => {
            let rows: Vec<Vec<String>> = planned
                .iter()
                .map(|p| vec![p.version.to_string(), p.description.clone()])
                .collect();
            print_tsv(&["version", "description"], &rows);
        }
    }
    Ok(())
}

fn render_revert(versions: &[i64], fmt: Resolved, phase: RevertPhase) -> miette::Result<()> {
    match fmt {
        Resolved::Pretty => {
            if versions.is_empty() {
                println!("{}", theme::muted().apply_to("nothing to revert"));
                return Ok(());
            }
            match phase {
                RevertPhase::Done => {
                    for v in versions {
                        println!("{} reverted {}", check(), theme::version().apply_to(v));
                    }
                    println!(
                        "{} reverted {}",
                        check(),
                        theme::applied().apply_to(count(versions.len(), "migration")),
                    );
                }
                RevertPhase::Plan => {
                    let mut builder = TableBuilder::new();
                    builder.push_record(header_row(["Version"]));
                    for v in versions {
                        builder.push_record([theme::version().apply_to(v).to_string()]);
                    }
                    print_table(builder);
                    println!(
                        "{}",
                        theme::muted().apply_to(format!(
                            "dry run — would revert {}",
                            count(versions.len(), "migration")
                        ))
                    );
                }
            }
        }
        Resolved::Json => {
            let out: Vec<VersionOut> = versions
                .iter()
                .map(|v| VersionOut { version: *v })
                .collect();
            print_json(&out)?;
        }
        Resolved::Tsv => {
            let rows: Vec<Vec<String>> = versions.iter().map(|v| vec![v.to_string()]).collect();
            print_tsv(&["version"], &rows);
        }
    }
    Ok(())
}

fn render_add(up: &std::path::Path, down: &std::path::Path, fmt: Resolved) -> miette::Result<()> {
    let up = up.display().to_string();
    let down = down.display().to_string();
    match fmt {
        Resolved::Pretty => {
            println!("{} created {}", check(), theme::version().apply_to(&up));
            println!("{} created {}", check(), theme::version().apply_to(&down));
        }
        Resolved::Json => print_json(&AddOut { up, down })?,
        Resolved::Tsv => print_tsv(&["up", "down"], &[vec![up, down]]),
    }
    Ok(())
}

/// The basename of a path, for compact display (falls back to the full path).
fn basename(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

fn render_convert(
    plan: &[(PathBuf, PathBuf)],
    to: Scheme,
    dry_run: bool,
    fmt: Resolved,
) -> miette::Result<()> {
    let rows: Vec<(String, String)> = plan
        .iter()
        .map(|(from, target)| (basename(from), basename(target)))
        .collect();
    match fmt {
        Resolved::Pretty => {
            if rows.is_empty() {
                println!(
                    "{} nothing to do — migrations already use the {} scheme",
                    check(),
                    to.label()
                );
                return Ok(());
            }
            let mut builder = TableBuilder::new();
            builder.push_record(header_row(["From", "To"]));
            for (from, target) in &rows {
                builder.push_record([
                    theme::muted().apply_to(from).to_string(),
                    theme::version().apply_to(target).to_string(),
                ]);
            }
            print_table(builder);
            let verb = if dry_run {
                "would convert"
            } else {
                "converted"
            };
            println!(
                "{}",
                theme::muted().apply_to(format!(
                    "{verb} {} to the {} scheme",
                    count(rows.len(), "migration file"),
                    to.label()
                ))
            );
        }
        Resolved::Json => {
            let out: Vec<ConvertOut> = rows
                .iter()
                .map(|(from, target)| ConvertOut {
                    from: from.clone(),
                    to: target.clone(),
                })
                .collect();
            print_json(&out)?;
        }
        Resolved::Tsv => {
            let tsv: Vec<Vec<String>> = rows
                .iter()
                .map(|(from, target)| vec![from.clone(), target.clone()])
                .collect();
            print_tsv(&["from", "to"], &tsv);
        }
    }
    Ok(())
}

fn render_force(version: i64, fmt: Resolved) -> miette::Result<()> {
    match fmt {
        Resolved::Pretty => println!(
            "{} forced {} as applied",
            check(),
            theme::version().apply_to(version),
        ),
        Resolved::Json => print_json(&ForceOut {
            version,
            forced: true,
        })?,
        Resolved::Tsv => print_tsv(
            &["version", "forced"],
            &[vec![version.to_string(), "true".into()]],
        ),
    }
    Ok(())
}

/// Build a [`clickhouse::Client`] from the connection URL and overrides.
///
/// Maps a DSN to the HTTP-only `clickhouse` crate:
/// - userinfo / `user`,`username`,`password` query params → credentials;
/// - first path segment / `database`,`db` query param → session default
///   database (see below);
/// - scheme containing `https` or a truthy `secure` query param → TLS;
/// - `x-*` query params (golang-migrate driver params) are ignored;
/// - any other query param → a ClickHouse setting.
///
/// `--user` / `--password` / `--database` override the URL.
///
/// # Session default database
///
/// The session default database is pinned **only when explicitly provided**
/// (via `--database`, the URL path, or a `database`/`db` query param). When
/// none is given, the client is left on the `clickhouse` crate's built-in
/// `default` database — which always exists — rather than a possibly-missing
/// app database. This is deliberate: chum's bookkeeping is fully-qualified to
/// its own database (`--bookkeeping-database`), so it never needs an app
/// database, and leaving the session default unpinned lets a migration
/// bootstrap its own database with fully-qualified DDL (`CREATE DATABASE app;
/// CREATE TABLE app.t …`). ClickHouse rejects any request whose session default
/// database is absent, so a `--database` that is set **must already exist**.
///
/// # No connection config is read from the environment
///
/// chum reads **no `CLICKHOUSE_*` env vars at all** — not `CLICKHOUSE_URL`,
/// `CLICKHOUSE_USER`, `CLICKHOUSE_PASSWORD`, nor `CLICKHOUSE_DATABASE`. The
/// connection target is supplied by an explicit `--url` (which accepts a full
/// DSN carrying user / password / database via userinfo + path + query) plus
/// the explicit `--user` / `--password` / `--database` override flags. When
/// `--url` is omitted it defaults to `http://localhost:8123`, so a bare `chum` targets
/// localhost — never an ambient cluster. A migrator that silently connected to
/// whatever target the environment happened to point at is how a stray
/// invocation ends up on the wrong (or prod) cluster; requiring an explicit
/// `--url` prevents that. Only `CHUM_TABLE` / `CHUM_BOOKKEEPING_DATABASE`
/// remain env-readable — they are `CHUM_*` tool config, not a connection
/// target.
fn build_client(cli: &Cli) -> miette::Result<clickhouse::Client> {
    let parsed = Url::parse(&cli.url)
        .into_diagnostic()
        .with_context(|| format!("invalid --url {:?}", cli.url))?;

    let host = parsed
        .host_str()
        .ok_or_else(|| miette!("connection URL {:?} has no host", cli.url))?;

    let mut secure = parsed.scheme().to_ascii_lowercase().contains("https");
    let mut user_q = None;
    let mut password_q = None;
    let mut database_q = None;
    let mut settings: Vec<(String, String)> = Vec::new();
    for (key, value) in parsed.query_pairs() {
        match key.as_ref().to_ascii_lowercase().as_str() {
            "user" | "username" => user_q = Some(value.into_owned()),
            "password" => password_q = Some(value.into_owned()),
            "database" | "db" => database_q = Some(value.into_owned()),
            "secure" => secure = matches!(value.as_ref(), "true" | "1" | "yes"),
            other if other.starts_with("x-") => {} // driver params; not CH settings
            _ => settings.push((key.into_owned(), value.into_owned())),
        }
    }

    // Credentials and database: explicit override > URL userinfo/path > query.
    let userinfo_user = (!parsed.username().is_empty()).then(|| decode(parsed.username()));
    let userinfo_password = parsed.password().map(decode);
    let path_database = {
        let trimmed = parsed.path().trim_matches('/');
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    };

    let user = cli.user.clone().or(userinfo_user).or(user_q);
    let password = cli.password.clone().or(userinfo_password).or(password_q);
    let database = cli.database.clone().or(path_database).or(database_q);

    let scheme = if secure { "https" } else { "http" };
    let endpoint = match parsed.port() {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    };

    // Consistency bundle (opt-in via `--strict-consistency`): make chum's
    // bookkeeping reads/writes as "sync" as possible on a managed/Replicated
    // ClickHouse, where a later command may be routed to a different replica
    // than an earlier one. Applied client-wide (so user-migration statements get
    // them too, which is harmless-to-desirable for a migrator) and *before* the
    // URL-derived settings loop below, so a `?setting=value` query param can
    // still override any of them. All six are sent as URL query params by the
    // `clickhouse` crate.
    //
    // OFF BY DEFAULT. When first exercised against the Aiven cluster, this
    // bundle hung *every* bookkeeping `INSERT`: the part is written and
    // readable, but the query never returns and blocks until
    // `insert_quorum_timeout` (600s). The `insert_quorum` settings are the
    // cause (dropping them returns instantly), but the precise interaction was
    // not isolated on the live cluster — `insert_quorum=auto` hangs even alone,
    // yet `insert_quorum=auto` + `insert_quorum_parallel=0` returned instantly
    // in isolation while the full bundle (which includes `parallel=0`) hung. So
    // re-enabling a *subset* is not known-safe. The bundle is gated behind
    // `--strict-consistency` so the default `migrate` works on the managed
    // cluster; the cross-replica guarantee is available where it is known to
    // work and actually needed.
    //
    // A — linearizable apply/read. The three travel together:
    // `select_sequential_consistency` only holds when inserts use non-parallel
    // quorum, so `insert_quorum_parallel=0` is a precondition, not an option.
    //   insert_quorum=auto                INSERT waits for a majority of replicas
    //   insert_quorum_parallel=0          makes quorum sequential
    //   select_sequential_consistency=1   SELECT refuses to read behind it
    // B — linearizable revert. `DELETE FROM` is a lightweight delete; sync=2
    // waits for all replicas. (Already the server default on recent ClickHouse;
    // pinned here against default drift on the managed cluster.)
    // C — defensive pins: never buffer the bookkeeping insert, and don't return
    // the HTTP response until the insert is fully committed.
    let mut client = clickhouse::Client::default().with_url(endpoint);
    if cli.strict_consistency {
        client = client
            .with_setting("insert_quorum", "auto")
            .with_setting("insert_quorum_parallel", "0")
            .with_setting("select_sequential_consistency", "1")
            .with_setting("lightweight_deletes_sync", "2")
            .with_setting("async_insert", "0")
            .with_setting("wait_end_of_query", "1");
    }
    if let Some(database) = &database {
        client = client.with_database(database);
    }
    if let Some(user) = &user {
        client = client.with_user(user);
    }
    if let Some(password) = &password {
        client = client.with_password(password);
    }
    for (key, value) in settings {
        client = client.with_setting(key, value);
    }
    Ok(client)
}

/// Percent-decode a URL component (userinfo is returned still-encoded by the
/// `url` crate; query pairs are already decoded).
fn decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// Translate revert flags into a target version. With neither flag, reverts a
/// single (the most recent) migration.
async fn resolve_revert_target(
    migrator: &Migrator,
    client: &clickhouse::Client,
    bookkeeping: &chum::Bookkeeping,
    target: Option<i64>,
    steps: Option<usize>,
) -> miette::Result<i64> {
    if let Some(target) = target {
        return Ok(target);
    }
    let steps = steps.unwrap_or(1);
    let mut applied: Vec<i64> = migrator
        .info(client, bookkeeping)
        .await?
        .into_iter()
        .filter(|s| s.state == State::Applied)
        .map(|s| s.version)
        .collect();
    applied.sort_unstable();
    if steps >= applied.len() {
        Ok(i64::MIN)
    } else {
        Ok(applied[applied.len() - steps - 1])
    }
}

/// A `YYYYMMDDHHMMSS` UTC version prefix for a newly scaffolded migration.
fn now_version() -> String {
    chrono::Utc::now().format("%Y%m%d%H%M%S").to_string()
}

/// At or above this, a version is a `YYYYMMDDHHMMSS` UTC timestamp; below it,
/// it is a sequential counter. A sequential scheme would need ~10^12 migrations
/// to reach the floor, and the earliest plausible timestamp
/// (`10000101000000`) sits well above it, so the two schemes never collide.
const TIMESTAMP_FLOOR: i64 = 1_000_000_000_000;

/// The minimum zero-padding width for a sequential version (`0001`).
const SEQUENTIAL_WIDTH: usize = 4;

/// The version prefix for the next migration in `dir`.
///
/// chum auto-detects the scheme already in use and continues it: an existing
/// sequential directory advances `0001` → `0002`; an existing timestamp
/// directory gets a fresh [`now_version`]. The `sequential` flag only matters
/// for a fresh (empty or missing) directory, where it chooses sequential over
/// the default timestamp.
fn next_version(dir: &Path, sequential: bool) -> miette::Result<String> {
    let max = source::max_version(dir)?;
    Ok(decide_version(max, sequential)?.unwrap_or_else(now_version))
}

/// Decide a new migration's version from the directory's current max version.
///
/// `Ok(None)` means "use a fresh timestamp" — the caller fills in
/// [`now_version`], which is non-deterministic and kept out of this pure,
/// testable decision. `Ok(Some(v))` is an explicit sequential version.
///
/// # Errors
///
/// Errors when `sequential` is requested but the directory already uses
/// timestamps, since the two schemes share one numeric ordering space and
/// grafting a small counter after a timestamp would sort it *before* every
/// existing migration.
fn decide_version(max: Option<(i64, usize)>, sequential: bool) -> miette::Result<Option<String>> {
    Ok(match max {
        // Fresh directory: the flag picks the scheme (timestamp by default).
        None => sequential.then(|| format!("{:0SEQUENTIAL_WIDTH$}", 1)),
        // Existing timestamp scheme: continue it; --sequential can't graft on.
        Some((m, _)) if m >= TIMESTAMP_FLOOR => {
            if sequential {
                bail!(
                    "existing migrations use timestamp versions (latest {m}); --sequential cannot \
                     continue them. Run `chum convert sequential` to renumber the directory \
                     first, or drop --sequential."
                );
            }
            None
        }
        // Existing sequential scheme: continue it, keeping its padding width
        // (never narrower than the default, so an over-rolled `10000` still
        // sorts correctly even though `ls` would order it lexically).
        Some((m, width)) => {
            let width = width.max(SEQUENTIAL_WIDTH);
            Some(format!("{next:0width$}", next = m + 1))
        }
    })
}

/// Renumber every migration in `dir` to the `to` scheme by renaming files.
/// Filesystem-only — see [`Command::Convert`].
fn run_convert(dir: &Path, to: Scheme, dry_run: bool, fmt: Resolved) -> miette::Result<()> {
    let files = source::migration_files(dir)?;
    if files.is_empty() {
        if fmt == Resolved::Pretty {
            println!("{} no migrations found in {}", check(), dir.display());
        }
        return Ok(());
    }

    // Distinct versions in ascending order (`migration_files` is sorted, so
    // the up/down duplicates of each version are adjacent).
    let mut versions: Vec<i64> = files.iter().map(|f| f.version).collect();
    versions.dedup();

    // Already wholly in the target scheme → converting would only churn (and,
    // for timestamps, re-stamp to fresh values), so render an empty plan.
    if versions.iter().all(|&v| scheme_of(v) == to) {
        return render_convert(&[], to, dry_run, fmt);
    }

    let new_versions = match to {
        Scheme::Sequential => sequential_versions(versions.len()),
        Scheme::Timestamp => synthesize_timestamps(versions.len(), chrono::Utc::now()),
    };
    let map: HashMap<i64, String> = versions.iter().copied().zip(new_versions).collect();

    let mut plan: Vec<(PathBuf, PathBuf)> = Vec::new();
    for f in &files {
        let prefix = &map[&f.version];
        let target = dir.join(format!("{prefix}_{}{}", f.name, f.direction.suffix()));
        if target != f.path {
            plan.push((f.path.clone(), target));
        }
    }
    if plan.is_empty() {
        return render_convert(&[], to, dry_run, fmt);
    }

    preflight(&plan)?;
    if !dry_run {
        rename_all(&plan)?;
    }
    render_convert(&plan, to, dry_run, fmt)
}

/// Canonical sequential versions `0001..=n`, zero-padded wide enough for `n`
/// (never narrower than [`SEQUENTIAL_WIDTH`]).
fn sequential_versions(n: usize) -> Vec<String> {
    let width = SEQUENTIAL_WIDTH.max(n.to_string().len());
    (1..=n).map(|i| format!("{i:0width$}")).collect()
}

/// Synthesize `n` distinct `YYYYMMDDHHMMSS` versions, one second apart and in
/// order, the latest landing one second *before* `base`.
///
/// Ordering comes from the caller (the existing filename sequence), never from
/// file metadata — no filesystem timestamp survives a checkout/copy intact, so
/// it cannot be trusted to reflect migration order. Keeping the latest strictly
/// before `base` means a later timestamped `add` stays monotonically greater.
/// `base` is injected (not read from the clock here) to keep this deterministic
/// and unit-testable.
fn synthesize_timestamps(n: usize, base: chrono::DateTime<chrono::Utc>) -> Vec<String> {
    (0..n)
        .map(|i| {
            let secs_before = i64::try_from(n - i).expect("migration count fits in i64");
            (base - chrono::Duration::seconds(secs_before))
                .format("%Y%m%d%H%M%S")
                .to_string()
        })
        .collect()
}

/// Fail before renaming anything if the plan maps two files onto one name, or
/// would clobber an existing file it does not itself manage. Both are
/// structurally impossible for the built-in conversions, but the guard keeps a
/// future mistake from silently destroying a file.
fn preflight(plan: &[(PathBuf, PathBuf)]) -> miette::Result<()> {
    let sources: HashSet<&PathBuf> = plan.iter().map(|(from, _)| from).collect();
    let mut targets: HashSet<&PathBuf> = HashSet::new();
    for (_, target) in plan {
        if !targets.insert(target) {
            bail!("rename plan maps two files onto {}", target.display());
        }
        // A target may legitimately equal a *source* — it will be vacated by
        // the two-phase rename; only an unmanaged existing file is a hazard.
        if target.exists() && !sources.contains(target) {
            bail!("refusing to overwrite existing file {}", target.display());
        }
    }
    Ok(())
}

/// Apply the rename plan in two phases so an output name colliding with a
/// not-yet-renamed input cannot clobber it: first move every source aside to a
/// unique temporary, then move each temporary to its final name.
fn rename_all(plan: &[(PathBuf, PathBuf)]) -> miette::Result<()> {
    let mut temps: Vec<(PathBuf, &PathBuf)> = Vec::with_capacity(plan.len());
    for (from, target) in plan {
        let tmp = with_suffix(from, ".chum-convert.tmp");
        std::fs::rename(from, &tmp)
            .into_diagnostic()
            .with_context(|| format!("renaming {} aside", from.display()))?;
        temps.push((tmp, target));
    }
    for (tmp, target) in &temps {
        std::fs::rename(tmp, target)
            .into_diagnostic()
            .with_context(|| format!("renaming {} -> {}", tmp.display(), target.display()))?;
    }
    Ok(())
}

/// Append a suffix to a path's filename (the suffix deliberately does not end
/// in `.up.sql`/`.down.sql`, so the temporary is never picked up as a
/// migration).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_flag_wins_over_format() {
        // The `--json` shorthand resolves to JSON regardless of `--format`.
        assert_eq!(Format::Pretty.resolve(true), Resolved::Json);
        assert_eq!(Format::Tsv.resolve(true), Resolved::Json);
        assert_eq!(Format::Auto.resolve(true), Resolved::Json);
    }

    #[test]
    fn explicit_formats_resolve_directly() {
        assert_eq!(Format::Pretty.resolve(false), Resolved::Pretty);
        assert_eq!(Format::Json.resolve(false), Resolved::Json);
        assert_eq!(Format::Tsv.resolve(false), Resolved::Tsv);
    }

    #[test]
    fn count_pluralises_on_count() {
        assert_eq!(count(1, "migration"), "1 migration");
        assert_eq!(count(0, "migration"), "0 migrations");
        assert_eq!(count(3, "migration"), "3 migrations");
    }

    #[test]
    fn state_str_matches_the_machine_names() {
        assert_eq!(state_str(State::Applied), "applied");
        assert_eq!(state_str(State::Pending), "pending");
        assert_eq!(state_str(State::Dirty), "dirty");
    }

    #[test]
    fn cli_parses_global_presentation_flags() {
        // Smoke-test that the clap definition is well-formed (value enums,
        // globals, the revert-scoped `--yes`).
        use clap::Parser as _;
        let cli = Cli::try_parse_from([
            "chum", "--json", "--color", "never", "revert", "--steps", "2", "--yes",
        ])
        .expect("valid args parse");
        assert!(cli.json);
        assert_eq!(cli.color, ColorChoice::Never);
        match cli.command {
            Command::Revert { steps, yes, .. } => {
                assert_eq!(steps, Some(2));
                assert!(yes);
            }
            _ => panic!("expected revert"),
        }
    }

    #[test]
    fn fresh_dir_defaults_to_timestamp_unless_sequential() {
        // No flag on an empty dir → timestamp (None signals `now_version`).
        assert_eq!(decide_version(None, false).unwrap(), None);
        // --sequential on an empty dir → the first sequential version.
        assert_eq!(decide_version(None, true).unwrap().as_deref(), Some("0001"));
    }

    #[test]
    fn sequential_scheme_is_continued_and_auto_detected() {
        // Continues regardless of whether --sequential is repeated.
        assert_eq!(
            decide_version(Some((1, 4)), false).unwrap().as_deref(),
            Some("0002")
        );
        assert_eq!(
            decide_version(Some((1, 4)), true).unwrap().as_deref(),
            Some("0002")
        );
        // Width never narrows below the default, and rolls past it cleanly.
        assert_eq!(
            decide_version(Some((9999, 4)), false).unwrap().as_deref(),
            Some("10000")
        );
        // A wider existing scheme keeps its padding.
        assert_eq!(
            decide_version(Some((41, 6)), false).unwrap().as_deref(),
            Some("000042")
        );
    }

    #[test]
    fn timestamp_scheme_is_continued_and_rejects_sequential() {
        // Without the flag, continue with a fresh timestamp.
        assert_eq!(
            decide_version(Some((20_260_626_111_407, 14)), false).unwrap(),
            None
        );
        // With the flag, refuse rather than emit a 14-digit "sequential" number.
        assert!(decide_version(Some((20_260_626_111_407, 14)), true).is_err());
    }

    #[test]
    fn scheme_of_splits_on_the_floor() {
        assert_eq!(scheme_of(1), Scheme::Sequential);
        assert_eq!(scheme_of(9999), Scheme::Sequential);
        assert_eq!(scheme_of(20_260_628_120_000), Scheme::Timestamp);
    }

    #[test]
    fn sequential_versions_pad_to_fit_count() {
        assert_eq!(sequential_versions(3), ["0001", "0002", "0003"]);
        // Widens past the default when the count needs more digits.
        let v = sequential_versions(10_000);
        assert_eq!(v.first().unwrap(), "00001");
        assert_eq!(v.last().unwrap(), "10000");
    }

    #[test]
    fn synthesized_timestamps_are_ordered_and_before_base() {
        let base: chrono::DateTime<chrono::Utc> = "2026-06-28T12:00:10Z".parse().unwrap();
        let v = synthesize_timestamps(3, base);
        // One second apart, latest one second before the base.
        assert_eq!(v, ["20260628120007", "20260628120008", "20260628120009"]);
        assert!(v.windows(2).all(|w| w[0] < w[1]));
        assert!(v.iter().all(|s| s.as_str() < "20260628120010"));
    }

    #[test]
    fn cli_parses_bookkeeping_database() {
        use clap::Parser as _;
        // Defaults to `_chum` when the flag is absent.
        let cli = Cli::try_parse_from(["chum", "info"]).expect("valid args parse");
        assert_eq!(cli.bookkeeping_database, chum::DEFAULT_BOOKKEEPING_DATABASE);
        assert_eq!(cli.bookkeeping_database, "_chum");
        // `--bookkeeping-database` overrides the default.
        let cli = Cli::try_parse_from(["chum", "--bookkeeping-database", "custom", "info"])
            .expect("valid args parse");
        assert_eq!(cli.bookkeeping_database, "custom");
    }

    #[test]
    fn connection_config_is_not_read_from_the_environment() {
        use clap::Parser as _;

        // Save and set all four `CLICKHOUSE_*` connection env vars. An
        // application sharing chum's `.env` legitimately sets these; chum reads
        // NONE of them. The four flags have no `env` binding, so no other code
        // path (or parallel test) reads them — the process-global mutation is
        // inert elsewhere; still, restore the ambient environment on the way out.
        //
        // SAFETY: the vars are not read by any concurrently-running thread (no
        // clap `env` binding references them), so set/remove here is sound.
        let vars = [
            "CLICKHOUSE_URL",
            "CLICKHOUSE_DATABASE",
            "CLICKHOUSE_USER",
            "CLICKHOUSE_PASSWORD",
        ];
        let saved = vars.map(|k| (k, std::env::var_os(k)));
        unsafe {
            std::env::set_var("CLICKHOUSE_URL", "http://ambient-host-from-env:9999");
            std::env::set_var("CLICKHOUSE_DATABASE", "appdb_from_env");
            std::env::set_var("CLICKHOUSE_USER", "user_from_env");
            std::env::set_var("CLICKHOUSE_PASSWORD", "password_from_env");
        }

        // Nothing is inherited: `--url` falls back to the localhost default (not
        // the ambient env value), and user/password/database resolve to None.
        let cli = Cli::try_parse_from(["chum", "info"]).expect("valid args parse");
        assert_eq!(
            cli.url, "http://localhost:8123",
            "CLICKHOUSE_URL must not be inherited; --url uses its localhost default"
        );
        assert_eq!(
            cli.database, None,
            "CLICKHOUSE_DATABASE must not be inherited"
        );
        assert_eq!(cli.user, None, "CLICKHOUSE_USER must not be inherited");
        assert_eq!(
            cli.password, None,
            "CLICKHOUSE_PASSWORD must not be inherited"
        );

        // Explicit flags still take effect.
        let cli = Cli::try_parse_from([
            "chum",
            "--url",
            "http://explicit-host:8123",
            "--database",
            "explicit_db",
            "--user",
            "explicit_user",
            "--password",
            "explicit_password",
            "info",
        ])
        .expect("valid args parse");
        assert_eq!(cli.url, "http://explicit-host:8123");
        assert_eq!(cli.database.as_deref(), Some("explicit_db"));
        assert_eq!(cli.user.as_deref(), Some("explicit_user"));
        assert_eq!(cli.password.as_deref(), Some("explicit_password"));

        // A full DSN passed via an explicit `--url` still populates user /
        // password / database. The `clickhouse::Client` exposes no getters, so a
        // successful `build_client` on a DSN carrying all three is the observable
        // contract that the DSN is still parsed.
        let cli = Cli::try_parse_from([
            "chum",
            "--url",
            "clickhouse+https://dsn_user:dsn_pass@localhost:8123/dsn_db?secure=true",
            "info",
        ])
        .expect("valid args parse");
        // The flags themselves stay None — the identity lives in the DSN, which
        // build_client resolves.
        assert_eq!(cli.user, None);
        assert_eq!(cli.password, None);
        assert_eq!(cli.database, None);
        build_client(&cli).expect("build client from a full DSN");

        // Restore the ambient environment.
        unsafe {
            for (k, v) in saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[test]
    fn cli_parses_convert() {
        use clap::Parser as _;
        let cli = Cli::try_parse_from(["chum", "convert", "sequential", "--dry-run"])
            .expect("valid args parse");
        match cli.command {
            Command::Convert { to, dry_run } => {
                assert_eq!(to, Scheme::Sequential);
                assert!(dry_run);
            }
            _ => panic!("expected convert"),
        }
    }

    #[test]
    fn convert_round_trips_on_disk() {
        use std::fs;

        // Unique per-test subdir; nextest isolates by process and the name is
        // unique under threaded `cargo test`, so no cross-test contention.
        let dir = std::env::temp_dir().join("chum-convert-round-trip");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        for name in [
            "0001_first.up.sql",
            "0001_first.down.sql",
            "0002_second.up.sql",
            "0002_second.down.sql",
        ] {
            fs::write(dir.join(name), "-- x\n").expect("write");
        }

        // sequential → timestamp: every file ends up timestamp-versioned, and
        // the original filename order is preserved (first < second).
        run_convert(&dir, Scheme::Timestamp, false, Resolved::Tsv).expect("to timestamp");
        let files = source::migration_files(&dir).expect("relist");
        assert!(
            files
                .iter()
                .all(|f| scheme_of(f.version) == Scheme::Timestamp)
        );
        let first = files
            .iter()
            .find(|f| f.name == "first")
            .expect("first")
            .version;
        let second = files
            .iter()
            .find(|f| f.name == "second")
            .expect("second")
            .version;
        assert!(first < second, "filename order preserved across conversion");

        // timestamp → sequential: restores the canonical 0001/0002 names.
        run_convert(&dir, Scheme::Sequential, false, Resolved::Tsv).expect("to sequential");
        let mut names: Vec<String> = fs::read_dir(&dir)
            .expect("readdir")
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "0001_first.down.sql",
                "0001_first.up.sql",
                "0002_second.down.sql",
                "0002_second.up.sql",
            ]
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
