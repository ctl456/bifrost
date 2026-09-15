//! Forwarding an upstream stream to a client.
//!
//! The upstream answers with newline-delimited JSON; a client is owed
//! server-sent events. Between the two sits a decision the original makes
//! implicitly and this module makes explicitly: **the response is not committed
//! until there is something to commit.**
//!
//! That matters because a status code can only be chosen once. If the upstream
//! fails, or times out, or produces nothing at all before any client-visible
//! output exists, the honest answer is a retryable JSON status — and a proxy
//! that had already sent `200 OK` can only apologise in a stream event. So the
//! first visible frame is what commits the response, and everything before it is
//! a chance to answer properly.
//!
//! The upstream is read by a task of its own rather than inside the poll loop of the
//! response body. The body is only polled while the client is reading it, so a
//! client that stops reading would otherwise stop the reads and keep the upstream
//! connection — and the tokens it is generating — alive for as long as it felt
//! like. The task and the bounded channel between them make the client's slowness
//! the upstream's problem instead of the gateway's memory.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::evidence::{CAPTURE_CAP, Captured, Evidence, TurnContext};
use crate::state::{Edge, InflightPermit};
use crate::upstream::LineBuffer;
use axum::body::{Body, Bytes};
use bifrost_core::{ChunkGenerator, Error, OutputAccumulator, OutputChunk};
use bifrost_protocol::SseFrame;
use bifrost_wire::cc::{CcDecoder, DecodedLine};
use tokio::sync::mpsc;

/// What the upstream produced before the response had to be committed.
pub enum Opening {
    /// Something client-visible arrived; the response is now a stream.
    Committed {
        frames: Vec<SseFrame>,
        session: Box<Session>,
    },
    /// Nothing was written, so a status is still available.
    ///
    /// The session itself is dropped with the status already recorded in its
    /// capture, which is how a turn that never committed still gets written down.
    Unwritten { error: Box<Error> },
}

/// How many chunks may sit between the reader and the client.
///
/// Small on purpose: it is the amount of an answer the gateway holds in memory on
/// behalf of a client that is not reading it.
const CHANNEL_CAPACITY: usize = 4;

/// The longest a single line may grow before the upstream is judged to be
/// speaking something other than this protocol.
///
/// Every frame is one line of JSON, so a line that has grown this far is a line
/// that is not going to end. Without a ceiling the decoder's half-line buffer
/// grows with the upstream instead — the same unbounded growth the bounded
/// channel exists to prevent, reached through the decoder rather than the
/// socket, and it is reached precisely when the client has stopped reading.
const MAX_LINE_BYTES: usize = 1024 * 1024;

/// One message from the reader.
enum Pump {
    Chunk(Bytes),
    /// The upstream ended its answer.
    End,
    /// The upstream connection failed.
    Failed(Box<Error>),
}

/// A task that lives exactly as long as the stream that owns it.
struct Task(tokio::task::JoinHandle<()>);

impl Drop for Task {
    /// Aborting drops whatever the task owns — which, for the reader, is the
    /// upstream response, and with it the connection.
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Whether the client is still taking what the stream produces.
///
/// The channel is bounded, so a reader that has been waiting to hand over a chunk
/// for longer than the deployment allows is a client that has stopped reading.
/// That is the same signal the original took from its socket's drain state, taken
/// where this design can see it.
#[derive(Debug, Default)]
struct Backpressure {
    /// When the hand-over in flight began, if there is one.
    pending: Mutex<Option<Instant>>,
}

impl Backpressure {
    fn begin(&self) {
        *self.pending.lock().expect("backpressure poisoned") = Some(Instant::now());
    }

    fn end(&self) {
        *self.pending.lock().expect("backpressure poisoned") = None;
    }

