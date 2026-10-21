//! Typed configuration from the process environment, a `.env` file and an
//! optional TOML file (feature `config`; TOML needs `config-toml`).
//!
//! Sources layer as process environment > `.env` > TOML. Environment keys take
//! a prefix and use `__` for nesting: with prefix `APP`, `APP__DB__URL` fills
//! `db.url`. Segments are lowercased, so they match Rust field names. Loading
//! never modifies the process environment.
//!
//! Loading reports **every** missing or invalid key in one [`ConfigError`],
//! not the first, so a deployment fixes its settings in one round. Messages
//! name keys, never values.
//!
//! ```
//! use rustclamp_runtime::config::{Config, Secret};
//! use serde::Deserialize;
//!
//! #[derive(Deserialize)]
//! struct Settings {
//!     port: u16,
//!     db: Db,
//! }
//!
//! #[derive(Deserialize)]
//! struct Db {
//!     url: Secret<String>,
//!     #[serde(default)]
//!     pool: Option<u32>,
//! }
//!
//! let vars = [("APP__PORT", "8080"), ("APP__DB__URL", "postgres://app@db/app")];
//! let settings: Settings = Config::new("APP")
//!     .without_env_file()
//!     .load_from(vars.map(|(key, value)| (key.to_owned(), value.to_owned())))?;
//! assert_eq!(settings.port, 8080);
//! assert_eq!(format!("{:?}", settings.db.url), "[redacted]");
//! assert_eq!(settings.db.url.expose(), "postgres://app@db/app");
//! # Ok::<(), rustclamp_runtime::config::ConfigError>(())
//! ```
//!
//! Supported shapes: structs (nested through `__`), scalars, `Option`
//! (unset or empty is `None`), unit enum variants by name, sequences as
//! comma-separated values or TOML arrays, and maps. `#[serde(default)]` and
//! `#[serde(deny_unknown_fields)]` work as usual.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::path::PathBuf;

use serde::de::value::{MapDeserializer, SeqDeserializer, StringDeserializer};
use serde::de::{self, DeserializeOwned, IntoDeserializer, Visitor};
use serde::{Deserialize, Deserializer};

/// Loads a typed configuration. See the [module docs](self).
#[derive(Clone, Debug)]
pub struct Config {
    prefix: String,
    env_file: Option<PathBuf>,
    #[cfg(feature = "config-toml")]
    toml_file: Option<PathBuf>,
}

