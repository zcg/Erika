//! 进程级流诊断槽：内核发布写入，宿主按槽轮询读取。
//!
//! 这里只放**与具体内核实现无关**的共享状态：快照结构、槽位静态量、带宽/吞吐
//! 估计、HTTP 请求画像。两个内核（上游 `source.rs` 的 `HttpRangeSource` 与本地
//! `source/legacy.rs`）都通过本模块发布，因此它是**被依赖方**，不依赖任何内核。
//!
//! 槽位约定：0 = 视频（或单流/直播），1 = DASH 音频。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 宿主可轮询的 `HttpRangeSource` 实时诊断快照（网络/缓冲状态）。
///
/// demux 线程写、宿主 UI 线程读：单播放窗口约束下通过全局槽传递，
/// 复用 Erika 现有的进程级诊断先例（`trace`）。宿主用它渲染"流诊断
/// 浮窗"——显示缓存窗口覆盖到哪里、还有多少段在下载、累计下载量
/// （差分得实时速度），从而判断"为什么卡"（网络慢 / 窗口外 seek 等预取）。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SourceDiagSnapshot {
    /// 媒体总长度（字节），HEAD/Content-Range 得到；未知为 None。
    pub content_length: Option<u64>,
    /// 当前缓存窗口起点（字节）。
    pub cache_start: u64,
    /// 当前缓存窗口终点（字节，含）。
    pub cache_end: u64,
    /// 正在下载的**段/连接数**（in-flight）。
    ///
    /// 语义分两条通路（2026-09-03）：
    ///   - 顺序长读（生产默认）：**0 或 1**——只有一条持续读的长连接，
    ///     0 可能是"正在背压等待"（缓冲已满）或"读完收工"；
    ///   - 旧分段路径：并行预取段数 + 整片桶（历史上稳态 5~6）。
    /// 宿主判"源是否空闲"必须结合 `demux_cache_end`（0 且窗口为空才是真空闲）：
    /// 长读路径下 0 只表示"此刻没在读 socket"，不代表没数据。
    pub prefetch_in_flight: usize,
    /// **实时**累计已接收字节（字节在线路上到达即计入，不等待整段完成）。
    ///
    /// 语义变更（2026-09-01）：此前只在「整段下载完成」时累加，慢网下一个
    /// 预取段要数秒到十几秒才完成，传输期间本值不动 → 宿主差分恒为 0 →
    /// 把「正在缓慢下载」误判成「无供给」，触发饥饿换源 / ABR 连环降档
    /// （用户反馈的「无故换源」「自动降级画质」根因）。现由 `fetch_http_range`
    /// 在 body 分块读循环里逐块 `fetch_add`，涓流也如实反映。
    ///
    /// 含**被丢弃**的字节（seek 后作废的在飞段、整片预载被前移丢弃的段）——
    /// 带宽估算要的是「线路上真实到达的量」，丢不丢不影响吞吐判定。
    pub downloaded_bytes: u64,
    /// 源内带宽估算（字节/秒，2s 滑动 EMA）。
    ///
    /// **拉模型**（2026-09-01 改）：此前是推模型——只在「段完成」事件里结算，
    /// 慢网下 est_bps 会在整段传输期间**冻结**在旧值（不归零、也不更新），
    /// 与宿主差分同样失真。现在任何读取方（`publish_diag` 或宿主
    /// `source_diag_snapshot_slot`）调用即以「实时已收字节总量」做窗口差分，
    /// 与段完成事件解耦——即使 demux 线程正阻塞在长 join 上，宿主的 500ms
    /// poll 也能读到新鲜值。
    pub est_bps: f64,
    /// 当前活跃 host（多源无缝换 host 用）：显示"现在用的是哪家 CDN"，
    /// 换源时随之变化，便于直观确认切换生效。无候选时为空。
    pub active_host: String,
    /// 当前活跃候选下标（0 = candidates[0]）。
    pub active_idx: usize,
    /// 候选 host 总数（0 = 单源模式）。
    pub candidate_count: usize,
    /// demux 端缓存窗口终点（**未合并**整片预载桶）。宿主策略算「真实可播
    /// 缓冲秒」必须用它：`cache_end = max(demux, 桶 covered_until)` 会把桶
    /// 从 0 顺序预载的进度混进来，seek 后窗口被严重高估 → ABR「余粮充足」
    /// 误判不降档 / starve 判不出来（2026-09-02 无感化改造 K4）。
    pub demux_cache_end: u64,
    /// **本片是否已完整缓冲到服务端声明的文件末尾**（"不需要再下载了"）。
    ///
    /// 判据：长读窗口的网络填充前沿 `write_pos` 已经到达服务端声明的总长
    /// `total`（首个 206 的 `Content-Range: bytes x-y/total` 回填，或 HEAD）。
    /// 旧分段路径下退化为 `cache_end >= content_length`。
    ///
    /// **为什么必须独立成字段**（真机事故 2026-09-15）：整片下载完之后
    /// `write_pos` 不再前进 → 「距上次收到字节」的时间戳一路涨到几百秒，
    /// 与"链路死了"在**任何只看到达字节的指标上完全同形**。宿主因此打出
    /// `vstall=596s` / `采用供给0.00Mbps`，读起来像"没网速"，实际是
    /// "已经下完了"。本字段是唯一能把这两种现实分开的**正面事实**：
    /// 它为真时，`stalled_sec` 大、`est_bps` 为 0 都是**预期**，不是故障。
    ///
    /// ⚠️ **不要**用 [`Self::cache_end`]（`max(demux, 整片预载桶)`）判：桶的
    /// `covered_until` 只增不减，且从 0 顺序预载，seek 后仍停在旧前沿。这里
    /// 用**长读窗口自己的** `write_pos`——它就是"这条流真的读到哪了"。
    pub whole_file_cached: bool,
    /// 源 host 代数（每次实际切换 +1）。宿主据此确认 request_host_switch 已生效。
    pub host_epoch: u64,
    /// 该槽的读路径此刻是否正处在「暂时读不到数据、退避重试中」的等待窗口
    /// （见 [`HttpRangeSource::read_range_with_transient_wait`]）。
    ///
    /// 语义与 `prefetch_in_flight` **不同**：后者只说明"有段在飞"，段挂死时同样
    /// 为真；本字段只在"读请求已经失败、源正在等待字节回来"时为真。
    /// 内核 worker 读它对外发 `BufferingChanged`，宿主据此显示"缓冲中"而不是
    /// 把这段静止当成"源已死"——这是 mpv `paused-for-cache` 的对等语义：
    /// **等数据，不判整条流失败**。
    ///
    /// 置位时机：等待窗口打开；清除时机：读恢复、等待窗口判定为真死、或源析构。
    pub transient_wait: bool,
    /// **累计已发出的 HTTP Range 请求数**（进程级单调，**含重试与断点续传**）。
    ///
    /// 口径：每一次真正打到网络的 GET 记 1。计数点在**唯一发请求的那一处**
    /// ——[`fetch_http_range`] 的重试循环里、`request.call()` 之前；因此一次
    /// 逻辑 fetch 因失败重试或断点续传发出的**每一个**请求都各记 1（三次
    /// 尝试 = +3）。**不在调用点计数**：调用点记数会把重试合并成 1 次，
    /// "请求放大"这一现象（重试/续传把请求数推高、每请求平均字节被拖低）
    /// 就看不出来了，而这恰恰是判定"是不是被 CDN 限速"的关键证据。
    ///
    /// **进程级单调、永不清零**：故意不在源构造/析构时复位。宿主同进程里
    /// 存在短命的"探针源"（`probe_http_total_length` 只为探长度而建），把
    /// 计数挂在源实例上、或构造时清零，探针一构造就会把正在播放那条流的
    /// 计数洗掉，宿主两次轮询的差分随即失真。单调总量 + 宿主差分是唯一稳的
    /// 口径（与 `downloaded_bytes` 的差分同一套做法）。换源/重开播都不复位：
    /// 宿主读到的（差分得到的）是"当前这一刻本槽的请求速率"，与源实例无关。
    pub http_requests: u64,
    /// 与 [`Self::http_requests`] **同口径**的累计已收字节（进程级单调）。
    ///
    /// 用途：算"平均每个请求收回多少字节"（`http_request_bytes /
    /// http_requests`），即"请求画像"的均值一侧。**不能拿 `downloaded_bytes`
    /// 去除**：后者是**源级**计数（`Drop` 时注销、换源即从 0 起），与进程级
    /// 单调的 `http_requests` 生命周期不同，换过源的会话里这个商会把分母
    /// 留在历史值上 → 得到远小于真值的伪均值。两个进程级单调量相除才自洽。
    ///
    /// 口径含失败请求：连接失败/5xx 的请求只进分母（收到 0 字节），均值因此
    /// 天然反映"重试浪费"——正常顺序预取时它应接近**段大小**（段上限
    /// 256KB）；显著偏低说明请求在空转（在飞段没填满 / 段被反复作废）。
    pub http_request_bytes: u64,
    // ---- 滑动窗口分层缓存（2026-09-04）----
    /// **内存层（热区）常驻字节数**：顺序长读窗口里实际躺着的字节
    /// （前向 read-ahead + 后向回看）。单位：字节。上限见 `memory_budget_bytes`。
    pub cache_memory_bytes: u64,
    /// **磁盘层（冷区）当前字节数**：热区淘汰下来、按段落盘的区间总量
    /// （含仍在内存缓冲写、尚未落盘的尾部）。单位：字节；上限由冷区预算
    /// （`ERIKA_HTTP_COLD_BUDGET_BYTES`，默认 256MiB）钉住，超限丢最久远的段。
    pub cache_disk_bytes: u64,
    /// **冷区（磁盘层）命中次数**（累计单调，含跨冷/热边界的前缀命中）。
    ///
    /// 口径：每一次"因为冷区里有字节而**没有**发网络请求"的读记 1。它是
    /// "回拖/重播不重下"的现场证据：用户在已缓冲区间里拖动时，这个数在涨，
    /// 而 `http_requests` 不涨。
    pub cold_hits: u64,
    /// 冷区命中字节数（累计单调，与 `cold_hits` 同口径配对）。
    pub cold_hit_bytes: u64,
    /// **内存层硬上限**（字节预算）：`前后向缓存字节数之和`永不超过它。
    /// 默认视频/单流 24MiB、音频 6MiB（`ERIKA_HTTP_WINDOW_MEMORY_BYTES` 覆盖），
    /// 且永不低于宿主配置的 read-ahead + back（不许比改造前更小）。
    pub memory_budget_bytes: u64,
    /// **前向预算**（字节）：当前"往前读到哪"的目标——由时间预算
    /// `N`（秒）× 实测码率算出，弱网下 N 变大（180s 上限），因此这个值也变大。
    /// 达到它读线程就停在那里（背压），消费推进后再往前补。
    pub forward_budget_bytes: u64,
    /// 当前**前向可播秒数** = 前向缓冲字节 ÷ 实测码率（无码率估计时为 0）。
    /// 这是"弱网更激进"最直观的验收数字：吞吐越低，它应当越大。
    pub forward_playable_secs: f64,
    /// **窗口未满期间实测到的链路吞吐**（字节/秒，500ms 窗口 + 0.3/0.7 EMA）。
    ///
    /// 口径与为什么可信：只有窗口**没满**（读线程真的在读 socket）的时间片才
    /// 采样；窗口满（背压）时不采样、不衰减，所以本值反映"链路能推多快"，
    /// 而**不会**被"客户端故意不读"钳成消费速率。宿主判"能不能升画质/换源"
    /// 要的是这个口径。读数路径：顺序长读线程直接写全局槽，宿主 500ms poll
    /// 直接读，**不经过 demux 线程**——demux 阻塞在别处也照样新鲜。
    /// 0.0 = 尚未测得（如全程都在背压/还未开始读）。
    ///
    /// **它同时是滑动窗口预算里"实测吞吐"的唯一来源**（内核 `supply_estimate_bps`）：
    /// 预算与宿主读的是同一个数，不存在两套互相打架的网速判断。
    pub fill_throughput_bps: f64,
    /// 顺序长读的**连接重建总次数**（= 除首个请求外多出来的请求数）。
    ///
    /// 恒等式：**某条流的请求数 = 1 + `stream_reconnects_total`**（首个请求不算
    /// 重建）。宿主可用它把自己的 `http_requests` 差分拆开。
    ///
    /// 现场判据：**正常顺序播放期间它应当为 0**（一条连接读到底）。它不是 0 时，
    /// 用下面四个分桶字段看是谁的锅——用户的真机实测正是靠这几个数分清
    /// "CDN 提前收线"与"我们的 reset/换源"。
    pub stream_reconnects_total: u64,
    /// 分桶：**服务端提前收线 / body 传输错误**（本次连接收到过字节）。CDN 行为。
    ///
    /// 用户真机实测的形态：CDN 声明了完整 `Content-Range` 却在约 1MB 处收线，
    /// 内核静默续传——每 ~1MB 一个请求，稳定 ~0.5 次/秒。这**不是**内核的 bug，
    /// 但也无法消除：请求数下界 = 实际下载字节 ÷ CDN 每次连接愿给的字节。
    pub stream_reconnects_early_close: u64,
    /// 分桶：连接建立后**一个字节都没收到**（限流/瞬时错误，退避重试）。
    pub stream_reconnects_empty: u64,
    /// 分桶：本地 `reset()`（seek 出窗 / 回拖超出后向窗口 / 乱序读）。
    /// 顺序播放期间应当为 0；它涨说明 demux 在窗口外跳读。
    pub stream_reconnects_reset: u64,
    /// 分桶：换 host（`host_epoch` 变化）导致的重建。
    pub stream_reconnects_host_switch: u64,
    /// **分片式 CDN 的正常续传次数**：服务端用有界 `Content-Range`
    /// （`end + 1 < total`）明确表示"这次只给这一段"，内核在分片边界续传。
    /// 计入 `stream_reconnects_total`，但语义是"按服务端节奏走"，不是故障。
    pub stream_chunk_resumes: u64,
    /// **判决实验（假说 D）**：请求起点与"上一次连接结束时的前沿"一致的次数
    /// ——即**断点续传**。单调递增的请求序列会全部落进这个桶。
    pub stream_range_contiguous: u64,
    /// 判决实验：起点**大于**前沿的次数（前向跳读）。
    pub stream_range_forward_jumps: u64,
    /// 判决实验：起点**小于**前沿的次数（**回拖 = demux 在 seek**）。
    /// 它明显非零时，请求放大的根因是 demux 的读模式，**与 CDN 无关**。
    pub stream_range_backward_jumps: u64,
    /// 最近一次 Range 请求的起点偏移（字节）。
    pub stream_range_last_start: u64,
    /// 最近一条连接实际收到的字节数。"每条连接给多少字节"的分布是判
    /// "CDN 有固定配额 / 还是停读导致"（调研疑问 1）的一手数据。
    pub stream_last_connection_bytes: u64,
    /// 所有连接里收到字节最多的那一条（看有没有长连接真跑出过大块）。
    pub stream_max_connection_bytes: u64,
    /// **最近一次换源的可读原因**（如
    /// `upos-a.bilivideo.com → upos-b.bilivideo.com：starving(前向0.8s<2.0s 持续3.2s)`）。
    /// 空串 = 本轮还没换过。宿主直接把它显示在诊断面板上：用户看到"换源 N 次"
    /// 时必须能同时看到"为什么换"（"看得见才算数"）。
    pub last_host_switch_reason: String,
    // ---- seek 出窗的**跨节点并行回填**（2026-09-15；形态约束见文件上方整段）----
    /// **累计跨节点并行回填批次数**：每次"seek 到未缓冲位置"尝试并行 +1。
    ///
    /// **正常顺播期间恒不涨**——这就是把三种形态分开的那个数：
    ///   - 纯顺播（一条 open-ended Range）：`seek_fill_batches` 不动；
    ///   - seek 走了跨节点 N 段：`seek_fill_batches` 与 `seek_fill_connections` 同涨；
    ///   - **seek 退化为单连接**（门禁否决/分段失败）：`seek_fill_batches` 涨、
    ///     `seek_fill_gate_denied` 或 `seek_fill_fallbacks` 涨、而
    ///     `seek_fill_connections` 几乎不涨。
    /// 有了这三个数，`http_requests` 上涨才不会被误读成"退化成 downloader"。
    pub seek_fill_batches: u64,
    /// 累计建立的并行分段连接数（每个候选节点一条；含只做了下载前预检的段）。
    pub seek_fill_connections: u64,
    /// 累计**被采用**的跨节点并行字节数（回退时被丢弃的段不计）。
    pub seek_fill_bytes: u64,
    /// **下载前门禁否决**次数（ETag/文件总长缺失或不一致 → 退回单节点）。
    /// 它涨说明"这次没并行是因为证据不足"，**不是并行没生效**。
    pub seek_fill_gate_denied: u64,
    /// 回退到单连接的批次次数（任一段失败/超时/比对不过/期间又 seek 了）。
    pub seek_fill_fallbacks: u64,
    /// 边界重叠逐字节比对**通过**的批次数（跨节点数据一致性的直接证据）。
    pub seek_fill_overlap_ok: u64,
    /// 边界重叠逐字节比对**失败**的批次数。`> 0` 时请立刻看日志那一行：
    /// 该批已被整批丢弃并回退，**没有任何可疑字节进过播放流**。
    pub seek_fill_overlap_mismatch: u64,
    /// 最近一次批次的段数（0 = 本次会话还没走过跨节点并行）。
    pub seek_fill_last_segments: u64,
    /// **当前在飞的分段连接数**（0 = 没有并行批次在跑）。用户要的"用了几条连接"
    /// 当场可读；它同时是硬上限 [`SEEK_PARALLEL_MAX_CONNECTIONS`] 的执行依据
    /// （超过上限的新批次会被整批放弃，见 `run_seek_fill`）。
    pub seek_fill_in_flight: u64,
    /// 最近一次批次被采用的字节数。
    pub seek_fill_last_bytes: u64,
    /// 最近一次批次墙钟（毫秒）。
    pub seek_fill_last_ms: u64,
    /// 最近一次批次"同一批数据用单连接拉"的预计墙钟（毫秒）。实测 ÷ 它
    /// 就是**可复核的提速幅度**（日志那一行同时给出两个数）。
    pub seek_fill_last_expected_ms: u64,
    /// 最近一次批次是否最终回退（true = 这次 seek 走了单连接串行）。
    pub seek_fill_last_fallback: bool,
    /// 最近一次**下载前门禁裁定的可读理由**（内核原文，宿主原样展示：
    /// 用户看到"没加速"时必须能同时看到"为什么"）。
    pub seek_fill_last_gate_reason: String,
    /// 自适应关闭：连续实测到"并行没有叠加"（例如 CDN 按出口 IP 限速）已停用
    /// 本会话的并行。此时 seek 仍走单连接——**不是功能坏了，是测出来没用**。
    pub seek_fill_disabled: bool,
}

