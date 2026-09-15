//! The command line.
//!
//! The process serves when it is given nothing and answers a question when it is
//! given a flag, and both questions belong to the deployment rather than to a
//! developer: a unit file runs `--check` before it starts anything, so a
//! configuration this build cannot use fails the unit instead of the first request
//! that needs it, and `--print-config` exists because the effective configuration
//! is built from three layers and only one of them is the file someone edited.
//!
//! `--journal`, `--verify`, `--audit` and `--turn` are the other half of the archive:
//! something keeps the bytes and the lines that name them, and neither is worth keeping
//! if reading them back means writing a script against the directory layout. `--journal`
//! and `--verify` answer one question each, and `--audit` asks the one an operator
//! asks after touching retention: of everything the lines name, how much does the
//! store still hold. `--turn` is the one that hands the bytes over, because a verdict
//! on a turn is not the same thing as being able to read what the client sent — and a
//! conversation is several turns, so the same selector that finds them in the journal
//! hands over all of their bytes. They read the same configuration the server does, so they answer
//! about the deployment the operator is looking at rather than about the directory
//! they happen to be standing in.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use bifrost_audit::{ArchiveRef, ArchiveStore, Held, digests_in, is_digest_name};
use bifrost_config::Config;
use serde_json::Value;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// What the operator asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Serve.
    Serve,
    /// Answer whether this configuration can be started with, and exit.
    Check { config: Option<PathBuf> },
    /// Print the resolved configuration, and exit.
    PrintConfig { config: Option<PathBuf> },
    /// Print the decision journal, and exit.
    Journal {
        config: Option<PathBuf>,
        since: Option<String>,
        kind: Option<String>,
        /// Only entries filed under this conversation.
        session: Option<String>,
        /// At most this many of the newest entries; 0 is all of them.
        limit: usize,
    },
    /// Hand over the turn a digest belongs to, or a whole conversation, and exit.
    Turn {
        config: Option<PathBuf>,
        /// Which turn: the digest of either of its halves.
        digest: Option<String>,
        /// Or which conversation: every turn of it is handed over, oldest first.
        session: Option<String>,
    },
    /// Count what the store still holds of everything the journal names, and exit.
    Audit {
        config: Option<PathBuf>,
        since: Option<String>,
        kind: Option<String>,
        session: Option<String>,
    },
    /// Say what is stored under a digest, and exit.
    Verify {
        config: Option<PathBuf>,
        digest: String,
        /// Also check that the stored bytes contain this text.
        quote: Option<String>,
    },
    /// Print how to use this, and exit.
    Help,
}

/// The exit code for a digest whose bytes are not in the archive.
///
/// A code of its own because it means something else than a failed claim: the turn
/// can still be read in the journal, and what is gone is the bytes a claim would be
/// checked against. A deployment whose retention is too short finds out this way.
pub const NOT_ARCHIVED: u8 = 3;

/// How to use this.
pub const USAGE: &str = "\
usage: bifrost [--check | --print-config | --journal | --audit | --turn [DIGEST] |
                --verify DIGEST] [--config PATH]

  (no arguments)  serve: run the gateway
  --check         validate the configuration and exit. A unit file runs this first,
                  so a bad file fails the unit rather than the first request
  --print-config  print the configuration this process resolved to, with secrets
                  redacted: the file and the environment are layered by then, which
                  is the reason for printing it at all
  --journal       print the decision journal, oldest entry first, one JSON line per
                  entry, exactly as it is stored. Nothing matching prints nothing
                  and exits 0
    --since TS    only entries at or after this RFC 3339 timestamp (--journal,
                  --audit). An entry whose timestamp cannot be read is shown either
                  way: a line that cannot be dated is not a line to drop
    --kind K      only entries of this kind, `turn` or `retention` (--journal,
                  --audit)
    --session ID  only entries filed under this conversation (--journal, --audit,
                  --turn). A client may name its own session, and this deployment
                  generates one per key when it does not. Unlike --since, this is an
                  equality: a line that names no session is not a line about this one
    --limit N     at most the newest N entries (--journal)
  --audit         count what the store still holds of everything the journal names.
                  One line of counts (`entries N, named N, intact N, tampered N,
                  gone N, malformed N`) and one line per finding. Exit 0 when every
                  digest named is intact, 1 when a blob no longer hashes to its name
                  or a name is not a digest, and 3 when the only finding is bytes
                  retention removed - the same code, with the same meaning, as
                  --verify
  --turn DIGEST   hand over the turn that digest belongs to: its journal line as it is
                  stored, and then, for each half, what --verify says about that half's
                  bytes followed by the bytes themselves. Identical bytes are stored
                  once, so more than one turn can name a digest, and every line that
                  names it is printed. Given --session instead of a digest, every turn
                  of that conversation is handed over, oldest first. Exit 0 when the
                  halves named are intact, 1 when one no longer hashes to its name, 3
                  when one is gone or was never archived. The bytes go out as they are stored - they are whatever the
                  client sent, so they are not necessarily text - and a newline follows
                  one only when it does not end in one, so the next line is its own
  --verify DIGEST say what is stored under a digest. Exit 0 when the bytes are
                  intact, 1 when they no longer hash to it or when --quote is not in
                  them, 3 when nothing is archived under it - which is what a pass of
                  retention leaves behind, and is not the same as a false claim
    --quote TEXT  also check that the stored bytes contain this text
  --config PATH   read this file instead of $BIFROST_CONFIG or ./bifrost.toml
  -h, --help      print this";

