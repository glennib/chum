# Templated migrations

**Status:** proposal for review, not implemented.

## Summary

A migration file opts in to [minijinja] templating by ending in `.sql.j2`.
Variables are plain strings that whoever runs chum passes with `--var key=value` or `CHUM_VAR_key=value`.
Templates read them from two namespaces: `env.*` for values describing the target environment
(`--var tier=prod`), and `this.*` for values meant for this migration only (`--var 20261106120000_cutoff=…`).
A template that uses a variable nobody passed fails to render,
and chum renders every migration it is about to run before it executes any of them.
`chum render` shows the rendered SQL without connecting to ClickHouse, so it can be reviewed before anything runs.
The values each migration used are recorded in a new `vars` column of the bookkeeping table,
so `chum info` and jobs outside chum can read back what each environment got, and `chum revert` can reuse them.
Rendering is a pure function of the template and the variables: no clock, no environment lookups, no database queries,
no includes.

## Motivation

Two problems share one mechanism.

1. **Values chosen at apply time, per environment.**
   A materialized view that only takes effect from a future cutoff `T`
   (`WHERE ts >= T`),
   followed by a backfill of `ts < T` once `T` has passed, gives a backfill with no overlap and no gap.
   Staging and production are migrated on different days,
   so `T` differs per environment and cannot be written into the file.
   The backfill runs outside chum, but it needs the exact `T` that environment used.
2. **Statements that belong in some environments only.**
   One migrations directory serves a local ClickHouse and the production cluster,
   and some statements only make sense in production, for example `GRANT`s to roles that exist only there.

Today both mean hand-editing SQL per environment or keeping duplicate files.

## Goals and non-goals

Goals:

- One migrations directory for local, CI, staging and production, including freshly created databases.
- Values that vary per environment or per apply are supplied explicitly by whoever runs chum. chum never infers
  or computes them.
- The SQL an environment will run can be inspected before running it, offline.
- What each environment ran can be read back afterwards.
- Migrations that are not templates behave exactly as today, with unchanged checksums.

Non-goals:

- Running backfills or other data jobs.
  They stay outside chum; chum only records the values they need.
- Waiting for, or gating on, a point in time.
  A cutoff is a value the operator picks.
- Typed variables.
  Values are strings; checking them is the SQL's job (see [Guards](#guards)).
