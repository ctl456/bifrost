//! The command line.
//!
//! The process serves when it is given nothing and answers a question when it is
//! given a flag, and both kinds of question belong to the deployment rather than to a
//! developer. `--check` is what a unit file runs before it starts anything, so a
//! configuration this build cannot use fails the unit instead of the first request
//! that needs it; `--print-config` exists because the effective configuration is
//! built from three layers and only one of them is the file someone edited; and the
//! three token commands are how a deployment that hands out credentials hands one
//! out, lists what it has given away and takes one back. They read the same
//! configuration the server reads, so they answer about the deployment the operator
//! is looking at rather than about the directory they happen to be standing in.

use std::path::{Path, PathBuf};

use crate::config::Config;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// What the operator asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Serve.
    Serve { config: Option<PathBuf> },
    /// Answer whether this configuration can be started with, and exit.
    Check { config: Option<PathBuf> },
    /// Print the resolved configuration, and exit.
    PrintConfig { config: Option<PathBuf> },
    /// Issue a token, print it once, and exit.
    TokenNew {
        config: Option<PathBuf>,
        name: String,
        /// Requests per minute; 0 is no limit.
        rpm: u32,
        /// Turns at once; 0 is no limit.
        concurrency: u32,
    },
    /// List the tokens this deployment has issued, and exit.
    TokenList { config: Option<PathBuf> },
    /// Take a token away, and exit.
    TokenRevoke { config: Option<PathBuf>, name: String },
    /// Print how to use this, and exit.
    Help,
}

/// How to use this.
pub const USAGE: &str = "\
usage: bifrost [--check | --print-config | --token-new NAME | --token-list |
                --token-revoke NAME] [--config PATH]

  (no arguments)  serve: run the gateway
  --check         validate the configuration and exit. A unit file runs this first,
                  so a bad file fails the unit rather than the first request
  --print-config  print the configuration this process resolved to, with secrets
                  redacted: the file and the environment are layered by then, which
                  is the reason for printing it at all
  --token-new NAME
                  issue a token for the caller NAME and print it, once. The file
                  keeps its sha256, so a lost token is replaced rather than found,
                  and a revoked one is refused from the next request on. Only a
                  deployment with access.enabled issues tokens: a token nothing
                  checks is a credential that does not exist
    --rpm N       at most N requests a minute for that token, refilled
                  continuously. 0, the default, is no limit
    --concurrency N
                  at most N requests at once for that token. 0, the default, is no
                  limit. This is what keeps one caller from filling the deployment's
                  own ceiling and leaving the others queued
  --token-list    print the tokens issued, one line each, oldest name first. No line
                  holds a token: the file has none to print
  --token-revoke NAME
                  take a token away. The name stays taken and the record stays in
                  the file, so that a name seen in an access line can be told from
                  one that was never issued
  --config PATH   read this file instead of $BIFROST_CONFIG or ./bifrost.toml
  -h, --help      print this";

/// Which question was asked, before the path that came with it is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Serve,
    Check,
    PrintConfig,
    TokenNew,
    TokenList,
    TokenRevoke,
    Help,
}

impl Command {
    /// Read the arguments, or say why they are not a command.
    ///
    /// No argument means the server, because that is what the binary is for; every
    /// other mode has to be asked for, so a mistyped flag cannot turn into a
    /// serving process.
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut mode = Mode::Serve;
        let mut config: Option<PathBuf> = None;
        let mut name: Option<String> = None;
        let mut rpm: Option<u32> = None;
        let mut concurrency: Option<u32> = None;
        let mut args = args.into_iter().peekable();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--check" => mode = choose(mode, Mode::Check)?,
                "--print-config" => mode = choose(mode, Mode::PrintConfig)?,
                "--config" => {
                    let path = args.next().ok_or_else(|| usage_error("--config needs a path"))?;
                    config = Some(PathBuf::from(path));
                }
                "--token-new" => {
                    mode = choose(mode, Mode::TokenNew)?;
                    name = Some(args.next().ok_or_else(|| usage_error("--token-new needs a name"))?);
                }
                "--token-list" => mode = choose(mode, Mode::TokenList)?,
                "--token-revoke" => {
                    mode = choose(mode, Mode::TokenRevoke)?;
                    name = Some(args.next().ok_or_else(|| usage_error("--token-revoke needs a name"))?);
                }
                "--rpm" => {
                    let value = args.next().ok_or_else(|| usage_error("--rpm needs a number"))?;
                    rpm = Some(
                        value
                            .parse()
                            .map_err(|_| usage_error(&format!("--rpm needs a number, got `{value}`")))?,
                    );
                }
                "--concurrency" => {
                    let value = args.next().ok_or_else(|| usage_error("--concurrency needs a number"))?;
                    concurrency = Some(
                        value
                            .parse()
                            .map_err(|_| usage_error(&format!("--concurrency needs a number, got `{value}`")))?,
                    );
                }
                "-h" | "--help" => mode = Mode::Help,
                other => return Err(usage_error(&format!("unknown argument `{other}`"))),
            }
        }
        // A limit belongs to the token it is set on, so it is only something to say
        // while issuing one: a `--rpm` that quietly changed nothing would be read as
        // having been applied to something.
        if (rpm.is_some() || concurrency.is_some()) && mode != Mode::TokenNew {
            return Err(usage_error(
                "--rpm and --concurrency only mean something with --token-new",
            ));
        }
        Ok(match mode {
            Mode::Serve => Command::Serve { config },
            Mode::Check => Command::Check { config },
            Mode::PrintConfig => Command::PrintConfig { config },
            Mode::TokenNew => Command::TokenNew {
                config,
                name: name.ok_or_else(|| usage_error("--token-new needs a name"))?,
                rpm: rpm.unwrap_or(0),
                concurrency: concurrency.unwrap_or(0),
            },
            Mode::TokenList => Command::TokenList { config },
            Mode::TokenRevoke => Command::TokenRevoke {
                config,
                name: name.ok_or_else(|| usage_error("--token-revoke needs a name"))?,
            },
            Mode::Help => Command::Help,
        })
    }

    /// The file the operator named, if they named one.
    #[must_use]
    pub fn config_path(&self) -> Option<&Path> {
        match self {
            Command::Serve { config }
            | Command::Check { config }
            | Command::PrintConfig { config }
            | Command::TokenNew { config, .. }
            | Command::TokenList { config }
            | Command::TokenRevoke { config, .. } => config.as_deref(),
            Command::Help => None,
        }
    }
}