/// Which question was asked, before the path that came with it is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Serve,
    Check,
    PrintConfig,
    Journal,
    Audit,
    Turn,
    Verify,
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
        let mut digest: Option<String> = None;
        let mut since: Option<String> = None;
        let mut kind: Option<String> = None;
        let mut session: Option<String> = None;
        let mut limit: Option<usize> = None;
        let mut quote: Option<String> = None;
        let mut args = args.into_iter().peekable();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--check" => mode = choose(mode, Mode::Check)?,
                "--print-config" => mode = choose(mode, Mode::PrintConfig)?,
                "--journal" => mode = choose(mode, Mode::Journal)?,
                "--audit" => mode = choose(mode, Mode::Audit)?,
                "--turn" => {
                    mode = choose(mode, Mode::Turn)?;
                    // The key is taken only when one is actually there: with `--session`
                    // and no digest the answer is a whole conversation. Another flag is
                    // left where it is, which is what lets the loop below refuse one
                    // nobody knows instead of searching for it as a digest.
                    if args.peek().is_some_and(|next| !next.starts_with('-')) {
                        digest = args.next();
                    }
                }
                "--verify" => {
                    mode = choose(mode, Mode::Verify)?;
                    digest = Some(args.next().ok_or_else(|| usage_error("--verify needs a digest"))?);
                }
                "--config" => {
                    let path = args.next().ok_or_else(|| usage_error("--config needs a path"))?;
                    config = Some(PathBuf::from(path));
                }
                "--since" => {
                    since = Some(
                        args.next()
                            .ok_or_else(|| usage_error("--since needs an RFC 3339 timestamp"))?,
                    );
                }
                "--kind" => {
                    kind = Some(args.next().ok_or_else(|| usage_error("--kind needs a value"))?);
                }
                "--session" => {
                    session = Some(args.next().ok_or_else(|| usage_error("--session needs an id"))?);
                }
                "--limit" => {
                    let value = args.next().ok_or_else(|| usage_error("--limit needs a number"))?;
                    limit = Some(
                        value
                            .parse()
                            .map_err(|_| usage_error(&format!("--limit needs a number, got `{value}`")))?,
                    );
                }
                "--quote" => {
                    quote = Some(args.next().ok_or_else(|| usage_error("--quote needs text"))?);
                }
                "-h" | "--help" => mode = Mode::Help,
                other => return Err(usage_error(&format!("unknown argument `{other}`"))),
            }
        }
        // A flag that changes nothing is the thing this build keeps removing from its
        // configuration, so each filter has to belong to the question it filters.
        if (since.is_some() || kind.is_some()) && !matches!(mode, Mode::Journal | Mode::Audit) {
            return Err(usage_error(
                "--since and --kind only mean something with --journal or --audit",
            ));
        }
        // An audit of the newest few entries would be an answer to a question nobody
        // asked: the point of it is to cover everything the journal names.
        if limit.is_some() && mode != Mode::Journal {
            return Err(usage_error("--limit only means something with --journal"));
        }
        // A session selects entries, so every question that selects entries takes it:
        // the lines (`--journal`), the counts (`--audit`) and the bytes (`--turn`).
        if session.is_some() && !matches!(mode, Mode::Journal | Mode::Audit | Mode::Turn) {
            return Err(usage_error(
                "--session only means something with --journal, --audit or --turn",
            ));
        }
        // One key, one question. A digest says which turn and a session says which
        // conversation, and asking both would only be a way to get an empty answer.
        if mode == Mode::Turn && digest.is_some() && session.is_some() {
            return Err(usage_error("--turn takes a digest or a --session, not both"));
        }
        if quote.is_some() && mode != Mode::Verify {
            return Err(usage_error("--quote only means something with --verify"));
        }
        Ok(match mode {
            Mode::Serve => Command::Serve,
            Mode::Check => Command::Check { config },
            Mode::PrintConfig => Command::PrintConfig { config },
            Mode::Journal => Command::Journal {
                config,
                since,
                kind,
                session,
                limit: limit.unwrap_or(0),
            },
            Mode::Audit => Command::Audit {
                config,
                since,
                kind,
                session,
            },
            Mode::Turn => {
                if digest.is_none() && session.is_none() {
                    return Err(usage_error(
                        "--turn needs a digest, or a --session to hand over a whole conversation",
                    ));
                }
                Command::Turn {
                    config,
                    digest,
                    session,
                }
            }
            Mode::Verify => Command::Verify {
                config,
                digest: digest.ok_or_else(|| usage_error("--verify needs a digest"))?,
                quote,
            },
            Mode::Help => Command::Help,
        })
    }

    /// The file the operator named, if they named one.
    #[must_use]
    pub fn config_path(&self) -> Option<&Path> {
        match self {
            Command::Check { config }
            | Command::PrintConfig { config }
            | Command::Journal { config, .. }
            | Command::Audit { config, .. }
            | Command::Turn { config, .. }
            | Command::Verify { config, .. } => config.as_deref(),
            Command::Serve | Command::Help => None,
        }
    }
}

