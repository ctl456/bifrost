//! The command line, run as the process a unit file runs.
//!
//! These spawn the binary rather than calling the parser, because what a deployment
//! depends on is an exit code: a unit file reads `0` or it reads non-zero, and an
//! exit code produced by a function nothing calls is not the one systemd would see.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bifrost_audit::{ArchiveRef, ArchiveStore, KIND_REQUEST, KIND_UPSTREAM_RESPONSE};
use bifrost_config::{Config, REDACTED};

/// The binary this file is about.
const BIN: &str = env!("CARGO_BIN_EXE_bifrost");

/// A configuration file of its own per call, so tests running at once cannot see
/// each other's.
fn write(text: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "bifrost-cli-{}-{}.toml",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    );
    let path = std::env::temp_dir().join(name);
    std::fs::write(&path, text).expect("the file is written");
    path
}

/// A deployment of its own: a directory, a configuration naming it, and nothing in
/// it. The archive and the journal are read and written by the binary under test, so
/// a test only has to prepare the side it is about.
fn deployment(label: &str) -> (PathBuf, PathBuf) {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("bifrost-cli-archive-{}-{label}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("the deployment directory is created");
    let config = write(&format!(
        "[audit]\njournal_dir = \"{}\"\narchive_dir = \"{}\"\n",
        root.join("journal").display(),
        root.join("archive").display()
    ));
    (root, config)
}

/// Three entries, in order, of the two kinds a deployment writes.
const JOURNAL: &str = concat!(
    "{\"timestamp\":\"2026-01-01T00:00:00Z\",\"kind\":\"turn\",\"data\":{\"model\":\"m\"}}\n",
    "{\"timestamp\":\"2026-01-02T00:00:00Z\",\"kind\":\"retention\",\"data\":{\"removed\":1}}\n",
    "{\"timestamp\":\"2026-01-03T00:00:00Z\",\"kind\":\"turn\",\"data\":{\"model\":\"m\"}}\n",
);

/// One journal line of the kind a turn writes: the timestamp, the model, and the digest
/// of the half of the turn that was archived. `--audit` reads what the lines name, so a
/// test needs the lines and the store rather than a whole serving process.
fn turn_line(at: &str, digest: &str) -> String {
    format!(
        "{{\"timestamp\":\"{at}\",\"kind\":\"turn\",\"data\":{{\"model\":\"m\",\"request\":{{\"sha256\":\"{digest}\",\"bytes\":3,\"lines\":1}}}}}}\n"
    )
}

/// The half of a journal line that names bytes, with the flags the writer records.
fn half_of(reference: &ArchiveRef, truncated: bool) -> String {
    format!(
        "{{\"sha256\":\"{}\",\"bytes\":{},\"lines\":{},\"sensitive\":false,\"truncated\":{truncated}}}",
        reference.sha256, reference.bytes, reference.lines
    )
}

/// One journal line whose two halves are given as the JSON halves themselves, so a test
/// can hand `null` to it for the half that was never archived.
fn line_with_halves(at: &str, session: &str, request: &str, response: &str) -> String {
    format!(
        "{{\"timestamp\":\"{at}\",\"kind\":\"turn\",\"data\":{{\"protocol\":\"openai-chat\",\"model\":\"m\",\"stream\":false,\"status\":200,\"session\":\"{session}\",\"key\":\"aaaa\",\"request\":{request},\"response\":{response}}}}}\n"
    )
}

/// A pass of retention, which names no bytes of its own.
const RETENTION_LINE: &str =
    "{\"timestamp\":\"2026-01-02T00:00:00Z\",\"kind\":\"retention\",\"data\":{\"removed\":1}}\n";

fn path_of(path: &Path) -> &str {
    path.to_str().expect("a utf-8 path")
}

/// Run the binary with nothing in its environment but what the test sets.
///
/// The environment is cleared because every one of these settings can also come
/// from it: a test that inherited the developer's `PORT` would be a test about the
/// developer's shell.
fn run(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new(BIN);
    command.env_clear().args(args);
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("the binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_configuration_this_build_can_use_is_accepted_quietly() {
    let file = write("port = 4055\n");
    let output = run(&["--check"], &[("BIFROST_CONFIG", path_of(&file))]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("4055"), "{}", stdout(&output));
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
}

/// The failure has to name the thing to fix: a unit that refuses to start and says
/// only "invalid config" costs more than the check saves.
#[test]
fn a_configuration_this_build_cannot_use_fails_the_check() {
    for (text, expected) in [
        ("[wire]\nadapter = \"cc/9\"\n", "wire.adapter"),
        ("prot = 4055\n", "unknown field"),
        ("port = 0\n", "port"),
    ] {
        let file = write(text);
        let output = run(&["--check"], &[("BIFROST_CONFIG", path_of(&file))]);
        assert_eq!(output.status.code(), Some(1), "{text:?} should fail");
        assert!(stderr(&output).contains(expected), "{text:?} said: {}", stderr(&output));
        assert!(stdout(&output).is_empty(), "{text:?} answered: {}", stdout(&output));
    }
}

#[test]
fn a_file_that_is_not_there_is_a_failure_rather_than_a_default() {
    let missing = std::env::temp_dir().join("bifrost-cli-there-is-no-such-file.toml");
    let output = run(&["--check"], &[("BIFROST_CONFIG", path_of(&missing))]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("failed to read config file"),
        "{}",
        stderr(&output)
    );
}

/// What the check is for: asking about a file that is about to be installed, not
/// about the one that is already there.
#[test]
fn the_file_named_out_loud_wins_over_the_environment() {
    let candidate = write("port = 4055\n");
    let installed = write("[wire]\nadapter = \"cc/9\"\n");
    let output = run(
        &["--check", "--config", path_of(&candidate)],
        &[("BIFROST_CONFIG", path_of(&installed))],
    );

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("4055"), "{}", stdout(&output));
}

/// The check is what a unit runs before it starts, so it must not take the socket
/// it is about to hand to the service.
#[test]
fn the_check_binds_nothing() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("a port of our own");
    let port = listener.local_addr().expect("the address").port();
    let file = write(&format!("host = \"127.0.0.1\"\nport = {port}\n"));

    let output = run(&["--check"], &[("BIFROST_CONFIG", path_of(&file))]);

    assert_eq!(
        output.status.code(),
        Some(0),
        "a busy port must not fail a check that binds nothing: {}",
        stderr(&output)
    );
}