/// 诊断槽数。DASH 双流（video + audio）各占一槽独立发布/清空，互不覆盖；
/// 单流/直播只用槽 0。见 `HttpRangeSource::with_http_headers_and_diag_slot`。
/// 内核 worker 遍历全部槽判定"源是否在等字节"（`core::source_transient_wait_active`）。
pub(crate) const SOURCE_DIAG_SLOTS: usize = 2;

pub(super) static SOURCE_DIAG: Mutex<[SourceDiagSnapshot; SOURCE_DIAG_SLOTS]> = Mutex::new([
    SourceDiagSnapshot {
        content_length: None,
        cache_start: 0,
        cache_end: 0,
        prefetch_in_flight: 0,
        downloaded_bytes: 0,
        est_bps: 0.0,
        active_host: String::new(),
        active_idx: 0,
        candidate_count: 0,
        demux_cache_end: 0,
        host_epoch: 0,
        whole_file_cached: false,
        transient_wait: false,
        http_requests: 0,
        http_request_bytes: 0,
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
    },
    SourceDiagSnapshot {
        content_length: None,
        cache_start: 0,
        cache_end: 0,
        prefetch_in_flight: 0,
        downloaded_bytes: 0,
        est_bps: 0.0,
        active_host: String::new(),
        active_idx: 0,
        candidate_count: 0,
        demux_cache_end: 0,
        host_epoch: 0,
        whole_file_cached: false,
        transient_wait: false,
        http_requests: 0,
        http_request_bytes: 0,
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
    },
]);
/// 每槽位的「实时已收字节」计数器集合（一个槽可能同时登记多个计数器：
/// 源自身的 `rx` + 整片预载桶的 `downloaded`）。
///
/// HTTP 段在 body 分块读循环里直接 `fetch_add` 到这里登记的计数器，因此
/// 慢网下「段在飞、未完成」期间总量也在增长——这是 `downloaded_bytes` /
/// `est_bps` 不再失真的基础。源构造时登记、`Drop` 时按 `Arc::ptr_eq` 注销。
pub(super) static SOURCE_RX: Mutex<[Vec<Arc<AtomicU64>>; SOURCE_DIAG_SLOTS]> =
    Mutex::new([Vec::new(), Vec::new()]);