/// Answer whether the configuration is one this process can start with.
pub fn check(path: Option<&Path>) -> Result<String, String> {
    let config = load(path)?;
    Ok(describe(&config))
}

/// The resolved configuration, as TOML with its secrets replaced.
pub fn print_config(path: Option<&Path>) -> Result<String, String> {
    let config = load(path)?;
    config.to_toml_redacted().map_err(|error| error.to_string())
}

/// The journal, oldest entry first, one line per entry.
///
/// The lines are printed as they are stored rather than re-rendered into a table: the
/// journal is a file an operator greps, and a command that reformatted it would make
/// the two disagree about what it says. Filtering is the only thing added, and an
/// entry whose timestamp cannot be read is kept by `--since` — dropping a line for
/// being undatable would be a query answering with less than it was given.
pub fn journal(
    path: Option<&Path>,
    since: Option<&str>,
    kind: Option<&str>,
    session: Option<&str>,
    limit: usize,
) -> Result<String, String> {
    let config = load(path)?;
    let (_, entries) = read_journal(&config)?;
    let since = since.map(parse_stamp).transpose()?;
    let mut rows = kept(&entries, since, kind, session);
    if limit > 0 && rows.len() > limit {
        // The newest ones: an operator looks at a journal from the end.
        rows = rows.split_off(rows.len() - limit);
    }

    let mut text = String::new();
    for entry in rows {
        text.push_str(&serde_json::to_string(entry).map_err(|error| error.to_string())?);
        text.push('\n');
    }
    Ok(text)
}

