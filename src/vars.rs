//! Variables supplied to templated migrations.
//!
//! Templates read supplied values from two namespaces: `env.<name>` holds
//! values describing the target environment, available to every migration,
//! and `this.<name>` holds values for one version only. The form of a key
//! picks the namespace: a plain key (`tier`) supplies `env.tier`, and a
//! version-prefixed key (`20261106120000_cutoff`) supplies `this.cutoff` for
//! version `20261106120000`. A name can't start with a digit, so the leading
//! digits of a key are unambiguously its version.
//!
//! A `this.*` name can also be supplied as *unset*: a decision that the
//! version runs without that value, which a template sees as a missing name
//! (`is defined` is false, `| default(…)` applies). Every `this.*` name a
//! version's templates refer to must be supplied or unset, so a forgotten or
//! misspelt key can't silently take a template's "no value" branch.
//!
//! The library never reads the process environment. A caller (the CLI) turns
//! its `--var`, `--unset`, `CHUM_VAR_*` and `CHUM_UNSET_*` keys into a
//! [`Vars`] with [`Vars::from_keys`], or builds one directly.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::btree_map::Entry;
use std::fmt;

use crate::error::ChumError;
use crate::error::Result;

/// Where a variable's value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Origin {
    /// Supplied explicitly: `--var` or `--unset` on the command line, or set
    /// by a library caller.
    Flag,
    /// A `CHUM_VAR_<key>` or `CHUM_UNSET_<key>` environment variable.
    Environment,
    /// The bookkeeping row of a version being reverted or forced.
    Recorded,
    /// Not supplied; the template's `| default(…)` or `is defined` covered it.
    Default,
    /// Not supplied, and unset by [`MissingThis::Unset`].
    MissingThis,
}

impl Origin {
    /// A short label for output: `--var`, `environment`, `recorded`,
    /// `default`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Flag => "--var",
            Origin::Environment => "environment",
            Origin::Recorded => "recorded",
            Origin::Default => "default",
            Origin::MissingThis => "--missing-this",
        }
    }

    /// How a user wrote `key` in this source: `--var <key>` or
    /// `CHUM_VAR_<key>`.
    #[must_use]
    pub fn describe(self, key: &str) -> String {
        match self {
            Origin::Flag => format!("--var {key}"),
            Origin::Environment => format!("CHUM_VAR_{key}"),
            Origin::Recorded | Origin::Default | Origin::MissingThis => key.to_owned(),
        }
    }

    /// How a user wrote an unset `key` in this source: `--unset <key>` or
    /// `CHUM_UNSET_<key>`.
    #[must_use]
    pub fn describe_unset(self, key: &str) -> String {
        match self {
            Origin::Flag => format!("--unset {key}"),
            Origin::Environment => format!("CHUM_UNSET_{key}"),
            Origin::Recorded | Origin::Default | Origin::MissingThis => key.to_owned(),
        }
    }

    /// A short label for output, like [`Origin::as_str`], for a value that is
    /// `unset`: `--unset` rather than `--var` for a flag.
    #[must_use]
    pub fn label(self, unset: bool) -> &'static str {
        match self {
            Origin::Flag if unset => "--unset",
            _ => self.as_str(),
        }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What to do with a `this.*` name a version's templates refer to that is
/// neither supplied, unset nor recorded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum MissingThis {
    /// Fail before anything runs, naming every such name.
    #[default]
    Error,
    /// Treat it as unset, and record it so. For databases built from scratch
    /// (a laptop, CI), which run every migration without per-apply values.
    Unset,
}

/// A variable, identified by its namespace and name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VarKey {
    /// `env.<name>`, shared by every migration.
    Env(String),
    /// `this.<name>` for one version.
    This {
        /// The version the value is for.
        version: i64,
        /// The name within `this`.
        name: String,
    },
}

impl VarKey {
    /// The name a template reads and the bookkeeping table records:
    /// `env.tier` or `this.cutoff`.
    #[must_use]
    pub fn namespaced(&self) -> String {
        match self {
            VarKey::Env(name) => format!("env.{name}"),
            VarKey::This { name, .. } => format!("this.{name}"),
        }
    }

    /// The canonical key that supplies this variable: `tier` or
    /// `20261106120000_cutoff`.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            VarKey::Env(name) => name.clone(),
            VarKey::This { version, name } => format!("{version}_{name}"),
        }
    }
}

/// A supplied value, or a supplied decision that there is none, and where it
/// came from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Supplied {
    /// The value, or `None` for a name supplied as unset.
    pub value: Option<String>,
    /// Its source.
    pub origin: Origin,
    /// The key as the user spelled it (`0005_x` for a `this.x` of version 5),
    /// for messages.
    pub key: String,
}