/// The file someone edited is one layer of three; this prints the other two.
#[test]
fn printing_the_configuration_shows_the_layers_that_were_applied() {
    let file = write("port = 4055\n");
    let output = run(
        &["--print-config", "--config", path_of(&file)],
        &[("CC_API_BASE", "https://other.test")],
    );

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let printed = stdout(&output);
    let config = Config::from_toml_str(&printed).expect("what is printed is a configuration");
    assert_eq!(config.port, 4055);
    assert_eq!(config.api_base, "https://other.test");
}

/// A serving process reads the file `--config` names.
///
/// The check is the one an operator would notice: the file is invalid, so a process
/// that read it must fail before it listens, and name the setting. A process that
/// ignored the argument would serve the defaults instead — and would then be found
/// only by whoever wondered why the rules in that file did nothing.
#[test]
fn a_serving_process_reads_the_file_config_names() {
    let file = write("port = 0\n");
    let mut child = Command::new(BIN)
        .env_clear()
        .args(["--config", path_of(&file)])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the binary runs");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut exited = None;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("wait") {
            exited = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if exited.is_none() {
        let _ = child.kill();
        panic!("an ignored --config left the process serving the defaults");
    }
    let output = child.wait_with_output().expect("the output of a process that exited");
    assert_ne!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("port"),
        "the setting is named: {}",
        stderr(&output)
    );
}

