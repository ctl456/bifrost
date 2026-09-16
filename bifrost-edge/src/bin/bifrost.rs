//! The edge as a running process.

use std::io::Write;
use std::process::ExitCode;

use bifrost_edge::cli::{self, Command, Verified};
use bifrost_edge::log;
use bifrost_edge::{Edge, app};

fn main() -> ExitCode {
    let command = match Command::parse(std::env::args().skip(1)) {
        Ok(command) => command,
        Err(message) => {
            // A usage mistake is not a configuration mistake: 2 is what a shell
            // answers for calling something wrong, and keeping the two apart means
            // a unit file's journal says which one it was.
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    match command {
        Command::Help => {
            println!("{}", cli::USAGE);
            ExitCode::SUCCESS
        }
        // A command that only answers a question answers it before anything is
        // logged: the answer is the output, and a timestamp in front of it makes it
        // a worse answer for whoever ran the command.
        Command::Check { config } => report(cli::check(config.as_deref())),
        Command::PrintConfig { config } => report(cli::print_config(config.as_deref())),
        Command::Journal {
            config,
            since,
            kind,
            session,
            limit,
        } => lines(cli::journal(
            config.as_deref(),
            since.as_deref(),
            kind.as_deref(),
            session.as_deref(),
            limit,
        )),
        Command::Turn {
            config,
            digest,
            session,
        } => {
            // The bytes of a turn are not text — they are whatever the client sent — so
            // the command writes them out itself and all that is left here is the code.
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            handed(cli::turn(
                config.as_deref(),
                digest.as_deref(),
                session.as_deref(),
                &mut out,
            ))
        }
        Command::Audit {
            config,
            since,
            kind,
            session,
        } => verified(cli::audit(
            config.as_deref(),
            since.as_deref(),
            kind.as_deref(),
            session.as_deref(),
        )),
        Command::Verify { config, digest, quote } => {
            verified(cli::verify(config.as_deref(), &digest, quote.as_deref()))
        }
        Command::Serve { config } => serve(config.as_deref()),
    }
}

/// Print what a question was answered with, and say whether it was answered.
///
/// The text goes to stdout and the reason it could not be produced goes to stderr,
/// so that a unit file's journal keeps the two apart: the exit code is what a
/// manager reads, and the message is what a person reads.
fn report(answer: Result<String, String>) -> ExitCode {
    match answer {
        Ok(text) => {
            println!("{text}");
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// Print a question's rows exactly as they were produced.
///
/// A question that answers with one line per entry is read by a script, and a newline
/// added here would be a row of its own: an empty answer prints nothing at all.
fn lines(answer: Result<String, String>) -> ExitCode {
    match answer {
        Ok(text) => {
            print!("{text}");
            // The buffer is flushed here rather than left to the runtime's exit
            // handler, because a caller reading this through a pipe started reading
            // when the process did.
            let _ = std::io::stdout().flush();
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// Report a verification, which answers with a code as well as with text.
///
/// The code is the answer a script acts on — intact, a claim that did not hold, or
/// bytes that are no longer archived — so it is carried out rather than flattened
/// into "something was wrong". An audit is the same kind of answer over a whole
/// journal instead of one digest, and it arrives here for the same reason: what a
/// monitor reads has to be the exit code.
fn verified(answer: Result<Verified, String>) -> ExitCode {
    match answer {
        Ok(Verified { text, code }) => {
            println!("{text}");
            ExitCode::from(code)
        }
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// Report a command that wrote its answer out itself.
///
/// The answer is bytes and the code is what a script acts on, so there is nothing to
/// print here beyond the reason a command could not finish.
fn handed(answer: Result<u8, String>) -> ExitCode {
    match answer {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

/// Run the gateway until it is asked to stop.
#[tokio::main]
async fn serve(path: Option<&std::path::Path>) -> ExitCode {
    let config = match bifrost_config::load_from(path) {
        Ok(config) => config,
        Err(error) => {
            // The level comes from the configuration, so this one is written
            // before there is a configuration to take it from. It is an error,
            // and the default level keeps it.
            log::error(error.to_string());
            return ExitCode::FAILURE;
        }
    };
    log::init(config.telemetry.log_level, config.telemetry.log_format);

    let address = format!("{}:{}", config.host, config.port);
    let listener = match tokio::net::TcpListener::bind(&address).await {
        Ok(listener) => listener,
        Err(error) => {
            log::error(format!("cannot listen on {address}: {error}"));
            return ExitCode::FAILURE;
        }
    };

    let edge = match Edge::new(config) {
        Ok(edge) => edge,
        Err(error) => {
            log::error(error);
            return ExitCode::FAILURE;
        }
    };

    let wire = edge.config().wire.adapter.clone();
    let base = edge.config().api_base.clone();
    log::info(format!("listening on {address}, speaking {wire} to {base}"));

    // Started here rather than inside `app`, because it is a property of the
    // process and not of the router: a test that builds an edge should not begin
    // reading a registry because the default asks it to.
    let _drift = bifrost_edge::drift::watch(&edge);

    // Started here for the same reason, and before the first request is served: a
    // process that just came up should not be waiting a day to be within its
    // retention, and nothing here is on a request path.
    let _retention = bifrost_edge::retention::watch(&edge);

    let shutdown = async {
        wait_for_stop().await;
        log::info("shutting down; streams in flight are left to finish");
    };

    if let Err(error) = axum::serve(listener, app(edge)).with_graceful_shutdown(shutdown).await {
        log::error(format!("server stopped: {error}"));
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Wait for whichever signal says to stop.
///
/// A terminal sends SIGINT and a service manager sends SIGTERM, and both mean the
/// same thing to this process. Waiting for both is what lets the unit file use the
/// signal systemd sends by default instead of being told to send the one this
/// build happens to catch.
async fn wait_for_stop() {
    let terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate());
    match terminate {
        Ok(mut terminate) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        }
        Err(error) => {
            // A deployment whose manager sends SIGTERM should not be left unable to
            // serve because the handler could not be installed, so this is said and
            // then SIGINT is waited for on its own.
            log::error(format!("cannot wait for SIGTERM ({error}); waiting for SIGINT only"));
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}
