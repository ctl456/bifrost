//! The command line, run as the process a unit file runs.
//!
//! These spawn the binary rather than calling the parser, because what a deployment
//! depends on is an exit code: a unit file reads `0` or it reads non-zero, and an
//! exit code produced by a function nothing calls is not the one systemd would see.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bifrost::config::{Config, REDACTED};

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

/// The unit validates the file it is about to serve.
///
/// A `--check` that read a different file would be a check of nothing, and the
/// difference would show up only as a service that started with a configuration
/// nobody validated — which is what the unit passes the flag for.
#[test]
fn the_unit_checks_the_configuration_it_serves() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("deploy/bifrost.service");
    let unit = std::fs::read_to_string(&path).expect("the unit is in the repository");
    let argument = |prefix: &str| {
        unit.lines()
            .find_map(|line| line.strip_prefix(prefix))
            .unwrap_or_else(|| panic!("the unit has no {prefix} line"))
            .split_whitespace()
            .filter(|word| *word != "--check")
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };

    let checked = argument("ExecStartPre=");
    let served = argument("ExecStart=");
    assert_eq!(
        checked, served,
        "the check names the same file and command the service does"
    );
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

/// A deployment of its own that issues tokens: a key file, a token file, and a
/// configuration naming both.
fn issuing_deployment(label: &str) -> (PathBuf, PathBuf) {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let unique = NEXT.fetch_add(1, Ordering::Relaxed);
    let root = std::env::temp_dir().join(format!("bifrost-cli-access-{}-{label}-{unique}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("the deployment directory is created");
    let key_file = root.join("auth.json");
    std::fs::write(&key_file, r#"{"apiKey":"user_deployment_key","userName":"ops"}"#).expect("the key is written");
    let config = write(&format!(
        "[access]\nenabled = true\nkey_file = \"{}\"\ntokens_file = \"{}\"\n",
        key_file.display(),
        root.join("tokens.json").display()
    ));
    (root, config)
}

/// The command that answers questions about a running deployment is the same one
/// that issues to it, so issuing, listing and revoking are three invocations and one
/// file — which is what the operator ends up doing.
#[test]
fn a_token_is_issued_listed_and_revoked() {
    let (root, config) = issuing_deployment("lifecycle");
    let config = config.display().to_string();

    let output = run(
        &[
            "--token-new",
            "laptop",
            "--rpm",
            "60",
            "--concurrency",
            "2",
            "--config",
            &config,
        ],
        &[],
    );
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let issued = stdout(&output);
    let token = issued
        .split_whitespace()
        .find(|word| word.starts_with("bfr_"))
        .expect("the token is printed")
        .to_owned();
    assert!(issued.contains("60 requests per minute"), "{issued}");
    assert!(issued.contains("shown here once"), "{issued}");

    // The token is nowhere in the file it was written to, which is what makes the
    // file safe to read, copy and back up.
    let stored = std::fs::read_to_string(root.join("tokens.json")).expect("the token file");
    assert!(!stored.contains(&token), "the file holds a digest, not the token");
    assert!(stored.contains("laptop"));

    let listed = run(&["--token-list", "--config", &config], &[]);
    assert_eq!(listed.status.code(), Some(0), "stderr: {}", stderr(&listed));
    let listed = stdout(&listed);
    assert!(listed.starts_with("tokens 1 in "), "{listed}");
    assert!(listed.contains("laptop rpm=60 concurrency=2"), "{listed}");
    assert!(!listed.contains(&token), "a listing has no token to print: {listed}");

    let revoked = run(&["--token-revoke", "laptop", "--config", &config], &[]);
    assert_eq!(revoked.status.code(), Some(0), "stderr: {}", stderr(&revoked));
    assert!(stdout(&revoked).contains("revoked"), "{}", stdout(&revoked));

    let listed = run(&["--token-list", "--config", &config], &[]);
    assert!(stdout(&listed).contains("revoked=yes"), "{}", stdout(&listed));

    // A name stays taken once it has been used, so the same name cannot come back
    // as a different credential without it being obvious.
    let again = run(&["--token-new", "laptop", "--config", &config], &[]);
    assert_eq!(again.status.code(), Some(1), "a revoked name is not free again");
    assert!(stderr(&again).contains("revoked"), "{}", stderr(&again));

    let _ = std::fs::remove_dir_all(&root);
}

/// A token nothing checks is a credential that does not exist, so a deployment that
/// forwards its callers' keys refuses to issue one rather than writing a file that
/// means nothing.
#[test]
fn issuing_is_refused_where_the_key_is_forwarded() {
    let file = write("[access]\nenabled = false\n");
    let output = run(&["--token-new", "laptop", "--config", path_of(&file)], &[]);

    assert_eq!(output.status.code(), Some(1), "stdout: {}", stdout(&output));
    assert!(
        stderr(&output).contains("access.enabled is false"),
        "{}",
        stderr(&output)
    );
}

/// The check is what a unit runs before it starts, so it reads the two files a
/// deployment that issues tokens depends on: a key file that cannot be read is a
/// deployment that would serve nothing but 401s.
#[test]
fn the_check_reads_what_a_deployment_that_issues_tokens_depends_on() {
    let (root, config) = issuing_deployment("check");
    let config = config.display().to_string();

    let output = run(&["--check", "--config", &config], &[]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(stdout(&output).contains("no token issued yet"), "{}", stdout(&output));
    assert!(stdout(&output).contains("key read from"), "{}", stdout(&output));

    let issued = run(&["--token-new", "laptop", "--config", &config], &[]);
    assert_eq!(issued.status.code(), Some(0), "stderr: {}", stderr(&issued));
    let output = run(&["--check", "--config", &config], &[]);
    assert!(
        stdout(&output).contains("1 token(s) in force, 0 revoked"),
        "{}",
        stdout(&output)
    );

    // The key file is the one thing the check cannot answer without.
    std::fs::remove_file(root.join("auth.json")).expect("the key file is removed");
    let output = run(&["--check", "--config", &config], &[]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    assert!(stderr(&output).contains("access.key_file"), "{}", stderr(&output));

    let _ = std::fs::remove_dir_all(&root);
}

/// A limit belongs to the token it is set on, so setting one outside `--token-new`
/// is a usage mistake rather than something to ignore.
#[test]
fn a_limit_outside_token_new_is_a_usage_mistake() {
    let file = write("port = 4055\n");
    let output = run(&["--check", "--rpm", "60", "--config", path_of(&file)], &[]);

    assert_eq!(output.status.code(), Some(2), "stdout: {}", stdout(&output));
    assert!(
        stderr(&output).contains("--rpm and --concurrency only mean something with --token-new"),
        "{}",
        stderr(&output)
    );
}