/// A rule table survives being printed and read back, so the configuration an
/// operator inspects is the configuration that is running.
#[test]
fn printing_the_configuration_keeps_the_model_rules() {
    let file = write("[models.aliases]\n\"claude-\" = \"deepseek/deepseek-v4-flash\"\n");
    let output = run(&["--print-config", "--config", path_of(&file)], &[]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let printed = stdout(&output);
    assert!(printed.contains("claude-"), "the rule is in what is printed: {printed}");
    let config = Config::from_toml_str(&printed).expect("what is printed is a configuration");
    assert_eq!(
        config.models.resolve("claude-sonnet-5"),
        Some("deepseek/deepseek-v4-flash")
    );
}

#[test]
fn a_secret_is_never_printed() {
    let output = run(
        &["--print-config"],
        &[("CC_FINGERPRINT_SALT", "rotates-every-identity")],
    );

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let printed = stdout(&output);
    assert!(!printed.contains("rotates-every-identity"), "{printed}");
    assert!(printed.contains(REDACTED), "{printed}");
}

#[test]
fn two_questions_at_once_are_a_usage_error() {
    let output = run(&["--check", "--print-config"], &[]);

    assert_eq!(
        output.status.code(),
        Some(2),
        "a usage mistake is not a failure to serve"
    );
    assert!(stderr(&output).contains("usage: bifrost"), "{}", stderr(&output));
}

#[test]
fn help_prints_the_usage_and_succeeds() {
    let output = run(&["--help"], &[]);

    assert_eq!(output.status.code(), Some(0));
    assert!(stdout(&output).contains("usage: bifrost"), "{}", stdout(&output));
}

/// The journal is printed as the lines it holds, in the order it holds them: it is a
/// file an operator greps, and a command that reformatted it would make the two
/// disagree about what it says.
#[test]
fn the_journal_is_printed_as_the_lines_it_holds() {
    let (root, config) = deployment("journal");
    std::fs::write(root.join("journal"), JOURNAL).expect("the journal is written");

    let output = run(&["--journal"], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(stdout(&output), JOURNAL);
}

/// A filter narrows the answer and never widens it: what is left out is out, and what
/// cannot be dated is not left out, because a query that drops what it cannot place is
/// answering with less than it was given.
#[test]
fn the_journal_is_narrowed_by_kind_timestamp_and_count() {
    let (root, config) = deployment("journal-filters");
    let undatable = "{\"timestamp\":\"whenever\",\"kind\":\"turn\",\"data\":{\"model\":\"x\"}}\n";
    std::fs::write(root.join("journal"), format!("{JOURNAL}{undatable}")).expect("the journal is written");
    let env = [("BIFROST_CONFIG", path_of(&config))];

    let output = run(&["--journal", "--kind", "retention"], &env);
    let text = stdout(&output);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(text.lines().count(), 1, "{text}");
    assert!(text.contains("removed"), "{text}");

    // The cutoff keeps the entries that came after it, and the undatable one, which
    // cannot be placed against it.
    let output = run(&["--journal", "--since", "2026-01-02T12:00:00Z"], &env);
    let text = stdout(&output);
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.contains("2026-01-03"), "{text}");
    assert!(text.contains("whenever"), "{text}");

    // A limit takes from the end, which is where an operator reads a journal from.
    let output = run(&["--journal", "--limit", "2"], &env);
    let text = stdout(&output);
    assert_eq!(text.lines().count(), 2, "{text}");
    assert!(text.contains("2026-01-03"), "{text}");
    assert!(!text.contains("2026-01-01"), "{text}");

    // A timestamp that is not one is a mistake in the argument, not an empty answer.
    let output = run(&["--journal", "--since", "yesterday"], &env);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    assert!(stderr(&output).contains("RFC 3339"), "{}", stderr(&output));
}

#[test]
fn a_journal_that_is_not_there_names_the_file_it_looked_for() {
    let (root, config) = deployment("journal-missing");

    let output = run(&["--journal"], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    let said = stderr(&output);
    assert!(said.contains(&root.join("journal").display().to_string()), "{said}");
}

/// A digest is looked up in the archive the configuration names, and the answer is
/// what is actually under it rather than what its name claims.
#[test]
fn verifying_a_digest_says_what_is_under_it() {
    let (root, config) = deployment("verify");
    let store = ArchiveStore::new(root.join("archive"));
    let reference = store.put(KIND_REQUEST, b"the turn's bytes").expect("store");
    let env = [("BIFROST_CONFIG", path_of(&config))];

    let output = run(&["--verify", &reference.sha256], &env);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let said = stdout(&output);
    assert!(said.contains("intact"), "{said}");
    assert!(said.contains("request"), "{said}");

    // A claim the bytes support, and one they do not: the difference is the exit code,
    // which is what a script reads.
    let output = run(&["--verify", &reference.sha256, "--quote", "turn's"], &env);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));

    let output = run(&["--verify", &reference.sha256, "--quote", "never sent"], &env);
    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("absent"), "{}", stdout(&output));
}

/// Bytes that no longer hash to their name is a finding about the disk, and it is
/// reported as one instead of being served as the turn's bytes.
#[test]
fn a_digest_whose_bytes_no_longer_match_is_reported_as_tampered() {
    let (root, config) = deployment("verify-tampered");
    let store = ArchiveStore::new(root.join("archive"));
    let reference = store.put(KIND_REQUEST, b"the original bytes").expect("store");
    std::fs::write(store.path_for(KIND_REQUEST, &reference), b"something else").expect("overwrite");

    let output = run(
        &["--verify", &reference.sha256, "--quote", "the original bytes"],
        &[("BIFROST_CONFIG", path_of(&config))],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).contains("tampered"), "{}", stdout(&output));
}