/// Answer whether the configuration is one this process can start with.
///
/// A deployment that issues tokens has two things that can be wrong before the
/// first request — a key file that cannot be read and a token file that cannot be
/// parsed — and both are read here for that reason. The unit runs this before it
/// starts anything, so a mistake fails the unit instead of every request that
/// depends on it.
pub fn check(path: Option<&Path>) -> Result<String, String> {
    let config = load(path)?;
    let mut answer = describe(&config);
    if config.access.enabled {
        answer.push('\n');
        answer.push_str(&access_report(&config)?);
    }
    Ok(answer)
}

/// What a deployment that issues tokens would start with.
fn access_report(config: &Config) -> Result<String, String> {
    let key_file = config
        .access
        .key_file
        .as_ref()
        .ok_or("access.key_file is required when access.enabled is true")?;
    let key = crate::edge::access::read_key(key_file)?;
    let document = crate::edge::access::Document::read(&config.access.tokens_file)?;
    let revoked = document.tokens.iter().filter(|token| token.revoked).count();
    let in_force = document.tokens.len() - revoked;
    let mut report = if document.tokens.is_empty() {
        format!(
            "access: no token issued yet, so every request will be refused; --token-new issues one into {}",
            config.access.tokens_file.display()
        )
    } else {
        format!(
            "access: {in_force} token(s) in force, {revoked} revoked, in {}",
            config.access.tokens_file.display()
        )
    };
    report.push_str(&format!("; key read from {}", key_file.display()));
    // Said rather than refused: the shape belongs to the upstream's client, and a
    // deployment whose key does not look like one may still be a deployment that
    // works — but an operator who pointed the setting at the wrong file wants to
    // hear about it now rather than from a wall of 401s.
    if crate::edge::access::key_looks_unusual(&key) {
        report.push_str("\nwarning: that key does not begin with `user_`, which is the shape the upstream issues");
    }
    Ok(report)
}

/// Issue a token and answer with it, once.
///
/// The token is printed rather than stored: what the file keeps is its sha256, so
/// the moment it is issued is the only moment it can be read. A deployment that does
/// not issue tokens refuses to issue one — a token nothing checks is a credential
/// that does not exist, and an operator who made one would believe otherwise.
pub fn token_new(path: Option<&Path>, name: &str, rpm: u32, concurrency: u32) -> Result<String, String> {
    let config = load(path)?;
    if !config.access.enabled {
        return Err(
            "access.enabled is false, so this deployment forwards its callers' keys rather than issuing tokens; set it to true to issue one"
                .to_owned(),
        );
    }
    let mut document = crate::edge::access::Document::read(&config.access.tokens_file)?;
    let token = document.issue(name, rpm, concurrency)?;
    document.write(&config.access.tokens_file)?;
    Ok(format!(
        "token `{name}` issued: {token}\nlimits: {}\nstored hashed in {}; shown here once, and a running deployment picks it up without a restart",
        limits(rpm, concurrency),
        config.access.tokens_file.display()
    ))
}

/// What this deployment has issued, one line per token.
pub fn token_list(path: Option<&Path>) -> Result<String, String> {
    let config = load(path)?;
    let document = crate::edge::access::Document::read(&config.access.tokens_file)?;
    let mut text = format!(
        "tokens {} in {}\n",
        document.tokens.len(),
        config.access.tokens_file.display()
    );
    for token in &document.tokens {
        text.push_str(&format!(
            "{} rpm={} concurrency={} created={} revoked={}\n",
            token.name,
            token.rpm,
            token.concurrency,
            stamp_of(token.created_at),
            if token.revoked { "yes" } else { "no" }
        ));
    }
    Ok(text)
}