impl Config {
    /// Reads environment keys that start with `{prefix}__`, and `.env` from the
    /// working directory when it exists. An empty prefix reads every key.
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            env_file: Some(".env".into()),
            #[cfg(feature = "config-toml")]
            toml_file: None,
        }
    }

    /// Reads this `.env` file instead of `./.env`. A missing file is skipped.
    pub fn env_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.env_file = Some(path.into());
        self
    }

    /// Reads no `.env` file, so a stray one where the process starts cannot
    /// change its settings.
    pub fn without_env_file(mut self) -> Self {
        self.env_file = None;
        self
    }

    /// Reads this TOML file as the lowest layer. Unlike `.env`, it must exist:
    /// a named file that is missing is an error. Its keys are used as written.
    #[cfg(feature = "config-toml")]
    pub fn toml_file(mut self, path: impl Into<PathBuf>) -> Self {
        self.toml_file = Some(path.into());
        self
    }

    /// Loads `T` from the process environment and the configured files.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] listing every unreadable source and every missing,
    /// invalid or unknown key.
    pub fn load<T: DeserializeOwned>(&self) -> Result<T, ConfigError> {
        // ponytail: non-UTF-8 variables are skipped; report them if an app needs that
        let vars = std::env::vars_os()
            .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)));
        self.load_from(vars)
    }

    /// Like [`load`](Self::load), with `vars` in place of the process
    /// environment: for tests and for processes that filter their environment.
    ///
    /// # Errors
    ///
    /// As [`load`](Self::load).
    pub fn load_from<T: DeserializeOwned>(
        &self,
        vars: impl IntoIterator<Item = (String, String)>,
    ) -> Result<T, ConfigError> {
        let mut problems = Vec::new();
        let mut root = Node::Table(BTreeMap::new());
        #[cfg(feature = "config-toml")]
        if let Some(path) = &self.toml_file {
            match read_toml(path) {
                Ok(table) => root = table,
                Err(problem) => problems.push(problem),
            }
        }
        if let Some(path) = &self.env_file {
            match std::fs::read_to_string(path) {
                Ok(text) => self.overlay(&mut root, parse_env_file(&text)),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => problems.push(Problem::Source {
                    source: path.display().to_string(),
                    message: error.to_string(),
                }),
            }
        }
        self.overlay(&mut root, vars);
        match self.deserialize(root, &mut problems) {
            Some(value) if problems.is_empty() => Ok(value),
            _ => Err(ConfigError { problems }),
        }
    }

    fn overlay(&self, root: &mut Node, vars: impl IntoIterator<Item = (String, String)>) {
        // Sorted, so a conflict such as `APP__DB` against `APP__DB__URL` resolves
        // the same way on every start: the nested key wins.
        let vars: BTreeMap<_, _> = vars.into_iter().collect();
        for (key, value) in vars {
            let rest = if self.prefix.is_empty() {
                Some(key.as_str())
            } else {
                key.strip_prefix(&self.prefix)
                    .and_then(|rest| rest.strip_prefix("__"))
            };
            let Some(rest) = rest else { continue };
            let path: Vec<String> = rest.split("__").map(str::to_lowercase).collect();
            if path.iter().all(|segment| !segment.is_empty()) {
                root.set(&path, Node::Value(value));
            }
        }
    }

    /// Deserializes, turning each failure into a problem and retrying with the
    /// failing key neutralized, until `T` builds or nothing more can be learnt.
    fn deserialize<T: DeserializeOwned>(
        &self,
        mut root: Node,
        problems: &mut Vec<Problem>,
    ) -> Option<T> {
        // ponytail: one full pass per problem; config trees are small
        for _ in 0..1024 {
            let error = match T::deserialize(De::new(root.clone(), Vec::new())) {
                Ok(value) => return Some(value),
                Err(error) => error,
            };
            let path = error.path.unwrap_or_default();
            let key = self.key(&path);
            let slot = root.get(&path).cloned();
            match error.kind {
                // First sighting: mark absent so a missing struct reports its
                // own missing fields instead of one key for the whole table.
                Kind::Missing(_) if slot.is_none() => root.set(&path, Node::Absent),
                Kind::Unknown(_) => {
                    problems.push(Problem::Unknown { key });
                    if !root.remove(&path) {
                        return None;
                    }
                }
                _ if matches!(slot, Some(Node::Poison)) => {
                    problems.push(Problem::Other {
                        key,
                        message: "cannot be checked further".into(),
                    });
                    return None;
                }
                kind => {
                    problems.push(match kind {
                        Kind::Missing(_) => Problem::Missing { key },
                        Kind::Invalid(expected) => Problem::Invalid { key, expected },
                        Kind::Unknown(_) => Problem::Unknown { key },
                        Kind::Custom(message) => Problem::Other { key, message },
                    });
                    root.set(&path, Node::Poison);
                }
            }
        }
        None
    }

    fn key(&self, path: &[String]) -> String {
        let mut segments: Vec<String> = path.iter().map(|s| s.to_uppercase()).collect();
        if !self.prefix.is_empty() {
            segments.insert(0, self.prefix.clone());
        }
        segments.join("__")
    }
}