/// **HTTP 请求画像**的进程级单调计数器（每槽一份）：累计已发出的 HTTP Range
/// 请求数（含重试与断点续传）。计数点在 [`fetch_http_range`] 里唯一发请求的
/// 那一处，口径见 [`SourceDiagSnapshot::http_requests`]。
///
/// 为什么是"进程级 + 单调 + 永不清零"：宿主同进程里存在短命的"探针源"
/// （`probe_http_total_length` 只为探总长而建），若计数挂在源实例上、或源
/// 构造时清零，探针一构造就会把正在播放那条流的计数洗掉，宿主两次轮询的
/// 差分随即失真。单调总量 + 宿主差分是唯一稳的口径（与 `downloaded_bytes`
/// 同一套做法）；换源、重开播都不复位。
pub(super) static SOURCE_HTTP_REQUESTS: [AtomicU64; SOURCE_DIAG_SLOTS] =
    [AtomicU64::new(0), AtomicU64::new(0)];

/// 与 `SOURCE_HTTP_REQUESTS` 配对的"累计已收字节"（同口径、同生命周期、
/// 同一处累加）。只用于算"平均每请求多少字节"——两个原子量必须同源同寿命，
/// 相除才自洽（拿**源级**的 `downloaded_bytes` 去除，换过源就会得到把分母
/// 留在历史值上的伪均值）。
pub(super) static SOURCE_HTTP_REQUEST_BYTES: [AtomicU64; SOURCE_DIAG_SLOTS] =
    [AtomicU64::new(0), AtomicU64::new(0)];