    fn blocked_for(&self) -> Duration {
        self.pending
            .lock()
            .expect("backpressure poisoned")
            .map_or(Duration::ZERO, |started| started.elapsed())
    }
}

/// Everything needed to keep forwarding one stream.
pub struct Session {
    /// What the reader has handed over and the client has not been given yet.
    incoming: mpsc::Receiver<Pump>,
    /// The reader task, aborted when the session is dropped. Held only to be
    /// dropped: what it owns is what has to be released.
    _reader: Task,
    /// The stall watchdog, when one is configured.
    _watchdog: Option<Task>,
    lines: LineBuffer,
    decoder: CcDecoder,
    renderer: Box<dyn ChunkGenerator<Event = SseFrame> + Send>,
    accumulator: OutputAccumulator,
    idle: Duration,
    heartbeat: Option<Duration>,
    /// When the last byte went to the client, for the heartbeat's idle test.
    last_sent: Instant,
    /// When the last byte arrived from upstream, for the idle timeout.
    last_read: Instant,
    done: bool,
    /// Whether the trailing partial line has been decoded yet.
    tail_read: bool,
    /// Frames produced while deciding whether the response could be committed.
    ///
    /// They were already rendered before the status was spent, so they belong at
    /// the head of the stream rather than being re-derived.
    pending: Option<Bytes>,
    /// The raw upstream bytes, when this deployment keeps evidence.
    capture: Option<Capture>,
}

/// The upstream bytes of one streamed turn, kept while it runs.
///
/// The archived stream is the upstream's side of the conversation as it actually
/// happened, which no reconstruction from the rendered events can be: the
/// rendering is where the interesting bugs live in the first place.
pub struct Capture {
    evidence: Arc<Evidence>,
    /// The half of the journal line that is already known.
    turn: TurnContext,
    /// The archived client request, if it was archived.
    request: Option<Captured>,
    /// Filled by the reader, which outlives the session's polls.
    state: Arc<Mutex<CaptureState>>,
}

/// What the reader has kept so far, and what the client is being told.
#[derive(Debug)]
struct CaptureState {
    bytes: Vec<u8>,
    truncated: bool,
    /// The status the client is being given. A committed stream is answered `200`
    /// whatever the stream goes on to say; a stream that never committed is
    /// answered with whatever the caller decided instead.
    status: u16,
    /// Set once the record has been written, so the last writer to leave does not
    /// write a second one.
    written: bool,
}

impl Default for CaptureState {
    /// A stream that got as far as being a stream is answered `200`. Only a turn
    /// that never committed is told otherwise, and only the caller can tell it.
    fn default() -> Self {
        Self {
            bytes: Vec::new(),
            truncated: false,
            status: 200,
            written: false,
        }
    }
}

/// The reader's half of a capture.
#[derive(Clone)]
struct Sink(Arc<Mutex<CaptureState>>);

impl Sink {
    /// Keep this read, up to the cap.
    fn keep(&self, chunk: &[u8]) {
        let mut state = self.0.lock().expect("capture poisoned");
        if state.truncated {
            return;
        }
        // What is kept and what is being added both have to fit: half a blob is not
        // evidence, so an over-long stream is written out as what fit, plus a flag
        // saying it is not all of it.
        if state.bytes.len() + chunk.len() > CAPTURE_CAP {
            state.truncated = true;
            return;
        }
        state.bytes.extend_from_slice(chunk);
    }
}

impl Capture {
    #[must_use]
    pub fn new(evidence: Arc<Evidence>, turn: TurnContext, request: Option<Captured>) -> Self {
        Self {
            evidence,
            turn,
            request,
            state: Arc::new(Mutex::new(CaptureState::default())),
        }
    }

    /// The handle the reader fills the capture through.
    fn sink(&self) -> Sink {
        Sink(Arc::clone(&self.state))
    }

    /// Name the status the client is being given, before anything was committed.
    fn set_status(&self, status: u16) {
        self.state.lock().expect("capture poisoned").status = status;
    }

    /// Write this turn's record, off the async thread.
    ///
    /// Off it because it is a file write, and the last thing a client waits for
    /// should not be a disk flush. A drop outside a runtime has no thread to write
    /// from, and losing a record is not worth panicking on the way out.
    fn finish(&self) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let Some((bytes, truncated, status)) = self.take() else {
            return;
        };
        let (evidence, turn, request) = (Arc::clone(&self.evidence), self.turn.clone(), self.request.clone());
        handle.spawn_blocking(move || {
            let response = crate::evidence::or_warn(evidence.archive_events(&bytes, truncated), "the upstream stream");
            let _ = crate::evidence::or_warn(
                evidence.record(&turn.turn(status, true, request, response)),
                "the turn journal",
            );
        });
    }

    /// What to write, once. A second caller gets nothing rather than a duplicate.
    fn take(&self) -> Option<(Vec<u8>, bool, u16)> {
        let mut state = self.state.lock().expect("capture poisoned");
        if state.written {
            return None;
        }
        state.written = true;
        Some((std::mem::take(&mut state.bytes), state.truncated, state.status))
    }
}