/// Take a token away.
pub fn token_revoke(path: Option<&Path>, name: &str) -> Result<String, String> {
    let config = load(path)?;
    let mut document = crate::edge::access::Document::read(&config.access.tokens_file)?;
    document.revoke(name)?;
    document.write(&config.access.tokens_file)?;
    Ok(format!(
        "token `{name}` revoked: a running deployment refuses it from the next request on"
    ))
}

/// How a token's limits read to a person.
fn limits(rpm: u32, concurrency: u32) -> String {
    let rate = if rpm == 0 {
        "no limit on requests per minute".to_owned()
    } else {
        format!("{rpm} requests per minute")
    };
    let at_once = if concurrency == 0 {
        "no limit on requests at once".to_owned()
    } else {
        format!("{concurrency} requests at once")
    };
    format!("{rate}, {at_once}")
}

/// A unix timestamp as the RFC 3339 an operator reads.
fn stamp_of(seconds: i64) -> String {
    OffsetDateTime::from_unix_timestamp(seconds)
        .ok()
        .and_then(|at| at.format(&Rfc3339).ok())
        .unwrap_or_else(|| seconds.to_string())
}

/// The resolved configuration, as TOML with its secrets replaced.
pub fn print_config(path: Option<&Path>) -> Result<String, String> {
    let config = load(path)?;
    config.to_toml_redacted().map_err(|error| error.to_string())
}

/// One line naming what the deployment will be.
///
/// It is the line a unit's journal keeps out of the check, so it names the four
/// things worth seeing at a glance: where this listens, which dialect it speaks,
/// which service it speaks it to, and how loud it is.
#[must_use]
pub fn describe(config: &Config) -> String {
    format!(
        "configuration is valid: {}:{} speaking {} to {} (log {} {})",
        config.host,
        config.port,
        config.wire.adapter,
        config.api_base,
        config.telemetry.log_level.name(),
        config.telemetry.log_format.name(),
    )
}

fn load(path: Option<&Path>) -> Result<Config, String> {
    crate::config::load_from(path).map_err(|error| error.to_string())
}

/// Move into the mode an argument asked for, refusing to be two things at once.
fn choose(mode: Mode, wanted: Mode) -> Result<Mode, String> {
    match mode {
        Mode::Serve | Mode::Help => Ok(wanted),
        _ if mode == wanted => Ok(mode),
        _ => Err(usage_error(
            "--check, --print-config and the token commands answer different questions; ask one",
        )),
    }
}

fn usage_error(why: &str) -> String {
    format!("{why}\n\n{USAGE}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Command, String> {
        Command::parse(args.iter().map(|argument| (*argument).to_owned()))
    }

    #[test]
    fn no_arguments_is_the_server() {
        assert_eq!(parse(&[]).expect("parses"), Command::Serve { config: None });
    }

    /// A file the operator named is the file the *serving* process reads, not only
    /// the one the read-back commands read: an argument that is accepted and then
    /// ignored would leave a service running a configuration nobody chose.
    #[test]
    fn a_named_file_serves_from_that_file() {
        let command = parse(&["--config", "/etc/bifrost/other.toml"]).expect("parses");
        assert_eq!(command.config_path(), Some(Path::new("/etc/bifrost/other.toml")));
    }

    #[test]
    fn a_flag_names_the_question() {
        assert_eq!(parse(&["--check"]).expect("parses"), Command::Check { config: None });
        assert_eq!(
            parse(&["--print-config"]).expect("parses"),
            Command::PrintConfig { config: None }
        );
        assert_eq!(parse(&["-h"]).expect("parses"), Command::Help);
    }

    #[test]
    fn a_named_file_rides_along_in_either_order() {
        let command = parse(&["--check", "--config", "/etc/bifrost/other.toml"]).expect("parses");
        assert_eq!(command.config_path(), Some(Path::new("/etc/bifrost/other.toml")));
        // Which order the two arguments come in is not something an operator
        // should have to know.
        let command = parse(&["--config", "/etc/bifrost/other.toml", "--check"]).expect("parses");
        assert_eq!(command.config_path(), Some(Path::new("/etc/bifrost/other.toml")));
    }

    #[test]
    fn two_questions_at_once_are_refused() {
        let error = parse(&["--check", "--print-config"]).expect_err("must be refused");
        assert!(error.contains("ask one"), "{error}");
    }

    #[test]
    fn a_path_without_a_file_is_refused() {
        assert!(parse(&["--check", "--config"]).is_err());
    }

    #[test]
    fn an_unknown_argument_shows_the_usage() {
        let error = parse(&["--serve"]).expect_err("must be refused");
        assert!(error.contains("unknown argument `--serve`"), "{error}");
        assert!(error.contains("usage: bifrost"), "{error}");
    }

    #[test]
    fn the_line_names_what_the_deployment_will_be() {
        let line = describe(&Config::default());
        assert!(
            line.starts_with("configuration is valid: 0.0.0.0:3050 speaking cc/1.53.1"),
            "{line}"
        );
        assert!(line.contains("https://api.commandcode.ai"), "{line}");
        assert!(line.ends_with("(log info json)"), "{line}");
    }
}