/// An audit of the whole archive, which answers with counts and a code.
///
/// `--verify` is one digest and one answer, which is the wrong shape for the question
/// an operator asks after touching retention, or a week after a disk incident: of
/// everything the journal names, how much does the store still hold? Answering it is
/// where the two halves of the archive meet, because only the journal knows which
/// digests a turn was made of, and only the store knows which of them are still there.
///
/// The counts are per digest and not per reference: identical bytes are stored once, so
/// a turn whose two halves are the same bytes names one digest twice, and counting it
/// twice would report an archive that holds less than it does.
///
/// The answer is a line of counts, always, and then one line per kind of finding rather
/// than a summary sentence. A monitor greps the first line, and a person reads the
/// rest — a run with nothing to report prints the counts and one line saying so, since
/// a silent exit 0 could also mean the command never ran.
pub fn audit(
    path: Option<&Path>,
    since: Option<&str>,
    kind: Option<&str>,
    session: Option<&str>,
) -> Result<Verified, String> {
    let config = load(path)?;
    let (_, entries) = read_journal(&config)?;
    let since = since.map(parse_stamp).transpose()?;
    let rows = kept(&entries, since, kind, session);

    let mut names: BTreeSet<String> = BTreeSet::new();
    for entry in &rows {
        names.extend(digests_in(entry));
    }

    let store = ArchiveStore::new(config.audit.archive_dir);
    let mut intact = 0u64;
    let mut tampered = 0u64;
    let mut gone = 0u64;
    let mut malformed = 0u64;
    for name in &names {
        // A name the store could never have held bytes under is a finding about the
        // journal rather than something to look up: it is counted apart, so that it is
        // neither dropped for being unreadable nor reported as bytes that went missing.
        if !is_digest_name(name) {
            malformed += 1;
            continue;
        }
        match store
            .held(name)
            .map_err(|error| format!("could not look up {name}: {error}"))?
        {
            Held::Intact { .. } => intact += 1,
            Held::Tampered { .. } => tampered += 1,
            Held::Gone => gone += 1,
        }
    }

    let mut text = format!(
        "entries {}, named {}, intact {intact}, tampered {tampered}, gone {gone}, malformed {malformed}\n",
        rows.len(),
        names.len()
    );
    if malformed > 0 {
        text.push_str(&format!(
            "not a digest, {malformed}: the journal names something no writer of this build produces\n"
        ));
    }
    if tampered > 0 {
        text.push_str(&format!(
            "no longer matching, {tampered}: the bytes stored under those names no longer hash to them, so the disk is what to look at\n"
        ));
    }
    if gone > 0 {
        text.push_str(&format!(
            "no longer archived, {gone}: the store does not hold those bytes, and the lines that name them are still readable - retention is what removes them\n"
        ));
    }
    if names.is_empty() {
        text.push_str("the journal names no digest: nothing was archived, or the filter kept no turn\n");
    } else if malformed == 0 && tampered == 0 && gone == 0 {
        text.push_str("every digest the journal names is intact\n");
    }

    // A blob that no longer hashes to its name and a name that is not a digest are both
    // things the disk or the journal got wrong, so one code covers them. Bytes that are
    // gone are the deployment's own retention, and they get the code `--verify` gives
    // them, meaning the same thing: what is missing was removed on purpose.
    let code = if malformed > 0 || tampered > 0 {
        1
    } else if gone > 0 {
        NOT_ARCHIVED
    } else {
        0
    };
    Ok(Verified { text, code })
}

/// The journal this deployment names, read back, or the reason it could not be.
///
/// Both archive questions start here, and both have to answer the same way when there
/// is no journal at all: the path is what the operator needs to see, because the usual
/// cause is a deployment that has not finished a turn rather than a missing file.
fn read_journal(config: &Config) -> Result<(PathBuf, Vec<Value>), String> {
    let journal = bifrost_audit::Journal::new(config.audit.journal_dir.clone());
    match journal.read_all() {
        Ok(entries) => Ok((journal.path().to_owned(), entries)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "there is no journal at {}: the archive is off, or this deployment has not finished a turn",
            journal.path().display()
        )),
        Err(error) => Err(format!("could not read {}: {error}", journal.path().display())),
    }
}

/// The entries the filters keep, in the order the journal holds them.
///
/// Every question that selects entries shares this, so none of them can come to disagree
/// about what `--since 2026-01-01` or `--session abc` selects.
///
/// The two kinds of filter answer different questions. `--since` asks *when*, and an
/// entry whose timestamp cannot be read cannot be placed against the cutoff — it is kept,
/// because a query that drops what it cannot date is answering with less than it was
/// given. `--kind` and `--session` ask *what*, and there is no such doubt: a line that
/// names no session is not a line about that conversation, so it is not a match.
fn kept<'a>(
    entries: &'a [Value],
    since: Option<SystemTime>,
    kind: Option<&str>,
    session: Option<&str>,
) -> Vec<&'a Value> {
    entries
        .iter()
        .filter(|entry| {
            let of_kind = kind.is_none_or(|wanted| entry.get("kind").and_then(Value::as_str) == Some(wanted));
            let in_window = since.is_none_or(|since| {
                entry
                    .get("timestamp")
                    .and_then(Value::as_str)
                    .and_then(stamp)
                    .is_none_or(|at| at >= since)
            });
            let of_session = session.is_none_or(|wanted| session_of(entry) == Some(wanted));
            of_kind && in_window && of_session
        })
        .collect()
}

