//! HTTP 直播源：不可 seek 的顺序长读（HTTP-FLV）。
//!
//! 从 `legacy.rs`（本地 `source.rs` 实现体）整段搬出，并自带它依赖的那一小撮 HTTP
//! 脚手架（[`HttpFetch`] / `send_request` / [`http_trace_log`] / [`json_escape`]），
//! 因此**本文件不依赖 `legacy`**：`legacy` 被删除后本文件仍自持。
//!
//! 依赖方向固定为单向的 `legacy` → `live`（点播路径暂时复用这里的脚手架），绝不反向。
//!
//! 与点播用的 `HttpRangeSource`（可 Range seek）的关键差别见
//! [`HttpLiveStreamSource`]。

use std::env;
use std::io::Read;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::sync::atomic::Ordering;
use std::time::Duration;

use super::diag::*;
use super::http::{self, BodyReader, SourceCancellation};
use super::{ByteRange, MediaSource, Result, SourceError};

use crate::trace;

// ---------------------------------------------------------------------------
// HTTP 脚手架（直播路径自持；点播路径暂时复用这里的同名项）。
// ---------------------------------------------------------------------------

/// 一次 HTTP 交换要用的客户端与逐请求 deadline。
///
/// 拆成两件是因为生效范围不同：**连接超时是 client 级的**（本进程只有一个 data
/// client，4s），而**整次请求（发出 → body 读完）的总时长是逐请求的**——顺序长读
/// 的 body 是"到文件尾"的无限长响应，必须不限时；有界的段请求则必须限时，否则
/// 挂死的分段会永久占着在飞配额。
#[derive(Clone, Copy)]
pub(super) struct HttpFetch {
    pub(super) client: &'static reqwest::Client,
    /// 整次请求（发出 → body 读完）的总时长上限；`None` = 不限。
    pub(super) timeout: Option<Duration>,
}

impl HttpFetch {
    pub(super) fn get(&self, uri: &str) -> reqwest::RequestBuilder {
        self.with_timeout(self.client.get(uri))
    }

    /// HEAD 探测专用（`len()` 走这条路，拿不到 Content-Length 时才回退到
    /// 一字节 Range 请求，见 `probe_http_total_length`）。
    pub(super) fn head(&self, uri: &str) -> reqwest::RequestBuilder {
        self.with_timeout(self.client.head(uri))
    }

    pub(super) fn with_timeout(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.timeout {
            Some(timeout) => request.timeout(timeout),
            None => request,
        }
    }
}

/// 一次 HTTP 交换失败的分类依据。
///
/// 存在的理由是一次**真实的行为差异**：reqwest 不把 4xx/5xx 当错误（要显式
/// `error_for_status()` 才转），而旧实现用的 ureq 默认就把它变成 `Err(StatusCode)`。
/// 若照搬 reqwest 的默认行为，5xx 会从"可重试的暂时性失败"退化成终局错误——
/// CDN 抖动一个 502 就会让宿主换源，而不是就地重试。所以分类口径必须显式搬过来，
/// 由调用点在 `status >= 400` 时构造 [`Self::Status`]、在 `send()` 失败时构造
/// [`Self::Transport`]。
#[derive(Debug)]
pub(super) enum HttpExchangeFailure {
    /// 服务端明确回了失败状态码。
    Status(u16),
    /// 传输层失败（连接复位 / 超时 / DNS-TLS 抖动 / body 中断），没有状态码。
    Transport(String),
}

impl HttpExchangeFailure {
    /// 传输层错误与 5xx 是链路问题，源很可能马上又能用；4xx 是确定性的。
    pub(super) fn is_retryable(&self) -> bool {
        match self {
            Self::Status(status) => *status >= 500,
            Self::Transport(_) => true,
        }
    }

    pub(super) fn message(&self) -> String {
        match self {
            Self::Status(status) => format!("http status: {status}"),
            Self::Transport(message) => message.clone(),
        }
    }
}