/// **填窗期实测吞吐**（每槽一份，`f64` 位模式装在 `AtomicU64` 里）。
///
/// 口径见 [`SourceDiagSnapshot::fill_throughput_bps`]：只在窗口未满（读线程
/// 真的在读 socket）的时间片上采样，因此不会被"客户端故意不读"的背压钳成
/// 消费速率。由顺序长读线程直接写、宿主的 500ms poll 直接读——不经过 demux
/// 线程，所以 demux 阻塞在长 join 上时宿主照样能读到新鲜值。
pub(super) static SOURCE_FILL_BPS: [AtomicU64; SOURCE_DIAG_SLOTS] =
    [AtomicU64::new(0), AtomicU64::new(0)];

/// 发布填窗期吞吐到全局槽（宿主 poll 读它）。
pub(super) fn store_fill_bps(slot: usize, bps: f64) {
    if let Some(entry) = SOURCE_FILL_BPS.get(slot) {
        entry.store(bps.to_bits(), Ordering::Relaxed);
    }
}

/// 读取填窗期吞吐（未测得为 0.0）。
pub(super) fn load_fill_bps(slot: usize) -> f64 {
    SOURCE_FILL_BPS
        .get(slot)
        .map(|entry| f64::from_bits(entry.load(Ordering::Relaxed)))
        .unwrap_or(0.0)
}

