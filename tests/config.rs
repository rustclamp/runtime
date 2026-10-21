//! Typed configuration: layering, nested keys, aggregated errors, `.env` parsing, `Secret`.
#![cfg(feature = "config")]

use std::path::PathBuf;

use rustclamp_runtime::config::{Config, ConfigError, Problem, Secret, parse_env_file};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Settings {
    name: String,
    port: u16,
    debug: bool,
    db: Db,
    #[serde(default)]
    hosts: Vec<String>,
    level: Option<Level>,
}

#[derive(Debug, Deserialize)]
struct Db {
    url: Secret<String>,
    #[serde(default = "default_pool")]
    pool: u32,
}

fn default_pool() -> u32 {
    5
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Level {
    Info,
    Debug,
}

fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

/// A scratch directory under the build's temp dir, unique per test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rustclamp-config-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn problems(error: ConfigError) -> Vec<String> {
    error.problems.iter().map(ToString::to_string).collect()
}

#[test]
fn nested_keys_fill_structs() {
    let settings: Settings = Config::new("APP")
        .without_env_file()
        .load_from(vars(&[
            ("APP__NAME", "site"),
            ("APP__PORT", " 8080 "),
            ("APP__DEBUG", "yes"),
            ("APP__DB__URL", "postgres://db/app"),
            ("APP__HOSTS", "a.test, b.test"),
            ("APP__LEVEL", "debug"),
            ("OTHER__PORT", "1"),
            ("APPX__PORT", "1"),
        ]))
        .unwrap();
    assert_eq!(settings.name, "site");
    assert_eq!(settings.port, 8080);
    assert!(settings.debug);
    assert_eq!(settings.db.url.expose(), "postgres://db/app");
    assert_eq!(settings.db.pool, 5, "serde default applies");
    assert_eq!(settings.hosts, ["a.test", "b.test"]);
    assert_eq!(settings.level, Some(Level::Debug));
}

#[test]
fn every_problem_is_listed_at_once() {
    let error = Config::new("APP")
        .without_env_file()
        .load_from::<Settings>(vars(&[
            ("APP__PORT", "eighty"),
            ("APP__DEBUG", "maybe"),
            ("APP__LEVEL", "loud"),
            ("APP__DB__POOL", "-1"),
        ]))
        .unwrap_err();
    let mut listed = problems(error);
    listed.sort();
    assert_eq!(
        listed,
        [
            "config key APP__DB__POOL is set but is not a valid u32",
            "config key APP__DB__URL is required but not set",
            "config key APP__DEBUG is set but is not a valid bool",
            "config key APP__LEVEL is set but is not a valid one of info, debug",
            "config key APP__NAME is required but not set",
            "config key APP__PORT is set but is not a valid u16",
        ]
    );
}

#[test]
fn errors_never_show_values() {
    let error = Config::new("APP")
        .without_env_file()
        .load_from::<Settings>(vars(&[("APP__PORT", "hunter2"), ("APP__DB", "hunter2")]))
        .unwrap_err();
    let shown = format!("{error} {error:?}");
    assert!(!shown.contains("hunter2"), "{shown}");
    assert!(shown.contains("APP__DB is set but is not a valid table of keys"));
}

#[test]
fn unknown_keys_are_listed_with_deny_unknown_fields() {
    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Strict {
        #[allow(dead_code)]
        port: u16,
    }
    let error = Config::new("APP")
        .without_env_file()
        .load_from::<Strict>(vars(&[("APP__PROT", "1"), ("APP__HOST", "x")]))
        .unwrap_err();
    let mut listed = problems(error);
    listed.sort();
    assert_eq!(
        listed,
        [
            "config key APP__HOST is not recognized",
            "config key APP__PORT is required but not set",
            "config key APP__PROT is not recognized",
        ]
    );
}

#[derive(Debug, Deserialize)]
struct Small {
    name: String,
    port: u16,
    token: Option<Secret<String>>,
}

#[test]
fn env_beats_env_file() {
    let dir = scratch("precedence");
    let file = dir.join(".env");
    std::fs::write(
        &file,
        "APP__NAME=from-file\nAPP__PORT=1\nAPP__TOKEN=file-token\n",
    )
    .unwrap();
    let small: Small = Config::new("APP")
        .env_file(&file)
        .load_from(vars(&[("APP__PORT", "2")]))
        .unwrap();
    assert_eq!(small.name, "from-file");
    assert_eq!(small.port, 2, "process environment wins");
    assert_eq!(small.token.unwrap().expose(), "file-token");
}

