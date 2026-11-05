//! Rendering `.sql.j2` migrations with minijinja.
//!
//! Rendering is a pure function of the template text and the supplied
//! variables: no clock, no process environment, no filesystem, no database,
//! and no `include`/`import`/`extends` (the `multi_template` engine feature is
//! off). The environment is configured for SQL:
//!
//! * strict undefined behaviour, so printing, iterating, comparing or testing
//!   a missing value is an error, with `| default(…)` and `is defined` as the
//!   explicit opt-outs;
//! * no auto-escaping;
//! * whitespace kept as written: with `trim_blocks`, an inline `{% endif %}`
//!   at the end of a line would swallow the newline and glue two SQL lines
//!   together.
//!
//! The top level of a template holds only `env`, `this` and the reserved
//! `chum`. `env` and `this` are objects that log each lookup, so a render
//! reports exactly which supplied values it used and which names it looked up
//! without finding a value.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use minijinja::AutoEscape;
use minijinja::Environment;
use minijinja::Error;
use minijinja::ErrorKind;
use minijinja::State;
use minijinja::UndefinedBehavior;
use minijinja::Value;
use minijinja::syntax::SyntaxConfig;
use minijinja::value::Enumerator;
use minijinja::value::Object;
use minijinja::value::ObjectRepr;

use crate::vars::References;

/// The output of a successful render.
#[derive(Debug)]
pub(crate) struct Rendered {
    /// The rendered SQL.
    pub sql: String,
    /// Every namespaced name (`env.tier`) the render looked up and found a
    /// value for, with that value.
    pub used: BTreeMap<String, String>,
    /// Every namespaced name the render looked up and found no value for.
    pub missing: BTreeSet<String>,
}

/// Why a render failed.
#[derive(Debug)]
pub(crate) struct Failure {
    /// minijinja's message, naming the template and line.
    pub message: String,
    /// The names looked up without a value that the failing line refers to,
    /// or every such name when the line refers to none of them by name.
    pub missing: Vec<String>,
}

fn environment() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_auto_escape_callback(|_| AutoEscape::None);
    env.set_syntax(
        SyntaxConfig::builder()
            .trim_blocks(false)
            .lstrip_blocks(false)
            .keep_trailing_newline(true)
            .build()
            .expect("the default delimiters are valid"),
    );
    env.add_filter("sql_string", sql_string);
    env.add_filter("sql_ident", sql_ident);
    env
}

/// Render the template `source`, named `name` in error messages, with the
/// given `env` and `this` namespaces.
pub(crate) fn render(
    name: &str,
    source: &str,
    env: &BTreeMap<String, String>,
    this: &BTreeMap<String, String>,
) -> Result<Rendered, Failure> {
    let log = Arc::new(Mutex::new(Log::default()));
    let context = Value::from_object(Context {
        env: Value::from_object(Namespace::new("env", env.clone(), &log)),
        this: Value::from_object(Namespace::new("this", this.clone(), &log)),
        chum: Value::from_object(Reserved),
    });
    let environment = environment();
    let result = environment
        .template_from_named_str(name, source)
        .and_then(|template| template.render(context));
    let log = std::mem::take(&mut *log.lock().unwrap_or_else(PoisonError::into_inner));
    match result {
        Ok(sql) => Ok(Rendered {
            sql,
            used: log.used,
            missing: log.missing,
        }),
        Err(err) => Err(Failure {
            missing: missing_on_line(&err, source, &log.missing),
            message: err.to_string(),
        }),
    }
}

/// Narrow the names looked up without a value to those the failing line
/// mentions, so a line reading both `this.cutoff` and `env.tier` names only
/// the one that was missing, and an earlier `is defined` check elsewhere
/// isn't blamed.
fn missing_on_line(err: &Error, source: &str, missing: &BTreeSet<String>) -> Vec<String> {
    if err.kind() != ErrorKind::UndefinedError {
        return Vec::new();
    }
    let line = err
        .line()
        .and_then(|n| source.lines().nth(n.checked_sub(1)?));
    if let Some(line) = line {
        let on_line: Vec<String> = missing
            .iter()
            .filter(|name| line.contains(name.as_str()))
            .cloned()
            .collect();
        if !on_line.is_empty() {
            return on_line;
        }
    }
    missing.iter().cloned().collect()
}

