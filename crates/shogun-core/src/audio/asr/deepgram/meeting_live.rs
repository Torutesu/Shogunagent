//! Meeting-only reconnect/replay. One 30s PCM16 ring per stream, no audio channel or disk spill.
//! Socket work never holds the ring lock. Drop cancels I/O and joins the sole worker.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Notify, Semaphore};
use tokio::time::{sleep, timeout, Instant};
use tungstenite::{client::IntoClientRequest, Message};

use super::{DeepgramAuth, DeepgramConfig, LiveMode, LiveResult, TraceabilitySink, SAMPLE_RATE};

const MAX_SAMPLES: usize = SAMPLE_RATE as usize * 30;
const CHUNK: usize = super::LIVE_CHUNK_SAMPLES;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const FINISH_TIMEOUT: Duration = Duration::from_secs(5);
/// Deepgram closes a live stream that receives no data for about ten seconds. An idle lane (a
/// muted or disconnected input sends nothing until a whole chunk is buffered) sends this
/// control-only message instead, never synthetic audio, like the dictation standby does.
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(4);
/// After this outage duration, surface a paused notice while the same worker keeps retrying.
/// Replay remains bounded to 30 seconds, including while offline.
const RECONNECT_BUDGET: Duration = Duration::from_secs(5 * 60);
// Globally bounded, even when a previous session is waiting on a stuck database. Jobs own
// only content-free metadata and a sink, never PCM, Shared, credentials, or a socket.
static TRACE_SLOTS: Semaphore = Semaphore::const_new(2);

/// Content-free status; loss remains visible for the rest of this stream's lifetime.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// A WebSocket handshake succeeded and this stream has not disconnected. This differs from
    /// silence and from the brief initial connection attempt before any retry is needed.
    pub connected: bool,
    /// The provider refused the key at the handshake (HTTP 401 / 403): a wrong, revoked or
    /// out-of-credit key. Retrying cannot fix it, so the stream stops and says so instead of
    /// reporting a network interruption for the whole meeting (issue #781).
    pub auth_rejected: bool,
    /// The provider refused the stream for payment (HTTP 402): the account is out of credit.
    /// Terminal like `auth_rejected`; the key itself may be fine.
    pub quota_exhausted: bool,
    pub reconnecting: bool,
    pub network_paused: bool,
    /// A retryable failure after reaching the service; never an offline claim.
    pub service_unavailable: bool,
    pub catching_up: bool,
    pub dropped_samples: u64,
    /// Samples still unconfirmed when the stream ended after Stop: the last words the provider
    /// never acknowledged. Part of `dropped_samples`; reported separately because any lost tail,
    /// however short, is the end of the meeting.
    pub lost_tail_samples: u64,
    pub stopped: bool,
}

/// Re-read before every handshake (issue #782): may the user's personal vocabulary still go to
/// the provider? It runs on a blocking thread, never on the socket task, so a settings read
/// cannot stall captions or teardown.
pub type PersonalVocabularyConsent = Arc<dyn Fn() -> bool + Send + Sync>;

/// Where a meeting stream connects, decided again before every (re)connect. Keyterms travel in the
/// query string, so a reconnect after the user switched vocabulary sharing off must not reuse a
/// URL that still carries their personal terms (issue #782).
struct Endpoint {
    hosted: Option<Arc<crate::audio::asr::hosted::Session>>,
    /// The URL built with the vocabulary the stream started with.
    personal_url: String,
    personal_has_keyterms: bool,
    /// The URL without personal terms (built-in hints only), used once consent is gone.
    plain_url: String,
    plain_has_keyterms: bool,
    /// `None`: the stream carries no personal terms, so there is nothing to re-check.
    consent: Option<PersonalVocabularyConsent>,
}

impl Endpoint {
    fn fixed(url: String, has_keyterms: bool) -> Self {
        Self {
            hosted: None,
            personal_url: url.clone(),
            personal_has_keyterms: has_keyterms,
            plain_url: url,
            plain_has_keyterms: has_keyterms,
            consent: None,
        }
    }

    /// The meeting URL with the stream's vocabulary, and the same URL with only
    /// `without_personal`, chosen per handshake by `consent`.
    fn with_consent(
        config: &DeepgramConfig,
        without_personal: Vec<String>,
        consent: PersonalVocabularyConsent,
    ) -> Self {
        let plain = config.clone().with_keyterms(without_personal);
        Self {
            hosted: None,
            personal_url: super::live_listen_url(config, LiveMode::Meeting),
            personal_has_keyterms: !config.keyterms.is_empty(),
            plain_url: super::live_listen_url(&plain, LiveMode::Meeting),
            plain_has_keyterms: !plain.keyterms.is_empty(),
            consent: Some(consent),
        }
    }

    fn with_hosted(mut self, hosted: Option<Arc<crate::audio::asr::hosted::Session>>) -> Self {
        if let Some(session) = &hosted {
            self.personal_url.push_str(&format!("&tag={}", session.user_tag));
            self.plain_url.push_str(&format!("&tag={}", session.user_tag));
        }
        self.hosted = hosted;
        self
    }

    /// The URL and whether it carries keyterms, given the consent just read.
    fn choose(&self, personal_allowed: bool) -> (&str, bool) {
        if personal_allowed {
            (&self.personal_url, self.personal_has_keyterms)
        } else {
            (&self.plain_url, self.plain_has_keyterms)
        }
    }
}

/// Pick this handshake's URL and put its keyterm hints on the ledger before they are sent
/// (invariant 3): one content-free `meeting_asr_keyterms` row per handshake that carries hints.
/// Consent is re-read here, so a reconnect after revocation sends no personal terms.
async fn prepare_handshake(
    endpoint: &Arc<Endpoint>,
    trace: &Arc<dyn TraceabilitySink>,
) -> Result<String, ()> {
    if endpoint.consent.is_none() && !endpoint.personal_has_keyterms {
        return Ok(endpoint.personal_url.clone());
    }
    let endpoint = Arc::clone(endpoint);
    let trace = Arc::clone(trace);
    let permit = TRACE_SLOTS.acquire().await.map_err(|_| ())?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let personal_allowed = endpoint.consent.as_ref().is_none_or(|consent| consent());
        let (url, has_keyterms) = endpoint.choose(personal_allowed);
        // Issue #831: the hints ride the handshake, so no row means no connection.
        super::record_keyterm_attempt(trace.as_ref(), "meeting_asr", has_keyterms)
            .map(|()| url.to_string())
    })
    .await
    .map_err(|_| ())?
    .map_err(|_| ())
}

/// Timestamp is an offset into captured audio, preserved across WebSocket sessions.
pub struct ResultEvent {
    pub result: LiveResult,
    pub offset_ms: u64,
    pub end_offset_ms: u64,
}

struct Buffer {
    pcm: VecDeque<i16>,
    /// Absolute sample offset of the oldest retained sample.
    start: u64,
    end: u64,
    confirmed: u64,
    finishing: bool,
    /// A handshake answer that retrying cannot fix (see [`Refusal`]): the worker stops.
    refused: bool,
    status: Status,
}

impl Buffer {
    fn new() -> Self {
        Self {
            pcm: VecDeque::with_capacity(MAX_SAMPLES),
            start: 0,
            end: 0,
            confirmed: 0,
            finishing: false,
            refused: false,
            status: Status::default(),
        }
    }

    fn push(&mut self, samples: &[f32]) {
        if self.finishing || self.status.stopped {
            return;
        }
        // Evict before extending, including oversized input: allocation never grows past 30s.
        let overflow = (self.pcm.len() + samples.len()).saturating_sub(MAX_SAMPLES);
        let existing = overflow.min(self.pcm.len());
        self.pcm.drain(..existing);
        let skip = overflow - existing;
        self.pcm.extend(
            samples[skip..]
                .iter()
                .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16),
        );
        self.end += samples.len() as u64;
        self.start += overflow as u64;
        self.status.dropped_samples += overflow as u64;
    }

    fn push_at(&mut self, samples: &[f32], offset: Option<u64>) {
        if self.finishing || self.status.stopped {
            return;
        }
        if let Some(offset) = offset {
            if offset < self.end {
                return;
            }
            let gap = offset - self.end;
            if gap > 0 {
                // Silence preserves the provider clock across missing capture frames. Retain
                // at most 30s, even if capture was stalled for hours; no gap-sized allocation.
                let keep = gap.min(MAX_SAMPLES as u64) as usize;
                let evict = (self.pcm.len() + keep).saturating_sub(MAX_SAMPLES);
                self.pcm.drain(..evict);
                self.pcm.extend(std::iter::repeat_n(0, keep));
                self.end = offset;
                self.start = self.end - self.pcm.len() as u64;
                self.status.dropped_samples += gap + evict as u64;
            }
        }
        self.push(samples);
    }

    fn finish_timeout(&self) -> Duration {
        // 1.25x playback plus connection/final-result grace, bounded by the 30s ring.
        FINISH_TIMEOUT
            + Duration::from_secs_f64(self.pcm.len() as f64 / (SAMPLE_RATE as f64 * 1.25))
    }

    fn acknowledge(&mut self, end: u64) {
        self.confirmed = self.confirmed.max(end);
        let count = end.saturating_sub(self.start).min(self.pcm.len() as u64) as usize;
        self.pcm.drain(..count);
        self.start += count as u64;
    }

    fn release(&mut self) {
        if self.finishing {
            self.status.lost_tail_samples += self.pcm.len() as u64;
        }
        self.status.dropped_samples += self.pcm.len() as u64;
        self.pcm = VecDeque::new();
        self.start = self.end;
        self.status.stopped = true;
        self.status.connected = false;
        self.status.reconnecting = false;
        self.status.network_paused = false;
        self.status.service_unavailable = false;
        self.status.catching_up = false;
    }
}