#[test]
fn missing_env_file_is_skipped() {
    let small: Small = Config::new("APP")
        .env_file(scratch("missing").join("nope.env"))
        .load_from(vars(&[
            ("APP__NAME", "x"),
            ("APP__PORT", "1"),
            ("APP__TOKEN", ""),
        ]))
        .unwrap();
    assert!(small.token.is_none(), "empty means unset for Option");
}

#[test]
fn unreadable_env_file_is_a_problem_alongside_keys() {
    // A directory cannot be read as a file.
    let error = Config::new("APP")
        .env_file(scratch("dir"))
        .load_from::<Small>(vars(&[("APP__PORT", "1")]))
        .unwrap_err();
    assert!(matches!(error.problems[0], Problem::Source { .. }));
    assert_eq!(
        error.problems[1].to_string(),
        "config key APP__NAME is required but not set"
    );
}

#[test]
fn env_file_edge_cases() {
    let parsed = parse_env_file(concat!(
        "# comment\n",
        "\n",
        "export A=plain\n",
        "B=\"quoted # not a comment\"\n",
        "C='single \\n literal'\n",
        "D=\"esc\\\"aped\\nline\"\n",
        "E=value # trailing comment\n",
        "F=a#b\n",
        "  G = spaced  \n",
        "H=\n",
        "I=\"unterminated\n",
        "J='x' # after quotes\n",
        "no equals sign\n",
        "=no key\n",
        "K=a=b\n",
    ));
    let expected = [
        ("A", "plain"),
        ("B", "quoted # not a comment"),
        ("C", "single \\n literal"),
        ("D", "esc\"aped\nline"),
        ("E", "value"),
        ("F", "a#b"),
        ("G", "spaced"),
        ("H", ""),
        ("I", "\"unterminated"),
        ("J", "x"),
        ("K", "a=b"),
    ];
    assert_eq!(parsed, vars(&expected));
}

#[test]
fn secret_redacts_debug_and_display() {
    let settings: Settings = Config::new("APP")
        .without_env_file()
        .load_from(vars(&[
            ("APP__NAME", "site"),
            ("APP__PORT", "1"),
            ("APP__DEBUG", "false"),
            ("APP__DB__URL", "postgres://user:hunter2@db/app"),
        ]))
        .unwrap();
    let shown = format!("{settings:?} {}", settings.db.url);
    assert!(!shown.contains("hunter2"), "{shown}");
    assert!(shown.contains("url: [redacted]"));
    assert_eq!(
        settings.db.url.into_inner(),
        "postgres://user:hunter2@db/app"
    );
}

#[cfg(feature = "config-toml")]
#[test]
fn toml_is_the_lowest_layer() {
    let dir = scratch("toml");
    let toml = dir.join("app.toml");
    std::fs::write(
        &toml,
        "name = \"from-toml\"\nport = 1\ndebug = true\nhosts = [\"a\", \"b,c\"]\n[db]\nurl = \"toml-url\"\npool = 9\n",
    )
    .unwrap();
    let env = dir.join(".env");
    std::fs::write(&env, "APP__PORT=2\nAPP__DB__POOL=3\n").unwrap();
    let settings: Settings = Config::new("APP")
        .env_file(&env)
        .toml_file(&toml)
        .load_from(vars(&[("APP__DB__POOL", "4")]))
        .unwrap();
    assert_eq!(settings.name, "from-toml");
    assert!(settings.debug);
    assert_eq!(settings.port, 2, ".env beats TOML");
    assert_eq!(settings.db.pool, 4, "environment beats .env and TOML");
    assert_eq!(settings.db.url.expose(), "toml-url");
    assert_eq!(settings.hosts, ["a", "b,c"], "TOML arrays keep commas");
}

#[cfg(feature = "config-toml")]
#[test]
fn bad_toml_is_reported_without_its_contents() {
    let dir = scratch("bad-toml");
    let toml = dir.join("app.toml");
    std::fs::write(&toml, "name = \"x\"\ntoken = hunter2\n").unwrap();
    let error = Config::new("APP")
        .without_env_file()
        .toml_file(&toml)
        .load_from::<Small>(Vec::new())
        .unwrap_err();
    let shown = error.to_string();
    assert!(!shown.contains("hunter2"), "{shown}");
    assert!(shown.contains("line 2"), "{shown}");
    assert!(shown.contains("APP__PORT is required"), "{shown}");

    let missing = Config::new("APP")
        .without_env_file()
        .toml_file(dir.join("absent.toml"))
        .load_from::<Small>(vars(&[("APP__NAME", "x"), ("APP__PORT", "1")]))
        .unwrap_err();
    assert!(matches!(missing.problems[..], [Problem::Source { .. }]));
}