/// Find the supplied names `source` refers to, without rendering it. A
/// template that fails to parse refers to nothing.
pub(crate) fn references(name: &str, source: &str) -> References {
    let environment = environment();
    let Ok(template) = environment.template_from_named_str(name, source) else {
        return References::default();
    };
    let mut refs = References::default();
    for var in template.undeclared_variables(true) {
        let mut parts = var.split('.');
        let target = match parts.next() {
            Some("env") => &mut refs.env,
            Some("this") => &mut refs.this,
            _ => continue,
        };
        match parts.next() {
            // `ns`, `ns[…]`, `ns | items`, and `ns.get(…)` can read any name.
            None => target.all = true,
            Some("get") => {
                target.all = true;
                target.names.insert("get".to_owned());
            }
            Some(attr) => {
                target.names.insert(attr.to_owned());
            }
        }
    }
    refs
}

/// `{{ value | sql_string }}`: a ClickHouse string literal in single quotes,
/// with `\` and `'` escaped by a backslash.
///
/// Taking a `String` rather than a `Value` or `&str` makes an undefined input
/// an undefined-value error under strict undefined behaviour, rather than an
/// empty literal or a type error, and accepts numbers.
#[expect(
    clippy::needless_pass_by_value,
    reason = "minijinja checks only owned `String` arguments for undefined"
)]
fn sql_string(value: String) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            c => out.push(c),
        }
    }
    out.push('\'');
    out
}

/// `{{ value | sql_ident }}`: one backtick-quoted identifier, with a backtick
/// doubled and a backslash escaped.
///
/// ClickHouse also accepts `` \` `` inside a quoted identifier, but chum's
/// statement splitter doesn't, so backticks are doubled. A backslash is
/// escaped so a trailing one can't escape the closing backtick.
#[expect(
    clippy::needless_pass_by_value,
    reason = "minijinja checks only owned `String` arguments for undefined"
)]
fn sql_ident(value: String) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('`');
    for c in value.chars() {
        match c {
            '`' => out.push_str("``"),
            '\\' => out.push_str("\\\\"),
            c => out.push(c),
        }
    }
    out.push('`');
    out
}

#[derive(Debug, Default)]
struct Log {
    used: BTreeMap<String, String>,
    missing: BTreeSet<String>,
}

/// The template's top level.
#[derive(Debug)]
struct Context {
    env: Value,
    this: Value,
    chum: Value,
}

impl Object for Context {
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        match key.as_str()? {
            "env" => Some(self.env.clone()),
            "this" => Some(self.this.clone()),
            "chum" => Some(self.chum.clone()),
            _ => None,
        }
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Str(&["chum", "env", "this"])
    }
}

/// The reserved `chum` namespace, empty for now.
#[derive(Debug)]
struct Reserved;

impl Object for Reserved {}

/// `env` or `this`: a map of supplied values that logs every lookup and
/// enumeration.
#[derive(Debug)]
struct Namespace {
    prefix: &'static str,
    values: BTreeMap<String, String>,
    log: Arc<Mutex<Log>>,
}

impl Namespace {
    fn new(prefix: &'static str, values: BTreeMap<String, String>, log: &Arc<Mutex<Log>>) -> Self {
        Self {
            prefix,
            values,
            log: Arc::clone(log),
        }
    }

    fn lookup(&self, name: &str) -> Option<Value> {
        let namespaced = format!("{}.{name}", self.prefix);
        let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(value) = self.values.get(name) {
            log.used.insert(namespaced, value.clone());
            Some(Value::from(value.as_str()))
        } else {
            log.missing.insert(namespaced);
            None
        }
    }
}

