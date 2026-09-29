//! Cancellation boundary for persistent HTTP read-ahead streams.
//!
//! Foreground and persistent background reads use reqwest on one shared Tokio
//! runtime. Cancellation drops the request future and interrupts connect, TLS,
//! response headers, and response body waits. System DNS resolution already
//! running in a blocking resolver may finish separately.

use std::future::Future;
use std::io::{self, Read};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{Result, SourceError};

#[derive(Clone, Debug)]
pub struct SourceCancellation(Arc<Mutex<CancellationState>>);

#[derive(Debug)]
#[allow(dead_code)]
struct CancellationState {
    token: CancellationToken,
    closed: bool,
    /// 见 [`SourceCancellation::interrupt`]。
    generation: u64,
}

impl Default for SourceCancellation {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(CancellationState {
            token: CancellationToken::new(),
            closed: false,
            generation: 0,
        })))
    }
}

impl SourceCancellation {
    pub fn cancel(&self) {
        let mut state = self.0.lock().expect("source cancellation mutex poisoned");
        state.closed = true;
        state.token.cancel();
    }

    /// 中断在途操作并记录代数，`resume` 据此判断"要恢复的是不是最新那一次"。
    ///
    /// 上游用它实现"seek 时中断在途读、seek 完成后恢复同一取消域"。本地 seek 走的是
    /// 窗口 reset + 重开连接（`StreamBuffer` 换代），不需要"中断后再恢复"的语义，因此
    /// 这两条暂时没有调用点；保留是为了让这份骨架与上游同形，将来若把 seek 改成
    /// "中断而非重连"可以直接用。
    #[allow(dead_code)]
    pub(crate) fn interrupt(&self) -> u64 {
        let mut state = self.0.lock().expect("source cancellation mutex poisoned");
        state.token.cancel();
        state.generation = state.generation.wrapping_add(1);
        state.generation
    }

    #[allow(dead_code)]
    pub(crate) fn resume(&self, generation: u64) {
        let mut state = self.0.lock().expect("source cancellation mutex poisoned");
        if !state.closed && state.generation == generation && state.token.is_cancelled() {
            state.token = CancellationToken::new();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.token().is_cancelled()
    }

    pub(super) fn child(&self) -> Self {
        Self(Arc::new(Mutex::new(CancellationState {
            token: self.token().child_token(),
            closed: false,
            generation: 0,
        })))
    }

    fn token(&self) -> CancellationToken {
        self.0
            .lock()
            .expect("source cancellation mutex poisoned")
            .token
            .clone()
    }
}

/// 上游把「client + cancellation」打包在这里，并提供一个**带取消**的同步桥接
/// [`HttpIo::run`]。本地等价能力由 `HttpFetch`（client + 逐请求 deadline）加上
/// `HttpRangeSource::cancellation` 字段承担，取消经 `MediaSource::cancellation`
/// 暴露给宿主拆除路径；这个类型因此暂时没有构造点。保留是为了与上游同形——将来若
/// 要把有界请求（段读、探测）也纳入同一条取消域，直接从这里接。
#[derive(Clone)]
#[allow(dead_code)]
pub(super) struct HttpIo {
    pub(super) client: reqwest::Client,
    pub(super) cancellation: SourceCancellation,
}

#[allow(dead_code)]
impl HttpIo {
    pub(super) fn new() -> Self {
        Self {
            client: shared_client().clone(),
            cancellation: SourceCancellation::default(),
        }
    }

    pub(super) fn child(&self) -> Self {
        Self {
            client: self.client.clone(),
            cancellation: self.cancellation.child(),
        }
    }