impl Supplied {
    /// A supplied value.
    #[must_use]
    pub fn new(value: impl Into<String>, origin: Origin, key: impl Into<String>) -> Self {
        Self {
            value: Some(value.into()),
            origin,
            key: key.into(),
        }
    }

    /// A name supplied as unset.
    #[must_use]
    pub fn unset(origin: Origin, key: impl Into<String>) -> Self {
        Self {
            value: None,
            origin,
            key: key.into(),
        }
    }

    /// How the user supplied it, e.g. `--var 5_x`, `CHUM_UNSET_5_x`.
    #[must_use]
    pub fn describe(&self) -> String {
        match self.value {
            Some(_) => self.origin.describe(&self.key),
            None => self.origin.describe_unset(&self.key),
        }
    }
}

/// The variables supplied to a migrator: the `env` namespace and one `this`
/// namespace per version.
#[derive(Debug, Clone, Default)]
pub struct Vars {
    env: BTreeMap<String, Supplied>,
    this: BTreeMap<i64, BTreeMap<String, Supplied>>,
}

impl Vars {
    /// No variables.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `env.<name>`, replacing any earlier value.
    #[must_use]
    pub fn with_env(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        let name = name.into();
        let supplied = Supplied::new(value, Origin::Flag, name.clone());
        self.insert(VarKey::Env(name), supplied);
        self
    }

    /// Set `this.<name>` for `version`, replacing any earlier value.
    #[must_use]
    pub fn with_this(
        mut self,
        version: i64,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        let name = name.into();
        let supplied = Supplied::new(value, Origin::Flag, format!("{version}_{name}"));
        self.insert(VarKey::This { version, name }, supplied);
        self
    }

    /// Supply `this.<name>` for `version` as unset, replacing any earlier
    /// value.
    #[must_use]
    pub fn with_unset(mut self, version: i64, name: impl Into<String>) -> Self {
        let name = name.into();
        let supplied = Supplied::unset(Origin::Flag, format!("{version}_{name}"));
        self.insert(VarKey::This { version, name }, supplied);
        self
    }

    /// Set a variable, replacing any earlier value. An unset `env.*` name
    /// reads as missing; [`Vars::from_keys`] rejects one.
    pub fn insert(&mut self, key: VarKey, supplied: Supplied) {
        match key {
            VarKey::Env(name) => {
                self.env.insert(name, supplied);
            }
            VarKey::This { version, name } => {
                self.this.entry(version).or_default().insert(name, supplied);
            }
        }
    }

    /// Build variables from command-line keys (`flags`: `--var` with a value,
    /// `--unset` with `None`) and environment keys (`environment`, without the
    /// prefix: `CHUM_VAR_*` with a value, `CHUM_UNSET_*` with `None`).
    ///
    /// A flag wins over an environment variable for the same key, whether
    /// either sets a value or unsets it. Within one source, a key may appear
    /// once, so a key given both a value and unset there is an error; `0005_x`
    /// and `5_x` are the same key.
    ///
    /// # Errors
    ///
    /// Returns [`ChumError::InvalidVarKey`] for a key that doesn't parse (see
    /// [`parse_key`]), that appears twice in one source, or that unsets an
    /// `env.*` name: only `this.*` names can be unset.
    pub fn from_keys<F, E>(flags: F, environment: E) -> Result<Self>
    where
        F: IntoIterator<Item = (String, Option<String>)>,
        E: IntoIterator<Item = (String, Option<String>)>,
    {
        let environment = collect(environment, Origin::Environment)?;
        let flags = collect(flags, Origin::Flag)?;
        let mut vars = Self::new();
        for (key, supplied) in environment.into_iter().chain(flags) {
            vars.insert(key, supplied);
        }
        Ok(vars)
    }

    /// The `env` namespace.
    #[must_use]
    pub fn env(&self) -> &BTreeMap<String, Supplied> {
        &self.env
    }

    /// The `this` namespace for `version`, if anything was supplied for it.
    #[must_use]
    pub fn this(&self, version: i64) -> Option<&BTreeMap<String, Supplied>> {
        self.this.get(&version)
    }

    /// Look up one variable.
    #[must_use]
    pub fn get(&self, key: &VarKey) -> Option<&Supplied> {
        match key {
            VarKey::Env(name) => self.env.get(name),
            VarKey::This { version, name } => self.this.get(version)?.get(name),
        }
    }

