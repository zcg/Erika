//! 多 host 候选包装层：把「同一媒体的多个等价 URL + CDN 抖动时无缝换源」从 HTTP
//! 内核里搬出来，包在 `Box<dyn MediaSource>` 外面。
//!
//! NipaPlay 的取流场景里，同一份媒体通常有多个等价 CDN 地址（`MediaRequest::video_candidates`），
//! 抖动/死连时要换到下一个而不打断播放。上游内核只认一个 URL，本层承接这套策略，
//! 使内核得以保持逐字不变（上游改动可干净合并）。
//!
//! **架构约束**：本层只依赖 [`MediaSource`] trait，不引用任何内核具体类型。
//! 换源时通过 [`SourceFactory`] 回调重建内层源，因此内核实现替换对本层零改动。
//!
//! **换源判据全部取自 trait 边界**，不需要窥探内核内部状态：
//! - `Err(SourceError::Http)` —— 终局失败（4xx/签名过期/Range 违约）：重试无意义，立即换。
//! - `Err(SourceError::Transient)` —— 暂时性失败：先记账，**持续** [`STARVE_PERSIST_MS`] 才换，
//!   避免把一次抖动误判成死连。
//! - 单次读取耗时超过 [`STALL_MS`] 且没有拿到字节 —— 视为 stall。
//! - 成功读到大块数据 —— 供给恢复，迟滞计时器与退避时长归位。

use std::collections::VecDeque;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use super::diag::HOST_SWITCH_REQ;
use super::{ByteRange, MediaSource, Result, SourceCancellation, SourceError};

/// 死连判定：超过该时长没收到字节、且上层仍在调用 `read_range`，视为连接已死。
const STALL_MS: u64 = 6_000;
/// 活跃判定：最近这段时间内被 `read_range` 调用过，才算「在播」。
const ACTIVE_MS: u64 = 6_000;
/// 饥饿迟滞：暂时性失败要**持续**这么久才算真饥饿（防瞬时抖动误判）。
const STARVE_PERSIST_MS: u64 = 3_000;
/// 自发换源的滑动窗口限流：窗口内最多 [`MAX_SWITCHES`] 次，超过就停在当前 host
/// 让宿主自愈链接手（避免在多个坏节点间来回横跳）。
const SWITCH_WINDOW_MS: u64 = 300_000;
const MAX_SWITCHES: usize = 5;
/// 换源冷却：避免一轮内对同一死连反复横跳；自发换源每次失败翻倍，上限
/// [`COOLDOWN_MAX`]，供给恢复时归位。
const COOLDOWN: Duration = Duration::from_secs(4);
const COOLDOWN_MAX: Duration = Duration::from_secs(32);
/// 宿主强制换源（[`MultiHostSource::request_host_switch`]）的冷却。比自发切换短——
/// 宿主只在用户明确点「换源」时下发，应当尽快生效。
const FORCED_COOLDOWN: Duration = Duration::from_secs(2);

/// 换源原因（诊断与日志直接展示，宿主读到就知道"为什么又换了"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwitchReason {
    /// 终局 HTTP 失败（4xx / 签名过期 / Range 违约）：重试多少次都不会好。
    HttpFatal,
    /// 暂时性失败持续超过迟滞阈值（真饥饿，而非一次抖动）。
    TransientPersist,
    /// 宿主经 request_host_switch 下发。
    HostRequest,
    /// 连接已死：超过 [`STALL_MS`] 没有字节，而上层仍在读。
    Stall,
}

impl std::fmt::Display for SwitchReason {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let text = match self {
            Self::HttpFatal => "http_fatal",
            Self::TransientPersist => "transient_persist",
            Self::HostRequest => "host_request",
            Self::Stall => "stall",
        };
        f.write_str(text)
    }
}