/// Nothing archived under a digest is not a claim that failed: the turn can still be
/// read in the journal, and what retention removed is the bytes. It has an exit code
/// of its own so that a script can tell the two apart.
#[test]
fn a_digest_with_no_bytes_is_a_different_answer_from_a_false_claim() {
    let (_root, config) = deployment("verify-gone");
    let digest = "ab".repeat(32);

    let output = run(&["--verify", &digest], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(3), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("gone"), "{}", stdout(&output));
}

/// The question `--verify` asks of one digest, asked of everything the journal names at
/// once: the counts are what an operator reads after changing retention.
#[test]
fn an_audit_counts_the_digests_the_journal_names() {
    let (root, config) = deployment("audit");
    let store = ArchiveStore::new(root.join("archive"));
    let reference = store.put(KIND_REQUEST, b"the turn's bytes").expect("store");
    std::fs::write(
        root.join("journal"),
        turn_line("2026-01-01T00:00:00Z", &reference.sha256),
    )
    .expect("the journal is written");

    let output = run(&["--audit"], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let said = stdout(&output);
    // The counts line is the contract a monitor reads, so its shape is pinned here.
    assert!(
        said.starts_with("entries 1, named 1, intact 1, tampered 0, gone 0, malformed 0\n"),
        "{said}"
    );
    assert!(said.contains("every digest the journal names is intact"), "{said}");
}

/// Identical bytes are stored once, so both halves of a turn can name the same digest.
/// Counting the reference twice would answer that the archive holds less than it does.
#[test]
fn an_audit_counts_a_digest_both_halves_name_once() {
    let (root, config) = deployment("audit-shared");
    let store = ArchiveStore::new(root.join("archive"));
    let reference = store.put(KIND_REQUEST, b"the same bytes both ways").expect("store");
    let line = format!(
        "{{\"timestamp\":\"2026-01-01T00:00:00Z\",\"kind\":\"turn\",\"data\":{{\"model\":\"m\",\"request\":{{\"sha256\":\"{}\"}},\"response\":{{\"sha256\":\"{}\"}}}}}}\n",
        reference.sha256, reference.sha256
    );
    std::fs::write(root.join("journal"), line).expect("the journal is written");

    let output = run(&["--audit"], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(
        stdout(&output).starts_with("entries 1, named 1, intact 1"),
        "{}",
        stdout(&output)
    );
}

/// Bytes retention removed are not a claim that failed, and the audit says so with the
/// code `--verify` gives a single digest: what is missing was removed on purpose.
#[test]
fn an_audit_of_bytes_retention_removed_uses_the_code_verify_uses() {
    let (root, config) = deployment("audit-gone");
    let digest = "ab".repeat(32);
    std::fs::write(root.join("journal"), turn_line("2026-01-01T00:00:00Z", &digest)).expect("the journal");

    let output = run(&["--audit"], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(3), "stderr: {}", stderr(&output));
    let said = stdout(&output);
    assert!(
        said.contains("named 1, intact 0, tampered 0, gone 1, malformed 0"),
        "{said}"
    );
    assert!(said.contains("no longer archived, 1"), "{said}");
}

/// Bytes that no longer hash to their name are a finding about the disk, and it is a
/// different one from bytes that are gone: this is the code an operator has to act on.
#[test]
fn an_audit_of_bytes_that_no_longer_match_is_a_finding_about_the_disk() {
    let (root, config) = deployment("audit-tampered");
    let store = ArchiveStore::new(root.join("archive"));
    let reference = store.put(KIND_REQUEST, b"the original bytes").expect("store");
    std::fs::write(store.path_for(KIND_REQUEST, &reference), b"something else").expect("overwrite");
    std::fs::write(
        root.join("journal"),
        turn_line("2026-01-01T00:00:00Z", &reference.sha256),
    )
    .expect("the journal is written");

    let output = run(&["--audit"], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(1));
    let said = stdout(&output);
    assert!(said.contains("tampered 1"), "{said}");
    assert!(said.contains("no longer matching, 1"), "{said}");
}

/// A journal this build did not write is said out loud rather than tripped over: a name
/// that is not a digest is counted as a finding, not looked up as bytes that went
/// missing, and it is not passed over in silence either.
#[test]
fn an_audit_of_a_name_that_is_not_a_digest_is_a_finding_about_the_journal() {
    let (root, config) = deployment("audit-malformed");
    std::fs::write(root.join("journal"), turn_line("2026-01-01T00:00:00Z", "not-a-digest"))
        .expect("the journal is written");

    let output = run(&["--audit"], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    let said = stdout(&output);
    assert!(said.contains("named 1") && said.contains("malformed 1"), "{said}");
    assert!(said.contains("not a digest, 1"), "{said}");
    assert!(!said.contains("every digest the journal names is intact"), "{said}");
}

/// The audit takes the journal's own filters, so what it covers is what the operator
/// asked about: narrowing to the turns inside a window narrows the counts with it.
#[test]
fn an_audit_covers_only_the_entries_the_filter_keeps() {
    let (root, config) = deployment("audit-filtered");
    let store = ArchiveStore::new(root.join("archive"));
    let reference = store.put(KIND_REQUEST, b"the turn's bytes").expect("store");
    // The older turn's bytes are gone and the newer turn's are still there, so the
    // counts and the exit code both change when the window is narrowed.
    let journal = format!(
        "{}{}{}",
        turn_line("2026-01-01T00:00:00Z", &"cd".repeat(32)),
        RETENTION_LINE,
        turn_line("2026-01-03T00:00:00Z", &reference.sha256),
    );
    std::fs::write(root.join("journal"), journal).expect("the journal is written");
    let env = [("BIFROST_CONFIG", path_of(&config))];

    let output = run(&["--audit"], &env);
    assert_eq!(output.status.code(), Some(3), "stderr: {}", stderr(&output));
    assert!(
        stdout(&output).starts_with("entries 3, named 2, intact 1, tampered 0, gone 1"),
        "{}",
        stdout(&output)
    );

    let output = run(&["--audit", "--since", "2026-01-02T00:00:00Z"], &env);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(
        stdout(&output).starts_with("entries 2, named 1, intact 1, tampered 0, gone 0"),
        "{}",
        stdout(&output)
    );

    // A filter that keeps only lines naming nothing is an answer, not a failure.
    let output = run(&["--audit", "--kind", "retention"], &env);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let said = stdout(&output);
    assert!(said.starts_with("entries 1, named 0"), "{said}");
    assert!(said.contains("the journal names no digest"), "{said}");
}

#[test]
fn an_audit_of_a_deployment_with_no_journal_names_the_file_it_looked_for() {
    let (root, config) = deployment("audit-missing");

    let output = run(&["--audit"], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    let said = stderr(&output);
    assert!(said.contains(&root.join("journal").display().to_string()), "{said}");
}

/// The bytes go in and come out: a digest is the only name the journal keeps for a turn,
/// and this is the command that turns one back into what the client sent and got.
#[test]
fn handing_over_a_turn_prints_its_line_and_both_halves() {
    let (root, config) = deployment("turn");
    let store = ArchiveStore::new(root.join("archive"));
    // One half ends in a newline and the other does not, because both cases have to be
    // readable: what is added is a newline where one is missing, and nothing anywhere else.
    let request = store.put(KIND_REQUEST, b"{\"model\":\"m\"}\n").expect("store");
    let response = store.put(KIND_UPSTREAM_RESPONSE, b"{\"text\":\"ok\"}").expect("store");
    let line = line_with_halves(
        "2026-01-01T00:00:00Z",
        "session-abcdefgh",
        &half_of(&request, false),
        &half_of(&response, false),
    );
    std::fs::write(root.join("journal"), &line).expect("the journal is written");

    let output = run(&["--turn", &request.sha256], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let expected = format!(
        "{line}\
         intact   {rq} in request: {rb} bytes, {rl} lines\n{{\"model\":\"m\"}}\n\
         intact   {rs} in upstream-response: {sb} bytes, {sl} lines\n{{\"text\":\"ok\"}}\n",
        rq = request.sha256,
        rb = request.bytes,
        rl = request.lines,
        rs = response.sha256,
        sb = response.bytes,
        sl = response.lines,
    );
    assert_eq!(stdout(&output), expected);
}

/// A body is whatever the client sent, so the command has to hand bytes over rather than
/// text: a body that is not UTF-8 comes back as it went in, not repaired into something
/// that would no longer hash to its digest.
#[test]
fn handing_over_a_turn_keeps_bytes_that_are_not_text() {
    let (root, config) = deployment("turn-bytes");
    let store = ArchiveStore::new(root.join("archive"));
    let body = b"{\"text\":\"\xff\xfe\"}";
    let request = store.put(KIND_REQUEST, body).expect("store");
    let response = store.put(KIND_UPSTREAM_RESPONSE, b"{}").expect("store");
    std::fs::write(
        root.join("journal"),
        line_with_halves(
            "2026-01-01T00:00:00Z",
            "session-abcdefgh",
            &half_of(&request, false),
            &half_of(&response, false),
        ),
    )
    .expect("the journal is written");

    let output = run(&["--turn", &request.sha256], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(
        output.stdout.windows(body.len()).any(|window| window == body),
        "the stored bytes have to come back byte for byte: {:?}",
        stdout(&output)
    );
    // U+FFFD is what a decoded body would leave behind, and it is checked for in the
    // bytes rather than in the lossy String above: the replacement character a text
    // conversion inserts would otherwise be produced by the test itself.
    assert!(
        !output
            .stdout
            .windows(3)
            .any(|window| window == '\u{fffd}'.to_string().as_bytes()),
        "a decoded body is not the body that was kept"
    );
}

/// Identical bytes are stored once, so two turns that sent the same request name the same
/// digest. Every line that names it is handed over: a digest cannot say which turn it
/// belonged to, and the lines can.
#[test]
fn handing_over_a_turn_prints_every_line_that_names_the_digest() {
    let (root, config) = deployment("turn-shared");
    let store = ArchiveStore::new(root.join("archive"));
    let request = store.put(KIND_REQUEST, b"{\"model\":\"m\"}").expect("store");
    let response = store.put(KIND_UPSTREAM_RESPONSE, b"{}").expect("store");
    let first = line_with_halves(
        "2026-01-01T00:00:00Z",
        "session-first",
        &half_of(&request, false),
        &half_of(&response, false),
    );
    let second = line_with_halves(
        "2026-01-02T00:00:00Z",
        "session-second",
        &half_of(&request, false),
        &half_of(&response, false),
    );
    std::fs::write(root.join("journal"), format!("{first}{second}")).expect("the journal is written");

    let output = run(&["--turn", &request.sha256], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let said = stdout(&output);
    assert!(
        said.contains("session-first") && said.contains("session-second"),
        "{said}"
    );
    // Two halves for each of the two lines, and the identical request bytes are handed
    // over twice rather than once: what was asked for is the turns, not the blobs.
    assert_eq!(said.matches("intact").count(), 4, "{said}");
}

/// A digest no line names is not an archive finding: it is a question asked of the wrong
/// journal, or of the wrong half, so it fails and says which journal was read.
#[test]
fn a_digest_no_line_names_fails_and_names_the_journal() {
    let (root, config) = deployment("turn-unknown");
    let store = ArchiveStore::new(root.join("archive"));
    let request = store.put(KIND_REQUEST, b"{\"model\":\"m\"}").expect("store");
    std::fs::write(
        root.join("journal"),
        line_with_halves("2026-01-01T00:00:00Z", "s", &half_of(&request, false), "null"),
    )
    .expect("the journal is written");
    let nowhere = "ab".repeat(32);

    let output = run(&["--turn", &nowhere], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    let said = stderr(&output);
    assert!(said.contains(&nowhere), "{said}");
    assert!(said.contains(&root.join("journal").display().to_string()), "{said}");
}

/// One half missing does not take the other with it: the bytes that are there are handed
/// over, and the half that is not is named as gone, which is what retention leaves behind.
#[test]
fn a_turn_whose_other_half_is_gone_still_hands_over_the_half_that_is_there() {
    let (root, config) = deployment("turn-gone");
    let store = ArchiveStore::new(root.join("archive"));
    let request = store.put(KIND_REQUEST, b"{\"model\":\"m\"}").expect("store");
    let absent = ArchiveRef {
        sha256: "cd".repeat(32),
        bytes: 10,
        lines: 1,
    };
    std::fs::write(
        root.join("journal"),
        line_with_halves(
            "2026-01-01T00:00:00Z",
            "s",
            &half_of(&request, false),
            &half_of(&absent, false),
        ),
    )
    .expect("the journal is written");

    let output = run(&["--turn", &request.sha256], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(3), "stderr: {}", stderr(&output));
    let said = stdout(&output);
    assert!(said.contains(&format!("gone     {}:", absent.sha256)), "{said}");
    assert!(
        said.contains("{\"model\":\"m\"}"),
        "the half that is there is still handed over: {said}"
    );
}

/// A half the line records as null is answered for rather than passed over: "nothing was
/// archived for this half" is not the same finding as "the bytes are gone", and a reader
/// who is handed one half has to be told that the other was never kept.
#[test]
fn a_half_the_line_never_recorded_is_reported_as_missing() {
    let (root, config) = deployment("turn-missing-half");
    let store = ArchiveStore::new(root.join("archive"));
    let request = store.put(KIND_REQUEST, b"{\"model\":\"m\"}").expect("store");
    std::fs::write(
        root.join("journal"),
        line_with_halves("2026-01-01T00:00:00Z", "s", &half_of(&request, false), "null"),
    )
    .expect("the journal is written");

    let output = run(&["--turn", &request.sha256], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(3), "stderr: {}", stderr(&output));
    let said = stdout(&output);
    assert!(said.contains("missing  response:"), "{said}");
    assert!(said.contains("nothing was archived for it"), "{said}");
}

/// A capture that stopped at the cap is a prefix of the answer, and handing it over as if
/// it were the whole one is worse than saying so.
#[test]
fn a_truncated_half_says_it_was_truncated() {
    let (root, config) = deployment("turn-truncated");
    let store = ArchiveStore::new(root.join("archive"));
    let request = store.put(KIND_REQUEST, b"{\"model\":\"m\"}").expect("store");
    let response = store.put(KIND_UPSTREAM_RESPONSE, b"partial").expect("store");
    std::fs::write(
        root.join("journal"),
        line_with_halves(
            "2026-01-01T00:00:00Z",
            "s",
            &half_of(&request, false),
            &half_of(&response, true),
        ),
    )
    .expect("the journal is written");

    let output = run(&["--turn", &request.sha256], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(
        stdout(&output).contains("(truncated: the rest was not kept)"),
        "{}",
        stdout(&output)
    );
}

/// Bytes that no longer hash to their name are the finding an operator has to act on, so
/// they outrank bytes that are simply not there: one code covers the pair, and it is the
/// one that means the disk is wrong.
#[test]
fn a_tampered_half_outranks_a_half_that_is_gone() {
    let (root, config) = deployment("turn-tampered");
    let store = ArchiveStore::new(root.join("archive"));
    let request = store.put(KIND_REQUEST, b"the original bytes").expect("store");
    std::fs::write(store.path_for(KIND_REQUEST, &request), b"something else").expect("overwrite");
    let absent = ArchiveRef {
        sha256: "cd".repeat(32),
        bytes: 1,
        lines: 1,
    };
    std::fs::write(
        root.join("journal"),
        line_with_halves(
            "2026-01-01T00:00:00Z",
            "s",
            &half_of(&request, false),
            &half_of(&absent, false),
        ),
    )
    .expect("the journal is written");

    let output = run(&["--turn", &request.sha256], &[("BIFROST_CONFIG", path_of(&config))]);

    assert_eq!(output.status.code(), Some(1), "stderr: {}", stderr(&output));
    let said = stdout(&output);
    assert!(said.contains("tampered"), "{said}");
    assert!(
        said.contains("gone"),
        "both findings are named, and the code is the worse one: {said}"
    );
}

/// A session is a conversation, which is several turns, so the selector has to reach the
/// lines: `--journal` is where an operator finds out what a session was.
#[test]
fn a_session_selects_the_lines_of_one_conversation() {
    let (root, config) = deployment("session-journal");
    let store = ArchiveStore::new(root.join("archive"));
    let reference = store.put(KIND_REQUEST, b"{\"model\":\"m\"}").expect("store");
    let journal = format!(
        "{}{}{}",
        line_with_halves(
            "2026-01-01T00:00:00Z",
            "session-first",
            &half_of(&reference, false),
            "null"
        ),
        line_with_halves(
            "2026-01-02T00:00:00Z",
            "session-second",
            &half_of(&reference, false),
            "null"
        ),
        RETENTION_LINE,
    );
    std::fs::write(root.join("journal"), journal).expect("the journal is written");
    let env = [("BIFROST_CONFIG", path_of(&config))];

    let output = run(&["--journal", "--session", "session-first"], &env);

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let said = stdout(&output);
    assert_eq!(said.lines().count(), 1, "{said}");
    assert!(said.contains("session-first"), "{said}");
    // A pass of retention is not a line about a conversation, and the selector says so.
    assert!(
        !said.contains("session-second") && !said.contains("retention"),
        "{said}"
    );
}

/// A line that names no session is not filed under the empty one: a filter is an equality,
/// unlike the window, where a line that cannot be dated is still a line about something.
#[test]
fn a_line_that_names_no_session_is_not_about_any_conversation() {
    let (root, config) = deployment("session-absent");
    let store = ArchiveStore::new(root.join("archive"));
    let reference = store.put(KIND_REQUEST, b"{\"model\":\"m\"}").expect("store");
    // The older helper writes a line with a model and a digest and no session at all.
    std::fs::write(
        root.join("journal"),
        turn_line("2026-01-01T00:00:00Z", &reference.sha256),
    )
    .expect("the journal is written");

    let output = run(
        &["--journal", "--session", "session-first"],
        &[("BIFROST_CONFIG", path_of(&config))],
    );

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
}

/// The audit takes the same selector, so "how much of this conversation is still on disk"
/// is a question an operator can ask: the counts are about the lines the filter kept.
#[test]
fn an_audit_of_a_session_counts_only_its_digests() {
    let (root, config) = deployment("session-audit");
    let store = ArchiveStore::new(root.join("archive"));
    let here = store.put(KIND_REQUEST, b"{\"model\":\"m\"}").expect("store");
    let gone = ArchiveRef {
        sha256: "cd".repeat(32),
        bytes: 4,
        lines: 1,
    };
    let journal = format!(
        "{}{}",
        line_with_halves("2026-01-01T00:00:00Z", "session-first", &half_of(&here, false), "null"),
        line_with_halves("2026-01-02T00:00:00Z", "session-second", &half_of(&gone, false), "null"),
    );
    std::fs::write(root.join("journal"), journal).expect("the journal is written");
    let env = [("BIFROST_CONFIG", path_of(&config))];

    let output = run(&["--audit", "--session", "session-first"], &env);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(
        stdout(&output).starts_with("entries 1, named 1, intact 1, tampered 0, gone 0"),
        "{}",
        stdout(&output)
    );

    // The same journal without the filter is the answer that does have a finding, which
    // is what says the selector was doing something.
    let output = run(&["--audit"], &env);
    assert_eq!(output.status.code(), Some(3), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("gone 1"), "{}", stdout(&output));
}

/// Handing a conversation over is the same question as handing a turn over, asked about
/// every turn of it: oldest first, because a conversation reads forwards.
#[test]
fn handing_over_a_session_prints_every_turn_of_it() {
    let (root, config) = deployment("session-turn");
    let store = ArchiveStore::new(root.join("archive"));
    let request = store.put(KIND_REQUEST, b"{\"model\":\"m\"}").expect("store");
    let response = store.put(KIND_UPSTREAM_RESPONSE, b"{}").expect("store");
    let journal = format!(
        "{}{}{}",
        line_with_halves(
            "2026-01-01T00:00:00Z",
            "session-first",
            &half_of(&request, false),
            &half_of(&response, false)
        ),
        line_with_halves(
            "2026-01-02T00:00:00Z",
            "session-second",
            &half_of(&request, false),
            &half_of(&response, false)
        ),
        line_with_halves(
            "2026-01-03T00:00:00Z",
            "session-first",
            &half_of(&request, false),
            &half_of(&response, false)
        ),
    );
    std::fs::write(root.join("journal"), journal).expect("the journal is written");

    let output = run(
        &["--turn", "--session", "session-first"],
        &[("BIFROST_CONFIG", path_of(&config))],
    );

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let said = stdout(&output);
    assert!(
        !said.contains("session-second"),
        "another conversation is not this one: {said}"
    );
    assert_eq!(said.matches("intact").count(), 4, "both turns, both halves: {said}");
    let (first, last) = (
        said.find("2026-01-01").expect("the older turn"),
        said.find("2026-01-03").expect("the newer turn"),
    );
    assert!(first < last, "a conversation is handed over oldest first: {said}");
}

#[test]
fn a_session_no_line_is_filed_under_fails_and_names_the_journal() {
    let (root, config) = deployment("session-unknown");
    let store = ArchiveStore::new(root.join("archive"));
    let request = store.put(KIND_REQUEST, b"{\"model\":\"m\"}").expect("store");
    std::fs::write(
        root.join("journal"),
        line_with_halves(
            "2026-01-01T00:00:00Z",
            "session-first",
            &half_of(&request, false),
            "null",
        ),
    )
    .expect("the journal is written");

    let output = run(
        &["--turn", "--session", "session-nobody-named"],
        &[("BIFROST_CONFIG", path_of(&config))],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
    let said = stderr(&output);
    assert!(said.contains("session-nobody-named"), "{said}");
    assert!(said.contains(&root.join("journal").display().to_string()), "{said}");
}