    pub(super) fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub(super) fn cancel(&self) {
        self.cancellation.cancel();
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    pub(super) fn run<T: Send + 'static>(
        &self,
        future: impl Future<Output = Result<T>> + Send + 'static,
    ) -> Result<T> {
        let cancellation = self.cancellation.token();
        let (sender, receiver) = crossbeam_channel::bounded(1);
        runtime().spawn(async move {
            // Keep the future in a narrower scope so its response, socket, and
            // partial buffers are dropped before completion is published to
            // the synchronous caller.
            let result = {
                tokio::pin!(future);
                tokio::select! {
                    biased;
                    // `SourceError` has no dedicated cancellation variant yet; a
                    // cancelled exchange is reported as a source read failure, which
                    // is how every caller already treats an interrupted read.
                    _ = cancellation.cancelled() => {
                        Err(SourceError::Http("source read cancelled".to_string()))
                    }
                    result = &mut future => result,
                }
            };
            let _ = sender.send(result);
        });
        receiver
            .recv()
            .unwrap_or_else(|_| Err(SourceError::Http("HTTP worker stopped".to_string())))
    }

    pub(super) fn wait_cancelled(&self, duration: Duration) -> bool {
        self.run(async move {
            tokio::time::sleep(duration).await;
            Ok(())
        })
        .is_err()
    }
}

/// 上游骨架里的通用 client（10s 连接超时、正常校验证书）。本地数据面请求一律走
/// [`data_client`]（4s 连接超时 + 忽略证书校验），因此这个没有调用点；保留是为了让
/// 这份骨架与上游同形。
#[allow(dead_code)]
fn shared_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .expect("build HTTP client")
    })
}

/// The client every range request and sequential stream goes through.
///
/// Two settings differ from [`shared_client`] on purpose, both carried over from
/// the `ureq` agents this replaced:
///
/// - `connect_timeout(4s)`: every local agent used a 4 s connect timeout so a slow
///   CDN fails fast. `open` runs synchronously on the host's UI thread, so a longer
///   connect timeout freezes the interface instead of letting bili-rs fall back.
/// - `danger_accept_invalid_certs(true)`: the host may dial by IP to bypass DNS
///   poisoning / unreachable IPv6, and certificate validation against a bare IP
///   fails. This is exactly what `TlsConfig::disable_verification(true)` did.
///
/// Per-request deadlines are deliberately **not** here: they differ per call site
/// (an unbounded read-ahead stream must stay open forever, a bounded seek piece
/// must not), so they belong on `RequestBuilder::timeout`.
pub(super) fn data_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(4))
            .danger_accept_invalid_certs(true)
            .build()
            .expect("build HTTP data client")
    })
}

fn runtime() -> &'static Runtime {
    static RUNTIME: OnceLock<Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("erika-http-io")
            .enable_all()
            .build()
            .expect("create HTTP I/O runtime")
    })
}

/// Run a future to completion from **synchronous** code.
///
/// This is the bridge the whole HTTP layer sits on. Erika's read path is
/// synchronous on purpose — it runs on the demuxer thread and owns a synchronous
/// byte window — so each async call is driven to completion here instead of
/// forcing the callers to become async.
///
/// Unlike [`HttpIo::run`] there is no cancellation token: callers on this path
/// (bounded range reads, probes) are already bounded by
/// [`HttpFetch::timeout`](super::HttpFetch), and the streaming reader has its own
/// [`BodyReader`] exit. Making cancellation reachable from host teardown is a
/// separate step; this keeps the port mechanical.
pub(super) fn block_on<T: Send + 'static>(
    future: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    let (sender, receiver) = crossbeam_channel::bounded(1);
    runtime().spawn(async move {
        let _ = sender.send(future.await);
    });
    receiver
        .recv()
        .unwrap_or_else(|_| Err(SourceError::Http("HTTP worker stopped".to_string())))
}

/// Pending chunks the body pump may hold ahead of the reader.
///
/// Small on purpose: the pump must not become a second cache. The reader's own
/// window (tens of MiB) is the buffer; this channel is only the hand-off, and a
/// short one keeps the pump honest about back-pressure.
const BODY_PUMP_CHUNKS: usize = 4;

enum BodyEvent {
    Chunk(Vec<u8>),
    End,
    /// The body died mid-transfer. Kept as a message (not a bare EOF) because the
    /// read path classifies a truncated body as a transient source error.
    Failed(String),
}