struct Shared {
    buffer: Mutex<Buffer>,
    wake: Notify,
    finish: Notify,
    /// Cuts the reconnect backoff short on Stop. Separate from `finish`, which only the outer
    /// worker may consume: it starts the bounded flush budget.
    retry_now: Notify,
    cancel: Notify,
}

impl Shared {
    fn buffer(&self) -> MutexGuard<'_, Buffer> {
        self.buffer.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Capture-facing handle. Owns exactly one network worker, including all reconnect attempts.
pub struct MeetingLive {
    shared: Arc<Shared>,
    results: mpsc::Receiver<ResultEvent>,
    worker: Option<JoinHandle<()>>,
}

impl MeetingLive {
    pub fn start(
        config: &DeepgramConfig,
        auth: &mut dyn DeepgramAuth,
        trace: Arc<dyn TraceabilitySink>,
    ) -> Result<Self, String> {
        if !config.mip_opt_out {
            return Err("Meeting transcription requires private processing.".into());
        }
        // BYOK is cached; hosted credentials refresh within the existing reservation on reconnect.
        let authorization = auth
            .authorization_header()
            .map_err(|e| super::public_error(&e))?;
        Self::start_endpoint(
            Endpoint::fixed(
                super::live_listen_url(config, LiveMode::Meeting),
                !config.keyterms.is_empty(),
            ).with_hosted(auth.hosted_session()?),
            authorization,
            trace,
        )
    }

    /// [`Self::start`] for a stream whose `config.keyterms` include the user's personal
    /// vocabulary (issue #782). `consent` is re-read before every reconnect; once it says no, the
    /// stream reconnects with `without_personal` (the built-in hints) instead, so switching
    /// sharing off mid-meeting stops personal terms from leaving on the next network blip.
    pub fn start_with_vocabulary_consent(
        config: &DeepgramConfig,
        without_personal: Vec<String>,
        consent: PersonalVocabularyConsent,
        auth: &mut dyn DeepgramAuth,
        trace: Arc<dyn TraceabilitySink>,
    ) -> Result<Self, String> {
        if !config.mip_opt_out {
            return Err("Meeting transcription requires private processing.".into());
        }
        let authorization = auth
            .authorization_header()
            .map_err(|e| super::public_error(&e))?;
        Self::start_endpoint(
            Endpoint::with_consent(config, without_personal, consent).with_hosted(auth.hosted_session()?),
            authorization,
            trace,
        )
    }

    #[cfg(test)]
    fn start_url(
        url: String,
        authorization: String,
        trace: Arc<dyn TraceabilitySink>,
    ) -> Result<Self, String> {
        Self::start_endpoint(Endpoint::fixed(url, false), authorization, trace)
    }

    fn start_endpoint(
        endpoint: Endpoint,
        authorization: String,
        trace: Arc<dyn TraceabilitySink>,
    ) -> Result<Self, String> {
        Self::start_endpoint_with_budget(endpoint, authorization, trace, RECONNECT_BUDGET)
    }

    fn start_endpoint_with_budget(
        endpoint: Endpoint,
        authorization: String,
        trace: Arc<dyn TraceabilitySink>,
        retry_budget: Duration,
    ) -> Result<Self, String> {
        let endpoint = Arc::new(endpoint);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| super::PUBLIC_PROVIDER_FAILURE.to_string())?;
        let shared = Arc::new(Shared {
            buffer: Mutex::new(Buffer::new()),
            wake: Notify::new(),
            finish: Notify::new(),
            retry_now: Notify::new(),
            cancel: Notify::new(),
        });
        // Backpressure retains finals instead of silently dropping them. A stalled consumer can
        // only fill this small text queue and the fixed PCM ring, never an unbounded audio queue.
        let (tx, results) = mpsc::channel(32);
        let state = shared.clone();
        let worker = std::thread::Builder::new()
            .name("meeting-asr".into())
            .spawn(move || {
                runtime.block_on(async {
                    tokio::select! {
                    biased;
                    _ = state.cancel.notified() => {}
                    _ = async {
                        if let Some(session) = &endpoint.hosted {
                            tokio::time::sleep_until(Instant::from_std(session.deadline)).await;
                        } else { std::future::pending::<()>().await; }
                    } => {}
                    _ = async { state.finish.notified().await; let budget = state.buffer().finish_timeout(); sleep(budget).await; } => {}
                    _ = reconnect(&endpoint, &authorization, &trace, &state, &tx, retry_budget) => {}
                    }
                });
                state.buffer().release();
                // Tokio's OS resolver can outlive cancellation, but owns no PCM/Shared. Do not wait
                // indefinitely for an OS DNS call during teardown, nor spawn a resolver per retry.
                runtime.shutdown_timeout(Duration::from_millis(100));
            })
            .map_err(|_| super::PUBLIC_PROVIDER_FAILURE.to_string())?;
        Ok(Self {
            shared,
            results,
            worker: Some(worker),
        })
    }

    pub fn push_pcm(&self, samples: &[f32]) {
        self.shared.buffer().push(samples);
        self.shared.wake.notify_one();
    }

    /// Offset counts all captured samples, including frames lost before reaching this ring.
    pub fn push_pcm_at(&self, samples: &[f32], offset: Option<u64>) {
        self.shared.buffer().push_at(samples, offset);
        self.shared.wake.notify_one();
    }

    pub fn try_recv(&mut self) -> Option<ResultEvent> {
        self.results.try_recv().ok()
    }

    pub fn status(&self) -> Status {
        let mut status = self.shared.buffer().status;
        status.stopped |= self.worker.as_ref().is_some_and(|w| w.is_finished());
        status
    }

    /// Request a final flush without blocking capture or the other meeting stream.
    pub fn begin_finish(&self) {
        self.shared.buffer().finishing = true;
        self.shared.finish.notify_one();
        self.shared.retry_now.notify_one();
        self.shared.wake.notify_one();
    }