/// 同步发出一次请求，并把失败归类成 [`HttpExchangeFailure`]。
///
/// 同步桥接见 [`http::block_on`]：读路径刻意是同步的（跑在 demux 线程上、自己
/// 持有同步字节窗口），所以异步调用在这里被驱动到完成，而不是把调用方变异步。
///
/// **注意这里只覆盖发送阶段**：reqwest 不把 4xx/5xx 当错误，状态码由调用点
/// 自己的 status 分支处理（并在 `>= 400` 时构造 [`HttpExchangeFailure::Status`]），
/// 以保持与旧实现一致的重试口径——5xx 可重试、4xx 终局。
pub(super) fn send_http_request(
    request: reqwest::RequestBuilder,
) -> std::result::Result<reqwest::Response, HttpExchangeFailure> {
    match http::block_on(async move {
        request
            .send()
            .await
            .map_err(|error| SourceError::Transient(error.to_string()))
    }) {
        Ok(response) => Ok(response),
        Err(SourceError::Transient(message)) => Err(HttpExchangeFailure::Transport(message)),
        Err(other) => Err(HttpExchangeFailure::Transport(other.to_string())),
    }
}

/// ERIKA_HTTP_TRACE 门控缓存（进程内 env 只在启动阶段写入，值不变）。
/// read_range 每个 cache hit 都会走 http_trace_log（FFmpeg AVIO 32KB 块读
/// → 视频流 ~100 次/秒）——此前每次都做 env::var 查询：Windows 上拿进程
/// 环境锁 + 线性查找，在 demux 热路径上是实打实的锁竞争开销。LazyLock
/// 首次求值后零开销（宿主在 main 早期 set_var，先于任何源创建，时序安全）。
static HTTP_TRACE_ENABLED: LazyLock<bool> = LazyLock::new(|| trace::env_flag("ERIKA_HTTP_TRACE"));

pub(super) fn http_trace_log(line: impl AsRef<str>) {
    if !*HTTP_TRACE_ENABLED {
        return;
    }
    let path = env::var_os("ERIKA_HTTP_TRACE_FILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp/erika_http_trace.jsonl"));
    trace::append_line(line.as_ref(), path);
}

pub(super) fn json_escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

// ---------------------------------------------------------------------------
// 直播源本体。
// ---------------------------------------------------------------------------

/// 直播流源：不可 seek 的 HTTP 流（HTTP-FLV over HTTPS）。
///
/// B 站直播 CDN 支持 HTTP-FLV，但**不支持 Range、无 Content-Length、
/// 连接长驻**——与 `HttpRangeSource`（可 Range seek 的 VOD）是完全不同的
/// 访问模型。本实现 GET 建立连接后从响应 body 顺序读取，demux 线程按需
/// 阻塞取字节，绝不把无限流一次性读进内存。
///
/// `len()` 恒返回 `None`（无总长度）、`seekable()` 恒返回 `false`：AVIO
/// 清掉 seekable 位后 flv demuxer 进入 live 模式（不 seek、按墙钟推进），
/// 播放层不会因"EOF"而结束（连接正常关闭才触发 EOF）。
pub struct HttpLiveStreamSource {
    uri: String,
    /// GET 响应 body 的读取器（连接长驻、数据持续推送）。`None` 表示连接已关闭。
    /// 由 [`BodyReader`] 提供：它把异步 body 泵成同步 `Read`，读取位置自持，
    /// 因此可以脱离 `Response` 独立持有。
    reader: Option<Box<dyn Read + Send + Sync>>,
    /// 已消费字节数。直播流不可 seek：`read_range` 校验请求起点 == 此值，
    /// 失配（AVIO seek 后）报错，防止把流当作可跳转文件读。
    offset: u64,
    /// 累计已下载字节（诊断快照差分实时速度）。
    downloaded_bytes: u64,
}

impl HttpLiveStreamSource {
    /// 单次读取上限。AVIO buffer 即 64KB，demux 粒度足够；截断大 `length`
    /// （如误用 `suffix_from(0)`）避免把无限流一次读进内存。
    const MAX_READ_BYTES: usize = 256 * 1024;