impl Object for Namespace {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Map
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        self.lookup(key.as_str()?)
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        let mut log = self.log.lock().unwrap_or_else(PoisonError::into_inner);
        for (name, value) in &self.values {
            log.used
                .insert(format!("{}.{name}", self.prefix), value.clone());
        }
        let pairs: Vec<(Value, Value)> = self
            .values
            .iter()
            .map(|(k, v)| (Value::from(k.as_str()), Value::from(v.as_str())))
            .collect();
        Enumerator::KeyValueIter(Box::new(pairs.into_iter()))
    }

    // Asking for the size or truthiness reads no value, so it must not go
    // through `enumerate` and log every value as used.
    fn enumerator_len(self: &Arc<Self>) -> Option<usize> {
        Some(self.values.len())
    }

    fn is_true(self: &Arc<Self>) -> bool {
        !self.values.is_empty()
    }

    /// `ns.get(name, default=none)`, as on a Python dict.
    fn call_method(
        self: &Arc<Self>,
        _state: &mut State<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        if method != "get" {
            return Err(Error::from(ErrorKind::UnknownMethod));
        }
        let (name, default) = match args {
            [name] => (name, Value::from(())),
            [name, default] => (name, default.clone()),
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidOperation,
                    "get() takes a name and an optional default",
                ));
            }
        };
        let name = name
            .as_str()
            .ok_or_else(|| Error::new(ErrorKind::InvalidOperation, "get() takes a string name"))?;
        Ok(self.lookup(name).unwrap_or(default))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(items: &[(&str, &str)]) -> BTreeMap<String, String> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn ok(source: &str, env: &[(&str, &str)], this: &[(&str, &str)]) -> Rendered {
        render("t.sql.j2", source, &map(env), &map(this))
            .unwrap_or_else(|f| panic!("render failed: {}", f.message))
    }

    fn err(source: &str, env: &[(&str, &str)], this: &[(&str, &str)]) -> Failure {
        match render("t.sql.j2", source, &map(env), &map(this)) {
            Ok(r) => panic!("expected a failure, rendered {:?}", r.sql),
            Err(f) => f,
        }
    }

    #[test]
    fn values_come_from_their_namespace() {
        let r = ok(
            "{{ env.tier }} {{ this.cutoff }}",
            &[("tier", "prod")],
            &[("cutoff", "T")],
        );
        assert_eq!(r.sql, "prod T");
        assert_eq!(r.used, map(&[("env.tier", "prod"), ("this.cutoff", "T")]));
    }

    #[test]
    fn env_and_this_never_compete() {
        // A plain value never reaches `this.*`.
        let f = err("{{ this.cutoff }}", &[("cutoff", "x")], &[]);
        assert_eq!(f.missing, ["this.cutoff"]);
    }

    #[test]
    fn strict_undefined_fails_print_compare_iterate_and_test() {
        for source in [
            "{{ env.x }}",
            "{% if env.x == 'a' %}{% endif %}",
            "{% for c in env.x %}{% endfor %}",
            "{% if env.x %}{% endif %}",
            "{{ env.x | sql_string }}",
            "{{ env.x | sql_ident }}",
        ] {
            let f = err(source, &[], &[]);
            assert!(f.message.contains("undefined"), "{source}: {}", f.message);
            assert_eq!(f.missing, ["env.x"], "{source}");
        }
    }

    #[test]
    fn default_and_is_defined_opt_out() {
        let r = ok(
            "{{ this.cutoff | default('none') }} {% if env.x is defined %}x{% else %}no x{% endif %}",
            &[],
            &[],
        );
        assert_eq!(r.sql, "none no x");
        assert!(r.used.is_empty(), "defaulted values are not used");
        assert_eq!(
            r.missing,
            ["env.x".to_owned(), "this.cutoff".to_owned()].into()
        );

        let r = ok(
            "{{ this.cutoff | default('none') }}",
            &[],
            &[("cutoff", "T")],
        );
        assert_eq!(r.sql, "T");
        assert_eq!(r.used, map(&[("this.cutoff", "T")]));
    }

    #[test]
    fn namespaces_are_always_present() {
        let r = ok(
            "{{ env is defined }} {{ this is defined }} {{ chum is defined }}",
            &[],
            &[],
        );
        assert_eq!(r.sql, "True True True");
    }

    #[test]
    fn reserved_jinja_names_are_ordinary_keys() {
        let r = ok(
            "{{ env.range }} {{ env.loop }} {{ env.global }}",
            &[("range", "1"), ("loop", "2"), ("global", "3")],
            &[],
        );
        assert_eq!(r.sql, "1 2 3");
    }

    #[test]
    fn get_method_is_tracked() {
        let r = ok(
            "{{ env.get('a') }} {{ env.get('b', 'dflt') }} {{ env.get('c') is none }}",
            &[("a", "1"), ("unused", "x")],
            &[],
        );
        assert_eq!(r.sql, "1 dflt True");
        assert_eq!(r.used, map(&[("env.a", "1")]));
    }

    #[test]
    fn enumeration_is_tracked() {
        let r = ok(
            "{% for k, v in env | items %}{{ k }}={{ v }};{% endfor %}",
            &[("a", "1"), ("b", "2")],
            &[],
        );
        assert_eq!(r.sql, "a=1;b=2;");
        assert_eq!(r.used, map(&[("env.a", "1"), ("env.b", "2")]));

        // Length and truthiness read no values.
        let r = ok(
            "{{ env | length }}{% if this %}x{% endif %}",
            &[("a", "1")],
            &[],
        );
        assert_eq!(r.sql, "1");
        assert!(r.used.is_empty());
    }

    #[test]
    fn error_names_the_missing_variable_on_the_failing_line() {
        // Both names are on the failing line; only the missing one is named.
        let f = err(
            "SELECT 1;\nSELECT {{ this.cutoff | sql_string }}, {{ env.tier | sql_string }}",
            &[],
            &[("cutoff", "T")],
        );
        assert_eq!(f.missing, ["env.tier"]);
        assert!(f.message.contains("undefined value"), "{}", f.message);
        assert!(f.message.contains("t.sql.j2:2"), "{}", f.message);

        // An optional value checked on another line isn't blamed.
        let f = err(
            "{% if this.cutoff is defined %}{% endif %}\nSELECT {{ env.tier }}",
            &[],
            &[],
        );
        assert_eq!(f.missing, ["env.tier"]);
    }

    #[test]
    fn whitespace_is_kept_as_written() {
        let r = ok(
            "ENGINE = {% if env.tier == 'prod' %}A{% else %}B{% endif %}\nORDER BY x\n",
            &[("tier", "local")],
            &[],
        );
        assert_eq!(r.sql, "ENGINE = B\nORDER BY x\n");
    }

    #[test]
    fn no_auto_escaping() {
        let r = ok("{{ env.x }}", &[("x", "<a & 'b'>")], &[]);
        assert_eq!(r.sql, "<a & 'b'>");
    }

    #[test]
    fn includes_do_not_exist() {
        let f = err("{% include 'other.sql' %}", &[], &[]);
        assert_eq!(f.missing, Vec::<String>::new());
    }

    #[test]
    fn sql_string_escapes_quotes_and_backslashes() {
        let r = ok(
            r#"{{ "it's" | sql_string }} {{ env.x | sql_string }}"#,
            &[("x", r"a\b")],
            &[],
        );
        assert_eq!(r.sql, r"'it\'s' 'a\\b'");
        // The splitter tokenizes the result as one string literal.
        let stmts = crate::split::split_statements(&format!("SELECT {}; SELECT 1", r.sql)).unwrap();
        assert_eq!(stmts.len(), 2);
    }

    #[test]
    fn sql_ident_is_one_quoted_identifier() {
        let r = ok(
            r#"{{ "db.t" | sql_ident }} {{ "a`b" | sql_ident }} {{ env.x | sql_ident }}"#,
            &[("x", "trailing\\")],
            &[],
        );
        assert_eq!(r.sql, r"`db.t` `a``b` `trailing\\`");
        let stmts =
            crate::split::split_statements(&format!("SELECT 1 AS {}; SELECT 2", r.sql)).unwrap();
        assert_eq!(stmts.len(), 2);
    }

    #[test]
    fn references_are_found_statically() {
        let refs = references(
            "t",
            "{% if env.tier == 'prod' %}{{ this.cutoff | sql_string }}{% endif %}{{ env.a.b }}",
        );
        assert!(!refs.env.all && !refs.this.all);
        assert_eq!(refs.env.names, ["a".to_owned(), "tier".to_owned()].into());
        assert_eq!(refs.this.names, ["cutoff".to_owned()].into());

        let refs = references("t", "{{ env.get('x') }}{% for k in this %}{% endfor %}");
        assert!(refs.env.all);
        assert!(refs.this.all);
        assert!(refs.env.contains("anything"));
    }

    // The examples from docs/design/templated-migrations.md.

    fn statements(r: &Rendered) -> Vec<String> {
        crate::split::split_statements(&r.sql).unwrap()
    }

    const GRANTS_UP: &str = "CREATE TABLE app.events