    /// Every supplied variable.
    pub fn iter(&self) -> impl Iterator<Item = (VarKey, &Supplied)> {
        let env = self
            .env
            .iter()
            .map(|(name, s)| (VarKey::Env(name.clone()), s));
        let this = self.this.iter().flat_map(|(version, names)| {
            names.iter().map(move |(name, s)| {
                (
                    VarKey::This {
                        version: *version,
                        name: name.clone(),
                    },
                    s,
                )
            })
        });
        env.chain(this)
    }

    /// Whether no variable was supplied.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.env.is_empty() && self.this.values().all(BTreeMap::is_empty)
    }
}

/// Parse and de-duplicate the keys of one source.
fn collect<I>(pairs: I, origin: Origin) -> Result<BTreeMap<VarKey, Supplied>>
where
    I: IntoIterator<Item = (String, Option<String>)>,
{
    let mut out: BTreeMap<VarKey, Supplied> = BTreeMap::new();
    for (key, value) in pairs {
        let supplied = match value {
            Some(value) => Supplied::new(value, origin, key),
            None => Supplied::unset(origin, key),
        };
        let invalid = |reason: String| ChumError::InvalidVarKey {
            key: supplied.describe(),
            reason,
        };
        let parsed = parse_key(&supplied.key).map_err(invalid)?;
        if supplied.value.is_none() && matches!(parsed, VarKey::Env(_)) {
            return Err(invalid(
                "only a version-prefixed key (`<version>_<name>`) can be unset".to_owned(),
            ));
        }
        match out.entry(parsed) {
            Entry::Occupied(earlier) => {
                return Err(invalid(format!(
                    "supplied twice, also as {}",
                    earlier.get().describe()
                )));
            }
            Entry::Vacant(slot) => {
                slot.insert(supplied);
            }
        }
    }
    Ok(out)
}

/// Parse a variable key: a plain `<name>` for `env.<name>`, or
/// `<version>_<name>` for `this.<name>` of that version.
///
/// The version is compared numerically, so `0005_x` and `5_x` are the same
/// key. A name matches `[A-Za-z_][A-Za-z0-9_]*`.
///
/// # Errors
///
/// Returns a reason when the name is empty or invalid, or the version
/// doesn't fit in `i64`.
pub fn parse_key(key: &str) -> std::result::Result<VarKey, String> {
    let digits = key.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        validate_name(key)?;
        return Ok(VarKey::Env(key.to_owned()));
    }
    let (version, rest) = key.split_at(digits);
    let Some(name) = rest.strip_prefix('_') else {
        return Err(
            "a key starting with a digit must be `<version>_<name>`, and a name can't start \
             with a digit"
                .to_owned(),
        );
    };
    let version: i64 = version
        .parse()
        .map_err(|_| format!("version {version} does not fit in a 64-bit integer"))?;
    validate_name(name)?;
    Ok(VarKey::This {
        version,
        name: name.to_owned(),
    })
}

fn validate_name(name: &str) -> std::result::Result<(), String> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return Err("the name is empty".to_owned());
    };
    let valid = (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if valid {
        Ok(())
    } else {
        Err(format!(
            "name {name:?} does not match [A-Za-z_][A-Za-z0-9_]*"
        ))
    }
}

/// The supplied names a template refers to, found statically.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct References {
    /// References into `env`.
    pub env: Refs,
    /// References into `this`.
    pub this: Refs,
}

/// References into one namespace.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Refs {
    /// The namespace is used as a whole (iterated, indexed, `.get(…)`), so
    /// any name in it may be read.
    pub all: bool,
    /// Names read as `ns.<name>`.
    pub names: BTreeSet<String>,
}