/// The conversation a line is filed under, when it is filed under one.
///
/// A line that names none is not filed under the empty session: it is a line about
/// something else, which is what a retention pass leaves behind.
fn session_of(entry: &Value) -> Option<&str> {
    entry.pointer("/data/session").and_then(Value::as_str)
}

/// What a verification found, and what a shell should make of it.
pub struct Verified {
    pub text: String,
    pub code: u8,
}

/// Say what is stored under a digest, and whether it still holds those bytes.
///
/// Three answers rather than two, because "the bytes no longer hash to their name" and
/// "there are no bytes" lead to different work: the first is a disk to look at, and the
/// second is retention the operator set. Only the first is a claim that failed.
pub fn verify(path: Option<&Path>, digest: &str, quote: Option<&str>) -> Result<Verified, String> {
    let config = load(path)?;
    let store = ArchiveStore::new(config.audit.archive_dir);
    let held = store
        .held(digest)
        .map_err(|error| format!("could not look up {digest}: {error}"))?;
    let mut text = held_line(digest, &held);
    let code = match held {
        Held::Intact { kind, bytes, lines } => {
            if let Some(quote) = quote {
                let reference = ArchiveRef {
                    sha256: digest.to_owned(),
                    bytes,
                    lines,
                };
                let found = store
                    .verify_quote(&kind, &reference, quote.as_bytes())
                    .map_err(|error| format!("could not read {digest}: {error}"))?;
                if !found {
                    return Ok(Verified {
                        text: format!("absent   {digest}: the stored bytes do not contain that quote"),
                        code: 1,
                    });
                }
                text.push_str("; and the quote is in them");
            }
            0
        }
        Held::Tampered { .. } => 1,
        // What retention leaves behind: the turn is still readable in the journal, and
        // the bytes a claim about it would be checked against are gone.
        Held::Gone => NOT_ARCHIVED,
    };
    Ok(Verified { text, code })
}

/// What the store says about a digest, in the words `--verify` answers with.
///
/// `--turn` prints this for each half of a turn, so the two commands cannot come to
/// describe the same digest differently: there is one sentence per answer, and both
/// callers use it.
fn held_line(digest: &str, held: &Held) -> String {
    match held {
        Held::Intact { kind, bytes, lines } => {
            format!("intact   {digest} in {kind}: {bytes} bytes, {lines} lines")
        }
        Held::Tampered { kind } => {
            format!("tampered {digest} under {kind}: the bytes stored under that name no longer hash to it")
        }
        Held::Gone => {
            format!("gone     {digest}: nothing is archived under this digest; a journal line may still name it")
        }
    }
}

/// The two halves of a turn, in the order a line records them.
const HALVES: [&str; 2] = ["request", "response"];

/// What one half of a turn came to.
///
/// The two ways a half can fail to be there are kept apart from each other in the text
/// and together in the code: a half the line never recorded and a half whose bytes are
/// gone both mean the bytes are not in the archive, which is what the code is about.
enum Half {
    Intact,
    NotThere,
    Tampered,
}

/// Hand over the bytes of one turn, or of a whole conversation, writing them out.
///
/// A turn is two halves of bytes — what the client sent and what came back — and the
/// journal keeps their digests rather than the bytes. This is the command that goes the
/// other way: the line as it is stored, then each half's verdict and the bytes behind it.
/// Without it the archive is write-only, which for evidence is not worth much.
///
/// Two things can be selected, and they are the two units a reader has: a digest, which
/// names one turn by half of its bytes, and a session, which names the conversation a
/// client was having — one conversation is several turns, so a session hands over every
/// line filed under it, oldest first. Which is which is decided by the flag that was
/// given rather than by the shape of the argument, because a session id is the client's
/// to choose and there is no shape to rely on.
pub fn turn(
    path: Option<&Path>,
    digest: Option<&str>,
    session: Option<&str>,
    out: &mut impl Write,
) -> Result<u8, String> {
    let config = load(path)?;
    let (journal, entries) = read_journal(&config)?;
    let store = ArchiveStore::new(config.audit.archive_dir);

    let what = match (digest, session) {
        (Some(digest), _) => {
            format!("names {digest}: this looks for the digest of either half of a turn")
        }
        (None, Some(session)) => format!(
            "is filed under session {session}: a client may name its own session, and this deployment generates one per key when it does not"
        ),
        // The parser refuses this, so reaching it means the two disagree; handing over the
        // whole journal would be a worse way to find out.
        (None, None) => return Err("--turn needs a digest or a --session".to_owned()),
    };
    let selected: Vec<&Value> = entries
        .iter()
        .filter(|entry| match (digest, session) {
            (Some(digest), _) => names(entry, digest),
            (None, Some(session)) => session_of(entry) == Some(session),
            (None, None) => false,
        })
        .collect();
    if selected.is_empty() {
        // The journal that was read is named, because the usual way to ask this about the
        // wrong deployment is to be standing in the wrong directory.
        return Err(format!("no turn in {} {what}", journal.display()));
    }

    hand_over(&store, &selected, out)
}