/// **滑动窗口预算使用的"实测吞吐"口径（唯一入口）。**
///
/// 为什么集中到这一个函数：网速判断必须只有一套。内核侧曾有三套来源
/// （`est_bps` 的 2s EMA、宿主 `policy::LinkThroughput` 的填窗期 EMA、以及
/// 自适应测量层）。本函数明确：**预算只用"填窗期实测吞吐"**，它的唯一事实源
/// 就是内核顺序长读线程自己采样并发布的 [`SourceDiagSnapshot::fill_throughput_bps`]
/// （全局槽 `SOURCE_FILL_BPS[slot]`，宿主 500ms 轮询读的也是同一个值）。
/// 宿主若要做统一供给估计（如 `supply_estimate_bps()`），**直接读这个字段即可**
/// ——不要再另起一套 EMA，也不要再喂 `est_bps`（`est_bps` 是"全部到达字节速率"，
/// 窗口满时会退化成消费速率，口径不同，只服务旧分段路径的自适应）。
///
/// 返回 `None` = **未知**（不是 0、更不是"充裕"）。调用方必须把"未知"当成
/// "我们不知道链路有多快"来处理：回退到保守的固定秒数/字节，见 `window_budget`。
///
/// 新鲜度：读线程在窗口未满时每 ≥500ms 发布一次（`FillMeter`），进入背压
/// （窗口满）时把该填窗期结算掉然后**冻结**读数——窗口满期间读到的就是"上一次
/// 真实填窗期测到的链路吞吐"，不会被"我们故意不读"稀释成 0。
pub(super) fn supply_estimate_bps(slot: usize) -> Option<f64> {
    let bps = load_fill_bps(slot);
    (bps.is_finite() && bps > 0.0).then_some(bps)
}