/// Parses `.env` text: `KEY=value` lines, optional `export ` prefixes, blank
/// lines and `#` comments. Double-quoted values support `\n`, `\t`, `\"` and
/// `\\`; single-quoted values are literal; a ` #` after an unquoted value
/// starts a comment. Lines without `=` are skipped.
pub fn parse_env_file(text: &str) -> Vec<(String, String)> {
    // ponytail: no `${VAR}` interpolation or multi-line values; add when an app needs them
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            (!key.is_empty()).then(|| (key.to_owned(), env_value(value.trim())))
        })
        .collect()
}

fn env_value(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix('\'')
        && let Some((value, _)) = rest.split_once('\'')
    {
        return value.to_owned();
    }
    if let Some(rest) = raw.strip_prefix('"') {
        let mut value = String::new();
        let mut chars = rest.chars();
        while let Some(c) = chars.next() {
            match c {
                '"' => return value,
                '\\' => match chars.next() {
                    Some('n') => value.push('\n'),
                    Some('t') => value.push('\t'),
                    Some(other) => value.push(other),
                    None => value.push('\\'),
                },
                c => value.push(c),
            }
        }
        // Unterminated: keep the line as written.
        return raw.to_owned();
    }
    let end = raw
        .char_indices()
        .find(|&(i, c)| c == '#' && raw[..i].ends_with(char::is_whitespace))
        .map_or(raw.len(), |(i, _)| i);
    raw[..end].trim_end().to_owned()
}

#[cfg(feature = "config-toml")]
fn read_toml(path: &std::path::Path) -> Result<Node, Problem> {
    let source = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|error| Problem::Source {
        source: source.clone(),
        message: error.to_string(),
    })?;
    let table: toml::Table = text.parse().map_err(|error: toml::de::Error| {
        // The message only: toml's Display quotes the offending line, which may hold a secret.
        let line = error
            .span()
            .map(|span| text[..span.start].lines().count().max(1));
        Problem::Source {
            message: match line {
                Some(line) => format!("line {line}: {}", error.message()),
                None => error.message().to_owned(),
            },
            source,
        }
    })?;
    Ok(Node::from_toml(toml::Value::Table(table)))
}

/// Every problem found while loading. Never empty.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct ConfigError {
    /// Problems in the order found: sources first, then keys.
    pub problems: Vec<Problem>,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid configuration: ")?;
        for (index, problem) in self.problems.iter().enumerate() {
            if index > 0 {
                f.write_str("; ")?;
            }
            write!(f, "{problem}")?;
        }
        Ok(())
    }
}

impl Error for ConfigError {}

/// One configuration problem. Keys are shown in environment form, such as
/// `APP__DB__URL`; values are never shown.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Problem {
    /// A required key is not set.
    Missing {
        /// The key.
        key: String,
    },
    /// A key is set but does not parse.
    Invalid {
        /// The key.
        key: String,
        /// What it should be, such as `u16`.
        expected: String,
    },
    /// A key is set that a `deny_unknown_fields` type does not accept.
    Unknown {
        /// The key.
        key: String,
    },
    /// A source file exists but cannot be read or parsed.
    Source {
        /// The file.
        source: String,
        /// Why, without file contents.
        message: String,
    },
    /// A type's own validation rejected a key.
    Other {
        /// The key.
        key: String,
        /// The type's message.
        message: String,
    },
}

impl fmt::Display for Problem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing { key } => write!(f, "config key {key} is required but not set"),
            Self::Invalid { key, expected } => {
                write!(f, "config key {key} is set but is not a valid {expected}")
            }
            Self::Unknown { key } => write!(f, "config key {key} is not recognized"),
            Self::Source { source, message } => write!(f, "config file {source}: {message}"),
            Self::Other { key, message } => write!(f, "config key {key}: {message}"),
        }
    }
}

/// A value that `Debug` and `Display` print as `[redacted]`. Reach it with
/// [`expose`](Self::expose), so every use of the secret is visible in code.
///
/// ```
/// use rustclamp_runtime::config::Secret;
///
/// let key = Secret::new("hunter2".to_owned());
/// assert_eq!(format!("{key:?} {key}"), "[redacted] [redacted]");
/// assert_eq!(key.expose(), "hunter2");
/// ```
// ponytail: memory is not zeroed on drop; add zeroize if a threat model needs it
#[derive(Clone, Copy, Default)]
pub struct Secret<T>(T);

