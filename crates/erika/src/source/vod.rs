//! 本地增强装配层：上游 HTTP 内核 + 多 host 换源 + 诊断发布。
//!
//! 这是「本地能力向上游内核移植」的落点。**内核是上游的**
//! [`HttpRangeSource`]，本模块只做包装，不复制任何内核逻辑：
//!
//! ```text
//! DiagSource                       发布宿主诊断槽（缓存窗口/吞吐/在途预取）
//!   └─ MultiHostSource             多 CDN 候选换源（候选 >= 2 时才有）
//!        └─ HttpRangeSource        上游内核（+ 其自身的流式预取/实体校验）
//! ```
//!
//! 上游内核把缓存窗口、流式前沿、预取进度放在私有字段里，通过 [`MediaSource`]
//! 的 `observed` 只读观察点暴露出来；本层负责把它翻译成宿主轮询的 `SourceDiagSnapshot`。
//! 换源状态由 [`MultiHostSource`] 自己发布（它才知道当前 host），二者用部分更新
//! 合写同一槽位。

use std::time::Instant;

use super::diag;
use super::legacy::HttpLiveStreamSource;
use super::multi_host::{MultiHostSource, SourceFactory};
use super::{
    ByteRange, HttpRangeObserved, HttpRangeSource, MediaSource, MediaSourceHint, Result,
    SourceCancellation, SourceError,
};

/// 吞吐估计的采样间隔：低于它就不重算（宿主本来就 500ms 轮询一次）。
const THROUGHPUT_SAMPLE_SECS: f64 = 0.25;

/// EMA 权重：新瞬时值 0.3 / 历史 0.7，与旧内核的带宽估计取值保持一致。
const THROUGHPUT_EMA_NEW: f64 = 0.3;

/// 诊断发布包装：把上游内核的可观察状态写进宿主轮询的全局槽。
struct DiagSource {
    inner: Box<dyn MediaSource>,
    slot: usize,
    last_progress: u64,
    last_sample: Instant,
    est_bps: f64,
    downloaded_bytes: u64,
    transient_wait: bool,
}
impl DiagSource {
    fn new(inner: Box<dyn MediaSource>, slot: usize) -> Self {
        Self {
            inner,
            slot,
            last_progress: 0,
            last_sample: Instant::now(),
            est_bps: 0.0,
            downloaded_bytes: 0,
            transient_wait: false,
        }
    }

    /// 发布缓存窗口、吞吐与在途预取深度（宿主 500ms 轮询读取）。
    fn publish(&mut self) {
        let Some(obs) = self.inner.observed() else {
            return;
        };
        let now = Instant::now();
        let dt = now.duration_since(self.last_sample).as_secs_f64();
        // 进度取「流式交付字节」与「缓存窗口末端」的较大者：非流式源只有后者在涨。
        let progress = obs.progress_bytes.max(obs.window.1);
        if dt >= THROUGHPUT_SAMPLE_SECS {
            let delta = progress.saturating_sub(self.last_progress) as f64;
            let instant = delta / dt;
            self.est_bps = if self.est_bps > 0.0 {
                self.est_bps * (1.0 - THROUGHPUT_EMA_NEW) + instant * THROUGHPUT_EMA_NEW
            } else {
                instant
            };
            self.last_progress = progress;
            self.last_sample = now;
        }
        self.downloaded_bytes = self.downloaded_bytes.max(progress);

        let (start, end) = obs.window;
        let length = obs.content_length;
        let pending = obs.pending_stripes;
        let frontier = obs.stream_frontier;
        let bps = self.est_bps;
        let downloaded = self.downloaded_bytes;
        let waiting = self.transient_wait;
        // 整片缓存：窗口从 0 起且已覆盖总长。
        let whole = length.is_some_and(|total| total > 0 && start == 0 && end >= total);
        diag::update_snapshot(self.slot, move |snap| {
            snap.content_length = length;
            snap.cache_start = start;
            snap.cache_end = end;
            snap.prefetch_in_flight = pending;
            snap.downloaded_bytes = downloaded;
            snap.est_bps = bps;
            snap.demux_cache_end = frontier;
            snap.whole_file_cached = whole;
            snap.transient_wait = waiting;
            snap.fill_throughput_bps = bps;
        });
    }
}

impl MediaSource for DiagSource {
    fn uri(&self) -> &str {
        self.inner.uri()
    }

    fn len(&mut self) -> Result<Option<u64>> {
        self.inner.len()
    }