/// 重建内层源：给定候选 URI，交给工厂造一个新的 [`MediaSource`]。
///
/// 用回调而不是具体类型，是「包装层不依赖内核」这条架构约束的落点：工厂由
/// 组装点（`source.rs` 的工厂函数）提供，本层只负责调用。
pub type SourceFactory = Box<dyn FnMut(&str) -> Result<Box<dyn MediaSource>> + Send>;

/// 候选 host：同一份媒体的一个等价地址 + 宿主给的偏好分。
#[derive(Debug, Clone)]
struct Candidate {
    uri: String,
    score: f64,
}

/// 当前活跃下标（0 = `candidates[0]`）。
const FIRST_CANDIDATE: usize = 0;

/// 取出 URL 的主机名（诊断展示用）；解析不出时退回原串。
fn host_of(uri: &str) -> String {
    let rest = uri.split("://").nth(1).unwrap_or(uri);
    rest.split(['/', '?', '#'])
        .next()
        .unwrap_or(rest)
        .to_string()
}

/// 多 host 包装层。见模块文档。
pub struct MultiHostSource {
    /// 内层源的构造函数（换源时用）。
    factory: SourceFactory,
    /// 当前内层源。
    inner: Box<dyn MediaSource>,
    /// 候选列表，按 `score` 降序；`active` 指向当前使用的那个。
    candidates: Vec<Candidate>,
    active: usize,
    /// 当前活跃 URI（`uri()` 的返回值，也是换源日志的依据）。
    active_uri: String,
    /// 诊断槽位：`host_switch` 全局请求计数器按槽位隔离（DASH 约定 video=0 / audio=1）。
    diag_slot: usize,

    // ---- 自愈状态 ----
    /// 最近一次成功读到非空字节的时刻。
    last_byte: Option<Instant>,
    /// 最近一次 `read_range` 进入的时刻（活跃检测：暂停时不误换）。
    last_read_call: Option<Instant>,
    /// 饥饿起始时刻（`None` = 当前不饥饿）。
    starve_since: Option<Instant>,
    /// 换源冷却截止时刻。
    cooldown_until: Option<Instant>,
    /// 自发换源的时间戳环（滑动窗口限流）。
    switch_times: VecDeque<Instant>,
    /// 当前这一次自发换源的冷却时长（复发退避）。
    switch_backoff: Duration,
    /// 最近一次换源的可读原因（诊断/日志直接展示）。
    last_switch_reason: String,
    /// host 代数：每次实际换源 +1，宿主据此确认换源已生效。
    host_epoch: u64,
    /// 上次消费到的全局强制换源请求计数（构造时同步，防止一开播就被旧请求误触发）。
    last_switch_req: u64,
    /// 换源重入护栏。
    switching: bool,
}

impl MultiHostSource {
    /// 用首个候选构造包装层。`candidates` 为空时退化为单 URL（不启用换源）。
    pub fn new(
        factory: SourceFactory,
        candidates: Vec<(String, f64)>,
        diag_slot: usize,
    ) -> Result<Self> {
        let mut source = Self {
            factory,
            // 占位：紧接着就会被下面第一次真实构造替换掉。
            inner: Box::new(PlaceholderSource),
            candidates: Vec::new(),
            active: FIRST_CANDIDATE,
            active_uri: String::new(),
            diag_slot,
            last_byte: None,
            last_read_call: None,
            starve_since: None,
            cooldown_until: None,
            switch_times: VecDeque::new(),
            switch_backoff: COOLDOWN,
            last_switch_reason: String::new(),
            host_epoch: 0,
            last_switch_req: 0,
            switching: false,
        };
        source.set_candidates(candidates);
        if source.candidates.is_empty() {
            return Err(SourceError::Unsupported(
                "multi-host source requires at least one candidate".to_string(),
            ));
        }
        let uri = source.candidates[FIRST_CANDIDATE].uri.clone();
        source.inner = (source.factory)(&uri)?;
        source.active_uri = uri;
        Ok(source)
    }