impl<T> Secret<T> {
    /// Wraps `value`.
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// The secret value.
    pub const fn expose(&self) -> &T {
        &self.0
    }

    /// The secret value, consuming the wrapper.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl<T> fmt::Display for Secret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Secret<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self)
    }
}

/// The merged configuration tree.
#[derive(Clone, Debug)]
enum Node {
    Table(BTreeMap<String, Node>),
    List(Vec<Node>),
    Value(String),
    /// A key reported missing once; a struct here is built from its own fields.
    Absent,
    /// A key already reported; it deserializes to a placeholder so the
    /// remaining keys can still be checked.
    Poison,
}

impl Node {
    fn get(&self, path: &[String]) -> Option<&Node> {
        path.iter().try_fold(self, |node, segment| match node {
            Node::Table(map) => map.get(segment),
            Node::List(items) => items.get(segment.parse::<usize>().ok()?),
            _ => None,
        })
    }

    fn set(&mut self, path: &[String], value: Node) {
        let Some((last, parents)) = path.split_last() else {
            *self = value;
            return;
        };
        let mut node = self;
        for segment in parents {
            node = node.child(segment);
        }
        *node.child(last) = value;
    }

    /// The child at `segment`, turning a non-container into a table.
    fn child(&mut self, segment: &str) -> &mut Node {
        let index = segment.parse::<usize>().ok();
        let is_item = matches!((&*self, index), (Node::List(items), Some(i)) if i < items.len());
        if !is_item && !matches!(self, Node::Table(_)) {
            *self = Node::Table(BTreeMap::new());
        }
        match self {
            Node::List(items) => &mut items[index.unwrap_or_default()],
            Node::Table(map) => map.entry(segment.to_owned()).or_insert(Node::Absent),
            _ => unreachable!("made a table above"),
        }
    }

    fn remove(&mut self, path: &[String]) -> bool {
        if let Some((last, parents)) = path.split_last()
            && let Some(Node::Table(map)) = self.get_mut(parents)
        {
            return map.remove(last).is_some();
        }
        false
    }

    fn get_mut(&mut self, path: &[String]) -> Option<&mut Node> {
        path.iter().try_fold(self, |node, segment| match node {
            Node::Table(map) => map.get_mut(segment),
            Node::List(items) => items.get_mut(segment.parse::<usize>().ok()?),
            _ => None,
        })
    }

    #[cfg(feature = "config-toml")]
    fn from_toml(value: toml::Value) -> Self {
        match value {
            toml::Value::String(s) => Node::Value(s),
            toml::Value::Integer(i) => Node::Value(i.to_string()),
            toml::Value::Float(f) => Node::Value(f.to_string()),
            toml::Value::Boolean(b) => Node::Value(b.to_string()),
            toml::Value::Datetime(d) => Node::Value(d.to_string()),
            toml::Value::Array(items) => {
                Node::List(items.into_iter().map(Self::from_toml).collect())
            }
            toml::Value::Table(map) => Node::Table(
                map.into_iter()
                    .map(|(key, value)| (key, Self::from_toml(value)))
                    .collect(),
            ),
        }
    }
}

/// A deserialization failure. `path` is filled in by the innermost node that
/// knows where it is.
#[derive(Debug)]
struct DeError {
    path: Option<Vec<String>>,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    /// The field name, when raised by a struct visitor.
    Missing(Option<String>),
    Invalid(String),
    Unknown(String),
    Custom(String),
}

impl DeError {
    fn new(kind: Kind) -> Self {
        Self { path: None, kind }
    }