    pub fn open(uri: &str, http_headers: Vec<(String, String)>) -> Result<Self> {
        // 直播专用 agent（无 recv_response/recv_body 总时长超时，见 http_live_agent）
        let agent = http_live_agent();
        let mut request = agent.get(uri);
        for (name, value) in &http_headers {
            request = request.header(name, value);
        }
        let response = send_http_request(request).map_err(|failure| {
            http_trace_log(format!(
                "{{\"event\":\"http_live_open_error\",\"error\":\"{}\"}}",
                json_escape(&failure.message()),
            ));
            SourceError::Http(failure.message())
        })?;
        let status = response.status().as_u16();
        if status >= 400 {
            http_trace_log(format!(
                "{{\"event\":\"http_live_open_status\",\"status\":{}}}",
                status,
            ));
            return Err(SourceError::Http(format!(
                "live stream HTTP status {status}"
            )));
        }
        http_trace_log(format!(
            "{{\"event\":\"http_live_open\",\"status\":{},\"content_length\":{}}}",
            status,
            response
                .headers()
                .get("content-length")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
                .unwrap_or_else(|| "null".to_string()),
        ));
        let reader = BodyReader::new(SourceCancellation::default(), response);
        Ok(Self {
            uri: uri.to_string(),
            reader: Some(Box::new(reader)),
            offset: 0,
            downloaded_bytes: 0,
        })
    }

    fn publish_diag(&self) {
        // 直播流走长连接顺序读，**不发 Range 请求**（不经 `fetch_http_range`），
        // 故这里如实上报槽 0 的进程级请求画像累计值：直播期间它不再增长，
        // 宿主差分为 0，正是"当前这条流没有 Range 请求"的正确表述。
        let (http_requests, http_request_bytes) = match http_request_profile(0) {
            Some(profile) => (
                profile.requests.load(Ordering::Relaxed),
                profile.bytes.load(Ordering::Relaxed),
            ),
            None => (0, 0),
        };
        if let Ok(mut slots) = SOURCE_DIAG.lock() {
            if let Some(slot) = slots.get_mut(0) {
                *slot = SourceDiagSnapshot {
                    content_length: None,
                    cache_start: 0,
                    cache_end: self.offset,
                    prefetch_in_flight: 0,
                    downloaded_bytes: self.downloaded_bytes,
                    est_bps: 0.0,
                    active_host: String::new(),
                    active_idx: 0,
                    candidate_count: 0,
                    demux_cache_end: self.offset,
                    host_epoch: 0,
                    // 直播流**没有"整片缓冲完"这个状态**（无总长、持续追加）：
                    // 恒 false 是正确语义，宿主不会把直播的停滞读成"下完了"。
                    whole_file_cached: false,
                    // 直播源走 `HttpLiveStreamSource`（无 body 总时长 deadline、
                    // 读阻塞即等待），不经过点播的「暂时读不到」等待窗口。
                    transient_wait: false,
                    http_requests,
                    http_request_bytes,
                    // 直播流没有滑动窗口分层（不可 seek、无缓存区间可言）：
                    // 内存层报当前已读字节在前面的缓冲，冷区恒 0。
                    cache_memory_bytes: 0,
                    cache_disk_bytes: 0,
                    cold_hits: 0,
                    cold_hit_bytes: 0,
                    memory_budget_bytes: 0,
                    forward_budget_bytes: 0,
                    forward_playable_secs: 0.0,
                    fill_throughput_bps: 0.0,
                    stream_reconnects_total: 0,
                    stream_reconnects_early_close: 0,
                    stream_reconnects_empty: 0,
                    stream_reconnects_reset: 0,
                    stream_reconnects_host_switch: 0,
                    stream_chunk_resumes: 0,
                    stream_range_contiguous: 0,
                    stream_range_forward_jumps: 0,
                    stream_range_backward_jumps: 0,
                    stream_range_last_start: 0,
                    stream_last_connection_bytes: 0,
                    stream_max_connection_bytes: 0,
                    last_host_switch_reason: String::new(),
                    // 直播没有"seek 到未缓冲位置"这个概念（不可 seek、无窗口），
                    // 跨节点并行回填对它不适用：全 0 是正确语义，宿主不会把直播
                    // 读成"走过并行分段"。
                    seek_fill_batches: 0,
                    seek_fill_connections: 0,
                    seek_fill_bytes: 0,
                    seek_fill_gate_denied: 0,
                    seek_fill_fallbacks: 0,
                    seek_fill_overlap_ok: 0,
                    seek_fill_overlap_mismatch: 0,
                    seek_fill_last_segments: 0,
                    seek_fill_in_flight: 0,
                    seek_fill_last_bytes: 0,
                    seek_fill_last_ms: 0,
                    seek_fill_last_expected_ms: 0,
                    seek_fill_last_fallback: false,
                    seek_fill_last_gate_reason: String::new(),
                    seek_fill_disabled: false,
                };
            }
        }
    }
}