    /// 宿主下发候选列表（同一媒体的多个等价地址）。按 `score` 降序排列，
    /// 首个候选成为初始 host。
    pub fn set_candidates(&mut self, list: Vec<(String, f64)>) {
        self.candidates = list
            .into_iter()
            .map(|(uri, score)| Candidate { uri, score })
            .collect();
        self.candidates.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        self.active = FIRST_CANDIDATE;
        // 同步为当前计数：否则开播瞬间会消费到上一轮遗留的强制换源请求。
        self.last_switch_req = self.host_switch_req();
    }

    /// 宿主请求立即换到下一个候选 host（返回 `false` = 槽位越界）。
    ///
    /// 本层不直接切换，而是置位全局请求计数，由下一次 `read_range` 消费——
    /// 与本地内核「不触碰 source 所有权、demux 线程安全」的约定一致：宿主线程
    /// 只 +1，实际切换发生在持有 `&mut self` 的读线程上。
    pub fn request_host_switch(&self) -> bool {
        super::request_host_switch(self.diag_slot)
    }

    /// 当前 host 代数（宿主据此确认换源已生效）。
    pub fn host_epoch(&self) -> u64 {
        self.host_epoch
    }

    /// 最近一次换源原因（空串 = 尚未换过）。
    pub fn last_switch_reason(&self) -> &str {
        &self.last_switch_reason
    }

    /// 候选数量（`< 2` 时换源能力不可用）。
    pub fn candidate_count(&self) -> usize {
        self.candidates.len()
    }

