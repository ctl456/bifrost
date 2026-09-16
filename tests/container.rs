//! What the container says about this build.
//!
//! The image is the one artifact here that no test can run: it does not exist until
//! `.github/workflows/docker.yml` builds it, and the two files describing it are read by
//! Docker rather than by a compiler. What can be checked is the part of it that is a
//! claim about *this* build rather than about Docker — the port it publishes, the path
//! its probe asks, the account it runs as, the trust store its TLS reads, and where its
//! state goes — so that an edit which makes the file wrong fails here instead of in a
//! deployment, where the symptom is a connection refused or an issuer nobody recognises.

use std::path::Path;

use bifrost::config::Config;

/// Read a file from the repository.
fn read(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Every Dockerfile instruction with this name, with its continuations joined.
///
/// Instructions are allowed to wrap, and one of these is wrapped on purpose: a check that
/// read a single line would read half the healthcheck and pass. All of them rather than
/// the first, because the question a stage is asked — does this one install what it needs
/// — is about its steps rather than about one of them.
fn instructions(document: &str, name: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut collected = String::new();
    let mut open = false;
    for line in document.lines() {
        let line = line.trim_end();
        let continued = line.ends_with('\\');
        let body = line.trim_end_matches('\\').trim();
        if open {
            collected.push(' ');
            collected.push_str(body);
            open = continued;
            if !open {
                found.push(std::mem::take(&mut collected));
            }
            continue;
        }
        let Some(rest) = body.strip_prefix(name) else { continue };
        // `USER` is not `USERADD`, and a comment naming an instruction is not that
        // instruction.
        if !rest.starts_with(char::is_whitespace) {
            continue;
        }
        collected.push_str(rest.trim());
        open = continued;
        if !open {
            found.push(std::mem::take(&mut collected));
        }
    }
    found
}

/// The first instruction with this name.
fn instruction(document: &str, name: &str) -> Option<String> {
    instructions(document, name).into_iter().next()
}

/// The `- ` items of an indented `key:` list.
///
/// Enough YAML to read two lists out of one file and no more: a parser dependency whose
/// only use here would be checking that two numbers agree is not worth carrying.
fn list(document: &str, key: &str) -> Vec<String> {
    let header = format!("{key}:");
    let mut items = Vec::new();
    let mut inside = false;
    for line in document.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if inside {
            match trimmed.strip_prefix("- ") {
                Some(item) => {
                    items.push(item.trim().to_owned());
                    continue;
                }
                None => inside = false,
            }
        }
        if line.starts_with(' ') && trimmed == header {
            inside = true;
        }
    }
    items
}

/// The port the image publishes is the port the binary listens on by default.
///
/// The image runs the binary with no arguments, so `EXPOSE` is a claim about
/// `Config::default().port` written in a file no compiler reads. Two numbers in two files
/// is exactly the arrangement that stops agreeing quietly, and the symptom is a container
/// publishing a port nothing is listening on — which a client reports as a refused
/// connection rather than as anything mentioning a port.
#[test]
fn the_image_publishes_the_port_the_binary_listens_on() {
    let dockerfile = read("Dockerfile");
    let exposed = instruction(&dockerfile, "EXPOSE").expect("the image publishes a port");
    let exposed: u16 = exposed
        .parse()
        .unwrap_or_else(|_| panic!("EXPOSE names a port: {exposed}"));

    assert_eq!(
        exposed,
        Config::default().port,
        "the image publishes the port the defaults bind, or it publishes nothing"
    );
}