/// 「这次 HTTP 请求画像记到哪两个原子量上」。
///
/// 为什么不直接传槽位下标：槽位是进程级共享数组的索引，而**单测需要一个
/// 不受并行用例干扰的计数**——静态数组是全进程共享的，同进程并行跑的两个
/// 用例会互相污染计数，"一次成功请求 = 1"这类断言就不可靠了。把"记到哪里"
/// 显式作为参数传入：生产路径指向 `SOURCE_HTTP_REQUESTS[diag_slot]`，用例
/// 指向自己的静态计数器，两者互不影响。
#[derive(Debug, Clone, Copy)]
pub(super) struct HttpRequestProfile {
    pub(super) requests: &'static AtomicU64,
    pub(super) bytes: &'static AtomicU64,
}

/// 取某槽位的请求画像计数目标；槽位越界返回 None（不计数，但不影响取数）。
pub(super) fn http_request_profile(slot: usize) -> Option<HttpRequestProfile> {
    Some(HttpRequestProfile {
        requests: SOURCE_HTTP_REQUESTS.get(slot)?,
        bytes: SOURCE_HTTP_REQUEST_BYTES.get(slot)?,
    })
}

impl HttpRequestProfile {
    /// 记录一次**真正发出**的 GET（重试与断点续传各记 1）。
    pub(super) fn record_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    /// 记录本次请求在 body 分块读循环里实际到达的字节。
    pub(super) fn record_bytes(&self, n: u64) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
    }
}