    /// Places an error raised below `path`. Missing and unknown fields name
    /// themselves; anything else belongs to `path`.
    fn at(mut self, path: &[String]) -> Self {
        if self.path.is_none() {
            let mut full = path.to_vec();
            match &mut self.kind {
                Kind::Missing(field @ Some(_)) => full.extend(field.take()),
                Kind::Unknown(field) => full.push(field.clone()),
                _ => {}
            }
            self.path = Some(full);
        }
        self
    }
}

impl fmt::Display for DeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.kind)
    }
}

impl Error for DeError {}

impl de::Error for DeError {
    fn custom<M: fmt::Display>(message: M) -> Self {
        Self::new(Kind::Custom(message.to_string()))
    }

    // Never quotes the unexpected value: it may be a secret.
    fn invalid_type(_: de::Unexpected<'_>, expected: &dyn de::Expected) -> Self {
        Self::new(Kind::Invalid(expected.to_string()))
    }

    fn invalid_value(_: de::Unexpected<'_>, expected: &dyn de::Expected) -> Self {
        Self::new(Kind::Invalid(expected.to_string()))
    }

    fn invalid_length(_: usize, expected: &dyn de::Expected) -> Self {
        Self::new(Kind::Invalid(expected.to_string()))
    }

    fn unknown_variant(_: &str, expected: &'static [&'static str]) -> Self {
        Self::new(Kind::Invalid(format!("one of {}", expected.join(", "))))
    }

    fn unknown_field(field: &str, _: &'static [&'static str]) -> Self {
        Self::new(Kind::Unknown(field.to_owned()))
    }

    fn missing_field(field: &'static str) -> Self {
        Self::new(Kind::Missing(Some(field.to_owned())))
    }
}

struct De {
    node: Node,
    path: Vec<String>,
}

impl De {
    fn new(node: Node, path: Vec<String>) -> Self {
        Self { node, path }
    }

    fn fail(&self, kind: Kind) -> DeError {
        DeError::new(kind).at(&self.path)
    }

    fn missing(&self) -> DeError {
        self.fail(Kind::Missing(None))
    }

    fn invalid(&self, expected: &str) -> DeError {
        self.fail(Kind::Invalid(expected.to_owned()))
    }

    fn children(
        self,
        entries: impl IntoIterator<Item = (String, Node)>,
    ) -> MapDeserializer<'static, impl Iterator<Item = (String, De)>, DeError> {
        let path = self.path;
        MapDeserializer::new(entries.into_iter().map(move |(key, node)| {
            let mut child = path.clone();
            child.push(key.clone());
            (key, De::new(node, child))
        }))
    }
}

impl IntoDeserializer<'_, DeError> for De {
    type Deserializer = Self;

    fn into_deserializer(self) -> Self {
        self
    }
}

fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Some(true),
        "false" | "0" | "no" | "off" => Some(false),
        _ => None,
    }
}

macro_rules! scalars {
    ($($method:ident => $visit:ident: $ty:ty),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
            match &self.node {
                Node::Value(text) => match text.trim().parse::<$ty>() {
                    Ok(value) => visitor.$visit(value).map_err(|e: DeError| e.at(&self.path)),
                    Err(_) => Err(self.invalid(stringify!($ty))),
                },
                Node::Poison => visitor.$visit(<$ty>::default()),
                Node::Absent => Err(self.missing()),
                _ => Err(self.invalid(stringify!($ty))),
            }
        }
    )*};
}

impl<'de> Deserializer<'de> for De {
    type Error = DeError;