/// How long a stream may go without a byte before it is considered dead.
pub struct StreamPlan {
    pub renderer: Box<dyn ChunkGenerator<Event = SseFrame> + Send>,
    pub response: reqwest::Response,
    pub idle: Duration,
    /// Send a comment frame when the stream has been quiet for a while.
    ///
    /// Only the Anthropic endpoint does this: its clients measure a first-byte
    /// timeout, and a reasoning model can think for minutes before it says
    /// anything. The other protocols send nothing, because their clients do not.
    pub heartbeat: Option<Duration>,
    /// Keep the upstream bytes for the archive, when the deployment asked for it.
    pub capture: Option<Capture>,
    /// The stall watchdog: how long the client may stop taking bytes before the
    /// upstream is cut loose, and whose counter to raise when that happens.
    pub stall: Option<Stall>,
}

/// The client-stall watchdog's two halves: the allowance and the counter that
/// records a breach.
///
/// The counter belongs to the gateway, and the watchdog task would otherwise have
/// no way to reach it: the plan is built by the router, which does hold the
/// gateway, so the gateway is what the plan carries.
pub struct Stall {
    /// How long the client may stop taking bytes before the upstream is cut loose.
    pub after: Duration,
    pub edge: Arc<Edge>,
}

impl Session {
    /// Watch the upstream until the response can be committed, or decide that it
    /// cannot be.
    pub async fn open(plan: StreamPlan) -> Opening {
        let StreamPlan {
            renderer,
            response,
            idle,
            heartbeat,
            capture,
            stall,
        } = plan;

        let (sender, incoming) = mpsc::channel(CHANNEL_CAPACITY);
        let backpressure = Arc::new(Backpressure::default());
        let sink = capture.as_ref().map(Capture::sink);
        let reader = tokio::spawn(pump(response, sender, sink, Arc::clone(&backpressure)));
        // The watchdog watches the reader, so it outlives nothing: a handle to abort
        // the reader is all it needs to close the upstream.
        let watchdog = stall.map(|stall| Task(tokio::spawn(watch(backpressure, reader.abort_handle(), stall))));

        let now = Instant::now();
        let mut session = Box::new(Self {
            incoming,
            _reader: Task(reader),
            _watchdog: watchdog,
            lines: LineBuffer::new(),
            decoder: CcDecoder::new(),
            renderer,
            accumulator: OutputAccumulator::new(),
            idle,
            heartbeat,
            last_sent: now,
            last_read: now,
            done: false,
            tail_read: false,
            pending: None,
            capture,
        });

        let mut frames = Vec::new();
        loop {
            match session.step().await {
                Step::Frames(mut produced) => {
                    frames.append(&mut produced);
                    if !frames.is_empty() {
                        // Something visible exists, so the status is now spent.
                        return Opening::Committed { frames, session };
                    }
                }
                Step::Idle => return unwritten(&session, crate::state::idle_timeout_error()),
                Step::Ended => {
                    // A stream that ended without ever saying anything. It may
                    // still have a terminal event to send, which is worth
                    // sending; if it does not, the turn produced nothing and the
                    // client is better served by a retryable status.
                    let closing = session.renderer.end_events();
                    if !closing.is_empty() {
                        return Opening::Committed {
                            frames: closing,
                            session,
                        };
                    }
                    if let Some(error) = session.accumulator.error.clone() {
                        return unwritten(&session, error);
                    }
                    return unwritten(&session, crate::state::empty_output_error());
                }
                Step::Failed(error) => return unwritten(&session, error),
            }
        }
    }

    /// Hand the session the frames that committed the response.
    pub fn prime(&mut self, frames: &[SseFrame]) {
        self.pending = Some(encode(frames));
    }

    /// The next batch of bytes for the client, or `None` at the end.
    pub async fn next_batch(&mut self) -> Option<Bytes> {
        if let Some(pending) = self.pending.take() {
            return Some(pending);
        }
        if self.done {
            return None;
        }
        loop {
            match self.step().await {
                Step::Frames(frames) => {
                    if !frames.is_empty() {
                        return Some(encode(&frames));
                    }
                }
                Step::Ended => {
                    self.done = true;
                    self.finalize();
                    let closing = self.renderer.end_events();
                    // An empty turn that got this far was already committed, so
                    // the only remaining way to say "this produced nothing" is in
                    // the stream's own vocabulary.
                    let closing = if closing.is_empty() && self.accumulator.is_empty_output() {
                        self.renderer.failure_frames(&crate::state::empty_output_error())
                    } else {
                        closing
                    };
                    return (!closing.is_empty()).then(|| encode(&closing));
                }
                Step::Idle => {
                    self.done = true;
                    self.finalize();
                    let frames = self.renderer.failure_frames(&crate::state::idle_timeout_error());
                    return (!frames.is_empty()).then(|| encode(&frames));
                }
                Step::Failed(error) => {
                    self.done = true;
                    self.finalize();
                    let frames = self.renderer.failure_frames(&error);
                    return (!frames.is_empty()).then(|| encode(&frames));
                }
            }
        }
    }