- Secrets.
  Recorded values are stored in plaintext (see [Secrets](#secrets)).
- Sharing template code between files (`include`, `import`, `extends`).
- Clusters that need `ON CLUSTER` for chum's own bookkeeping. chum's bookkeeping table is replicated only by a
  `Replicated` database (see `src/backend.rs`); a classic `ON CLUSTER` setup needs a configurable bookkeeping engine
  first, which templating doesn't provide.

## Design

### Rendering is pure

The rendered SQL depends on the template text and the variables,
and nothing else. chum adds no template functions that read the clock, the process environment,
the filesystem or the database.
That is what makes the recorded variables enough to reproduce what ran, what lets `chum render` work offline,
and it keeps behaviour predictable: knowing the template and the variables means knowing the SQL.

### Template files

A migration is a template if and only if its filename ends in `.sql.j2`:

```text
20261106120000_events_hourly_mv.up.sql.j2
20261106120000_events_hourly_mv.down.sql.j2
```

- Up and down are independent.
  A template up with a plain down is fine.
- A version may not have both `….up.sql` and `….up.sql.j2`; loading such a source is an error.
- Plain `.sql` files are never rendered.
  A `{{` in an existing migration keeps its current meaning, and nothing changes for users who don't opt in.

### Rendering

minijinja is configured as follows:

- **Strict undefined behaviour.**
  Printing an undefined variable, iterating it, comparing it, or testing it with `{% if %}` is an error.
  The opt-outs are explicit: `{{ x | default("…") }}` and `{% if x is defined %}`.
- **No auto-escaping.** The output is SQL, not HTML.
- **Whitespace is kept as written.**
  `trim_blocks` and `lstrip_blocks` stay off: with `trim_blocks`,
  an inline `{% endif %}` at the end of a line swallows the newline,
  so `ENGINE = {% if … %}A{% else %}B{% endif %}` followed by `ORDER BY …` on the next line renders
  as `ENGINE = BORDER BY …`.
  Blank lines left by block tags are harmless in SQL.
- **Engine features `builtins` and `macros` only.**
  Without `multi_template`, `{% include %}`, `{% import %}`, `{% from %}` and `{% extends %}` don't exist,
  so every template is self-contained by construction.
  With these features minijinja has no transitive dependencies.
- **Errors name the file, the line and the missing variable.** minijinja's own message is
  `undefined value (in <file>:<line>)`. chum adds the names the template looked up that had no value, so a line using
  both `this.cutoff` and `env.tier` says which one was missing.

chum adds two filters.
Both take a string argument, so an undefined input is an error rather than an empty literal.

- `sql_string` renders a ClickHouse string literal in single quotes, with `\` and `'` escaped by a backslash:
  `{{ "it's" | sql_string }}` gives `'it\'s'`.
- `sql_ident` renders a backtick-quoted identifier, escaping a backtick by doubling it.
  The whole value is one identifier: `{{ "db.t" | sql_ident }}` gives `` `db.t` ``, not a qualified name.

The rendered text goes through the existing statement splitter, so a template may emit several statements,
including from a loop.

### Supplying variables

Variables come from two sources, which accept the same keys:

- `--var <key>=value`, repeatable, a global flag like `--format`.
- `CHUM_VAR_<key>=value` environment variables, with `<key>` matched exactly, as with Terraform's `TF_VAR_<name>`.
  This is the one `CHUM_*` variable family that clap cannot declare
  (the prefix is dynamic), so the README documents it separately.
  The environment is a first-class source, because chum is often deployed as a container
  (e.g. a Kubernetes Job)
  where the environment is the natural interface: per-environment constants come from a ConfigMap,
  per-release values from the Job spec.

Templates never see user-supplied values at the top level.
They read them from two namespaces, and the form of the key picks the namespace:

- **`env.<name>`** holds values that describe the target environment,
  available to every migration rendered in the command, such as `env.tier`.
  A plain key, `<name>` (e.g. `tier`), supplies one.
- **`this.<name>`** holds values for this migration only, such as `this.cutoff`.
  A version-prefixed key, `<version>_<name>` (e.g. `20261106120000_cutoff`), supplies one for that version only.
  A name can't start with a digit, so the leading digits are unambiguously the version.
  The same spelling works on the command line and in the environment (`CHUM_VAR_20261106120000_cutoff`),
  and it is a valid shell and Kubernetes variable name.

`env.cutoff` and `this.cutoff` are different variables and never compete.
A plain `cutoff` never reaches `this.cutoff`, and a template reading `{{ this.cutoff }}` does not fall back to it.
Both namespaces are always present, empty when nothing was supplied,
so `is defined` and `| default(…)` behave on a missing key exactly as they would on a missing bare name.

`env` does not read the process environment.
It holds the plain keys, whether they came from `--var` or `CHUM_VAR_*`,
so rendering still depends only on the template and the supplied variables.
An unrelated `PATH` or `CLICKHOUSE_URL` never appears in it.

The top level of a template holds only `this`, `env` and `chum`,
which is reserved for future built-in context such as the migration's own version.
Because user-supplied names live inside a namespace, there are no reserved names:
any identifier matching `[A-Za-z_][A-Za-z0-9_]*` works, `range`, `loop` and `global` included,
and a future minijinja built-in can't collide with one.

**The template author decides each variable's kind.**
Whether a value is per-environment or per-apply is decided once, by whoever writes the migration,
by choosing the namespace.
An operator can't override `env.tier` for a single migration unless the template allows it,
e.g. `{{ this.tier | default(env.tier) }}`.
That gives up ad-hoc per-migration overrides, and in exchange a leftover value can't feed the wrong migration:
a `CHUM_VAR_cutoff` left in a Job spec only ever fills `env.cutoff`,
which a template written for a per-apply cutoff doesn't read.

Key rules:

- The version in a key is compared numerically,
  so `0005_x` and `5_x` address the same version and count as the same key.
- Supplying the same key twice from the same source is an error, including `0005_x` and `5_x`,
  or `CHUM_VAR_0005_x` and `CHUM_VAR_5_x`.
- A key with an empty name (`20261106120000_`), a version that doesn't fit in `i64`,
  or a name outside `[A-Za-z_][A-Za-z0-9_]*` is an error, naming the offending `--var` or environment variable.
- `chum convert` renumbers versions, which orphans version-prefixed keys in deploy specs.
  It prints a reminder to rename them.

#### Precedence

When rendering for `run` and `render`, highest first:

1. `--var`
2. `CHUM_VAR_<key>`
3. `| default(…)` in the template

`revert` and `force` render a migration that already has bookkeeping rows, and those rows record the values it ran with.
The two namespaces treat them differently:

- **`this.*`: recorded values win.**
  They describe what was actually applied, and a per-apply value has no other correct source.
  A supplied value that differs from the recorded one is an error, not an override,
  when the migration being rendered reads that name.
  When it doesn't, the supplied value is ignored,
  so a stale version-prefixed key left in a deploy spec can't block a revert or force that never uses it.
  A supplied value for a name that wasn't recorded fills the gap.
- **`env.*`: supplied values win, recorded values fill gaps.**
  They describe the environment as it is now,
  which may have changed since the apply. chum warns when a supplied value differs from the recorded one.

#### Warnings

- A supplied key, from either source, that no template in the source refers to produces a warning.
  This is checked statically across every template, not only the ones rendered in this command,
  so a long-lived environment-wide constant never warns, but a typo such as `CHUM_VAR_teir` does.
- A `--var` that no template rendered in this command uses also warns, since it was most likely meant for this run.

`run --dry-run` lists, per migration, every value it would use and where it came from
(`--var`, environment, recorded, or default), so an unexpected value is visible before anything runs.

### Rendering offline: `chum render`

`chum render` prints the SQL a migration renders to, split into statements, and never connects to ClickHouse.
It is the way to look at what an environment will run before running it, in a terminal, in code review, or as a CI step.

```text
chum render [VERSION]... [--from VERSION] [--down] [--var key=value]...
```

- With versions, it renders those migrations.
  With `--from`, every migration from that version on.
  With neither, every template in the source.
- `--down` renders the down migrations instead.
- Plain migrations are printed as they are, so a range shows everything that would run, not only the templates.
- Each migration is preceded by a header comment naming the file, and the output is valid SQL,
  so it can be piped to `clickhouse client --multiquery` for inspection.
- Variables come from `--var` and `CHUM_VAR_*` only.
  `render` can't see recorded values,
  so `render --down` shows what `revert` would run only when given the values the up migration used.
- It fails exactly as `run` would on missing variables, template errors and split errors,
  so it doubles as a pre-flight check.

Because rendering is pure, `chum render --from <version> --var …` with an environment's values shows the SQL
that environment's next `run` executes, except for which versions are already applied there.
`run --dry-run` answers that part; it connects to read the bookkeeping table.

### When rendering happens

| Command                      | Renders                                         | Variables from                                       |
| ---------------------------- | ----------------------------------------------- | ---------------------------------------------------- |
| `render` (offline)           | the requested migrations                        | `--var`, `CHUM_VAR_*`                                |
| `run`, `run --dry-run`       | every pending template up to `--target`         | `--var`, `CHUM_VAR_*`                                |
| `revert`, `revert --dry-run` | the down template of each version reverted      | supplied and recorded, see [Precedence](#precedence) |
| `force <version>`            | the up template, without executing it           | supplied and recorded, see [Precedence](#precedence) |
| `info`, `add`, `convert`     | nothing                                         | none                                                 |

Within one database, an applied migration's up is never rendered again,
so a cutoff passed to one `run` is not needed by later runs against that database.
A fresh database needs it again; see [Fresh databases](#fresh-databases).

`run` renders and splits all of its pending migrations before executing the first migration statement,
and `revert` does the same with its down migrations.
A missing variable or template error anywhere aborts the command before any migration statement runs and
before any marker is written, and the error lists every migration that failed to render.
(chum's own `CREATE … IF NOT EXISTS` for the bookkeeping database and table still runs first, as today.)
This ordering also helps plain migrations:
today a split error in the third pending migration only surfaces after the first two have been applied.

An automated deploy that reaches a migration needing a per-apply value therefore stops before doing anything.
Whoever owns the migration applies it by hand
(`chum run --target <version> --var …`) or passes the value through the pipeline, and automated runs proceed from there.

### Recording variables

The bookkeeping table gets one column:

```sql
vars Map(String, String)
```

Every marker (`success = false` and `success = true`) records the supplied values the migration uses:

- **From the up migration:** every namespaced name the render actually looked up
  and found a value for. chum wraps `this` and `env` in minijinja objects that log each lookup and enumeration,
  so `env.get("x")`, `env | items` and `{{ this.cutoff | default(…) }}` are all tracked exactly.
- **From the down migration:** every supplied value its template refers to,
  found statically with minijinja's `Template::undeclared_variables(true)`.
  The down isn't rendered at apply time,
  but a revert should be able to fall back on recorded values for names only the down uses.

Values that came from `| default(…)` aren't recorded: they are in the template,
and the template plus the recorded values reproduce the rendered SQL.
Plain migrations record an empty map.

Keys keep their namespace: the column stores `env.tier` and `this.cutoff`, never the key form that supplied them
(`tier`, `20261106120000_cutoff`), so revert's fallback to recorded values maps straight back onto the namespaces.

`chum info` shows the latest recorded values: JSON output gets a `vars` object per version,
the table and TSV a `vars` column.
A job outside chum must read them from an **applied** version only,
because a dirty marker records the values of an attempt that failed:

```bash
chum --url "$URL" --json info \
  | jq -r '.[] | select(.version == 20261106120000 and .state == "applied") | .vars["this.cutoff"] // empty'
```

```sql
SELECT argMax(vars, seq)['this.cutoff'] AS cutoff
FROM _chum._chum_migrations
WHERE version = 20261106120000
GROUP BY version
HAVING argMax(success, seq) AND mapContains(argMax(vars, seq), 'this.cutoff')
```

The SQL form is the better fit for a backfill job.
It needs only `SELECT` on the bookkeeping table,
while `chum info` needs the migrations source and the grants chum itself needs.
Both return nothing, rather than an empty string, when the version isn't applied or has no cutoff,
which is the expected case in a fresh database.

Reverting a version deletes its rows, and its recorded variables with them, as happens to its markers today.

### Upgrading existing bookkeeping tables

chum upgrades its own bookkeeping table automatically.
It already creates the bookkeeping database and table on every command, it is the table's only writer,
and a manual upgrade step would have to be repeated in every environment and would be forgotten in one of them.

The rule that makes this safe: **automatic bookkeeping changes are additive and backward-compatible only.**
A change qualifies if an older chum can still read and write the upgraded table.
Adding `vars` qualifies: chum's inserts and selects name their columns,
so an older chum ignores the new column and its rows get an empty map,
and a newer chum reads rows written before the upgrade as empty maps.
A future change that can't meet the rule
(renaming or retyping a column, changing the sort key)
is a breaking release with an explicit upgrade command, never something chum does on its own.

Mechanics:

- `ensure_table` creates new tables with the column.
- **Only writing commands upgrade.**
  `run`, `revert` and `force` look the column up in `system.columns`
  and run `ALTER TABLE … ADD COLUMN IF NOT EXISTS vars Map(String, String)` when it is missing.
  `IF NOT EXISTS` covers two chum processes upgrading at the same time.
  Read-only commands (`info`, `--dry-run`) never alter the table; against an old table they read `vars` as empty.
  Inspecting production therefore never changes it, and a read-only consumer never waits on distributed DDL.
- chum logs the upgrade at `info` level, so it is visible in deploy logs.
- The upgrade needs `ALTER` on the bookkeeping table, in addition to the `CREATE` chum needs today.
  If it is refused, the error names the exact statement for an administrator to run.
- On a `Replicated` database behind a load balancer,
  the column check and a later insert can reach replicas that haven't applied the `ALTER` yet.
  The first write of a migration is its `success = false` marker,
  so an insert that fails for that reason fails before any migration statement runs, and a retry succeeds.
- No separate schema-version record is needed while every change is additive: the columns present are the version.

### Clearing a failed attempt

A migration can fail before changing anything, most often because a guard rejected its inputs,
and the version is then dirty.
`chum force` is the wrong tool for that: it marks the version applied.
This design adds the missing command:

```text
chum reset <version>
```

`reset` deletes the bookkeeping rows of a **dirty** version, so it reads as pending again,
and the next `run` applies it with whatever values are supplied then.
It refuses applied versions (`revert` handles those) and pending ones (there is nothing to reset).
Like `revert`, it asks for confirmation and needs `--yes` when not interactive.
It doesn't touch the database objects: whoever runs it is asserting that the failed attempt left nothing behind,
or has cleaned up by hand.

`force` becomes stricter alongside it.
It renders the up migration without executing it, with recorded `this.*` values winning as described above,
and records the result as applied.
On a version with no rows (after `convert`, or for a migration applied by hand), there is nothing to fall back on,
so every value the template needs must be supplied.

### Checksums

Unchanged.
The checksum is computed over the file as written, which for a template is the template text,
with the existing normalization.
As a result:

- existing migrations keep their checksums;
- staging and production record the same checksum for a template even though they ran different SQL;
  the recorded variables account for the difference;
- editing a template after it has been applied anywhere is drift, as today.

### Library API

- A cargo feature `template = ["dep:minijinja"]`, enabled by `cli`.
  The no-default-features library keeps its documented dependency list.
  Without the feature, loading a source
  that contains a `.sql.j2` file fails with `ChumError::TemplatingDisabled` instead of skipping the file.
- `Migration` gains `pub template: bool`.
- `Migrator::vars(Vars)`, a builder next to `ignore_missing`,
  where `Vars` holds the `env` map and one `this` map per version.
  The library never reads the environment or parses keys; the CLI turns `--var` and `CHUM_VAR_*` keys into a `Vars`,
  applying precedence as it merges them.
- `Migrator::render(version, direction) -> Result<Vec<String>>` returns a migration's statements without a client,
  backing `chum render`.
- `Migrator::reset(client, bookkeeping, version)` backs `chum reset`.
- `AppliedMigration`, `Status` and `Applied` gain `vars: BTreeMap<String, String>`, keyed by namespaced name
  (`env.tier`).
- New `ChumError` variants with diagnostic codes and help: `Render(Vec<RenderFailure>)`,
  one entry per migration that failed, each with its version, direction, message and missing names; `InvalidVarKey`,
  `VarConflict` (a supplied `this.*` value that differs from the recorded one and is read by the render), `NotDirty`
  (from `reset`) and `TemplatingDisabled`.
  Unused variables are not errors; the library reports them so the CLI can warn.
- `Migration`, `AppliedMigration`, `Status`, `Applied` and `ChumError` become `#[non_exhaustive]`,
  so the next additive change doesn't break callers again.

All of this breaks code that builds these types with struct literals or matches `ChumError` exhaustively,
so it ships as 0.3.0.

### CLI

- `--var <key>=value` (global, repeatable) and `CHUM_VAR_<key>`,
  with plain keys for `env.*` and version-prefixed keys for `this.*`,
  added to the README's flag table along with a "Templated migrations" section.
- `chum render` (above), which needs no connection flags.
- `chum reset <version>` (above).
- `chum add --template <name>` scaffolds a `.sql.j2` pair.
  Without `--template`, `add` creates plain files as today.
- `chum info` shows the recorded `vars`.
  For pending templates, the JSON output also lists `template_vars`, the namespaced names the template refers to
  (`env.tier`, `this.cutoff`), so an operator can see what to pass.
- `chum convert` keeps the `.j2` suffix when renaming, and reminds about version-prefixed keys.

## Examples

### Production-only grants

`20261106090000_events.up.sql.j2`:

```sql
CREATE TABLE app.events
(
    ts      DateTime,
    user_id UInt64
)
ENGINE = MergeTree
ORDER BY (user_id, ts);

{% if env.tier == "prod" %}
GRANT SELECT ON app.events TO analytics_reader;
{% endif %}
```

`20261106090000_events.down.sql.j2`:

```sql
{% if env.tier == "prod" %}
REVOKE SELECT ON app.events FROM analytics_reader;
{% endif %}

DROP TABLE app.events SYNC;
```

The template compares `env.tier` instead of testing `env.tier is defined`.
Strict mode makes `env.tier` required everywhere, so every environment must say what it is:
production's ConfigMap sets `CHUM_VAR_tier=prod`, and a developer's local setup sets `CHUM_VAR_tier=local`
(in `mise.toml`, a `.env` file or the shell).
If production's ConfigMap isn't mounted, or the key is misspelt, the render fails and nothing runs.
With `is defined`, the same mistake would silently skip the `GRANT` in production.
A guard can additionally restrict the values (see [Guards](#guards)).

Production records `{"env.tier": "prod"}` for this version and local records `{"env.tier": "local"}`.

`GRANT` writes access entities, which a `Replicated` database doesn't replicate.
Whether a grant reaches every replica depends on how the server stores access control
(a replicated user directory, or `GRANT … ON CLUSTER`);
check that for the production cluster before relying on this pattern.

### Materialized view with a future cutoff

`20261106120000_events_hourly_mv.up.sql.j2`:

```sql
{% if this.cutoff is defined %}
SELECT throwIf(
    toDateTime({{ this.cutoff | sql_string }}, 'UTC') <= now()
        AND (SELECT count() FROM app.events) > 0,
    'cutoff must be in the future while app.events has data'
);
{% else %}
SELECT throwIf(
    (SELECT count() FROM app.events) > 0,
    'app.events has data: pass 20261106120000_cutoff'
);
{% endif %}

CREATE MATERIALIZED VIEW app.events_hourly_mv
TO app.events_hourly AS
SELECT toStartOfHour(ts) AS hour, count() AS events
FROM app.events
{% if this.cutoff is defined %}
WHERE ts >= toDateTime({{ this.cutoff | sql_string }}, 'UTC')
{% endif %}
GROUP BY hour;
```

`20261106120000_events_hourly_mv.down.sql` needs no variables, so it is a plain migration:

```sql
DROP VIEW app.events_hourly_mv;
```

The cutoff is optional, and the guards decide when it's needed, based on data rather than on the environment's name:

- **Source table has data** (staging, production): a cutoff is required and must be in the future.
  Without one, the MV would cover rows the backfill also covers.
- **Source table is empty** (local, CI, a new environment, a rebuilt database): no cutoff is needed,
  and the MV covers everything from the start, so no backfill is needed either.
  A stale cutoff in the past is accepted too, since there is nothing to overlap with.

Staging, on a Thursday:

```bash
chum --url "$STAGING_URL" run --var 20261106120000_cutoff='2026-11-06 14:00:00'
```

Production, the following Tuesday, where chum runs as a Kubernetes Job.
The environment-wide constant comes from the environment's ConfigMap,
and the release that ships this migration adds the version-prefixed cutoff to the Job's environment:

```yaml
env:
  - name: CHUM_VAR_20261106120000_cutoff
    value: "2026-11-10 09:00:00"
envFrom:
  - configMapRef:
      name: chum-prod # holds CHUM_VAR_tier: prod
```

`run` ignores the version-prefixed entry once the migration is applied, and it can't reach another migration.
It isn't inert everywhere, though: `revert` and `force` would see it, which is why recorded `this.*` values win there,
and a rebuilt database with data restored before migrating would be checked against it by the guard.
Remove it from the Job spec once every environment has applied the migration.

Once the cutoff has passed, each environment's backfill job reads that environment's `this.cutoff` back
(see [Recording variables](#recording-variables)) and inserts `WHERE ts < cutoff`.
If the query returns nothing, the migration was applied without a cutoff, to an empty table,
and there is nothing to backfill.

## Writing templates

### Guards

chum doesn't type or validate variables.
Check them in SQL, next to where they are used, with `throwIf`.
It runs on the server before the statements that follow it:

```sql
SELECT throwIf({{ env.tier | sql_string }} NOT IN ('local', 'staging', 'prod'), 'unknown tier');
SELECT throwIf(isNull(toUInt32OrNull({{ env.retention_days | sql_string }})),
               'retention_days must be an integer');
```

A failing guard leaves the version dirty with nothing applied.
Fix the input, run `chum reset <version>`, and run again.

### Fresh databases

Every migration runs in every new database: a developer's laptop, CI, a preview environment, a rebuild.
Within one database, a per-apply value is needed once; across databases, it is needed every time,
unless the template makes it optional.

Write guards against the condition that actually matters, not against an environment:

- A guard that only holds for the environment the migration was written for
  (`cutoff > now()`) fails everywhere else forever, because the cutoff is in the past by then.
- Make it conditional on the state that makes the value necessary, usually whether the affected tables have data,
  as in the [materialized view example](#materialized-view-with-a-future-cutoff).
  A guard that checks data works in every environment without knowing its name.
- Where a value can't be optional, document in a `{# … #}` comment at the top of the template what a fresh environment
  should pass.

`chum render --from <version>` with an empty `this.*`, and with a local `env.*`,
shows in advance whether a fresh database can migrate without per-apply values.

### Required values and booleans

Values are strings, and in Jinja every non-empty string is true, `"false"` included.
Compare (`{% if env.tier == "prod" %}`); don't rely on the truthiness of a value meant as a boolean.

Prefer a required, compared value over `is defined` for anything where a missing value is dangerous.
`is defined` switches strict mode off for that check,
so a missing ConfigMap or a misspelt key silently selects the other branch.
Use `is defined` only where both branches are correct, as with the optional cutoff above,
and back it with a guard that fails when the missing value matters.

### Comments and literal braces

Jinja processes the whole file, SQL comments included: `-- {{ env.x }}` still needs `env.x`.
Use `{# … #}` for comments about the template; they don't reach the rendered SQL.

SQL that contains Jinja syntax, such as ClickHouse's `format('{{}}', x)`, must be wrapped in `{% raw %}…{% endraw %}`.

### Quoting

Use `sql_string` for values that end up in string literals and `sql_ident` for identifiers.
An unfiltered `{{ env.x }}` inserts the value verbatim,
which is right for numbers and SQL fragments but breaks on a value containing a quote.

### Secrets

Anyone who can read the bookkeeping table can read the recorded variables.
Don't pass credentials as variables.
Keep them in ClickHouse named collections and refer to the collection by name in the migration.

## Compatibility and risks

- **An older chum and template files.**
  Releases without templating ignore any file that doesn't end in `.up.sql` or `.down.sql`.
  While a template migration is still pending, an older chum skips it and applies later versions past it,
  and a newer chum then applies the skipped one out of order without complaint.
  Once the template migration is applied, an older chum fails every command with `VersionMissing`,
  because an applied version is missing from its source, unless `--ignore-missing` is passed.
  Pin the chum version used against shared environments.
  Separately, `run` could refuse a pending version older than the newest applied one,
  which would catch this and other ordering mistakes.
- **Long-lived version-prefixed keys.**
  They don't affect `run` once their migration is applied, but `revert`, `force`,
  and guards on a rebuilt database still see them.
  Recorded values winning for `this.*` covers `revert` and `force`; removing old keys from deploy specs covers the rest.
- **Guards run on the server, after the dirty marker.**
  A failed guard therefore leaves a dirty version, recoverable with `chum reset`.

## Alternatives considered

- **Plain `${name}` substitution.**
  Smaller, but it has no conditionals, so production-only statements would need duplicate files.
  Starting there and moving to Jinja later would change the syntax of every existing template.
- **Templating every file.**
  No opt-in is needed, but it changes the meaning of existing files that contain `{{`, `{%` or `{#`,
  and it hides which migrations vary per environment.
- **Per-environment file variants** (`x.up.sql` next to `x.up.prod.sql`).
  Explicit, but they duplicate whole migrations that differ by one statement, and the copies drift apart.
- **ClickHouse query parameters** (`{name:Type}`).
  Server-side and typed, but they have no conditionals,
  and inside `CREATE VIEW` they define a parameterized view instead of being substituted.
- **Capturing values at apply time and gating on time inside chum.**
  This moves scheduling and backfill orchestration into the migrator.
  Values the operator picks, plus guards, cover the same case.
- **Relative values resolved by the CLI**
  (`--var 20261106120000_cutoff=now+2h`, turned into an absolute time once and recorded).
  Rendering would stay pure and Job specs wouldn't carry stale timestamps, but it brings the clock back into chum,
  and conditional guards already handle environments that apply the migration late or fresh.
- **Recording the rendered SQL instead of the variables, or a checksum of it as well.**
  The SQL is exact, but bulky, and it gives an outside job no value to read.
  A rendered-SQL checksum would let a later re-render with the recorded variables be checked against what ran.
  Neither is in the first version; both can be added later as another additive column.
- **A `.j2.sql` suffix, or a header comment marking a template.**
  `.j2.sql` keeps `.sql` last for SQL tooling, and a header comment needs no rename,
  but `.sql.j2` makes templates visible in a directory listing and is the common Jinja convention.
- **minijinja as a core dependency.**
  With `builtins` and `macros` it pulls in no transitive dependencies,
  but a `template` feature keeps the documented no-default-features dependency list unchanged.
- **`chum force <version> --unapplied` instead of `chum reset`.**
  `reset` does the opposite of what `force` does, so it gets its own command.
- **Applying up to the first migration that doesn't render.**
  It would let earlier pending migrations through when a later one lacks a variable,
  but a run would then stop halfway by design.
  Rendering everything first keeps a run all-or-nothing up to execution, and `--target` covers the partial case.
- **Bare variable names with version-scoped overrides.**
  Templates read `{{ cutoff }}`, and a version-prefixed key overrides a plain one for that migration.
  The operator can then override any value for one migration,
  but a plain `CHUM_VAR_cutoff` left in a container spec silently feeds the next migration that reads `cutoff`,
  and only guidance prevents it.
  It also needs six precedence levels
  (scope × source, then recorded, then default) and a list of names reserved for minijinja built-ins.
- **Guards that run before the dirty marker**, marked by a directive comment.
  It would avoid `reset` for the common case, but adds a statement class chum must recognize,
  and `reset` is needed anyway for other first-statement failures.
- **An error, rather than a warning, for unused keys**
  (a supplied value whose name no template in the source reads, such as a misspelt `CHUM_VAR_teir`).
  An error would also stop chum when the deploy config is legitimately ahead of or wider than the migrations:
  a key added to the ConfigMap before the release whose migration reads it,
  a ConfigMap shared by the chum Jobs of several services whose migrations read different keys,
  or a key a pending template stopped reading before release.
  A typo in a required value already fails the render, because the correctly spelt value is then missing.

## Implementation outline

1. `source.rs`: accept `.sql.j2`, set `Migration::template`,
   reject a plain and a template file for the same version and direction,
   and return `TemplatingDisabled` without the feature.
2. New `template.rs` behind `template`: environment setup
   (strict, no auto-escape, no whitespace trimming),
   the `sql_string` and `sql_ident` filters taking strings, key parsing and validation,
   `this` and `env` as lookup-logging objects
   (always present, even when empty), and `render(&Migration, &Vars) -> Result<Rendered { statements, used, missing }>`.
3. `backend.rs`: the `vars` column in `ensure_table`, the conditional `ALTER` for writing commands,
   tolerating a missing column on reads, `record` taking the map
   (bound as `mapFromArrays(?, ?)` from two string arrays),
   `list_applied` reading `argMax(vars, seq)`, and deleting a dirty version's rows for `reset`.
4. `migrator.rs`: the `vars` builder, rendering and splitting everything before the first migration statement in `run`,
   `plan`, `undo` and `revert_plan`, the namespace-specific precedence in `undo`, `revert_plan` and `force`, `reset`,
   and reporting unused variables.
5. `bin/chum.rs`: `--var`, collecting `CHUM_VAR_*`, the `render` and `reset` subcommands, `add --template`,
   `info` output, and `convert`'s suffix handling and reminder.
6. `README.md`: the flag table, a "Templated migrations" section, `render`, `reset`, and the bookkeeping schema.
7. `Cargo.toml` and `mise.toml`: the `template` feature,
   plus `build:lib` and `clippy:lib` variants with `--features template`.

Tests:

- Unit: filename parsing; strict undefined, `default` and `is defined` on namespaced keys; the filters,
  including undefined input and backtick doubling; key parsing
  (leading zeros, duplicates across spellings, empty names, overflow)
  and a plain key never reaching `this.*`; lookup tracking, including `env.get(…)`, `env | items` and defaulted lookups;
  both precedence rules, including the `VarConflict` error; error messages naming the missing variable;
  unchanged checksums for plain files; and rendering every example in this document, compared against expected SQL.
- End-to-end: apply a template and read its variables from `info` and with the documented SQL;
  a missing variable in the last pending migration aborts with no markers written;
  a failing guard leaves the version dirty, and `reset` makes it pending again;
  `revert` renders the down from recorded variables; the `ALTER` adds `vars` to a table created with the old schema,
  read-only commands don't add it, and inserts without `vars` (as an older chum does them) still work.

[minijinja]: https://docs.rs/minijinja