(
    ts      DateTime,
    user_id UInt64
)
ENGINE = MergeTree
ORDER BY (user_id, ts);

{% if env.tier == \"prod\" %}
GRANT SELECT ON app.events TO analytics_reader;
{% endif %}
";

    const GRANTS_DOWN: &str = "{% if env.tier == \"prod\" %}
REVOKE SELECT ON app.events FROM analytics_reader;
{% endif %}

DROP TABLE app.events SYNC;
";

    const CREATE_EVENTS: &str = "CREATE TABLE app.events
(
    ts      DateTime,
    user_id UInt64
)
ENGINE = MergeTree
ORDER BY (user_id, ts)";

    #[test]
    fn example_production_only_grants() {
        let prod = ok(GRANTS_UP, &[("tier", "prod")], &[]);
        assert_eq!(
            statements(&prod),
            [
                CREATE_EVENTS,
                "GRANT SELECT ON app.events TO analytics_reader"
            ]
        );
        assert_eq!(prod.used, map(&[("env.tier", "prod")]));

        let local = ok(GRANTS_UP, &[("tier", "local")], &[]);
        assert_eq!(statements(&local), [CREATE_EVENTS]);
        assert_eq!(local.used, map(&[("env.tier", "local")]));

        let f = err(GRANTS_UP, &[], &[]);
        assert_eq!(f.missing, ["env.tier"]);

        let down = ok(GRANTS_DOWN, &[("tier", "prod")], &[]);
        assert_eq!(
            statements(&down),
            [
                "REVOKE SELECT ON app.events FROM analytics_reader",
                "DROP TABLE app.events SYNC"
            ]
        );
        let down = ok(GRANTS_DOWN, &[("tier", "local")], &[]);
        assert_eq!(statements(&down), ["DROP TABLE app.events SYNC"]);
    }

    const MV_UP: &str = "{% if this.cutoff is defined %}
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
";

    #[test]
    fn example_materialized_view_with_a_cutoff() {
        let with = ok(MV_UP, &[], &[("cutoff", "2026-11-10 09:00:00")]);
        assert_eq!(
            statements(&with),
            [
                "SELECT throwIf(
    toDateTime('2026-11-10 09:00:00', 'UTC') <= now()
        AND (SELECT count() FROM app.events) > 0,
    'cutoff must be in the future while app.events has data'
)",
                "CREATE MATERIALIZED VIEW app.events_hourly_mv
TO app.events_hourly AS
SELECT toStartOfHour(ts) AS hour, count() AS events
FROM app.events

WHERE ts >= toDateTime('2026-11-10 09:00:00', 'UTC')

GROUP BY hour"
            ]
        );
        assert_eq!(with.used, map(&[("this.cutoff", "2026-11-10 09:00:00")]));

        let without = ok(MV_UP, &[], &[]);
        assert_eq!(
            statements(&without),
            [
                "SELECT throwIf(
    (SELECT count() FROM app.events) > 0,
    'app.events has data: pass 20261106120000_cutoff'
)",
                "CREATE MATERIALIZED VIEW app.events_hourly_mv
TO app.events_hourly AS
SELECT toStartOfHour(ts) AS hour, count() AS events
FROM app.events

GROUP BY hour"
            ]
        );
        assert!(without.used.is_empty());
    }

    #[test]
    fn example_guards() {
        let r = ok(
            "SELECT throwIf({{ env.tier | sql_string }} NOT IN ('local', 'staging', 'prod'), \
             'unknown tier');
SELECT throwIf(isNull(toUInt32OrNull({{ env.retention_days | sql_string }})),
               'retention_days must be an integer');",
            &[("tier", "prod"), ("retention_days", "30")],
            &[],
        );
        assert_eq!(
            statements(&r),
            [
                "SELECT throwIf('prod' NOT IN ('local', 'staging', 'prod'), 'unknown tier')",
                "SELECT throwIf(isNull(toUInt32OrNull('30')),
               'retention_days must be an integer')"
            ]
        );
    }

    #[test]
    fn example_comments_and_literal_braces() {
        let r = ok(
            "{# a note #}\n-- {{ env.x }}\nSELECT {% raw %}format('{{}}', 1){% endraw %}",
            &[("x", "v")],
            &[],
        );
        assert_eq!(r.sql, "\n-- v\nSELECT format('{{}}', 1)");
        let f = err("-- {{ env.x }}\nSELECT 1", &[], &[]);
        assert_eq!(f.missing, ["env.x"]);
    }
}