    fn host_switch_req(&self) -> u64 {
        HOST_SWITCH_REQ
            .get(self.diag_slot)
            .map(|counter| counter.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    fn now_ms_since(earlier: Instant) -> u64 {
        earlier.elapsed().as_millis() as u64
    }

    /// 消费宿主下发的强制换源请求（每次 `read_range` 入口调用）。
    fn consume_host_request(&mut self) {
        let current = self.host_switch_req();
        if current == self.last_switch_req {
            return;
        }
        self.last_switch_req = current;
        if let Some(until) = self.cooldown_until
            && until > Instant::now()
        {
            // 冷却期内忽略：宿主连点不该造成连续换源。
            return;
        }
        let _ = self.switch_next(SwitchReason::HostRequest, FORCED_COOLDOWN);
    }

    /// 供给恢复：清饥饿、退避归位。
    fn note_supply_ok(&mut self) {
        self.last_byte = Some(Instant::now());
        self.starve_since = None;
        self.switch_backoff = COOLDOWN;
    }

    /// 记一次暂时性失败（起迟滞计时）。
    fn note_transient(&mut self) {
        if self.starve_since.is_none() {
            self.starve_since = Some(Instant::now());
        }
    }

    /// 判据：当前是否算「在播」——最近有 `read_range` 调用。暂停时不换源。
    fn is_active(&self) -> bool {
        self.last_read_call
            .map(|at| Self::now_ms_since(at) <= ACTIVE_MS)
            .unwrap_or(false)
    }

    /// 判据：暂时性失败是否已持续到超过迟滞阈值。
    fn starve_persisted(&self) -> bool {
        self.starve_since
            .map(|since| Self::now_ms_since(since) >= STARVE_PERSIST_MS)
            .unwrap_or(false)
    }

    /// 判据：连接是否已死——超过 [`STALL_MS`] 没拿到字节，且仍在被读取。
    fn is_stalled(&self) -> bool {
        if !self.is_active() {
            return false;
        }
        match self.last_byte {
            Some(at) => Self::now_ms_since(at) >= STALL_MS,
            // 从未读到过字节：以开始读取的时刻起算。
            None => self
                .last_read_call
                .map(|at| Self::now_ms_since(at) >= STALL_MS)
                .unwrap_or(false),
        }
    }

    /// 滑动窗口限流：窗口内自发换源是否已用尽额度。
    fn switch_budget_exhausted(&mut self) -> bool {
        let window = Duration::from_millis(SWITCH_WINDOW_MS);
        let now = Instant::now();
        while let Some(front) = self.switch_times.front() {
            if now.duration_since(*front) > window {
                self.switch_times.pop_front();
            } else {
                break;
            }
        }
        self.switch_times.len() >= MAX_SWITCHES
    }

    /// 换到下一个候选 host，重建内层源。返回是否真的换了。
    fn switch_next(&mut self, reason: SwitchReason, cooldown: Duration) -> bool {
        if self.switching || self.candidates.len() < 2 {
            return false;
        }
        self.switching = true;
        let next = (self.active + 1) % self.candidates.len();
        let uri = self.candidates[next].uri.clone();
        let rebuilt = (self.factory)(&uri);
        self.switching = false;
        let new_inner = match rebuilt {
            Ok(inner) => inner,
            Err(err) => {
                // 新候选也建不起来：停在原 host，把失败暴露给上层。
                self.last_switch_reason = format!("{reason}: rebuild failed: {err}");
                return false;
            }
        };
        self.inner = new_inner;
        self.active = next;
        self.active_uri = uri;
        self.host_epoch = self.host_epoch.saturating_add(1);
        self.last_switch_reason = reason.to_string();
        self.cooldown_until = Some(Instant::now() + cooldown);
        self.switch_times.push_back(Instant::now());
        // 换源后供给尚未确认：重置字节时刻，让 stall 判定重新起算。
        self.last_byte = Some(Instant::now());
        self.starve_since = None;
        true
    }

    /// 终局失败或持续饥饿时决定是否换源。
    fn should_switch(&mut self, err: &SourceError) -> Option<SwitchReason> {
        if self.candidates.len() < 2 {
            return None;
        }
        if let Some(until) = self.cooldown_until
            && until > Instant::now()
        {
            return None;
        }
        // 自发换源的滑窗限流：窗口内最多 MAX_SWITCHES 次，防止在两个坏 host
        // 之间乒乓。宿主强制换源与死连救援不走这条通路，它们各有自己的冷却。
        if self.switch_budget_exhausted() {
            return None;
        }
        match err {
            // 终局：4xx/签名过期/Range 违约——重试多少次都不会好，立即换。
            SourceError::Http(_) => Some(SwitchReason::HttpFatal),
            // 暂时性：要持续到迟滞阈值才算真饥饿，否则交给上层退避重试。
            SourceError::Transient(_) if self.starve_persisted() => {
                Some(SwitchReason::TransientPersist)
            }
            _ => None,
        }
    }

    /// 读一次（含供给记账）。
    fn attempt(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        let started = Instant::now();
        let out = self.inner.read_range(range);
        let took = started.elapsed();
        let healthy = matches!(&out, Ok(bytes) if !bytes.is_empty());
        if healthy {
            self.note_supply_ok();
        } else if matches!(&out, Err(SourceError::Transient(_))) {
            self.note_transient();
        }
        let _ = took;
        out
    }
}

impl MultiHostSource {
    /// 本地增强：把换源状态写进宿主诊断槽（部分更新，不碰缓存窗口字段）。
    fn publish_host_state(&self) {
        let host = host_of(&self.active_uri);
        let idx = self.active;
        let count = self.candidates.len();
        let epoch = self.host_epoch;
        let reason = self.last_switch_reason.clone();
        super::diag::update_snapshot(self.diag_slot, move |snap| {
            snap.active_host = host;
            snap.active_idx = idx;
            snap.candidate_count = count;
            snap.host_epoch = epoch;
            snap.last_host_switch_reason = reason;
        });
    }
}

impl MediaSource for MultiHostSource {
    fn uri(&self) -> &str {
        &self.active_uri
    }

    fn len(&mut self) -> Result<Option<u64>> {
        self.inner.len()
    }

    fn seekable(&self) -> bool {
        self.inner.seekable()
    }

    fn read_range(&mut self, range: ByteRange) -> Result<Vec<u8>> {
        self.last_read_call = Some(Instant::now());
        // 宿主下发的强制换源在这里消费（本方法独占 `&mut self`，无需额外加锁）。
        self.consume_host_request();
        self.publish_host_state();
        // 死连救援：超过 STALL_MS 没拿到字节，而上层仍在读（demux 在等数据）——
        // 再等下去只是白等，直接换 host。不走 `switch_budget_exhausted`：那是给
        // 「慢但没死」的自发换源限流用的，硬故障必须每次都能救。
        if self.is_stalled() {
            let backoff = self.switch_backoff;
            if self.switch_next(SwitchReason::Stall, backoff) {
                self.switch_backoff = (self.switch_backoff * 2).min(COOLDOWN_MAX);
            }
        }

        let first = self.attempt(range);
        match first {
            Ok(bytes) => Ok(bytes),
            Err(err) => {
                let reason = self.should_switch(&err);
                let switched = match reason {
                    Some(reason) => {
                        let backoff = self.switch_backoff;
                        if self.switch_next(reason, backoff) {
                            // 复发退避：连续换源时冷却翻倍，成功供给后归位。
                            self.switch_backoff = (self.switch_backoff * 2).min(COOLDOWN_MAX);
                            true
                        } else {
                            false
                        }
                    }
                    None => false,
                };
                // 换源对上层透明：`read_range` 幂等，直接在新 host 上重放。
                if switched {
                    self.attempt(range)
                } else {
                    Err(err)
                }
            }
        }
    }

    fn prefetch_hint(&mut self, target: u64) {
        self.inner.prefetch_hint(target);
    }

    fn cancellation(&self) -> Option<SourceCancellation> {
        self.inner.cancellation()
    }

    fn release_buffer(&mut self) {
        self.inner.release_buffer();
    }

    /// 本地增强：把内层内核的只读观察点透传给诊断包装层。
    fn observed(&self) -> Option<super::HttpRangeObserved> {
        self.inner.observed()
    }
}

impl std::fmt::Debug for MultiHostSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MultiHostSource")
            .field("active_uri", &self.active_uri)
            .field("host_epoch", &self.host_epoch)
            .field("candidates", &self.candidates.len())
            .finish()
    }
}