/// Print the selected lines, and for each half what became of it and its bytes.
///
/// One place for both ways of asking, because what the reader does with the answer is the
/// same either way. The bytes are written out rather than returned as text because they
/// are not text: they are whatever the client sent, and a command that decoded them into
/// a `String` would hand back something other than what it kept. That is also why the
/// answer goes out as it is produced instead of being assembled first — a conversation can
/// be megabytes — and why a failure part way through leaves what was already printed.
///
/// More than one line can name a digest, because identical bytes are stored once, so
/// every line that names it is handed over: a digest cannot say which turn it belonged
/// to, and the lines are what can.
fn hand_over(store: &ArchiveStore, lines: &[&Value], out: &mut impl Write) -> Result<u8, String> {
    let mut tampered = false;
    let mut not_there = false;
    for &entry in lines {
        // The line goes out as it is stored, so what the reader holds afterwards is the
        // same record `--journal` prints, and the halves below are read out of it.
        let line = serde_json::to_string(entry).map_err(|error| error.to_string())?;
        write(out, line.as_bytes())?;
        write(out, b"\n")?;
        for half in HALVES {
            match one_half(store, entry, half, out)? {
                Half::Intact => {}
                Half::NotThere => not_there = true,
                Half::Tampered => tampered = true,
            }
        }
    }
    // A blob that no longer hashes to its name is the one to act on, so it outranks
    // bytes that are simply not there.
    Ok(if tampered {
        1
    } else if not_there {
        NOT_ARCHIVED
    } else {
        0
    })
}

/// Whether a line names `digest` as one of the two halves of its turn.
fn names(entry: &Value, digest: &str) -> bool {
    HALVES
        .iter()
        .any(|half| entry.pointer(&format!("/data/{half}/sha256")).and_then(Value::as_str) == Some(digest))
}

/// Write out one half of a turn, and say what became of it.
fn one_half(store: &ArchiveStore, entry: &Value, half: &str, out: &mut impl Write) -> Result<Half, String> {
    // A half the writer recorded as absent is a half the reader has to be told about:
    // "nothing was archived for this half" is an answer, and passing over it silently
    // would make a turn with one half look like a turn that was never sent.
    let Some(digest) = entry.pointer(&format!("/data/{half}/sha256")).and_then(Value::as_str) else {
        let text = format!("missing  {half}: the line records no such half, so nothing was archived for it\n");
        write(out, text.as_bytes())?;
        return Ok(Half::NotThere);
    };
    let held = store
        .held(digest)
        .map_err(|error| format!("could not look up {digest}: {error}"))?;
    let mut text = held_line(digest, &held);
    if entry
        .pointer(&format!("/data/{half}/truncated"))
        .and_then(Value::as_bool)
        == Some(true)
    {
        // Handing over a prefix as if it were the whole answer is worse than saying so.
        text.push_str(" (truncated: the rest was not kept)");
    }
    text.push('\n');
    write(out, text.as_bytes())?;

    match held {
        Held::Intact { kind, bytes, lines } => {
            let reference = ArchiveRef {
                sha256: digest.to_owned(),
                bytes,
                lines,
            };
            let body = store
                .read(&kind, &reference)
                .map_err(|error| format!("could not read {digest}: {error}"))?;
            write(out, &body)?;
            // Body and header are on stdout together, and this one is a file format as
            // much as a report: a newline is added only when the bytes do not end in
            // one, so the next line is a line of its own — and the count in the line
            // above is what says exactly how many bytes were printed.
            if !body.ends_with(b"\n") {
                write(out, b"\n")?;
            }
            Ok(Half::Intact)
        }
        Held::Tampered { .. } => Ok(Half::Tampered),
        Held::Gone => Ok(Half::NotThere),
    }
}