/// 带宽估算的窗口采样状态（拉模型用）。
pub(super) struct BandwidthSample {
    pub(super) at: Instant,
    pub(super) total: u64,
    pub(super) bps: f64,
}

/// 每槽位上一窗口的采样点。宿主 poll 与 `publish_diag` 共用——先到者触发结算，
/// 后到者读到缓存值（窗口未满时复用），保证「同一窗口内多次读取得到同一值」。
pub(super) static SOURCE_BW: Mutex<[Option<BandwidthSample>; SOURCE_DIAG_SLOTS]> =
    Mutex::new([None, None]);

pub(super) const BANDWIDTH_ESTIMATE_WINDOW: Duration = Duration::from_secs(2);

/// EMA 权重：新瞬时值 0.3 / 历史 0.7（沿用推模型取值，行为连续）。
pub(super) const BANDWIDTH_EMA_NEW: f64 = 0.3;

/// 槽位实时已收字节总量（该槽登记的所有计数器之和）。
pub(super) fn live_rx_bytes(slot: usize) -> u64 {
    let Ok(counters) = SOURCE_RX.lock() else {
        return 0;
    };
    counters
        .get(slot)
        .map(|list| {
            list.iter()
                .map(|c| c.load(Ordering::Relaxed))
                .fold(0u64, u64::saturating_add)
        })
        .unwrap_or(0)
}