/// `new` 在构造出真正的内层源之前需要一个占位值。
///
/// 它是**不可达**的：`MultiHostSource::new` 在返回前一定会用工厂的产物把它换掉，
/// 且任何 `MediaSource` 方法都无法在替换发生前被外部调用（`&mut self` 尚未交出）。
struct PlaceholderSource;

impl MediaSource for PlaceholderSource {
    fn uri(&self) -> &str {
        "placeholder:"
    }

    fn len(&mut self) -> Result<Option<u64>> {
        Err(SourceError::Unsupported("placeholder source".to_string()))
    }

    fn read_range(&mut self, _range: ByteRange) -> Result<Vec<u8>> {
        Err(SourceError::Unsupported("placeholder source".to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 每次读取都返回同一个错误的假源，用来驱动换源判据。
    struct FailingFatal {
        uri: String,
    }

    impl MediaSource for FailingFatal {
        fn uri(&self) -> &str {
            &self.uri
        }

        fn len(&mut self) -> Result<Option<u64>> {
            Ok(None)
        }

        fn read_range(&mut self, _range: ByteRange) -> Result<Vec<u8>> {
            Err(SourceError::Http("403".to_string()))
        }
    }

    /// 每次读取都返回暂时性错误的假源。
    struct FailingTransient {
        uri: String,
    }

    impl MediaSource for FailingTransient {
        fn uri(&self) -> &str {
            &self.uri
        }

        fn len(&mut self) -> Result<Option<u64>> {
            Ok(None)
        }

        fn read_range(&mut self, _range: ByteRange) -> Result<Vec<u8>> {
            Err(SourceError::Transient("timeout".to_string()))
        }
    }

    /// 每次读取都返回固定字节的假源。
    struct Serving {
        uri: String,
        data: Vec<u8>,
    }

    impl MediaSource for Serving {
        fn uri(&self) -> &str {
            &self.uri
        }

        fn len(&mut self) -> Result<Option<u64>> {
            Ok(Some(self.data.len() as u64))
        }

        fn read_range(&mut self, _range: ByteRange) -> Result<Vec<u8>> {
            Ok(self.data.clone())
        }
    }

    fn candidates() -> Vec<(String, f64)> {
        vec![
            ("http://a/media".to_string(), 1.0),
            ("http://b/media".to_string(), 0.5),
        ]
    }

    /// 剧本：首个候选（构造时的那次）总是终局失败，之后一律正常供给。
    fn scripted() -> (SourceFactory, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let factory: SourceFactory = Box::new(move |uri: &str| {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(Box::new(FailingFatal {
                    uri: uri.to_string(),
                }))
            } else {
                Ok(Box::new(Serving {
                    uri: uri.to_string(),
                    data: b"payload".to_vec(),
                }))
            }
        });
        (factory, calls)
    }

    #[test]
    fn fatal_http_error_switches_host_and_replays_the_read() {
        let (factory, calls) = scripted();
        let mut source = MultiHostSource::new(factory, candidates(), 0).expect("construct");
        assert_eq!(source.candidate_count(), 2);
        assert_eq!(source.host_epoch(), 0);

        // 首个 host 返回终局错误 → 切到第二个 host 并就地重放这次读。
        let bytes = source
            .read_range(ByteRange::suffix_from(0))
            .expect("failover read");
        assert_eq!(bytes, b"payload");
        assert_eq!(source.uri(), "http://b/media");
        assert_eq!(source.host_epoch(), 1);
        assert_eq!(source.last_switch_reason(), "http_fatal");
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn single_candidate_never_switches() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let factory: SourceFactory = Box::new(move |uri: &str| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FailingFatal {
                uri: uri.to_string(),
            }))
        });
        let mut source =
            MultiHostSource::new(factory, vec![("http://only/media".to_string(), 1.0)], 0)
                .expect("construct");