/// Write, or say which command could not finish and why.
fn write(out: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    out.write_all(bytes)
        .map_err(|error| format!("could not write the turn: {error}"))
}

/// The instant an RFC 3339 timestamp names.
fn stamp(text: &str) -> Option<SystemTime> {
    OffsetDateTime::parse(text, &Rfc3339).ok().map(SystemTime::from)
}

/// The same, as an answer to the operator when it does not parse.
fn parse_stamp(text: &str) -> Result<SystemTime, String> {
    stamp(text).ok_or_else(|| format!("--since needs an RFC 3339 timestamp, got `{text}`"))
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
    bifrost_config::load_from(path).map_err(|error| error.to_string())
}

/// Move into the mode an argument asked for, refusing to be two things at once.
fn choose(mode: Mode, wanted: Mode) -> Result<Mode, String> {
    match mode {
        Mode::Serve | Mode::Help => Ok(wanted),
        _ if mode == wanted => Ok(mode),
        _ => Err(usage_error(
            "--check, --print-config, --journal, --audit, --turn and --verify answer different questions; ask one",
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
        assert_eq!(parse(&[]).expect("parses"), Command::Serve);
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
    fn the_archive_questions_are_asked_with_their_filters() {
        assert_eq!(
            parse(&["--journal"]).expect("parses"),
            Command::Journal {
                config: None,
                since: None,
                kind: None,
                session: None,
                limit: 0,
            }
        );
        assert_eq!(
            parse(&[
                "--journal",
                "--kind",
                "turn",
                "--since",
                "2026-01-01T00:00:00Z",
                "--session",
                "session-abcdefgh",
                "--limit",
                "5"
            ])
            .expect("parses"),
            Command::Journal {
                config: None,
                since: Some("2026-01-01T00:00:00Z".to_owned()),
                kind: Some("turn".to_owned()),
                session: Some("session-abcdefgh".to_owned()),
                limit: 5,
            }
        );
        assert_eq!(
            parse(&["--journal", "--since", "2026-01-01T00:00:00Z"]).expect("parses"),
            Command::Journal {
                config: None,
                since: Some("2026-01-01T00:00:00Z".to_owned()),
                kind: None,
                session: None,
                limit: 0,
            }
        );
        assert_eq!(
            parse(&["--verify", "ab".repeat(32).as_str(), "--quote", "hello"]).expect("parses"),
            Command::Verify {
                config: None,
                digest: "ab".repeat(32),
                quote: Some("hello".to_owned()),
            }
        );
    }

    /// A filter that changes nothing is the thing this build keeps removing from its
    /// configuration, so it is refused rather than accepted and ignored.
    #[test]
    fn a_filter_without_the_question_it_belongs_to_is_refused() {
        for args in [
            vec!["--since", "2026-01-01T00:00:00Z"],
            vec!["--kind", "turn"],
            vec!["--check", "--kind", "turn"],
            vec!["--verify", "ab", "--since", "2026-01-01T00:00:00Z"],
        ] {
            let error = parse(&args).expect_err("must be refused");
            assert!(
                error.contains("--since and --kind only mean something with --journal or --audit"),
                "{args:?}: {error}"
            );
        }
        // The newest few is a way to read a journal, and an audit that looked at part
        // of one would be answering a question nobody asked.
        for args in [
            vec!["--limit", "5"],
            vec!["--check", "--limit", "5"],
            vec!["--audit", "--limit", "5"],
        ] {
            let error = parse(&args).expect_err("must be refused");
            assert!(
                error.contains("--limit only means something with --journal"),
                "{args:?}: {error}"
            );
        }
        let error = parse(&["--check", "--quote", "hi"]).expect_err("must be refused");
        assert!(error.contains("only means something with --verify"), "{error}");
    }

    /// The audit is the journal's filters over the store's question, so it takes both
    /// of the ones that select entries.
    #[test]
    fn the_audit_is_asked_with_the_filters_the_journal_takes() {
        assert_eq!(
            parse(&["--audit"]).expect("parses"),
            Command::Audit {
                config: None,
                since: None,
                kind: None,
                session: None,
            }
        );
        assert_eq!(
            parse(&["--audit", "--kind", "turn", "--since", "2026-01-01T00:00:00Z"]).expect("parses"),
            Command::Audit {
                config: None,
                since: Some("2026-01-01T00:00:00Z".to_owned()),
                kind: Some("turn".to_owned()),
                session: None,
            }
        );
        let command = parse(&["--audit", "--config", "/etc/bifrost/other.toml"]).expect("parses");
        assert_eq!(command.config_path(), Some(Path::new("/etc/bifrost/other.toml")));
    }

    #[test]
    fn the_archive_questions_need_their_arguments() {
        let error = parse(&["--verify"]).expect_err("needs a digest");
        assert!(error.contains("--verify needs a digest"), "{error}");
        let error = parse(&["--journal", "--limit", "five"]).expect_err("needs a number");
        assert!(error.contains("--limit needs a number"), "{error}");
        assert!(parse(&["--journal", "--since"]).is_err());
        assert!(parse(&["--journal", "--kind"]).is_err());
        assert!(parse(&["--verify", "ab", "--quote"]).is_err());
        let error = parse(&["--turn"]).expect_err("needs a key");
        assert!(error.contains("--turn needs a digest, or a --session"), "{error}");
    }

    /// Handing bytes over is keyed either by a digest — the only name the journal keeps
    /// for one turn's bytes — or by a session, which names a conversation and therefore
    /// several turns. Which one it is comes from the flag, never from the shape of the
    /// argument: a session id is the client's to choose.
    #[test]
    fn the_turn_is_asked_for_by_a_digest_or_by_a_session() {
        let digest = "ab".repeat(32);
        assert_eq!(
            parse(&["--turn", &digest]).expect("parses"),
            Command::Turn {
                config: None,
                digest: Some(digest.clone()),
                session: None,
            }
        );
        let command = parse(&["--turn", &digest, "--config", "/etc/bifrost/other.toml"]).expect("parses");
        assert_eq!(command.config_path(), Some(Path::new("/etc/bifrost/other.toml")));
        assert_eq!(
            parse(&["--turn", "--session", "session-abcdefgh"]).expect("parses"),
            Command::Turn {
                config: None,
                digest: None,
                session: Some("session-abcdefgh".to_owned()),
            }
        );
        // A filter it does not take is refused rather than accepted and ignored: the
        // key already says which turn, and a window could only hide the answer.
        let error = parse(&["--turn", &digest, "--since", "2026-01-01T00:00:00Z"]).expect_err("refused");
        assert!(error.contains("--since and --kind only mean something"), "{error}");
        // One key, one question: asking for both would only be a way to get nothing.
        let error = parse(&["--turn", &digest, "--session", "session-abcdefgh"]).expect_err("refused");
        assert!(
            error.contains("--turn takes a digest or a --session, not both"),
            "{error}"
        );
        // A session is a selector over entries, so it belongs to the questions that
        // select them and nowhere else.
        for args in [
            vec!["--check", "--session", "s"],
            vec!["--verify", "ab", "--session", "s"],
            vec!["--session", "s"],
        ] {
            let error = parse(&args).expect_err("must be refused");
            assert!(error.contains("--session only means something"), "{args:?}: {error}");
        }
        // A flag where a digest would go is left to the loop, so an unknown one is
        // refused as unknown rather than searched for as a digest, and a known one is
        // the combination that has to work.
        let error = parse(&["--turn", "--nonsense"]).expect_err("must be refused");
        assert!(error.contains("unknown argument `--nonsense`"), "{error}");
        let error = parse(&["--turn", "--session"]).expect_err("needs an id");
        assert!(error.contains("--session needs an id"), "{error}");
    }

    #[test]
    fn the_archive_questions_are_not_asked_two_at_a_time() {
        for args in [
            vec!["--journal", "--verify", "ab"],
            vec!["--audit", "--journal"],
            vec!["--audit", "--check"],
            vec!["--turn", "ab", "--verify", "cd"],
            vec!["--journal", "--turn", "ab"],
        ] {
            let error = parse(&args).expect_err("must be refused");
            assert!(error.contains("ask one"), "{args:?}: {error}");
        }
    }

    #[test]
    fn a_timestamp_is_read_as_the_instant_it_names() {
        let earlier = stamp("2026-01-01T00:00:00Z").expect("parses");
        let later = stamp("2026-01-02T00:00:00Z").expect("parses");
        assert!(later > earlier, "a later timestamp must order after an earlier one");
        assert_eq!(stamp("yesterday"), None);

        let error = parse_stamp("yesterday").expect_err("must be refused");
        assert!(error.contains("RFC 3339"), "{error}");
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