    fn seekable(&self) -> bool {
        self.inner.seekable()
    }

    fn prefetch_hint(&mut self, target: u64) {
        self.inner.prefetch_hint(target);
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        let result = self.inner.read_range(range);
        // 只能观察到「返回给上层」的结果：内核内部重试成功则看不到 Transient。
        self.transient_wait = matches!(result, Err(SourceError::Transient(_)));
        self.publish();
        result
    }

    fn release_buffer(&mut self) {
        self.inner.release_buffer();
    }

    fn cancellation(&self) -> Option<SourceCancellation> {
        self.inner.cancellation()
    }

    fn observed(&self) -> Option<HttpRangeObserved> {
        self.inner.observed()
    }
}
/// 构造上游内核（换源包装的 factory 复用同一个构造器，保证行为一致）。
fn build_kernel(
    uri: &str,
    http_headers: Vec<(String, String)>,
    read_ahead: Option<u64>,
) -> Result<Box<dyn MediaSource>> {
    Ok(Box::new(HttpRangeSource::with_http_headers_and_read_ahead(
        uri,
        http_headers,
        read_ahead,
    )))
}

/// `uri` 置顶后的候选列表：保证首次打开仍走宿主指定的源。
fn ordered_candidates(uri: &str, candidates: Vec<(String, f64)>) -> Vec<(String, f64)> {
    let top = candidates
        .iter()
        .map(|(_, score)| *score)
        .fold(f64::NEG_INFINITY, f64::max);
    let mut ordered = Vec::with_capacity(candidates.len() + 1);
    ordered.push((uri.to_string(), top + 1.0));
    ordered.extend(candidates);
    ordered
}

/// 多候选时套换源包装层；单候选直接返回内核（零额外间接调用）。
fn build_multi_host(
    uri: &str,
    http_headers: Vec<(String, String)>,
    diag_slot: usize,
    candidates: Vec<(String, f64)>,
    read_ahead: Option<u64>,
) -> Result<Box<dyn MediaSource>> {
    if candidates.len() < 2 {
        return build_kernel(uri, http_headers, read_ahead);
    }
    let ordered = ordered_candidates(uri, candidates);
    let factory: SourceFactory = Box::new(move |target: &str| {
        build_kernel(target, http_headers.clone(), read_ahead)
    });
    Ok(Box::new(MultiHostSource::new(factory, ordered, diag_slot)?))
}

/// 本地增强装配点：与旧内核工厂**同签名**，`playback.rs` 的调用形态不变。
///
/// - `HttpStream` 直播走专用源（无总长、不可 seek）；
/// - 其余走「上游内核 + 换源包装 + 诊断发布」。
pub(crate) fn source_from_uri_with_hint_and_headers_slot_and_read_ahead(
    uri: &str,
    source_hint: MediaSourceHint,
    http_headers: Vec<(String, String)>,
    diag_slot: usize,
    candidates: Vec<(String, f64)>,
    http_read_ahead_bytes: Option<u64>,
) -> Result<Box<dyn MediaSource>> {
    if matches!(source_hint, MediaSourceHint::HttpStream) {
        let live = HttpLiveStreamSource::open(uri, http_headers)?;
        return Ok(Box::new(DiagSource::new(Box::new(live), diag_slot)));
    }
    // 非 HTTP（本地文件 / fd:// / 相对路径）：交由上游工厂原样处理，
    // 不套 HTTP 包装层——它们没有网络窗口，也没有 CDN 可换。
    if !(uri.starts_with("http://") || uri.starts_with("https://")) {
        return super::source_from_uri_with_options(
            uri,
            source_hint,
            http_headers,
            http_read_ahead_bytes,
            None,
        );
    }
    let inner = build_multi_host(
        uri,
        http_headers,
        diag_slot,
        candidates,
        http_read_ahead_bytes,
    )?;
    Ok(Box::new(DiagSource::new(inner, diag_slot)))
}

/// DASH 音频源入口：只有槽位语义与视频不同，其余完全一致。
pub(crate) fn source_from_uri_with_hint_and_headers_slot(
    uri: &str,
    source_hint: MediaSourceHint,
    http_headers: Vec<(String, String)>,
    diag_slot: usize,
    candidates: Vec<(String, f64)>,
) -> Result<Box<dyn MediaSource>> {
    source_from_uri_with_hint_and_headers_slot_and_read_ahead(
        uri,
        source_hint,
        http_headers,
        diag_slot,
        candidates,
        None,
    )
}