/// A response body exposed as a **synchronous** [`Read`].
///
/// The read-ahead thread consumes the body through `Read` and owns the window,
/// the back-pressure and the cache policy. This type only bridges that synchronous
/// consumer to reqwest's async body, so none of that policy had to change.
///
/// Back-pressure is structural: the pump awaits `mpsc::send` on a bounded channel,
/// so when the reader stops reading the pump stops polling the socket and the
/// kernel's receive window closes — the same "read until the window is full, then
/// stop reading" contract the ureq implementation had.
///
/// Dropping the reader (seek, reopen, teardown) closes the channel; the next
/// `send` fails and the pump returns, dropping the response together with its
/// socket and partial buffers. That is what makes teardown prompt without a
/// timeout, and what the cancellation token covers when a read is in flight but
/// the reader is still alive.
pub(super) struct BodyReader {
    receiver: mpsc::Receiver<BodyEvent>,
    current: Vec<u8>,
    offset: usize,
    finished: bool,
    error: Option<String>,
}

impl BodyReader {
    pub(super) fn new(cancellation: SourceCancellation, response: reqwest::Response) -> Self {
        let (sender, receiver) = mpsc::channel::<BodyEvent>(BODY_PUMP_CHUNKS);
        let token = cancellation.token();
        runtime().spawn(async move {
            tokio::pin!(response);
            loop {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => {
                        let _ = sender.send(BodyEvent::Failed("cancelled".to_string())).await;
                        break;
                    }
                    chunk = response.chunk() => match chunk {
                        Ok(Some(bytes)) => {
                            if sender.send(BodyEvent::Chunk(bytes.to_vec())).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => {
                            let _ = sender.send(BodyEvent::End).await;
                            break;
                        }
                        Err(error) => {
                            let _ = sender.send(BodyEvent::Failed(error.to_string())).await;
                            break;
                        }
                    },
                }
            }
        });
        Self {
            receiver,
            current: Vec::new(),
            offset: 0,
            finished: false,
            error: None,
        }
    }
}

impl Read for BodyReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.offset < self.current.len() {
                let available = self.current.len() - self.offset;
                let take = available.min(buf.len());
                buf[..take].copy_from_slice(&self.current[self.offset..self.offset + take]);
                self.offset += take;
                return Ok(take);
            }
            if self.finished {
                return match self.error.take() {
                    Some(message) => Err(io::Error::new(io::ErrorKind::Other, message)),
                    None => Ok(0),
                };
            }
            match self.receiver.blocking_recv() {
                Some(BodyEvent::Chunk(bytes)) => {
                    self.current = bytes;
                    self.offset = 0;
                }
                Some(BodyEvent::End) => self.finished = true,
                Some(BodyEvent::Failed(message)) => {
                    self.finished = true;
                    self.error = Some(message);
                }
                // The pump is gone (cancel, or the runtime is shutting down).
                None => self.finished = true,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_interrupt_cannot_resume_newer_or_terminal_cancellation() {
        let cancellation = SourceCancellation::default();
        let first = cancellation.interrupt();
        let latest = cancellation.interrupt();
        cancellation.resume(first);
        assert!(cancellation.is_cancelled());
        cancellation.resume(latest);
        assert!(!cancellation.is_cancelled());
        cancellation.cancel();
        cancellation.resume(latest);
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn cancellation_drops_operation_resources_before_returning() {
        struct Resource(std::sync::mpsc::Sender<()>);
        impl Drop for Resource {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }

        let io = HttpIo::new();
        let cancellation = io.cancellation.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (dropped_tx, dropped_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            io.run(async move {
                let _resource = Resource(dropped_tx);
                started_tx.send(()).unwrap();
                std::future::pending::<Result<()>>().await
            })
        });
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        cancellation.cancel();
        assert!(matches!(worker.join().unwrap(), Err(SourceError::Http(_))));
        assert!(dropped_rx.try_recv().is_ok());
    }
}