/// 带宽估算（**拉模型**）：以「实时已收字节总量」为基准做窗口差分并更新 EMA。
///
/// 关键性质：与「段完成事件」解耦。旧推模型在慢网下整段传输期间不结算，
/// est_bps 冻结在旧值；现在任何调用方（demux 的 `publish_diag` 或宿主的
/// `source_diag_snapshot_slot`）调用都会按真实窗口时间结算——宿主 500ms
/// 轮询即使撞上 demux 正阻塞在长 join 上，也能读到新鲜供给值。
///
/// 跨源（换源）时总量会回落（旧计数器注销、新计数器从 0 起），此处用
/// `saturating_sub` 兜底——该窗口瞬时值记 0，EMA 平滑吸收，不会出负速率。
pub(super) fn refresh_bandwidth(slot: usize, total_rx: u64) -> f64 {
    let now = Instant::now();
    let Ok(mut slots) = SOURCE_BW.lock() else {
        return 0.0;
    };
    let Some(entry) = slots.get_mut(slot) else {
        return 0.0;
    };
    let Some(prev) = entry.as_ref() else {
        // 冷启动：只记基线，本窗口不出估算（避免把"从 0 起"算成瞬时速率）。
        *entry = Some(BandwidthSample {
            at: now,
            total: total_rx,
            bps: 0.0,
        });
        return 0.0;
    };
    let elapsed = now.duration_since(prev.at);
    if elapsed < BANDWIDTH_ESTIMATE_WINDOW {
        return prev.bps;
    }
    let inst_bps = total_rx.saturating_sub(prev.total) as f64 / elapsed.as_secs_f64();
    let mut bps = if prev.bps > 0.0 {
        prev.bps * (1.0 - BANDWIDTH_EMA_NEW) + inst_bps * BANDWIDTH_EMA_NEW
    } else {
        inst_bps
    };
    // 零供给地板：长时间无字节（暂停/挂起数分钟）时 EMA 指数衰减趋零但**永不
    // 等于 0**——denormal 级的 est_bps 仍满足 `> 0.0`，自适应路径
    // （`prefetch_join_timeout` 的 `seg / est_bps`）会拿它当真实带宽去除，
    // 得到天文数字秒数，`Duration::from_secs_f64` 对 "too big" 直接 panic。
    // 低于 1bps 归零，回到「est_bps <= 0 = 未知」语义（下游走默认值分支）。
    if bps < 1.0 {
        bps = 0.0;
    }
    *entry = Some(BandwidthSample {
        at: now,
        total: total_rx,
        bps,
    });
    bps
}

pub fn source_diag_snapshot() -> SourceDiagSnapshot {
    source_diag_snapshot_slot(0)
}

/// 读取指定槽位的诊断快照。DASH 双流约定 video=0、audio=1，
/// 各源独立发布/清空，宿主据此分别显示两条流的缓冲状态。
/// 槽位越界或无对应源时返回零值。
pub fn source_diag_snapshot_slot(slot: usize) -> SourceDiagSnapshot {
    let mut snap = SOURCE_DIAG
        .lock()
        .unwrap()
        .get(slot)
        .cloned()
        .unwrap_or_default();
    // **读取时刻刷新供给信号**（拉模型的核心）。快照其余字段（缓存窗口/在飞段
    // 数/总长）由 demux 线程写，demux 阻塞期间不动是合理的；但供给速度必须实时
    // ——宿主 500ms 一次的策略 poll 正是靠这里拿到真值。若 demux 正卡在长 join
    // 上（慢网常态），没有这一步 est_bps 与 downloaded_bytes 会一起冻结。
    let total = live_rx_bytes(slot);
    snap.downloaded_bytes = total;
    snap.est_bps = refresh_bandwidth(slot, total);
    // 填窗期吞吐：由顺序长读线程直接写全局槽（不经过 demux 线程），这里取最新值
    // ——宿主 500ms 轮询必须在 demux 卡住时也能读到新鲜链路吞吐（判升画质用）。
    snap.fill_throughput_bps = load_fill_bps(slot);
    // HTTP 请求画像同理由**读取时刻直接读**（不依赖 demux 线程发布）：这两个
    // 量是进程级单调计数，demux 卡在长 join 上时它照样在涨（预取线程/整片
    // 预载桶是独立线程），宿主 1s 差分拿到的就是真值。
    if let Some(profile) = http_request_profile(slot) {
        snap.http_requests = profile.requests.load(Ordering::Relaxed);
        snap.http_request_bytes = profile.bytes.load(Ordering::Relaxed);
    }
    snap
}

/// 宿主在下一个 HTTP 决策点强制换 host 的请求计数器（0 = 视频，1 = 音频）。
///
/// 宿主判定当前 CDN 节点已经劣化时调用 [`request_host_switch`]，源在下一个
/// 重连/预取决策点观察到计数变化后换节点，而不是等 TCP 自己超时。
pub(super) static HOST_SWITCH_REQ: [AtomicU64; SOURCE_DIAG_SLOTS] =
    [AtomicU64::new(0), AtomicU64::new(0)];

/// 请求 `slot` 对应的源在下一个决策点切换 host。
///
/// 返回 `false` 表示 `slot` 越界（只有 0/1 两槽）。本函数只递增计数器，
/// 不做任何 I/O，可在 UI 线程安全调用。
pub fn request_host_switch(slot: usize) -> bool {
    let Some(counter) = HOST_SWITCH_REQ.get(slot) else {
        return false;
    };
    counter.fetch_add(1, Ordering::Relaxed);
    true
}