/// The probe asks the path this build serves, on the port it publishes.
///
/// `/health` is the route that needs no key, which is what makes it the one a probe can
/// ask, and it is declared in `edge::routes` — what is checked here is only that the file
/// Docker reads names it and falls back to the published port. The route itself is
/// exercised against a running edge in `tests/edge.rs`, and against a running container
/// by the workflow.
#[test]
fn the_probe_asks_a_path_this_build_serves() {
    let dockerfile = read("Dockerfile");
    let exposed: u16 = instruction(&dockerfile, "EXPOSE")
        .expect("the image publishes a port")
        .parse()
        .expect("EXPOSE names a port");
    let probe = instruction(&dockerfile, "HEALTHCHECK").expect("the image declares a probe");

    // The whole path, not a substring of it: `/health` is also how `/healthz` begins.
    let asks = probe
        .split(|character: char| character.is_whitespace() || character == '"')
        .any(|word| word.ends_with("/health"));
    assert!(asks, "the probe asks the one path served without a key: {probe}");
    assert!(
        probe.contains(&exposed.to_string()),
        "a probe that did not fall back to the published port would call a deployment that \
         moved its port unhealthy: {probe}"
    );
}

/// The container runs as an unprivileged account the image creates.
///
/// The same decision as `User=bifrost` in the unit file. A `USER` naming an account that
/// nothing creates is not a weaker sandbox but a container that never starts, so the
/// second half of this is checked rather than assumed.
#[test]
fn the_container_runs_as_an_account_that_exists() {
    let dockerfile = read("Dockerfile");
    let user = instruction(&dockerfile, "USER").expect("the runtime drops root");
    let user = user.trim();

    assert!(
        !matches!(user, "root" | "0"),
        "the runtime does not run as root: {user}"
    );
    if user.parse::<u32>().is_err() {
        assert!(
            dockerfile.lines().any(|line| line.trim_end().ends_with(user)),
            "`USER {user}` names an account this file has to create, and no line here ends in it"
        );
    }
}

/// The runtime carries the trust store its TLS verification reads.
///
/// reqwest verifies the upstream through `rustls-platform-verifier`, which on Linux reads
/// the system store rather than a compiled-in one. A runtime slimmed one package too far
/// turns every turn into an unknown-issuer error, which reads like an upstream fault and
/// is not one. What is asked is whether a step installs it: the file says the name in a
/// comment as well, and a comment installs nothing.
#[test]
fn the_runtime_can_verify_the_upstream_certificate() {
    let dockerfile = read("Dockerfile");
    let (_, runtime) = dockerfile
        .rsplit_once("\nFROM ")
        .expect("the runtime is a stage of its own");

    assert!(
        instructions(runtime, "RUN")
            .iter()
            .any(|run| run.contains("ca-certificates")),
        "the stage that runs the binary installs the store the binary verifies with"
    );
}

/// Compose publishes the port the image publishes, on the container side.
///
/// The host side is the operator's to choose; the container side is not, because the
/// binary inside binds what the image declares. A mapping naming another number routes to
/// a closed port, and the probe beside it would disagree as well.
#[test]
fn compose_publishes_the_port_the_image_listens_on() {
    let exposed = instruction(&read("Dockerfile"), "EXPOSE").expect("the image publishes a port");
    let compose = read("docker-compose.yml");
    let ports = list(&compose, "ports");

    assert_eq!(ports.len(), 1, "one mapping: {ports:?}");
    let container = ports[0].rsplit(':').next().expect("a mapping has two sides");
    assert_eq!(
        container.trim_matches('"'),
        exposed.trim(),
        "the container side is the port the binary binds"
    );
}

/// A container that may not write anywhere is given the one place it writes.
///
/// The unit's `StateDirectory`. Without it a deployment that issues tokens cannot write
/// `var/tokens.json`, and the moment that is discovered is `--token-new` rather than
/// startup, which is the worst time to find it out.
#[test]
fn a_read_only_container_is_given_somewhere_to_write() {
    let compose = read("docker-compose.yml");

    assert!(
        compose.contains("read_only: true"),
        "the filesystem is read-only, as the unit's is"
    );
    let volumes = list(&compose, "volumes");
    assert!(
        volumes.iter().any(|volume| volume.contains("/var/lib/bifrost")),
        "the state directory is mounted, or this deployment cannot issue a token: {volumes:?}"
    );
}