impl Refs {
    /// Whether the template may read `name` from this namespace.
    pub fn contains(&self, name: &str) -> bool {
        self.all || self.names.contains(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, Option<String>)> {
        items
            .iter()
            .map(|(k, v)| ((*k).to_owned(), Some((*v).to_owned())))
            .collect()
    }

    fn unsets(keys: &[&str]) -> Vec<(String, Option<String>)> {
        keys.iter().map(|k| ((*k).to_owned(), None)).collect()
    }

    #[test]
    fn plain_key_is_env() {
        assert_eq!(parse_key("tier"), Ok(VarKey::Env("tier".into())));
        assert_eq!(parse_key("_x1"), Ok(VarKey::Env("_x1".into())));
        assert_eq!(parse_key("range"), Ok(VarKey::Env("range".into())));
    }

    #[test]
    fn version_prefixed_key_is_this() {
        assert_eq!(
            parse_key("20261106120000_cutoff"),
            Ok(VarKey::This {
                version: 20_261_106_120_000,
                name: "cutoff".into()
            })
        );
        // Leading zeros don't change the version.
        assert_eq!(parse_key("0005_x"), parse_key("5_x"));
        // The name may itself contain underscores and digits.
        assert_eq!(
            parse_key("5__a_1"),
            Ok(VarKey::This {
                version: 5,
                name: "_a_1".into()
            })
        );
    }

    #[test]
    fn invalid_keys_are_rejected() {
        assert!(parse_key("").is_err(), "empty");
        assert!(parse_key("20261106120000_").is_err(), "empty name");
        assert!(parse_key("5").is_err(), "no name");
        assert!(parse_key("5x").is_err(), "name starting with a digit");
        assert!(parse_key("5_-x").is_err(), "invalid name");
        assert!(parse_key("my-var").is_err(), "dash");
        assert!(parse_key("x.y").is_err(), "dot");
        assert!(
            parse_key("99999999999999999999_x").is_err(),
            "version overflow"
        );
    }

    #[test]
    fn flags_win_over_environment() {
        let vars = Vars::from_keys(
            pairs(&[("tier", "prod")]),
            pairs(&[("tier", "local"), ("region", "eu")]),
        )
        .unwrap();
        let tier = &vars.env()["tier"];
        assert_eq!(tier.value.as_deref(), Some("prod"));
        assert_eq!(tier.origin, Origin::Flag);
        assert_eq!(vars.env()["region"].origin, Origin::Environment);
    }

    #[test]
    fn plain_key_never_reaches_this() {
        let vars = Vars::from_keys(pairs(&[("cutoff", "x")]), []).unwrap();
        assert!(vars.this(5).is_none());
        assert!(vars.get(&parse_key("5_cutoff").unwrap()).is_none());
    }

    #[test]
    fn duplicates_within_one_source_are_rejected() {
        let err = Vars::from_keys(pairs(&[("0005_x", "a"), ("5_x", "b")]), []).unwrap_err();
        let ChumError::InvalidVarKey { key, reason } = err else {
            panic!("expected InvalidVarKey");
        };
        assert_eq!(key, "--var 5_x");
        assert!(reason.contains("--var 0005_x"), "{reason}");

        let err = Vars::from_keys([], pairs(&[("0005_x", "a"), ("5_x", "b")])).unwrap_err();
        assert!(matches!(err, ChumError::InvalidVarKey { key, .. } if key == "CHUM_VAR_5_x"));

        // The same key from two different sources is not a duplicate.
        let vars = Vars::from_keys(pairs(&[("0005_x", "a")]), pairs(&[("5_x", "b")])).unwrap();
        assert_eq!(vars.this(5).unwrap()["x"].value.as_deref(), Some("a"));
    }

    #[test]
    fn unset_is_a_value_like_any_other_for_precedence() {
        // A flag wins over the environment, whichever of them unsets.
        let vars = Vars::from_keys(unsets(&["5_x"]), pairs(&[("5_x", "a")])).unwrap();
        let x = &vars.this(5).unwrap()["x"];
        assert_eq!(x.value, None);
        assert_eq!(x.describe(), "--unset 5_x");
        let vars = Vars::from_keys(pairs(&[("5_x", "a")]), unsets(&["5_x"])).unwrap();
        assert_eq!(vars.this(5).unwrap()["x"].value.as_deref(), Some("a"));
        let vars = Vars::from_keys([], unsets(&["5_x"])).unwrap();
        assert_eq!(vars.this(5).unwrap()["x"].describe(), "CHUM_UNSET_5_x");

        // A value and an unset for the same key from one source conflict.
        let mut flags = pairs(&[("5_x", "a")]);
        flags.extend(unsets(&["0005_x"]));
        let err = Vars::from_keys(flags, []).unwrap_err();
        let ChumError::InvalidVarKey { key, reason } = err else {
            panic!("expected InvalidVarKey");
        };
        assert_eq!(key, "--unset 0005_x");
        assert!(reason.contains("--var 5_x"), "{reason}");
    }

    #[test]
    fn only_this_names_can_be_unset() {
        let err = Vars::from_keys([], unsets(&["tier"])).unwrap_err();
        assert!(
            matches!(err, ChumError::InvalidVarKey { ref key, ref reason }
                if key == "CHUM_UNSET_tier" && reason.contains("version-prefixed")),
            "{err}"
        );
    }

    #[test]
    fn invalid_key_error_names_the_source() {
        let err = Vars::from_keys([], pairs(&[("20261106120000_", "a")])).unwrap_err();
        assert!(
            matches!(err, ChumError::InvalidVarKey { ref key, .. } if key == "CHUM_VAR_20261106120000_"),
            "{err}"
        );
    }
}