    /// One turn of the read loop.
    async fn step(&mut self) -> Step {
        // With a heartbeat the wait is cut short on purpose: a quiet stream still
        // has something to do. Without one there is nothing to do until bytes
        // arrive, so the timeout *is* the wait.
        let tick = match self.heartbeat {
            Some(interval) => interval.min(self.idle),
            None => self.idle,
        };

        match tokio::time::timeout(tick, self.incoming.recv()).await {
            Ok(Some(Pump::Chunk(chunk))) => {
                self.last_read = Instant::now();
                self.absorb(&chunk)
            }
            Ok(Some(Pump::End)) => {
                // A stream need not end on a newline. The tail is a real event
                // that happens to be the last one, so it is rendered before the
                // stream is declared over — dropping it would lose the final
                // accounting in exactly the case where it is least recoverable.
                if !self.tail_read {
                    self.tail_read = true;
                    let tail = self.finish_tail();
                    if !tail.is_empty() {
                        self.last_sent = Instant::now();
                        return Step::Frames(tail);
                    }
                }
                Step::Ended
            }
            Ok(Some(Pump::Failed(error))) => Step::Failed(*error),
            // The reader went away without finishing its answer. That happens when
            // it was aborted for a client that stopped reading, and a stream that
            // ends for a reason nobody will read is still a stream that did not end.
            Ok(None) => Step::Failed(Error::upstream("Upstream stream ended without a terminal event")),
            Err(_elapsed) => {
                // Either the heartbeat is due, or the stream is dead, or both.
                if self.last_read.elapsed() >= self.idle {
                    return Step::Idle;
                }
                if let Some(interval) = self.heartbeat
                    && self.last_sent.elapsed() >= interval
                {
                    self.last_sent = Instant::now();
                    return Step::Frames(vec![bifrost_protocol::anthropic::response::ping_frame()]);
                }
                Step::Frames(Vec::new())
            }
        }
    }

    /// Decode one read and render whatever it completed.
    fn absorb(&mut self, chunk: &[u8]) -> Step {
        let mut frames = Vec::new();
        for line in self.lines.push(chunk) {
            self.absorb_line(&line, &mut frames);
        }
        if self.lines.pending_len() > MAX_LINE_BYTES {
            return Step::Failed(Error::upstream(format!(
                "Upstream line exceeds {MAX_LINE_BYTES} bytes without a terminator"
            )));
        }
        if frames.is_empty() {
            // Deliberately not `Ended`: a read that produced no visible frame is
            // still progress, and reporting it as an end would truncate the
            // stream.
            return Step::Frames(Vec::new());
        }
        self.last_sent = Instant::now();
        Step::Frames(frames)
    }

    fn absorb_line(&mut self, line: &str, frames: &mut Vec<SseFrame>) {
        let DecodedLine { chunks, .. } = self.decoder.decode_line(line);
        for chunk in &chunks {
            if let OutputChunk::UpstreamError { error } = chunk {
                // The drain below still records it, but the caller needs to hear
                // about it as a failure rather than as output.
                self.accumulator.push(chunk);
                frames.extend(self.renderer.failure_frames(error));
                continue;
            }
            self.accumulator.push(chunk);
            frames.extend(self.renderer.generate(chunk));
        }
    }

    /// The tail of a stream that did not end on a newline.
    fn finish_tail(&mut self) -> Vec<SseFrame> {
        let mut frames = Vec::new();
        if let Some(tail) = self.lines.take_remainder() {
            self.absorb_line(&tail, &mut frames);
        }
        frames
    }

    /// Write the audit record for a turn that will produce nothing more.
    ///
    /// Committed streams are answered `200` whatever the stream then says, and the
    /// frames this archive keeps are where the rest of the story is.
    fn finalize(&mut self) {
        if let Some(capture) = self.capture.take() {
            capture.finish();
        }
    }
}

impl Drop for Session {
    /// Record what the stream produced, however it ended.
    ///
    /// The body is dropped when the client goes away, and that is precisely the
    /// case where the evidence matters most: a client that stopped reading is why
    /// the turn ended early, and the upstream's side of it is otherwise lost.
    fn drop(&mut self) {
        self.finalize();
    }
}