    scalars! {
        deserialize_i8 => visit_i8: i8,
        deserialize_i16 => visit_i16: i16,
        deserialize_i32 => visit_i32: i32,
        deserialize_i64 => visit_i64: i64,
        deserialize_i128 => visit_i128: i128,
        deserialize_u8 => visit_u8: u8,
        deserialize_u16 => visit_u16: u16,
        deserialize_u32 => visit_u32: u32,
        deserialize_u64 => visit_u64: u64,
        deserialize_u128 => visit_u128: u128,
        deserialize_f32 => visit_f32: f32,
        deserialize_f64 => visit_f64: f64,
        deserialize_char => visit_char: char,
    }

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match &self.node {
            Node::Value(_) => self.deserialize_string(visitor),
            Node::Table(_) => self.deserialize_map(visitor),
            Node::List(_) => self.deserialize_seq(visitor),
            Node::Poison => visitor.visit_unit(),
            Node::Absent => Err(self.missing()),
        }
    }

    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match &self.node {
            Node::Value(text) => match parse_bool(text) {
                Some(value) => visitor
                    .visit_bool(value)
                    .map_err(|e: DeError| e.at(&self.path)),
                None => Err(self.invalid("bool")),
            },
            Node::Poison => visitor.visit_bool(false),
            Node::Absent => Err(self.missing()),
            _ => Err(self.invalid("bool")),
        }
    }

    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match &self.node {
            Node::Value(text) => visitor
                .visit_str(text)
                .map_err(|e: DeError| e.at(&self.path)),
            Node::Poison => visitor.visit_str(""),
            Node::Absent => Err(self.missing()),
            _ => Err(self.invalid("string")),
        }
    }

    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        match &self.node {
            Node::Absent | Node::Poison => visitor.visit_none(),
            Node::Value(text) if text.is_empty() => visitor.visit_none(),
            _ => visitor.visit_some(self),
        }
    }

    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_unit()
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        visitor.visit_unit()
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        visitor.visit_newtype_struct(self)
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        let items = match self.node {
            Node::List(items) => items,
            // ponytail: plain comma split, no escaping; use a TOML array for values with commas
            Node::Value(ref text) => text
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(|item| Node::Value(item.to_owned()))
                .collect(),
            Node::Poison => Vec::new(),
            Node::Absent => return Err(self.missing()),
            Node::Table(_) => return Err(self.invalid("list")),
        };
        let path = self.path.clone();
        let items = items.into_iter().enumerate().map(|(index, node)| {
            let mut child = path.clone();
            child.push(index.to_string());
            De::new(node, child)
        });
        visitor
            .visit_seq(SeqDeserializer::new(items))
            .map_err(|e: DeError| e.at(&path))
    }

    fn deserialize_tuple<V: Visitor<'de>>(self, _: usize, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        _: usize,
        visitor: V,
    ) -> Result<V::Value, DeError> {
        self.deserialize_seq(visitor)
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        let entries = match &self.node {
            Node::Table(map) => map.clone(),
            Node::Poison => BTreeMap::new(),
            Node::Absent => return Err(self.missing()),
            _ => return Err(self.invalid("table of keys")),
        };
        let path = self.path.clone();
        visitor
            .visit_map(self.children(entries))
            .map_err(|e: DeError| e.at(&path))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        let entries: BTreeMap<String, Node> = match &self.node {
            Node::Table(map) => map.clone(),
            // Absent: the visitor names each field it needs, one per pass.
            Node::Absent => BTreeMap::new(),
            Node::Poison => fields
                .iter()
                .map(|field| ((*field).to_owned(), Node::Poison))
                .collect(),
            _ => return Err(self.invalid("table of keys")),
        };
        let path = self.path.clone();
        visitor
            .visit_map(self.children(entries))
            .map_err(|e: DeError| e.at(&path))
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DeError> {
        // ponytail: unit variants by name only; data-carrying variants need a table form
        let variant = match &self.node {
            Node::Value(text) => text.trim().to_owned(),
            Node::Poison => variants.first().copied().unwrap_or_default().to_owned(),
            Node::Absent => return Err(self.missing()),
            _ => return Err(self.invalid(&format!("one of {}", variants.join(", ")))),
        };
        let access: StringDeserializer<DeError> = variant.into_deserializer();
        visitor
            .visit_enum(access)
            .map_err(|e: DeError| e.at(&self.path))
    }

    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        self.deserialize_string(visitor)
    }

    fn deserialize_ignored_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DeError> {
        visitor.visit_unit()
    }
}