    /// Flush the bounded backlog plus five seconds of grace; report any unconfirmed tail.
    pub fn finish(mut self, mut emit: impl FnMut(ResultEvent)) -> Status {
        self.begin_finish();
        loop {
            while let Some(result) = self.try_recv() {
                emit(result);
            }
            if self.worker.as_ref().is_none_or(|w| w.is_finished()) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        while let Some(result) = self.try_recv() {
            emit(result);
        }
        self.join();
        self.status()
    }

    fn join(&mut self) {
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for MeetingLive {
    fn drop(&mut self) {
        self.shared.cancel.notify_one();
        self.join();
        // Also handles a worker panic: the handle always relinquishes the audio allocation.
        self.shared.buffer().release();
    }
}

/// Content-free timing of the per-chunk audit record (issue #917): how long each record took from
/// request to written (slot wait + shared-connection wait + insert), and how long an upload that
/// was due had to wait for it. Logged once per stream; numbers only.
struct AuditTimings {
    record: crate::metrics::Histogram,
    upload_wait: crate::metrics::Histogram,
}

impl Drop for AuditTimings {
    fn drop(&mut self) {
        if self.record.count() > 0 {
            eprintln!(
                "[meeting-asr] audit record {} | upload held by audit {}",
                self.record.summary(),
                self.upload_wait.summary()
            );
        }
    }
}

async fn reconnect(
    endpoint: &Arc<Endpoint>,
    authorization: &str,
    trace: &Arc<dyn TraceabilitySink>,
    shared: &Shared,
    results: &mpsc::Sender<ResultEvent>,
    retry_budget: Duration,
) {
    let mut retry = Duration::from_millis(250);
    // Only consecutive DNS/TCP failures can become an offline notice.
    let mut outage_since: Option<Instant> = None;
    let mut auth_failure_since: Option<Instant> = None;
    let mut next_speaker = 0_u32;
    let mut timings = AuditTimings {
        record: crate::metrics::Histogram::latency_ms(),
        upload_wait: crate::metrics::Histogram::latency_ms(),
    };
    loop {
        let started = Instant::now();
        // Once Stop is requested, one more attempt is made only for a tail awaiting confirmation.
        let final_attempt = {
            let buffer = shared.buffer();
            if buffer.finishing && buffer.pcm.is_empty() {
                return;
            }
            buffer.finishing
        };
        let refreshed;
        let authorization = if let Some(session) = &endpoint.hosted {
            let session = session.clone();
            match tokio::task::spawn_blocking(move || session.authorization_header()).await {
                Ok(Ok(value)) => {
                    auth_failure_since = None;
                    refreshed = value;
                    refreshed.as_str()
                }
                _ => {
                    if final_attempt
                        || auth_failure_since.get_or_insert_with(Instant::now).elapsed() >= retry_budget
                    {
                        return;
                    }
                    {
                        let mut buffer = shared.buffer();
                        outage_since = None;
                        buffer.status.connected = false;
                        buffer.status.reconnecting = true;
                        buffer.status.network_paused = false;
                        buffer.status.service_unavailable = true;
                    }
                    sleep(retry).await;
                    retry = (retry * 2).min(Duration::from_secs(20));
                    continue;
                }
            }
        } else { authorization };
        let outcome = connection(
            endpoint,
            authorization,
            trace,
            shared,
            results,
            &mut next_speaker,
            &mut timings,
        )
        .await;
        let closed = outcome.is_ok();
        // A connection that stayed up this long was healthy: a new outage starts now.
        let healthy = shared.buffer().status.connected && started.elapsed() > Duration::from_secs(10);
        if healthy {
            retry = Duration::from_millis(250);
            outage_since = None;
        }
        let offline = if outcome == Err(ConnectionFailure::Connect) {
            outage_since.get_or_insert_with(Instant::now).elapsed() >= retry_budget
        } else {
            outage_since = None;
            false
        };
        {
            let mut buffer = shared.buffer();
            if buffer.refused || (buffer.finishing && (closed || final_attempt)) {
                return;
            }
            // Stop landed while this attempt was connecting or flushing: retry at once. The
            // outer flush budget bounds it; Drop cancels it separately.
            if buffer.finishing {
                continue;
            }
            buffer.status.connected = false;
            buffer.status.network_paused = offline;
            buffer.status.service_unavailable = outcome == Err(ConnectionFailure::Other);
            buffer.status.reconnecting = true;
        }
        tokio::select! {
            _ = sleep(retry) => {}
            _ = shared.retry_now.notified() => {}
        }
        retry = (retry * 2).min(Duration::from_secs(5));
    }
}

/// What a refused handshake means for the stream. Only network trouble, rate limiting (429) and
/// provider-side errors (5xx) are worth retrying; every other answer would repeat itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refusal {
    /// 401 / 403: the key itself was refused.
    Key,
    /// 402: the account is out of credit.
    Quota,
    /// Any other 4xx (bad request, gone, …): stop and say transcription stopped.
    Fatal,
    /// 408, 429, 5xx, or no HTTP answer at all: retry with bounded backoff.
    Transient,
}

fn classify_refusal(error: &tungstenite::Error) -> Refusal {
    let tungstenite::Error::Http(response) = error else {
        return Refusal::Transient;
    };
    match response.status().as_u16() {
        401 | 403 => Refusal::Key,
        402 => Refusal::Quota,
        408 | 429 => Refusal::Transient,
        400..=499 => Refusal::Fatal,
        _ => Refusal::Transient,
    }
}

/// Record a refused handshake on the shared status. Content-free: only the class is logged.
fn note_refusal(shared: &Shared, refusal: Refusal) {
    let mut buffer = shared.buffer();
    match refusal {
        Refusal::Key => buffer.status.auth_rejected = true,
        Refusal::Quota => buffer.status.quota_exhausted = true,
        Refusal::Fatal => {}
        Refusal::Transient => return,
    }
    buffer.refused = true;
    drop(buffer);
    crate::elog!("[meeting-asr] handshake refused class={refusal:?}; stream stopped");
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionFailure {
    Connect,
    Other,
}

impl From<()> for ConnectionFailure {
    fn from(_: ()) -> Self {
        Self::Other
    }
}

async fn connection(
    endpoint: &Arc<Endpoint>,
    authorization: &str,
    trace: &Arc<dyn TraceabilitySink>,
    shared: &Shared,
    results: &mpsc::Sender<ResultEvent>,
    next_speaker: &mut u32,
    timings: &mut AuditTimings,
) -> Result<(), ConnectionFailure> {
    // Provider speaker IDs restart on reconnect. Never attach a previous connection's name
    // to a newly numbered voice; allocate a fresh bounded integer namespace, not a voice map.
    let speaker_base = *next_speaker;
    let config = tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(64 * 1024))
        .max_frame_size(Some(64 * 1024))
        .max_write_buffer_size(64 * 1024)
        .write_buffer_size(0);
    // Every URL an endpoint can pick shares one host and port (only the hints differ), so the
    // address is resolved before the URL is chosen.
    let target = endpoint
        .personal_url
        .as_str()
        .into_client_request()
        .map_err(|_| ())?;
    let host = target.uri().host().ok_or(())?.to_string();
    let port = target
        .uri()
        .port_u16()
        .unwrap_or(if target.uri().scheme_str() == Some("wss") {
            443
        } else {
            80
        });
    // Only one DNS lookup may be outstanding per stream. The outer cancellation still stops
    // this worker immediately, even if the OS resolver takes longer than our connect budget.
    let addresses: Vec<_> = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|_| ConnectionFailure::Connect)?
        .collect();
    let tcp = timeout(
        CONNECT_TIMEOUT,
        tokio::net::TcpStream::connect(addresses.as_slice()),
    )
    .await
    .map_err(|_| ConnectionFailure::Connect)?
    .map_err(|_| ConnectionFailure::Connect)?;
    let (mut socket, _) = timeout(CONNECT_TIMEOUT, async {
        // Keyterms ride in the URL, so they leave with the handshake: choose the URL under the
        // current consent and record the hints before it is sent (issue #782). After the TCP
        // connect, so an offline retry loop does not fill the ledger with requests that never
        // left the device.
        let url = prepare_handshake(endpoint, trace).await?;
        let mut request = url.into_client_request().map_err(|_| ())?;
        request
            .headers_mut()
            .insert("Authorization", authorization.parse().map_err(|_| ())?);
        tokio_tungstenite::client_async_tls_with_config(request, tcp, Some(config), None)
            .await
            .map_err(|error| note_refusal(shared, classify_refusal(&error)))
    })
    .await
    .map_err(|_| ())??;
    let base = {
        let mut buffer = shared.buffer();
        buffer.status.connected = true;
        buffer.status.reconnecting = false;
        buffer.status.network_paused = false;
        buffer.status.service_unavailable = false;
        buffer.start
    };
    let mut sent = base;
    let mut next_send = Instant::now();
    let mut closing = false;
    let mut pending_audio = Vec::new();
    let mut pending_trace = None;
    let mut traced = false;
    let mut keep_alive_at = Instant::now() + KEEP_ALIVE_INTERVAL;
    loop {
        let (bytes, finishing) = {
            let mut buffer = shared.buffer();
            // The oldest unsent audio was evicted. Restart so provider timestamps stay aligned
            // with the actual retained audio; never concatenate across an unmarked hole.
            if sent < buffer.start {
                return Err(ConnectionFailure::Other);
            }
            buffer.status.catching_up = buffer.end.saturating_sub(sent) > SAMPLE_RATE as u64;
            let available = buffer.end.saturating_sub(sent);
            let bytes = if pending_audio.is_empty()
                && !closing
                && (available >= CHUNK as u64 || buffer.finishing)
            {
                buffer
                    .pcm
                    .iter()
                    .skip((sent - buffer.start) as usize)
                    .take(CHUNK)
                    .flat_map(|sample| sample.to_le_bytes())
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            (bytes, buffer.finishing)
        };
        if !bytes.is_empty() {
            let count = bytes.len() / 2;
            pending_audio = bytes;
            let trace = Arc::clone(trace);
            // Start logging during the pacing interval. Poll it alongside socket reads so a
            // busy database cannot hold already-transcribed captions behind the next upload.
            // A chunk traced here whose send then fails is replayed after the reconnect and
            // traced again, so the ledger can slightly over-count bytes around a reconnect. It
            // never under-counts: nothing is sent without its own record first (invariant 3).
            let requested = Instant::now();
            pending_trace = Some(Box::pin(async move {
                let permit = TRACE_SLOTS.acquire().await.map_err(|_| ())?;
                let recorded = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    super::record_asr_attempt(
                        trace.as_ref(),
                        "meeting_asr",
                        count * 2,
                        count as u64 * 1000 / SAMPLE_RATE as u64,
                        false,
                    )
                })
                .await
                .map_err(|_| ())?;
                // Issue #831: no ledger row, no upload. The stream ends here with the chunk
                // unsent and still buffered; the reconnect path retries it once the log is
                // writable again, and nothing leaves until then.
                if recorded.is_err() {
                    crate::elog!("[meeting] traceability log unavailable; audio not sent");
                    return Err(());
                }
                Ok(requested.elapsed())
            }));
        }
        if traced && Instant::now() >= next_send {
            let count = pending_audio.len() / 2;
            timeout(
                Duration::from_secs(2),
                socket.send(Message::Binary(std::mem::take(&mut pending_audio).into())),
            )
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
            traced = false;
            sent += count as u64;
            keep_alive_at = Instant::now() + KEEP_ALIVE_INTERVAL;
            // Keep 1.25x replay pacing relative to the actual send, never burst after a stall.
            next_send = Instant::now()
                + Duration::from_secs_f64(count as f64 / (SAMPLE_RATE as f64 * 1.25));
            // Prepare/log the next chunk immediately instead of first waiting out this timer.
            continue;
        }
        if finishing && !closing && sent == shared.buffer().end {
            timeout(
                Duration::from_secs(2),
                socket.send(Message::Text(r#"{"type":"CloseStream"}"#.into())),
            )
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
            closing = true;
        }
        // An idle lane (nothing captured, or less than one chunk) would otherwise be closed by
        // the provider and reconnect in a loop (issue #1005). Control-only: no audio, no trace.
        if !closing && Instant::now() >= keep_alive_at {
            timeout(
                Duration::from_secs(2),
                socket.send(Message::Text(r#"{"type":"KeepAlive"}"#.into())),
            )
            .await
            .map_err(|_| ())?
            .map_err(|_| ())?;
            keep_alive_at = Instant::now() + KEEP_ALIVE_INTERVAL;
        }
        tokio::select! {
            ready = async {
                match pending_trace.as_mut() {
                    Some(trace) => trace.await,
                    None => std::future::pending().await,
                }
            } => {
                let took = ready?;
                timings.record.record(took.as_secs_f64() * 1000.0);
                let held = Instant::now().saturating_duration_since(next_send);
                timings.upload_wait.record(held.as_secs_f64() * 1000.0);
                pending_trace = None;
                traced = true;
            }
            message = socket.next() => {
                match message.ok_or(())?.map_err(|_| ())? {
                    Message::Text(text) => {
                        // Final Results (including empty silence results) are the acknowledgement,
                        // not a successful socket write or an interim transcript.
                        let body: serde_json::Value = serde_json::from_str(&text).map_err(|_| ())?;
                        // CloseStream's summary follows all final Results and confirms processing
                        // of the trailing silence too: https://developers.deepgram.com/docs/close-stream
                        if closing && body.get("type").and_then(|v| v.as_str()) == Some("Metadata") {
                            if let Some(duration) = body.get("duration").and_then(|v| v.as_f64()) {
                                if metadata_confirms(duration, sent - base) {
                                    shared.buffer().acknowledge(sent);
                                    return Ok(());
                                }
                            }
                        }
                        if body.get("type").and_then(|v| v.as_str()) == Some("Results") {
                            let Some((start, end)) = result_range(&body, base, sent) else { return Err(ConnectionFailure::Other); };
                            let final_result = body.get("is_final").and_then(|v| v.as_bool()) == Some(true);
                            let confirmed = shared.buffer().confirmed;
                            if end > confirmed {
                                // Replay begins at the committed audio boundary, so provider ranges
                                // cannot straddle it unless the response is inconsistent.
                                if start < confirmed { return Err(ConnectionFailure::Other); }
                                for (index, mut result) in super::parse_live_messages(&text).into_iter().enumerate() {
                                    result.speaker = result.speaker.and_then(|slot| speaker_base.checked_add(slot));
                                    if let Some(slot) = result.speaker {
                                        *next_speaker = (*next_speaker).max(slot.saturating_add(1));
                                    }
                                    results.send(ResultEvent { result, offset_ms: start * 1000 / SAMPLE_RATE as u64 + index as u64, end_offset_ms: end * 1000 / SAMPLE_RATE as u64 })
                                        .await.map_err(|_| ())?;
                                }
                                if final_result { shared.buffer().acknowledge(end); }
                            }
                        }
                    }
                    Message::Close(_) => return if closing { Ok(()) } else { Err(ConnectionFailure::Other) },
                    Message::Ping(_) => { socket.flush().await.map_err(|_| ())?; }
                    _ => {}
                }
            }
            _ = sleep(next_send.saturating_duration_since(Instant::now())), if traced => {}
            _ = tokio::time::sleep_until(keep_alive_at), if !closing => {}
            _ = shared.wake.notified(), if !closing => {}
        }
    }
}

// Permit at most 1ms of duration serialization rounding in the closing summary only.
// Transcript Results retain their strict sent-range validation.
fn metadata_confirms(duration: f64, sent_samples: u64) -> bool {
    duration.is_finite()
        && duration >= 0.0
        && (duration * SAMPLE_RATE as f64 - sent_samples as f64).abs()
            <= SAMPLE_RATE as f64 / 1000.0
}

fn result_range(body: &serde_json::Value, base: u64, sent: u64) -> Option<(u64, u64)> {
    let start = body.get("start")?.as_f64()?;
    let duration = body.get("duration")?.as_f64()?;
    if !start.is_finite() || !duration.is_finite() || start < 0.0 || duration < 0.0 {
        return None;
    }
    let start = base.checked_add((start * SAMPLE_RATE as f64).round() as u64)?;
    let end = start.checked_add((duration * SAMPLE_RATE as f64).round() as u64)?;
    (end <= sent).then_some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::traceability::RecordingSink;
    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn replay_ring_bounds_acknowledgement_and_release() {
        let mut b = Buffer::new();
        b.push(&vec![0.25; MAX_SAMPLES * 2]);
        assert_eq!(b.pcm.len(), MAX_SAMPLES);
        assert_eq!(b.pcm.capacity(), MAX_SAMPLES);
        assert_eq!(b.status.dropped_samples, MAX_SAMPLES as u64);
        b.acknowledge(b.start + 100);
        assert_eq!(b.pcm.len(), MAX_SAMPLES - 100);
        let confirmed = b.confirmed;
        b.acknowledge(0);
        assert_eq!(b.confirmed, confirmed);
        b.release();
        assert_eq!(b.pcm.capacity(), 0);
        b.push(&[1.0; 320]);
        assert!(b.pcm.is_empty());
        b.release();
        assert_eq!(b.status.dropped_samples, (MAX_SAMPLES * 2 - 100) as u64);
    }

    #[test]
    fn capture_gaps_preserve_time_without_growing_the_ring() {
        let mut b = Buffer::new();
        b.push_at(&[0.25; CHUNK], Some(0));
        b.push_at(&[0.5; CHUNK], Some((CHUNK + SAMPLE_RATE as usize) as u64));
        assert_eq!(b.end, (2 * CHUNK + SAMPLE_RATE as usize) as u64);
        assert_eq!(b.pcm[CHUNK], 0);
        assert_eq!(b.pcm[CHUNK + SAMPLE_RATE as usize], 16383);
        assert_eq!(b.status.dropped_samples, SAMPLE_RATE as u64);
        let offset = SAMPLE_RATE as u64 * 3600;
        b.push_at(&[0.5; CHUNK], Some(offset));
        assert_eq!(b.end, offset + CHUNK as u64);
        assert_eq!(b.start, b.end - MAX_SAMPLES as u64);
        assert_eq!(b.pcm.capacity(), MAX_SAMPLES);
        b.release();
        assert_eq!(b.pcm.capacity(), 0);
    }

    #[test]
    fn blocked_trace_cannot_hold_audio_or_delay_drop() -> TestResult {
        struct BlockingTrace {
            entered: std::sync::mpsc::SyncSender<()>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl TraceabilitySink for BlockingTrace {
            fn record(&self, _: crate::llm::traceability::TraceRecord) {
                let _ = self.entered.send(());
                if let Ok(rx) = self.release.lock() {
                    let _ = rx.recv_timeout(Duration::from_secs(5));
                }
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("ws://{}", listener.local_addr()?);
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let trace = Arc::new(BlockingTrace {
            entered: entered_tx,
            release: Mutex::new(release_rx),
        });
        let weak_trace = Arc::downgrade(&trace);
        let live = MeetingLive::start_url(url, "Token test".into(), trace)?;
        let weak_audio = Arc::downgrade(&live.shared);
        live.push_pcm(&[0.25; CHUNK]);
        let (tcp, _) = listener.accept()?;
        tcp.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut ws = tungstenite::accept(tcp)?;
        entered_rx.recv_timeout(Duration::from_secs(5))?;
        let started = std::time::Instant::now();
        drop(live);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(weak_audio.upgrade().is_none());
        assert!(
            ws.read().is_err(),
            "no audio may leave before tracing returns"
        );
        release_tx.send(())?;
        wait_until(|| weak_trace.upgrade().is_none())?;
        Ok(())
    }

    #[test]
    fn an_unwritable_trace_log_keeps_audio_on_the_device() -> TestResult {
        // Issue #831: the ledger write failed, so the chunk must not be uploaded.
        struct FailingTrace;
        impl TraceabilitySink for FailingTrace {
            fn record(&self, _: crate::llm::traceability::TraceRecord) {}
            fn try_record(
                &self,
                _: crate::llm::traceability::TraceRecord,
            ) -> Result<(), crate::llm::traceability::TraceUnrecorded> {
                Err(crate::llm::traceability::TraceUnrecorded)
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("ws://{}", listener.local_addr()?);
        let live = MeetingLive::start_url(url, "Token test".into(), Arc::new(FailingTrace))?;
        live.push_pcm(&[0.25; CHUNK]);
        let (tcp, _) = listener.accept()?;
        tcp.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut ws = tungstenite::accept(tcp)?;
        // Read until the client gives up on this socket (or the timeout): nothing binary arrives.
        while let Ok(message) = ws.read() {
            assert!(
                !matches!(message, tungstenite::Message::Binary(_)),
                "audio left without a ledger row"
            );
        }
        drop(live);
        Ok(())
    }

    #[test]
    fn incoming_results_do_not_wait_for_the_next_audio_trace() -> TestResult {
        struct BlockingSecondTrace {
            calls: std::sync::atomic::AtomicUsize,
            entered: std::sync::mpsc::Sender<()>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl TraceabilitySink for BlockingSecondTrace {
            fn record(&self, _: crate::llm::traceability::TraceRecord) {
                if self
                    .calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    == 1
                {
                    let _ = self.entered.send(());
                    if let Ok(rx) = self.release.lock() {
                        let _ = rx.recv_timeout(Duration::from_secs(5));
                    }
                }
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("ws://{}", listener.local_addr()?);
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let mut live = MeetingLive::start_url(
            url,
            "Token test".into(),
            Arc::new(BlockingSecondTrace {
                calls: std::sync::atomic::AtomicUsize::new(0),
                entered: entered_tx,
                release: Mutex::new(release_rx),
            }),
        )?;
        let (tcp, _) = listener.accept()?;
        tcp.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut ws = tungstenite::accept(tcp)?;
        live.push_pcm(&[0.25; CHUNK]);
        assert_eq!(ws.read()?.into_data().len(), CHUNK * 2);
        live.push_pcm(&[0.5; CHUNK]);
        entered_rx.recv_timeout(Duration::from_secs(5))?;
        ws.send(Message::Text(
            final_message(0.0, "partial")
                .into_text()?
                .replace("\"is_final\":true", "\"is_final\":false")
                .into(),
        ))?;
        ws.send(final_message(0.0, "already transcribed"))?;
        let started = std::time::Instant::now();
        let mut received = Vec::new();
        while started.elapsed() < Duration::from_millis(500) {
            if let Some(r) = live.try_recv() {
                received.push((r.result.text, r.result.is_final));
            }
            if received.len() == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let receive_delay = started.elapsed();
        // Pings must also be serviced without sending the untraced second audio chunk.
        ws.send(Message::Ping(vec![1].into()))?;
        let pong = ws.read();
        // Release even on failure so the regression cannot leave a blocked trace worker.
        release_tx.send(())?;
        drop(live);
        assert_eq!(pong?, Message::Pong(vec![1].into()));
        eprintln!("incoming captions during blocked trace: {receive_delay:?}");
        assert_eq!(
            received,
            [
                ("partial".into(), false),
                ("already transcribed".into(), true)
            ]
        );
        Ok(())
    }

    #[test]
    fn trace_work_overlaps_audio_pacing() -> TestResult {
        struct SlowTrace(RecordingSink);
        impl TraceabilitySink for SlowTrace {
            fn record(&self, record: crate::llm::traceability::TraceRecord) {
                std::thread::sleep(Duration::from_millis(100));
                self.0.record(record);
            }
        }
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("ws://{}", listener.local_addr()?);
        let trace = Arc::new(SlowTrace(RecordingSink::new()));
        let live = MeetingLive::start_url(url, "Token test".into(), trace.clone())?;
        let (tcp, _) = listener.accept()?;
        tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut ws = tungstenite::accept(tcp)?;
        let started = std::time::Instant::now();
        live.push_pcm(&vec![0.25; CHUNK * 20]);
        let mut previous = started;
        let mut gaps = Vec::new();
        for i in 0..20 {
            assert_eq!(ws.read()?.into_data().len(), CHUNK * 2);
            assert!(
                trace.0.records().len() > i,
                "audio arrived before its trace"
            );
            let now = std::time::Instant::now();
            gaps.push(now.duration_since(previous));
            previous = now;
        }
        let elapsed = started.elapsed();
        drop(live);
        assert_eq!(trace.0.records().len(), 20);
        assert!(trace.0.records().iter().all(|r| r.chunk_bytes == CHUNK * 2));
        gaps.sort();
        eprintln!(
            "20 audio chunks with 100ms trace writes: total={elapsed:?}, gap p50={:?}, p95={:?}",
            gaps[9], gaps[18]
        );
        // Serial trace-then-send measured ~3.65s; overlapped measured ~2.1s. The bound
        // catches serial pacing, with ~0.25s margin for scheduler variation.
        assert!(
            elapsed < Duration::from_millis(3400),
            "pacing compounded trace latency: {elapsed:?}"
        );
        Ok(())
    }

    #[test]
    fn stop_flushes_ten_second_backlog_before_releasing_audio() -> TestResult {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("ws://{}", listener.local_addr()?);
        let server = std::thread::spawn(move || -> Result<(), String> {
            let run = || -> TestResult {
                let (tcp, _) = listener.accept()?;
                tcp.set_read_timeout(Some(Duration::from_secs(15)))?;
                let mut ws = tungstenite::accept(tcp)?;
                for i in 0..100 {
                    assert_eq!(ws.read()?.into_data().len(), CHUNK * 2);
                    ws.send(final_message(i as f64 / 10.0, "speech"))?;
                }
                assert!(ws.read()?.into_text()?.contains("CloseStream"));
                ws.send(Message::Text(
                    r#"{"type":"Metadata","duration":10.0}"#.into(),
                ))?;
                Ok(())
            };
            run().map_err(|e| e.to_string())
        });
        let live =
            MeetingLive::start_url(url, "Token test".into(), Arc::new(RecordingSink::new()))?;
        let weak = Arc::downgrade(&live.shared);
        live.push_pcm(&vec![0.25; SAMPLE_RATE as usize * 10]);
        let mut offsets = Vec::new();
        let status = live.finish(|r| offsets.push(r.offset_ms));
        assert_eq!(offsets, (0..100).map(|i| i * 100).collect::<Vec<_>>());
        assert_eq!(status.dropped_samples, 0);
        assert!(weak.upgrade().is_none());
        server.join().map_err(|_| "server panicked")??;
        Ok(())
    }

    #[test]
    fn closing_metadata_tolerates_rounding_but_not_missing_audio() {
        let sent = CHUNK as u64 + 1;
        assert!(metadata_confirms(0.1, sent));
        assert!(metadata_confirms(0.1001, sent));
        for duration in [0.098, 0.102, -0.1, f64::NAN, f64::INFINITY, 1e300] {
            assert!(!metadata_confirms(duration, sent));
        }
    }

    #[test]
    fn provider_ranges_cannot_acknowledge_unsent_or_invalid_audio() {
        let body = serde_json::json!({"start": 0.02, "duration": 0.02});
        assert_eq!(result_range(&body, 160, 800), Some((480, 800)));
        assert_eq!(result_range(&body, 160, 799), None);
        assert_eq!(
            result_range(&serde_json::json!({"start": -1, "duration": 1}), 0, 16000),
            None
        );
        assert_eq!(
            result_range(
                &serde_json::json!({"start": 0, "duration": 1e300}),
                10,
                16000
            ),
            None
        );
    }

    pub(super) fn wait_until(mut done: impl FnMut() -> bool) -> TestResult {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done() {
            if std::time::Instant::now() >= deadline {
                return Err("condition timed out".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    pub(super) fn final_message(start: f64, text: &str) -> Message {
        Message::Text(
            serde_json::json!({
                "type":"Results", "start": start, "duration":0.1,
                "is_final":true, "speech_final":true,
                "channel":{"alternatives":[{"transcript":text,"confidence":0.99}]}
            })
            .to_string()
            .into(),
        )
    }

    #[test]
    fn reconnect_replays_only_unconfirmed_speech_or_silence_and_preserves_finals() -> TestResult {
        for silence in [false, true] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let url = format!("ws://{}", listener.local_addr()?);
            let server = std::thread::spawn(move || -> Result<(), String> {
                let run = || -> Result<(), Box<dyn std::error::Error>> {
                    let (tcp, _) = listener.accept()?;
                    tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
                    let mut ws = tungstenite::accept(tcp)?;
                    assert_eq!(ws.read()?.into_data().len(), CHUNK * 2);
                    ws.send(final_message(0.0, if silence { "" } else { "first" }))?;
                    let unconfirmed = ws.read()?.into_data();
                    drop(ws); // Lose the socket after send but before the second acknowledgement.
                    let (tcp, _) = listener.accept()?;
                    tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
                    let mut ws = tungstenite::accept(tcp)?;
                    assert_eq!(ws.read()?.into_data(), unconfirmed);
                    let final_result = final_message(0.0, if silence { "" } else { "second" });
                    ws.send(final_result.clone())?;
                    ws.send(final_result)?; // Duplicate final must not append twice.
                    assert!(ws.read()?.into_text()?.contains("CloseStream"));
                    ws.send(Message::Text(
                        r#"{"type":"Metadata","duration":0.1}"#.into(),
                    ))?;
                    ws.close(None)?;
                    Ok(())
                };
                run().map_err(|e| e.to_string())
            });
            let mut live =
                MeetingLive::start_url(url, "Token test".into(), Arc::new(RecordingSink::new()))?;
            let weak = Arc::downgrade(&live.shared);
            live.push_pcm(&vec![if silence { 0.0 } else { 0.25 }; CHUNK]);
            live.push_pcm(&vec![if silence { 0.0 } else { 0.75 }; CHUNK]);
            let mut lines = Vec::new();
            wait_until(|| {
                while let Some(r) = live.try_recv() {
                    lines.push((r.result.text, r.offset_ms));
                }
                live.shared.buffer().confirmed == (CHUNK * 2) as u64
            })?;
            let status = live.finish(|r| lines.push((r.result.text, r.offset_ms)));
            assert!(status.stopped);
            assert_eq!(status.dropped_samples, 0);
            assert!(
                weak.upgrade().is_none(),
                "worker and handle release all shared RAM"
            );
            if silence {
                assert!(lines.is_empty());
            } else {
                assert_eq!(lines, vec![("first".into(), 0), ("second".into(), 100)]);
            }
            server.join().map_err(|_| "server panicked")??;
        }
        Ok(())
    }

    /// Accept one WebSocket handshake and return the request target it asked for.
    // The callback's error type is tungstenite's own HTTP error response.
    #[allow(clippy::result_large_err)]
    fn accept_target(
        listener: &std::net::TcpListener,
    ) -> Result<(String, tungstenite::WebSocket<std::net::TcpStream>), Box<dyn std::error::Error>> {
        let (tcp, _) = listener.accept()?;
        tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
        let target = Arc::new(Mutex::new(String::new()));
        let seen = Arc::clone(&target);
        let ws = tungstenite::accept_hdr(
            tcp,
            move |request: &tungstenite::handshake::server::Request,
                  response: tungstenite::handshake::server::Response| {
                if let Ok(mut slot) = seen.lock() {
                    *slot = request.uri().to_string();
                }
                Ok(response)
            },
        )?;
        let target = target.lock().map_err(|_| "target lock poisoned")?.clone();
        Ok((target, ws))
    }

    fn keyterm_rows(sink: &RecordingSink) -> usize {
        sink.records()
            .iter()
            .filter(|r| r.purpose == "meeting_asr_keyterms")
            .count()
    }

    /// Issue #782: every handshake that carries vocabulary hints is on the ledger first, and a
    /// reconnect after the user switched sharing off goes out without their personal terms.
    #[test]
    fn revoked_vocabulary_consent_drops_personal_terms_on_reconnect_and_every_handshake_is_traced(
    ) -> TestResult {
        use std::sync::atomic::{AtomicBool, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let base = format!("ws://{}/", listener.local_addr()?);
        let allowed = Arc::new(AtomicBool::new(true));
        let consent_flag = Arc::clone(&allowed);
        let sink = Arc::new(RecordingSink::new());
        let live = MeetingLive::start_endpoint(
            Endpoint {
                hosted: None,
            personal_url: format!("{base}?keyterm=PersonalTerm&keyterm=Builtin"),
                personal_has_keyterms: true,
                plain_url: format!("{base}?keyterm=Builtin"),
                plain_has_keyterms: true,
                consent: Some(Arc::new(move || consent_flag.load(Ordering::SeqCst))),
            },
            "Token test".into(),
            sink.clone(),
        )?;

        let (first, ws) = accept_target(&listener)?;
        assert!(first.contains("PersonalTerm"), "consent on: the personal term rides along");
        assert_eq!(keyterm_rows(&sink), 1, "the first handshake's hints were recorded");

        // The user switches sharing off mid-meeting; then the network blips.
        allowed.store(false, Ordering::SeqCst);
        drop(ws);
        let (second, _ws) = accept_target(&listener)?;
        assert!(!second.contains("PersonalTerm"), "revoked: no personal term on reconnect");
        assert!(second.contains("Builtin"), "built-in hints still go");
        assert_eq!(keyterm_rows(&sink), 2, "one row per handshake that carried hints");
        assert!(
            sink.records().iter().all(|r| !format!("{r:?}").contains("PersonalTerm")),
            "rows never carry the hint text"
        );
        drop(live);
        Ok(())
    }

    #[test]
    fn an_unreachable_provider_adds_no_keyterm_rows_while_retrying() -> TestResult {
        // Nothing listens on this port: every attempt fails at TCP connect, before the URL could
        // leave, so the retry loop must not fill the ledger.
        let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        let sink = Arc::new(RecordingSink::new());
        let live = MeetingLive::start_endpoint(
            Endpoint::fixed(format!("ws://127.0.0.1:{port}/?keyterm=Builtin"), true),
            "Token test".into(),
            sink.clone(),
        )?;
        wait_until(|| live.status().reconnecting)?;
        std::thread::sleep(Duration::from_millis(600));
        assert_eq!(keyterm_rows(&sink), 0);
        drop(live);
        Ok(())
    }

    #[test]
    fn endpoint_choice_follows_consent() {
        let endpoint = Endpoint {
            hosted: None,
            personal_url: "wss://x/?keyterm=Mine".into(),
            personal_has_keyterms: true,
            plain_url: "wss://x/".into(),
            plain_has_keyterms: false,
            consent: None,
        };
        assert_eq!(endpoint.choose(true), ("wss://x/?keyterm=Mine", true));
        assert_eq!(endpoint.choose(false), ("wss://x/", false));
        let fixed = Endpoint::fixed("wss://y/".into(), false);
        assert_eq!(fixed.choose(false), fixed.choose(true));
    }

    #[test]
    fn the_consent_endpoint_has_a_plain_url_without_personal_terms() {
        // What `start_with_vocabulary_consent` connects with: the two URLs differ only by the
        // personal hints, and the plain one still carries the built-ins.
        let config = DeepgramConfig::default()
            .with_diarize(true)
            .with_keyterms(["PersonalTerm".to_string(), "Builtin".to_string()]);
        let endpoint = Endpoint::with_consent(&config, vec!["Builtin".into()], Arc::new(|| false));
        let (personal, personal_hints) = endpoint.choose(true);
        let (plain, plain_hints) = endpoint.choose(false);
        assert!(personal.contains("PersonalTerm") && personal_hints);
        assert!(!plain.contains("PersonalTerm") && plain.contains("Builtin") && plain_hints);
        assert!(plain.contains("diarize=true") && plain.contains("mip_opt_out=true"));
        let none = Endpoint::with_consent(&config, Vec::new(), Arc::new(|| false));
        assert!(!none.choose(false).1, "no hints left means no keyterm row either");
    }

    #[test]
    fn drop_cancels_stalled_handshake_and_releases_audio() -> TestResult {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("ws://{}", listener.local_addr()?);
        let live =
            MeetingLive::start_url(url, "Token test".into(), Arc::new(RecordingSink::new()))?;
        let weak = Arc::downgrade(&live.shared);
        live.push_pcm(&vec![0.1; MAX_SAMPLES]);
        let (mut tcp, _) = listener.accept()?; // Accept TCP but never answer the WS handshake.
        assert!(
            !live.status().reconnecting,
            "initial handshake is not a reconnect"
        );
        let started = std::time::Instant::now();
        drop(live);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(weak.upgrade().is_none());
        tcp.set_read_timeout(Some(Duration::from_secs(1)))?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut tcp, &mut bytes)?;
        Ok(())
    }

    /// Issue #781: a refused key stops the stream with `auth_rejected` instead of retrying for
    /// the whole meeting behind a "connection interrupted" notice.
    #[test]
    fn a_refused_key_stops_retrying_and_says_so() -> TestResult {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("ws://{}", listener.local_addr()?);
        let live =
            MeetingLive::start_url(url, "Token wrong".into(), Arc::new(RecordingSink::new()))?;
        let (mut tcp, _) = listener.accept()?;
        tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut request = [0_u8; 1024];
        let _ = std::io::Read::read(&mut tcp, &mut request)?;
        std::io::Write::write_all(
            &mut tcp,
            b"HTTP/1.1 401 Unauthorized\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        )?;
        drop(tcp);
        wait_until(|| live.status().auth_rejected && live.status().stopped)?;
        assert!(!live.status().reconnecting, "a refused key is not a network interruption");
        listener.set_nonblocking(true)?;
        std::thread::sleep(Duration::from_millis(600));
        assert!(listener.accept().is_err(), "no retry after the key was refused");
        drop(live);
        Ok(())
    }

    /// Answer the next handshake on `listener` with a bare HTTP `status` line.
    fn refuse_handshake(listener: &std::net::TcpListener, status: &str) -> TestResult {
        let (mut tcp, _) = listener.accept()?;
        tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut request = [0_u8; 1024];
        let _ = std::io::Read::read(&mut tcp, &mut request)?;
        std::io::Write::write_all(
            &mut tcp,
            format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                .as_bytes(),
        )?;
        Ok(())
    }

    /// Issue #1005: a refusal that retrying cannot fix stops the stream with its own state,
    /// instead of "reconnecting" for the rest of the meeting.
    #[test]
    fn quota_and_other_permanent_refusals_stop_without_retrying() -> TestResult {
        for (status, quota) in [("402 Payment Required", true), ("400 Bad Request", false)] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let url = format!("ws://{}", listener.local_addr()?);
            let live =
                MeetingLive::start_url(url, "Token test".into(), Arc::new(RecordingSink::new()))?;
            refuse_handshake(&listener, status)?;
            wait_until(|| live.status().stopped)?;
            let observed = live.status();
            assert_eq!(observed.quota_exhausted, quota, "{status}");
            assert!(!observed.auth_rejected && !observed.reconnecting, "{status}");
            listener.set_nonblocking(true)?;
            std::thread::sleep(Duration::from_millis(600));
            assert!(listener.accept().is_err(), "no retry after {status}");
            drop(live);
        }
        Ok(())
    }

    #[test]
    fn rate_limits_and_provider_errors_are_retried() -> TestResult {
        for status in ["429 Too Many Requests", "503 Service Unavailable"] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
            let url = format!("ws://{}", listener.local_addr()?);
            let live = MeetingLive::start_endpoint_with_budget(
                Endpoint::fixed(url, false),
                "Token test".into(),
                Arc::new(RecordingSink::new()),
                Duration::from_millis(1),
            )?;
            refuse_handshake(&listener, status)?;
            wait_until(|| live.status().reconnecting)?;
            refuse_handshake(&listener, status)?; // A failure after the notice budget.
            wait_until(|| live.status().service_unavailable)?;
            let (tcp, _) = listener.accept()?; // Still retrying after the budget.
            let observed = live.status();
            assert!(!observed.stopped && !observed.quota_exhausted && !observed.network_paused, "{status}");
            drop(tcp);
            drop(live);
        }
        Ok(())
    }

    #[test]
    fn transient_failures_remain_recoverable_after_the_notice_budget() -> TestResult {
        let port = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        let live = MeetingLive::start_endpoint_with_budget(
            Endpoint::fixed(format!("ws://127.0.0.1:{port}"), false),
            "Token test".into(),
            Arc::new(RecordingSink::new()),
            Duration::from_millis(300),
        )?;
        wait_until(|| live.status().network_paused)?;
        let observed = live.status();
        assert!(observed.reconnecting && !observed.stopped);
        live.push_pcm(&vec![0.1; MAX_SAMPLES + CHUNK]);
        assert_eq!(live.shared.buffer().pcm.len(), MAX_SAMPLES);
        let listener = std::net::TcpListener::bind(("127.0.0.1", port))?;
        let (tcp, _) = listener.accept()?;
        tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut socket = tungstenite::accept(tcp)?;
        wait_until(|| live.status().connected)?;
        assert!(!live.status().network_paused);
        assert!(!live.status().stopped);
        // Receiving retained audio proves the original worker resumed its bounded replay.
        assert!(socket.read()?.is_binary());
        assert!(!observed.auth_rejected && !observed.quota_exhausted);
        drop(live);
        Ok(())
    }

    /// An idle lane keeps its socket with a control-only KeepAlive instead of being closed by
    /// the provider and reconnecting every few seconds (issue #1005).
    #[test]
    fn an_idle_stream_sends_keep_alive_and_no_audio() -> TestResult {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("ws://{}", listener.local_addr()?);
        let live =
            MeetingLive::start_url(url, "Token test".into(), Arc::new(RecordingSink::new()))?;
        let (tcp, _) = listener.accept()?;
        tcp.set_read_timeout(Some(KEEP_ALIVE_INTERVAL + Duration::from_secs(2)))?;
        let mut ws = tungstenite::accept(tcp)?;
        let message = ws.read()?;
        assert_eq!(message, Message::Text(r#"{"type":"KeepAlive"}"#.into()));
        listener.set_nonblocking(true)?;
        assert!(listener.accept().is_err(), "the idle socket was kept, not replaced");
        drop(live);
        Ok(())
    }

    #[test]
    fn reconnect_status_starts_only_after_failure_and_clears_on_recovery() -> TestResult {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("ws://{}", listener.local_addr()?);
        let live =
            MeetingLive::start_url(url, "Token test".into(), Arc::new(RecordingSink::new()))?;
        let (tcp, _) = listener.accept()?;
        assert!(!live.status().reconnecting);
        drop(tcp); // Reject the initial handshake.
        wait_until(|| live.status().reconnecting)?;
        let (tcp, _) = listener.accept()?;
        tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
        let _ws = tungstenite::accept(tcp)?;
        wait_until(|| !live.status().reconnecting)?;
        drop(live);
        Ok(())
    }

    #[test]
    fn finish_offline_has_deadline_and_reports_discarded_tail() -> TestResult {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("ws://{}", listener.local_addr()?);
        let live =
            MeetingLive::start_url(url, "Token test".into(), Arc::new(RecordingSink::new()))?;
        let weak = Arc::downgrade(&live.shared);
        live.push_pcm(&[0.1; CHUNK]);
        let started = std::time::Instant::now();
        let status = live.finish(|_| {});
        // The stalled handshake can be retried once after Stop, so it may use the whole budget.
        assert!(started.elapsed() < FINISH_TIMEOUT + Duration::from_secs(2));
        assert_eq!(status.dropped_samples, CHUNK as u64);
        assert!(weak.upgrade().is_none());
        Ok(())
    }
}

#[cfg(test)]
mod finish_tests {
    use super::*;
    use crate::llm::traceability::RecordingSink;

    #[test]
    fn close_stream_preserves_tail_final_before_releasing_silence(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        let url = format!("ws://{}", listener.local_addr()?);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
        let server =
            std::thread::spawn(move || -> Result<(), String> {
                let run =
                    || -> Result<(), Box<dyn std::error::Error>> {
                        let (tcp, _) = listener.accept()?;
                        tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
                        let mut ws = tungstenite::accept(tcp)?;
                        assert_eq!(ws.read()?.into_data().len(), CHUNK * 2);
                        ready_tx.send(())?;
                        assert_eq!(ws.read()?.into_data().len(), 2);
                        assert!(ws.read()?.into_text()?.contains("CloseStream"));
                        ws.send(Message::Text(serde_json::json!({
                    "type":"Results", "start":0.0, "duration":0.05,
                    "is_final":true, "speech_final":true,
                    "channel":{"alternatives":[{"transcript":"tail", "confidence":0.99}]}
                }).to_string().into()))?;
                        ws.send(Message::Text(
                            r#"{"type":"Metadata","duration":0.1}"#.into(),
                        ))?;
                        Ok(())
                    };
                run().map_err(|e| e.to_string())
            });
        let live =
            MeetingLive::start_url(url, "Token test".into(), Arc::new(RecordingSink::new()))?;
        let weak = Arc::downgrade(&live.shared);
        live.push_pcm(&[0.25; CHUNK + 1]);
        ready_rx.recv_timeout(Duration::from_secs(5))?;
        let mut finals = Vec::new();
        let status = live.finish(|r| finals.push(r.result.text));
        assert_eq!(finals, ["tail"]);
        assert_eq!(status.dropped_samples, 0);
        assert!(weak.upgrade().is_none());
        server.join().map_err(|_| "server panicked")??;
        Ok(())
    }
}

#[cfg(test)]
mod reconnect_finish_tests {
    use super::tests::{final_message, wait_until};
    use super::*;
    use crate::llm::traceability::RecordingSink;
    use std::net::{TcpListener, TcpStream};
    use std::sync::mpsc as sync_mpsc;
    use std::time::Instant as WallInstant;

    type TestResult = Result<(), Box<dyn std::error::Error>>;
    type Server = JoinHandle<Result<(), String>>;
    /// Three refused retries grow the backoff to 2s (250ms, 500ms, 1s, then 2s): a window wide
    /// enough that the test's finish request lands inside the wait, not after it.
    const REFUSALS: usize = 3;

    fn accept_before(listener: &TcpListener) -> Result<TcpStream, Box<dyn std::error::Error>> {
        let deadline = WallInstant::now() + Duration::from_secs(3);
        loop {
            match listener.accept() {
                Ok((tcp, _)) => {
                    // Accepted sockets inherit nonblocking mode on macOS.
                    tcp.set_nonblocking(false)?;
                    tcp.set_read_timeout(Some(Duration::from_secs(6)))?;
                    tcp.set_write_timeout(Some(Duration::from_secs(3)))?;
                    return Ok(tcp);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if WallInstant::now() >= deadline {
                        return Err("no recovery connection".into());
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Confirms the first chunk, drops the unconfirmed tail, refuses `refusals` retries, then
    /// serves the recovery. `lost` fires once the worker is in the backoff; with `hold`,
    /// one more retry is kept mid-handshake until the test sends on it, then refused.
    fn reconnecting_stream(
        refusals: usize,
        hold: Option<sync_mpsc::Receiver<()>>,
    ) -> Result<(MeetingLive, Server, sync_mpsc::Receiver<()>), Box<dyn std::error::Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("ws://{}", listener.local_addr()?);
        let (lost_tx, lost_rx) = sync_mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let run = || -> TestResult {
                let mut ws = tungstenite::accept(accept_before(&listener)?)?;
                assert_eq!(ws.read()?.into_data().len(), CHUNK * 2);
                ws.send(final_message(0.0, "first words"))?;
                let unconfirmed = ws.read()?.into_data();
                assert_eq!(unconfirmed.len(), CHUNK * 2);
                drop(ws); // Uploaded tail has not been confirmed.
                for _ in 0..refusals {
                    drop(accept_before(&listener)?);
                }
                match &hold {
                    Some(hold) => {
                        let tcp = accept_before(&listener)?;
                        lost_tx.send(())?;
                        hold.recv_timeout(Duration::from_secs(3))?;
                        drop(tcp);
                    }
                    None => {
                        // Let the refused handshake fail so the worker is usually inside the 2s
                        // wait. A slow runner may still be mid-attempt, which takes the retry path.
                        std::thread::sleep(Duration::from_millis(200));
                        lost_tx.send(())?;
                    }
                }

                let mut ws = tungstenite::accept(accept_before(&listener)?)?;
                assert_eq!(
                    ws.read()?.into_data(),
                    unconfirmed,
                    "replay must exclude already confirmed audio"
                );
                assert!(ws.read()?.into_text()?.contains("CloseStream"));
                let final_result = final_message(0.0, "last words");
                ws.send(final_result.clone())?;
                ws.send(final_result)?; // Duplicate provider finals must remain suppressed.
                ws.send(Message::Text(
                    r#"{"type":"Metadata","duration":0.1}"#.into(),
                ))?;
                Ok(())
            };
            run().map_err(|error| error.to_string())
        });
        let live =
            MeetingLive::start_url(url, "Token test".into(), Arc::new(RecordingSink::new()))?;
        let mut samples = vec![0.125; CHUNK];
        samples.extend(vec![0.25; CHUNK]);
        live.push_pcm(&samples);
        Ok((live, server, lost_rx))
    }

    fn tail_lost(live: &MeetingLive, lost: &sync_mpsc::Receiver<()>) -> TestResult {
        lost.recv_timeout(Duration::from_secs(5))?;
        wait_until(|| {
            let buffer = live.shared.buffer();
            buffer.status.reconnecting && buffer.confirmed == CHUNK as u64
        })
    }

    #[test]
    fn repeated_finish_during_backoff_recovers_unconfirmed_tail() -> TestResult {
        let (live, server, lost) = reconnecting_stream(REFUSALS, None)?;
        tail_lost(&live, &lost)?;
        let weak = Arc::downgrade(&live.shared);
        let started = WallInstant::now();
        live.begin_finish();
        live.begin_finish();
        live.begin_finish();
        let mut finals = Vec::new();
        let status = live.finish(|event| finals.push((event.result.text, event.offset_ms)));
        server.join().map_err(|_| "server panicked")??;
        assert_eq!(
            finals,
            [("first words".into(), 0), ("last words".into(), 100)]
        );
        assert_eq!(status.dropped_samples, 0);
        assert!(status.stopped);
        assert!(weak.upgrade().is_none());
        // Stop cuts the 2s backoff short instead of spending the flush budget waiting.
        assert!(started.elapsed() < Duration::from_secs(1));
        Ok(())
    }

    #[test]
    fn single_finish_during_backoff_recovers_unconfirmed_tail() -> TestResult {
        let (live, server, lost) = reconnecting_stream(REFUSALS, None)?;
        tail_lost(&live, &lost)?;
        let mut finals = Vec::new();
        let status = live.finish(|event| finals.push(event.result.text));
        server.join().map_err(|_| "server panicked")??;
        assert_eq!(finals, ["first words", "last words"]);
        assert_eq!(status.dropped_samples, 0);
        assert_eq!(status.lost_tail_samples, 0, "a recovered tail is not lost");
        Ok(())
    }

    #[test]
    fn finish_while_connecting_still_recovers_unconfirmed_tail() -> TestResult {
        let (hold_tx, hold_rx) = sync_mpsc::sync_channel(1);
        let (live, server, lost) = reconnecting_stream(0, Some(hold_rx))?;
        tail_lost(&live, &lost)?;
        // The retry in flight fails after Stop; one final attempt must still replay the tail.
        live.begin_finish();
        hold_tx.send(())?;
        let mut finals = Vec::new();
        let status = live.finish(|event| finals.push(event.result.text));
        server.join().map_err(|_| "server panicked")??;
        assert_eq!(finals, ["first words", "last words"]);
        assert_eq!(status.dropped_samples, 0);
        Ok(())
    }

    #[test]
    fn desktop_parallel_finish_drains_both_reconnecting_streams() -> TestResult {
        let (mut me, me_server, me_lost) = reconnecting_stream(REFUSALS, None)?;
        let (mut other, other_server, other_lost) = reconnecting_stream(REFUSALS, None)?;
        tail_lost(&me, &me_lost)?;
        tail_lost(&other, &other_lost)?;
        let weak_me = Arc::downgrade(&me.shared);
        let weak_other = Arc::downgrade(&other.shared);
        // Match desktop Stop/pause: request both, drain both, then join both.
        me.begin_finish();
        other.begin_finish();
        let mut me_finals = Vec::new();
        let mut other_finals = Vec::new();
        wait_until(|| {
            while let Some(event) = me.try_recv() {
                me_finals.push(event.result.text);
            }
            while let Some(event) = other.try_recv() {
                other_finals.push(event.result.text);
            }
            me.status().stopped && other.status().stopped
        })?;
        let me_status = me.finish(|event| me_finals.push(event.result.text));
        let other_status = other.finish(|event| other_finals.push(event.result.text));
        me_server.join().map_err(|_| "server panicked")??;
        other_server.join().map_err(|_| "server panicked")??;
        assert_eq!(me_finals, ["first words", "last words"]);
        assert_eq!(other_finals, ["first words", "last words"]);
        assert_eq!(
            (me_status.dropped_samples, other_status.dropped_samples),
            (0, 0)
        );
        assert!(weak_me.upgrade().is_none() && weak_other.upgrade().is_none());
        Ok(())
    }

    #[test]
    fn finish_during_backoff_with_nothing_unconfirmed_makes_no_final_attempt() -> TestResult {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("ws://{}", listener.local_addr()?);
        let live =
            MeetingLive::start_url(url, "Token test".into(), Arc::new(RecordingSink::new()))?;
        let mut ws = tungstenite::accept(accept_before(&listener)?)?;
        live.push_pcm(&[0.25; CHUNK]);
        assert_eq!(ws.read()?.into_data().len(), CHUNK * 2);
        ws.send(final_message(0.0, "all words"))?;
        wait_until(|| live.shared.buffer().confirmed == CHUNK as u64)?;
        drop(ws);
        for _ in 0..REFUSALS {
            drop(accept_before(&listener)?);
        }
        std::thread::sleep(Duration::from_millis(200)); // Usually into the 2s wait, as above.
        let started = WallInstant::now();
        let status = live.finish(|_| {});
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(status.dropped_samples, 0);
        // The worker is joined, so any final connection would already sit in the backlog.
        assert!(
            matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "nothing to flush, so no final connection"
        );
        Ok(())
    }

    #[test]
    fn failed_final_attempt_after_stop_gives_up_without_retrying() -> TestResult {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("ws://{}", listener.local_addr()?);
        let live =
            MeetingLive::start_url(url, "Token test".into(), Arc::new(RecordingSink::new()))?;
        let mut ws = tungstenite::accept(accept_before(&listener)?)?;
        live.push_pcm(&[0.25; CHUNK]);
        assert_eq!(ws.read()?.into_data().len(), CHUNK * 2);
        drop(ws); // Uploaded tail has not been confirmed.

        // Stop lands while a retry is mid-handshake; that retry fails, then so does the one
        // final attempt. The worker must give up instead of retrying until the budget runs out.
        let in_flight = accept_before(&listener)?;
        live.begin_finish();
        drop(in_flight);
        drop(accept_before(&listener)?);
        let started = WallInstant::now();
        let status = live.finish(|_| {});
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(status.dropped_samples, CHUNK as u64);
        assert_eq!(status.lost_tail_samples, CHUNK as u64, "the unconfirmed tail is reported");
        assert!(
            matches!(listener.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
            "exactly one final attempt after Stop"
        );
        Ok(())
    }

    #[test]
    fn repeated_finish_offline_reports_tail_and_releases_audio() -> TestResult {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let url = format!("ws://{}", listener.local_addr()?);
        drop(listener);
        let live =
            MeetingLive::start_url(url, "Token test".into(), Arc::new(RecordingSink::new()))?;
        let weak = Arc::downgrade(&live.shared);
        live.push_pcm(&[0.25; CHUNK]);
        wait_until(|| live.status().reconnecting)?;
        live.begin_finish();
        live.begin_finish();
        let started = WallInstant::now();
        let status = live.finish(|_| {});
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(status.dropped_samples, CHUNK as u64);
        assert!(status.stopped && weak.upgrade().is_none());
        Ok(())
    }

    #[test]
    fn repeated_finish_with_stalled_final_results_keeps_deadline() -> TestResult {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("ws://{}", listener.local_addr()?);
        let (ready_tx, ready_rx) = sync_mpsc::sync_channel(1);
        let server = std::thread::spawn(move || {
            let run = || -> TestResult {
                let mut ws = tungstenite::accept(accept_before(&listener)?)?;
                assert_eq!(ws.read()?.into_data().len(), CHUNK * 2);
                ready_tx.send(())?;
                assert!(ws.read()?.into_text()?.contains("CloseStream"));
                // No final result or metadata: the worker must cancel this stalled socket.
                assert!(ws.read().is_err());
                Ok(())
            };
            run().map_err(|error| error.to_string())
        });
        let live =
            MeetingLive::start_url(url, "Token test".into(), Arc::new(RecordingSink::new()))?;
        let weak = Arc::downgrade(&live.shared);
        live.push_pcm(&[0.25; CHUNK]);
        ready_rx.recv_timeout(Duration::from_secs(3))?;
        let budget = live.shared.buffer().finish_timeout();
        let started = WallInstant::now();
        live.begin_finish();
        // Repeat after the deadline has started, as well as immediately inside finish().
        std::thread::sleep(Duration::from_millis(100));
        live.begin_finish();
        let status = live.finish(|_| {});
        let elapsed = started.elapsed();
        server.join().map_err(|_| "server panicked")??;
        assert!(elapsed >= budget && elapsed < budget + Duration::from_secs(1));
        assert_eq!(status.dropped_samples, CHUNK as u64);
        assert!(status.stopped && weak.upgrade().is_none());
        Ok(())
    }

    #[test]
    fn drop_during_backoff_cancels_without_waiting_for_recovery() -> TestResult {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("ws://{}", listener.local_addr()?);
        let live =
            MeetingLive::start_url(url, "Token test".into(), Arc::new(RecordingSink::new()))?;
        let weak = Arc::downgrade(&live.shared);
        let mut ws = tungstenite::accept(accept_before(&listener)?)?;
        live.push_pcm(&[0.25; CHUNK]);
        assert_eq!(ws.read()?.into_data().len(), CHUNK * 2);
        drop(ws);
        wait_until(|| live.status().reconnecting)?;
        let started = WallInstant::now();
        drop(live);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(weak.upgrade().is_none());
        Ok(())
    }
}