/// Hand back a status that was never spent, recording it first.
///
/// The session is dropped as this returns, and its capture is written from the
/// status recorded here: a turn that never became a stream is answered with a
/// status, and the record should name the one the client was given.
fn unwritten(session: &Session, error: Error) -> Opening {
    if let Some(capture) = &session.capture {
        capture.set_status(error.kind.status());
    }
    Opening::Unwritten { error: Box::new(error) }
}

/// Read the upstream and hand what arrives to the session.
///
/// A task of its own, because the session is only polled while the client is
/// reading it. Everything the reader owns is dropped when this ends — including,
/// when a watchdog aborts it, the upstream response.
async fn pump(
    mut response: reqwest::Response,
    sender: mpsc::Sender<Pump>,
    sink: Option<Sink>,
    backpressure: Arc<Backpressure>,
) {
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if let Some(sink) = &sink {
                    sink.keep(&chunk);
                }
                if !hand_over(&sender, Pump::Chunk(chunk), &backpressure).await {
                    // The client is gone; there is nobody left to hand the rest to.
                    return;
                }
            }
            Ok(None) => {
                let _ = hand_over(&sender, Pump::End, &backpressure).await;
                return;
            }
            Err(error) => {
                let failed = Error::upstream(format!("Upstream error: {error}"));
                let _ = hand_over(&sender, Pump::Failed(Box::new(failed)), &backpressure).await;
                return;
            }
        }
    }
}

/// Hand one message to the session, reporting whether the session is still there.
///
/// The wait around it is the whole point: a bounded channel that is full is a
/// client that is not reading, and how long that lasts is what the watchdog reads.
async fn hand_over(sender: &mpsc::Sender<Pump>, message: Pump, backpressure: &Backpressure) -> bool {
    backpressure.begin();
    let sent = sender.send(message).await.is_ok();
    backpressure.end();
    sent
}

/// Cut the upstream loose when the client has stopped taking what it produces.
async fn watch(backpressure: Arc<Backpressure>, reader: tokio::task::AbortHandle, stall: Stall) {
    // A fraction of the allowance, so the deadline is honoured within it. Bounded at
    // both ends: a deployment that allows a second does not want a wake-up every
    // 250 µs, and one that allows an hour should still notice.
    let interval = (stall.after / 4).clamp(Duration::from_millis(10), Duration::from_secs(1));
    loop {
        tokio::time::sleep(interval).await;
        if backpressure.blocked_for() >= stall.after {
            // Dropping the reader drops the upstream response, which closes the
            // connection: the point is that the upstream stops generating tokens
            // that nobody is going to read.
            reader.abort();
            stall.edge.note_client_stall();
            crate::log::warn(format!(
                "client stopped reading a stream for {}ms; closed the upstream",
                stall.after.as_millis()
            ));
            return;
        }
    }
}

/// One turn of the read loop.
enum Step {
    Frames(Vec<SseFrame>),
    Ended,
    Idle,
    Failed(Error),
}

/// Join frames into one write.
///
/// The original writes each frame separately and lets the runtime coalesce them;
/// batching here is the same bytes with fewer syscalls, and the client sees
/// frames either way because the frame boundary is what it parses.
fn encode(frames: &[SseFrame]) -> Bytes {
    let mut buffer = String::new();
    for frame in frames {
        buffer.push_str(frame.as_str());
    }
    Bytes::from(buffer)
}

/// The response body of a committed stream.
///
/// The admission permit is carried inside the stream state rather than held by
/// the handler: a streaming response outlives the handler that produced it, and
/// a slot released at return time would let an unlimited number of streams run
/// while the counter reported an empty house.
pub fn into_body(session: Box<Session>, permit: Option<InflightPermit>) -> Body {
    let stream = futures_util::stream::unfold((session, permit), |(mut session, permit)| async move {
        session
            .next_batch()
            .await
            .map(|bytes| (Ok::<Bytes, Infallible>(bytes), (session, permit)))
    });
    Body::from_stream(stream)
}

/// The headers every streamed response carries.
#[must_use]
pub fn headers() -> axum::http::HeaderMap {
    use axum::http::{HeaderMap, HeaderValue, header};

    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    headers.insert(header::CONNECTION, HeaderValue::from_static("keep-alive"));
    // Tells an nginx in front of the proxy not to buffer the stream, which would
    // otherwise hold every frame until the response ended.
    headers.insert("x-accel-buffering", HeaderValue::from_static("no"));
    headers
}