        let err = source
            .read_range(ByteRange::suffix_from(0))
            .expect_err("fatal error must surface");
        assert!(matches!(err, SourceError::Http(_)));
        assert_eq!(source.host_epoch(), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn transient_error_waits_for_hysteresis_before_switching() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        let factory: SourceFactory = Box::new(move |uri: &str| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FailingTransient {
                uri: uri.to_string(),
            }))
        });
        let mut source = MultiHostSource::new(factory, candidates(), 0).expect("construct");

        // 一次暂时性失败不足以换源：先交给上层退避重试。
        let err = source
            .read_range(ByteRange::suffix_from(0))
            .expect_err("transient error must surface");
        assert!(matches!(err, SourceError::Transient(_)));
        assert_eq!(source.host_epoch(), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn empty_candidate_list_is_rejected() {
        let factory: SourceFactory = Box::new(|uri: &str| {
            Ok(Box::new(FailingFatal {
                uri: uri.to_string(),
            }))
        });
        assert!(MultiHostSource::new(factory, Vec::new(), 0).is_err());
    }

    #[test]
    fn host_request_switches_without_a_read_error() {
        // 用 slot 1：slot 0 的全局计数被其它用例共享，避免互相干扰。
        let (factory, _calls) = scripted();
        let mut source = MultiHostSource::new(factory, candidates(), 1).expect("construct");
        assert!(source.request_host_switch(), "slot 1 must exist");

        let bytes = source
            .read_range(ByteRange::suffix_from(0))
            .expect("read after forced switch");
        assert_eq!(bytes, b"payload");
        assert_eq!(source.host_epoch(), 1);
        assert_eq!(source.last_switch_reason(), "host_request");
    }
}