impl MediaSource for HttpLiveStreamSource {
    fn uri(&self) -> &str {
        &self.uri
    }

    fn len(&mut self) -> Result<Option<u64>> {
        Ok(None)
    }

    fn seekable(&self) -> bool {
        false
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        if range.start != self.offset {
            // AVIO seek 改动 offset 后的下一次读会走到这里：直播流不可跳转。
            http_trace_log(format!(
                "{{\"event\":\"http_live_seek_rejected\",\"requested\":{},\"stream_at\":{}}}",
                range.start, self.offset,
            ));
            return Err(SourceError::Io(format!(
                "live stream is not seekable (requested {} at stream offset {})",
                range.start, self.offset,
            )));
        }
        let Some(reader) = self.reader.as_mut() else {
            return Err(SourceError::Io(
                "live stream connection is closed".to_string(),
            ));
        };
        let length = usize::try_from(range.length.unwrap_or(Self::MAX_READ_BYTES as u64))
            .unwrap_or(Self::MAX_READ_BYTES)
            .min(Self::MAX_READ_BYTES);
        let mut buf = vec![0u8; length.max(1)];
        // 阻塞读：直播流持续推送，read 等到有数据才返回；0 表示服务器关闭
        // （断流 → AVIO EOF → 播放层按流结束处理）。
        let n = reader.read(&mut buf).map_err(|error| {
            let message = format!("live stream read error: {error}");
            http_trace_log(format!(
                "{{\"event\":\"http_live_read_error\",\"error\":\"{}\"}}",
                json_escape(&error.to_string()),
            ));
            SourceError::Http(message)
        })?;
        buf.truncate(n);
        if n == 0 {
            // 直播源没有"正常结束"概念：read 返回 0 即服务器关闭连接（断流）。
            // 记录流位置供宿主诊断断流点；空 buf → AVIO EOF → 播放层按流结束
            // 处理。这里不清空 reader：EOF 后继续 read 仍返回 Ok(0)，保持
            // ffmpeg 干净的 EOF 语义；清空会让下一次读报 Io 错误、破坏结束路径。
            http_trace_log(format!(
                "{{\"event\":\"http_live_eof\",\"stream_offset\":{},\"downloaded_bytes\":{}}}",
                self.offset, self.downloaded_bytes,
            ));
        }
        self.offset = self.offset.saturating_add(n as u64);
        self.downloaded_bytes = self.downloaded_bytes.saturating_add(n as u64);
        self.publish_diag();
        Ok(buf)
    }
}

impl Drop for HttpLiveStreamSource {
    fn drop(&mut self) {
        // 直播流用槽 0；销毁时清空自己的诊断槽。
        if let Ok(mut slots) = SOURCE_DIAG.lock() {
            if let Some(slot) = slots.get_mut(0) {
                *slot = SourceDiagSnapshot::default();
            }
        }
    }
}

/// 直播流专用。**不能复用点播侧的 `http_agent`**：直播流是无限长 body，任何"整次请求"
/// 的总时长上限都会让它播放约 N 秒后必断流。旧实现在 ureq 上踩过这个坑——
/// `timeout_recv_body` 是自响应头起算的总时长，直播因此约 15s 断流，表现为
/// "播着播着就自动重连"，与网络无关，根因就记在这里以免重犯。
///
/// 直播源只保留连接超时（慢源快速失败）；CDN 主动断开（FIN/RST）时读返回
/// EOF/错误 → 宿主断流检测触发自动重连（新 URL 换源）。
fn http_live_agent() -> HttpFetch {
    HttpFetch {
        client: http::data_client(),
        timeout: None,
    }
}
