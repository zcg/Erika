/// Track-based collision avoidance layout engine.
/// Inspired by Next2's track + compaction approach for correct pre-computed layout.
///
/// Key design: per-type track arrays storing lightweight collision records,
/// compact expired items before each placement, assign to first non-colliding track,
/// compute Y from track index.
use smallvec::SmallVec;

use crate::danmaku::dfm_core::model::{DanmakuItem, DanmakuType, GlobalFlags};

type DisplacedIndices = SmallVec<[usize; 4]>;

// ---------------------------------------------------------------------------
// 排布可观测性
// ---------------------------------------------------------------------------

/// 一轮排布（一个 `DanmakuRetainer` 的生命周期）的工作量计数。
///
/// 这些数字全都是**排布真实做过的事**，没有任何估算：`collide_checks` 是真正
/// 调用碰撞判定的次数，`compact_popped` 是真正被清理掉的轨道条目数。它同时是
/// danmu2ass「一条轨道只需要记住一条摘要」这一优化是否生效的直接证据——
/// 改前每个新时间戳都要把所有滚动轨道全量过滤一遍，滚动轨的 `compact_scanned`
/// 会等于「轨道数 × 每条轨道条目数 × 时间戳数」。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlacementCounters {
    /// 碰撞判定调用次数
    pub collide_checks: u64,
    /// 滚动轨清理**扫描过的条目数**。
    ///
    /// ⚠️ **口径不是全局的**，读这个数前先记住三条限定：
    /// 1. **只覆盖滚动轨**（`TrackData::compact`）。固定轨走
    ///    [`compact_fixed_tracks`]，它不接收 `stats`：既不进 `compact_scanned`
    ///    也不进 `compact_popped`。
    /// 2. 同一条滚动轨的两条路径口径也不同：升序快路径只计**真正被弹出的队首
    ///    前缀**（队首没过期就直接跳过整条轨道、不计）；顺序不可信的兜底路径
    ///    扫完整条轨道，计的是**整条轨道长度**。
    /// 3. 所以它不能与 `collide_checks` 之类全量口径的计数直接比较，也不是
    ///    "所有轨道清理扫描过的条目数"——别那样写文档。
    pub compact_scanned: u64,
    /// 轨道清理移除的过期条目数（同样只覆盖滚动轨，见上）
    pub compact_popped: u64,
    /// 成功放到轨道上的弹幕数
    pub placed: u64,
    /// 没有轨道可放、被丢弃的弹幕数
    pub dropped: u64,
    /// 被覆盖策略顶掉的已有弹幕数
    pub displaced: u64,
}

/// 排布阶段的进程级读数：累计值 + 最近一轮。
///
/// 播放器可以直接把它显示出来（例如诊断浮窗里的一行），不需要重开视频、
/// 也不需要专门的测试素材：弹幕一排完，下一帧读到的就是新数字。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlacementReport {
    /// 已经跑完的排布轮数
    pub rounds: u64,
    /// 进程启动以来的累计
    pub totals: PlacementCounters,
    /// 累计排布耗时（毫秒）
    pub total_elapsed_ms: u64,
    /// 最近跑完的一轮；`None` = 还没有跑完过一轮排布
    pub last_round: Option<PlacementRound>,
}

/// 一轮排布（一个 `DanmakuRetainer` 从建到丢弃）的完整读数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlacementRound {
    pub counters: PlacementCounters,
    /// 这一轮的墙钟耗时（毫秒）。排布在后台线程同步跑完，所以它就是
    /// "这批弹幕排了多久"。
    pub elapsed_ms: u64,
}

mod global_stats {
    use super::{PlacementCounters, PlacementReport, PlacementRound};
    use std::sync::atomic::{AtomicU64, Ordering};

    static ROUNDS: AtomicU64 = AtomicU64::new(0);
    static COLLIDE: AtomicU64 = AtomicU64::new(0);
    static SCANNED: AtomicU64 = AtomicU64::new(0);
    static POPPED: AtomicU64 = AtomicU64::new(0);
    static PLACED: AtomicU64 = AtomicU64::new(0);
    static DROPPED: AtomicU64 = AtomicU64::new(0);
    static DISPLACED: AtomicU64 = AtomicU64::new(0);
    static TOTAL_ELAPSED_MS: AtomicU64 = AtomicU64::new(0);

    static LAST_COLLIDE: AtomicU64 = AtomicU64::new(0);
    static LAST_SCANNED: AtomicU64 = AtomicU64::new(0);
    static LAST_POPPED: AtomicU64 = AtomicU64::new(0);
    static LAST_PLACED: AtomicU64 = AtomicU64::new(0);
    static LAST_DROPPED: AtomicU64 = AtomicU64::new(0);
    static LAST_DISPLACED: AtomicU64 = AtomicU64::new(0);
    static LAST_ELAPSED_MS: AtomicU64 = AtomicU64::new(0);

    /// 一轮排布结束时调用（`DanmakuRetainer` 析构）：累加累计值并覆盖
    /// “最近一轮”快照。
    pub fn record_round(counters: &PlacementCounters, elapsed_ms: u64) {
        ROUNDS.fetch_add(1, Ordering::Relaxed);
        COLLIDE.fetch_add(counters.collide_checks, Ordering::Relaxed);
        SCANNED.fetch_add(counters.compact_scanned, Ordering::Relaxed);
        POPPED.fetch_add(counters.compact_popped, Ordering::Relaxed);
        PLACED.fetch_add(counters.placed, Ordering::Relaxed);
        DROPPED.fetch_add(counters.dropped, Ordering::Relaxed);
        DISPLACED.fetch_add(counters.displaced, Ordering::Relaxed);
        TOTAL_ELAPSED_MS.fetch_add(elapsed_ms, Ordering::Relaxed);
        LAST_COLLIDE.store(counters.collide_checks, Ordering::Relaxed);
        LAST_SCANNED.store(counters.compact_scanned, Ordering::Relaxed);
        LAST_POPPED.store(counters.compact_popped, Ordering::Relaxed);
        LAST_PLACED.store(counters.placed, Ordering::Relaxed);
        LAST_DROPPED.store(counters.dropped, Ordering::Relaxed);
        LAST_DISPLACED.store(counters.displaced, Ordering::Relaxed);
        LAST_ELAPSED_MS.store(elapsed_ms, Ordering::Relaxed);
    }

    pub fn snapshot() -> PlacementReport {
        let rounds = ROUNDS.load(Ordering::Relaxed);
        PlacementReport {
            rounds,
            totals: PlacementCounters {
                collide_checks: COLLIDE.load(Ordering::Relaxed),
                compact_scanned: SCANNED.load(Ordering::Relaxed),
                compact_popped: POPPED.load(Ordering::Relaxed),
                placed: PLACED.load(Ordering::Relaxed),
                dropped: DROPPED.load(Ordering::Relaxed),
                displaced: DISPLACED.load(Ordering::Relaxed),
            },
            total_elapsed_ms: TOTAL_ELAPSED_MS.load(Ordering::Relaxed),
            last_round: (rounds > 0).then(|| PlacementRound {
                counters: PlacementCounters {
                    collide_checks: LAST_COLLIDE.load(Ordering::Relaxed),
                    compact_scanned: LAST_SCANNED.load(Ordering::Relaxed),
                    compact_popped: LAST_POPPED.load(Ordering::Relaxed),
                    placed: LAST_PLACED.load(Ordering::Relaxed),
                    dropped: LAST_DROPPED.load(Ordering::Relaxed),
                    displaced: LAST_DISPLACED.load(Ordering::Relaxed),
                },
                elapsed_ms: LAST_ELAPSED_MS.load(Ordering::Relaxed),
            }),
        }
    }
}

/// 读一次排布读数。计数器是 `Relaxed` 原子，读到的值可能比真实值旧一瞬——
/// 用于展示足够，不要用它做正确性判断。
pub fn placement_report() -> PlacementReport {
    global_stats::snapshot()
}

/// Lightweight record stored in tracks for collision detection.
/// Avoids needing to look up items by index from an external array.
/// For fixed danmaku, `time_ms` is used as the track's END time (not start time),
/// so that Pass 1 can correctly check if a new danmaku starts after the track is free.
#[derive(Debug, Clone)]
struct TrackEntry {
    time_ms: i64,
    duration_ms: i64,
    paint_width: f32,
    step_x: f32,
    danmaku_type: DanmakuType,
    danmaku_index: usize,
}

impl TrackEntry {
    fn from_item(item: &DanmakuItem, index: usize) -> Self {
        Self {
            time_ms: item.time_ms,
            duration_ms: item.duration_ms,
            paint_width: item.paint_width,
            step_x: item.step_x,
            danmaku_type: item.danmaku_type,
            danmaku_index: index,
        }
    }

    fn end_ms(&self) -> i64 {
        self.time_ms + self.duration_ms
    }
}

#[derive(Debug, Clone)]
struct TrackData {
    tracks: Vec<Vec<TrackEntry>>,
    last_compact_ms: i64,
    /// 每条轨道内的条目是否按 `end_ms()` 非递减（由 `push_entry` 维护）。
    /// 为 false 时 `compact` 退化为全量过滤——只影响速度，不影响结果。
    end_ms_sorted: bool,
}

impl TrackData {
    fn new() -> Self {
        Self {
            tracks: Vec::new(),
            last_compact_ms: i64::MIN,
            end_ms_sorted: true,
        }
    }

    fn ensure_track_count(&mut self, count: usize) {
        if self.tracks.len() != count {
            self.tracks.resize_with(count, Vec::new);
        }
    }

    /// 追加一个条目，并维护「同一轨道内条目按 `end_ms()` 非递减」这一不变量。
    ///
    /// `select_scroll_track` 按时间递增顺序处理弹幕，而 `dfm.rs` 给同一视口下
    /// 的所有滚动弹幕同一个 duration，所以正常路径恒为升序；但 `fix*` 是公开
    /// API，乱序调用、或同一轨道混入不同 duration 都是允许的——一旦发现不满足
    /// 就把整组轨道标记为「顺序不可信」，让 `compact` 走回全量过滤的兜底路径。
    fn push_entry(&mut self, row: usize, entry: TrackEntry) {
        let out_of_order = match self.tracks[row].last() {
            Some(last) => last.end_ms() > entry.end_ms(),
            None => false,
        };
        if out_of_order {
            self.end_ms_sorted = false;
        }
        self.tracks[row].push(entry);
    }

    /// Compact expired entries from every track.
    ///
    /// DFM original keeps entries for start+2*duration, which is unnecessarily long.
    /// We match Next2's approach: an entry is expired when its full duration has elapsed
    /// since it started (i.e., it has scrolled completely off screen).
    ///
    /// 这里吸收了 danmu2ass「一条轨道只需要记住一条摘要」的思想：条目既然按
    /// 结束时间升序，过期条目必然构成队首的一段前缀，于是清理从「每个新时间戳
    /// 都把全部轨道全量过滤一遍」变成「只看队首、摊销 O(1)」。
    /// 参考：`danmu2ass/src/canvas/lane.rs`（`Lane` 只保存 `last_shoot_time` /
    /// `last_length`；`canvas/mod.rs::draw_float` 因此完全不需要扫描历史）。
    fn compact(
        &mut self,
        current_time_ms: i64,
        _current_duration_ms: i64,
        stats: &mut PlacementCounters,
    ) {
        if current_time_ms == self.last_compact_ms {
            return;
        }
        self.last_compact_ms = current_time_ms;

        if !self.end_ms_sorted {
            // 兜底路径：顺序不可信时逐条判断（结果与升序路径完全一致）
            for track in self.tracks.iter_mut() {
                let before = track.len();
                stats.compact_scanned += before as u64;
                track.retain(|existing| current_time_ms < existing.end_ms());
                stats.compact_popped += (before - track.len()) as u64;
            }
            return;
        }

        for track in self.tracks.iter_mut() {
            // 升序轨道上队首没过期 ⇒ 后面的都没过期，一次比较即可跳过整条轨道
            let expired = track
                .iter()
                .take_while(|existing| existing.end_ms() <= current_time_ms)
                .count();
            if expired == 0 {
                continue;
            }
            stats.compact_scanned += expired as u64;
            stats.compact_popped += expired as u64;
            if expired == track.len() {
                track.clear();
            } else {
                track.drain(..expired);
            }
        }
    }

    fn clear(&mut self) {
        self.tracks.clear();
        self.last_compact_ms = i64::MIN;
        self.end_ms_sorted = true;
    }
}

/// Track-based collision avoidance engine.
#[derive(Debug, Clone)]
pub struct DanmakuRetainer {
    r2l_tracks: TrackData,
    lr_tracks: TrackData,
    top_tracks: TrackData,
    bottom_tracks: TrackData,
    margin: f32,
    track_gap_ratio: f32,
    /// 本轮排布的工作量（`fix*` 与轨道清理各自累加）。
    /// 析构时汇总进进程级读数，见 [`placement_report`]。
    stats: PlacementCounters,
    /// 本轮排布的起点，用于在析构时算出"这批弹幕排了多久"。
    started_at: std::time::Instant,
}

impl DanmakuRetainer {
    pub fn new(margin: f32, track_gap_ratio: f32) -> Self {
        Self {
            r2l_tracks: TrackData::new(),
            lr_tracks: TrackData::new(),
            top_tracks: TrackData::new(),
            bottom_tracks: TrackData::new(),
            margin,
            track_gap_ratio,
            stats: PlacementCounters::default(),
            started_at: std::time::Instant::now(),
        }
    }

    pub fn clear(&mut self) {
        self.r2l_tracks.clear();
        self.lr_tracks.clear();
        self.top_tracks.clear();
        self.bottom_tracks.clear();
    }

    /// Assign a Y position to a danmaku item using track-based collision avoidance.
    /// Returns true if a position was found, false if the item should be dropped.
    /// Returns indices of any displaced danmaku that should be marked as filtered.
    pub fn fix(
        &mut self,
        item: &mut DanmakuItem,
        view_width: f32,
        view_height: f32,
        flags: &GlobalFlags,
        display_area: f32,
        is_me: bool,
    ) -> (bool, DisplacedIndices) {
        self.fix_with_options(
            item,
            view_width,
            view_height,
            flags,
            display_area,
            is_me,
            false,
            true,
        )
    }

    pub fn fix_with_options(
        &mut self,
        item: &mut DanmakuItem,
        view_width: f32,
        view_height: f32,
        flags: &GlobalFlags,
        display_area: f32,
        is_me: bool,
        allow_stacking: bool,
        allow_scroll_overwrite: bool,
    ) -> (bool, DisplacedIndices) {
        self.fix_with_options_and_preferred_track(
            item,
            view_width,
            view_height,
            flags,
            display_area,
            is_me,
            allow_stacking,
            allow_scroll_overwrite,
            None,
        )
    }

    pub fn fix_with_options_and_preferred_track(
        &mut self,
        item: &mut DanmakuItem,
        view_width: f32,
        view_height: f32,
        flags: &GlobalFlags,
        display_area: f32,
        is_me: bool,
        allow_stacking: bool,
        allow_scroll_overwrite: bool,
        preferred_track: Option<usize>,
    ) -> (bool, DisplacedIndices) {
        let (placed, displaced) = self.fix_inner(
            item,
            view_width,
            view_height,
            flags,
            display_area,
            is_me,
            allow_stacking,
            allow_scroll_overwrite,
            preferred_track,
        );
        if placed {
            self.stats.placed += 1;
        } else {
            self.stats.dropped += 1;
        }
        self.stats.displaced += displaced.len() as u64;
        (placed, displaced)
    }

    /// 真正的排布逻辑；计数与上报都在公开的 `fix*` 包装里做。
    fn fix_inner(
        &mut self,
        item: &mut DanmakuItem,
        view_width: f32,
        view_height: f32,
        flags: &GlobalFlags,
        display_area: f32,
        is_me: bool,
        allow_stacking: bool,
        allow_scroll_overwrite: bool,
        preferred_track: Option<usize>,
    ) -> (bool, DisplacedIndices) {
        // Each type has independent track systems (r2l_tracks, top_tracks, etc.).
        // Respect display_area exactly; display_area=1.0 means full-screen danmaku.
        let effective_height = view_height * display_area;
        let track_height = item.paint_height + item.paint_height * self.track_gap_ratio;
        let track_count = (effective_height / track_height).floor().max(1.0) as usize;
        let danmaku_index = item.index as usize;

        let entry = TrackEntry::from_item(item, danmaku_index);

        match item.danmaku_type {
            DanmakuType::ScrollRL => {
                self.r2l_tracks.ensure_track_count(track_count);
                match select_scroll_track(
                    &entry,
                    &mut self.r2l_tracks,
                    &mut self.stats,
                    track_count,
                    view_width,
                    is_me,
                    allow_stacking,
                    allow_scroll_overwrite,
                    preferred_track,
                ) {
                    Some((row, displaced)) => {
                        item.y = self.margin + row as f32 * track_height;
                        item.is_shown = true;
                        item.flags.visible = flags.visible_flag;
                        (true, displaced)
                    }
                    None => (false, SmallVec::new()),
                }
            }
            DanmakuType::ScrollLR => {
                self.lr_tracks.ensure_track_count(track_count);
                match select_scroll_track(
                    &entry,
                    &mut self.lr_tracks,
                    &mut self.stats,
                    track_count,
                    view_width,
                    is_me,
                    allow_stacking,
                    allow_scroll_overwrite,
                    preferred_track,
                ) {
                    Some((row, displaced)) => {
                        item.y = self.margin + row as f32 * track_height;
                        item.is_shown = true;
                        item.flags.visible = flags.visible_flag;
                        (true, displaced)
                    }
                    None => (false, SmallVec::new()),
                }
            }
            DanmakuType::FixTop => {
                self.top_tracks.ensure_track_count(track_count);
                match select_fixed_track(&entry, &mut self.top_tracks, track_count, preferred_track)
                {
                    Some(row) => {
                        item.y = self.margin + row as f32 * track_height;
                        item.is_shown = true;
                        item.flags.visible = flags.visible_flag;
                        (true, SmallVec::new())
                    }
                    None => (false, SmallVec::new()),
                }
            }
            DanmakuType::FixBottom => {
                self.bottom_tracks.ensure_track_count(track_count);
                match select_fixed_track(
                    &entry,
                    &mut self.bottom_tracks,
                    track_count,
                    preferred_track,
                ) {
                    Some(row) => {
                        item.y = effective_height - (row as f32 + 1.0) * track_height;
                        item.is_shown = true;
                        item.flags.visible = flags.visible_flag;
                        (true, SmallVec::new())
                    }
                    None => (false, SmallVec::new()),
                }
            }
            DanmakuType::Special => {
                item.y = 0.0;
                item.is_shown = true;
                item.flags.visible = flags.visible_flag;
                (true, SmallVec::new())
            }
        }
    }
}

/// 一轮排布结束：把本轮的工作量汇总进进程级读数（见 [`placement_report`]）。
///
/// 放在 `Drop` 里是因为 `dfm.rs::prepare_layout` 每一轮都新建一个 retainer 并按轮
/// 丢弃它——这样不必改动任何调用方，就能拿到"这一轮排布到底做了多少事"，播放器
/// 浮窗/日志直接读 `placement_report()` 即可。
///
/// 注意：`DanmakuRetainer` 是 `Clone` 的，克隆出来的副本会各自上报一次（生产
/// 路径 `dfm.rs` 从不克隆它，只有诊断数字会因此偏大）。
impl Drop for DanmakuRetainer {
    fn drop(&mut self) {
        let counters = std::mem::take(&mut self.stats);
        let elapsed_ms = self.started_at.elapsed().as_millis() as u64;
        global_stats::record_round(&counters, elapsed_ms);
    }
}

// ---------------------------------------------------------------------------
// Track selection
// ---------------------------------------------------------------------------

/// Select a track for a scroll danmaku.
/// Returns (track_index, displaced_indices) or None if the item should be dropped.
/// The upper 40% of tracks form a stable zone that overwriteInsert never
/// selects as a victim. The lower 60% form an overflow zone where DFM's
/// strategy may replace the track whose items have progressed furthest left.
///
/// This mirrors the previous implementation's behaviour exactly — the old
/// single loop already restricted victims with `i >= overwrite_start` — and
/// only makes the two zones explicit.
///
/// One documented exception: a self-sent danmaku (`is_me`) still claims track
/// 0 unconditionally, clearing whatever occupies it. That is a deliberate
/// product rule and the only way a stable-zone track loses its contents.
fn select_scroll_track(
    new_entry: &TrackEntry,
    track_data: &mut TrackData,
    stats: &mut PlacementCounters,
    track_count: usize,
    view_width: f32,
    is_me: bool,
    allow_stacking: bool,
    allow_scroll_overwrite: bool,
    preferred_track: Option<usize>,
) -> Option<(usize, DisplacedIndices)> {
    track_data.compact(new_entry.time_ms, new_entry.duration_ms, stats);

    if let Some(row) = preferred_track.filter(|row| *row < track_count) {
        let can_reuse = allow_stacking
            || track_data.tracks[row].iter().rev().all(|existing| {
                stats.collide_checks += 1;
                !scroll_entries_collide(new_entry, existing, view_width)
            });
        if can_reuse {
            track_data.push_entry(row, new_entry.clone());
            return Some((row, SmallVec::new()));
        }
    }

    if allow_stacking && track_count > 0 {
        let row = stack_track_for(new_entry, track_count);
        track_data.push_entry(row, new_entry.clone());
        return Some((row, SmallVec::new()));
    }

    let overwrite_count = ((track_count as f32 * 0.6).ceil() as usize)
        .max(1)
        .min(track_count);
    let overwrite_start = track_count - overwrite_count;

    let mut best_track = overwrite_start;
    let mut min_right_edge = f32::MAX;

    // Phase 1: fill/reuse the stable zone without ever selecting it as an
    // overwrite candidate.
    //
    // 同一视口下滚动弹幕的 duration 相同（`dfm.rs` 统一给 `scroll_dur_ms`），
    // 因此轨道内「结束时间升序」等价于「发射时间升序」，而「越晚发射的条目对
    // 新弹幕约束越紧」——danmu2ass 让「只与轨道最后一条比较」成立用的正是这条
    // 单调性（见 `danmu2ass/src/canvas/lane.rs::Lane::available_for`：它只保存
    // 最后一条的发射时间与长度，就能判定新弹幕会不会撞上整条轨道）。这里判定
    // 仍要逐条做（DFM 的两点采样判据比 danmu2ass 的解析判据宽松，不能直接换成
    // 只比最后一条），但**从最新一条开始扫**能让命中碰撞的短路几乎总在第一步。
    for i in 0..overwrite_start {
        if track_data.tracks[i].is_empty() {
            track_data.push_entry(i, new_entry.clone());
            return Some((i, SmallVec::new()));
        }
        let collides = track_data.tracks[i].iter().rev().any(|existing| {
            stats.collide_checks += 1;
            scroll_entries_collide(new_entry, existing, view_width)
        });
        if !collides {
            track_data.push_entry(i, new_entry.clone());
            return Some((i, SmallVec::new()));
        }
    }

    // Phase 2: fill/reuse the overflow zone and remember the safest sacrifice
    // candidate in that zone only.
    for i in overwrite_start..track_count {
        if track_data.tracks[i].is_empty() {
            track_data.push_entry(i, new_entry.clone());
            return Some((i, SmallVec::new()));
        }
        let mut collides = false;
        let mut track_min_right = f32::MAX;
        for existing in &track_data.tracks[i] {
            stats.collide_checks += 1;
            if scroll_entries_collide(new_entry, existing, view_width) {
                collides = true;
            }
            let right_edge = entry_right_edge_at(existing, new_entry.time_ms, view_width);
            if right_edge < track_min_right {
                track_min_right = right_edge;
            }
        }
        if !collides {
            track_data.push_entry(i, new_entry.clone());
            return Some((i, SmallVec::new()));
        }
        if track_min_right < min_right_edge {
            min_right_edge = track_min_right;
            best_track = i;
        }
    }

    if is_me && track_count > 0 {
        let displaced: DisplacedIndices = track_data.tracks[0]
            .iter()
            .map(|e| e.danmaku_index)
            .collect();
        track_data.tracks[0].clear();
        track_data.push_entry(0, new_entry.clone());
        return Some((0, displaced));
    }

    if !allow_scroll_overwrite {
        return None;
    }

    if min_right_edge < f32::MAX {
        let displaced: DisplacedIndices = track_data.tracks[best_track]
            .iter()
            .map(|e| e.danmaku_index)
            .collect();
        track_data.tracks[best_track].clear();
        track_data.push_entry(best_track, new_entry.clone());
        return Some((best_track, displaced));
    }

    None
}

fn stack_track_for(entry: &TrackEntry, track_count: usize) -> usize {
    let mut value = entry.danmaku_index.wrapping_mul(1_103_515_245usize);
    value ^= (entry.time_ms.max(0) as usize).rotate_left(11);
    value ^= (entry.paint_width.to_bits() as usize).rotate_left(5);
    value % track_count.max(1)
}

fn entry_right_edge_at(entry: &TrackEntry, time_ms: i64, view_width: f32) -> f32 {
    entry_x_at(entry, time_ms, view_width) + entry.paint_width
}

/// Compact expired entries from fixed tracks.
/// Unlike scroll tracks which compact based on time windows, fixed tracks chain items
/// sequentially (each starts when the previous ends). This removes items from the front
/// of each track's chain whose end time has passed, freeing tracks for new items.
/// Mirrors Next2's `compact_static_tracks` (which clears entire tracks when the item ends).
fn compact_fixed_tracks(tracks: &mut [Vec<TrackEntry>], current_time_ms: i64) {
    for track in tracks.iter_mut() {
        // Remove all entries from the front that have fully expired.
        // The chain structure guarantees entries are in time order and
        // entries[i].end_ms() == entries[i+1].time_ms (no gaps).
        let mut remove_count = 0;
        for entry in track.iter() {
            if entry.end_ms() <= current_time_ms {
                remove_count += 1;
            } else {
                break;
            }
        }
        if remove_count > 0 {
            track.drain(0..remove_count);
        }
    }
}

fn select_fixed_track(
    new_entry: &TrackEntry,
    track_data: &mut TrackData,
    track_count: usize,
    preferred_track: Option<usize>,
) -> Option<usize> {
    let new_start = new_entry.time_ms;

    if new_start != track_data.last_compact_ms {
        track_data.last_compact_ms = new_start;
        compact_fixed_tracks(&mut track_data.tracks, new_start);
    }

    let tracks = &mut track_data.tracks;
    if let Some(row) = preferred_track.filter(|row| *row < track_count) {
        let track = &mut tracks[row];
        if track.is_empty() || new_start >= track.last().expect("track is not empty").end_ms() {
            track.push(new_entry.clone());
            return Some(row);
        }
    }
    for i in 0..track_count {
        if tracks[i].is_empty() {
            tracks[i].push(new_entry.clone());
            return Some(i);
        }
        let last = tracks[i].last().unwrap();
        let last_end = last.end_ms();
        if new_start >= last_end {
            tracks[i].push(new_entry.clone());
            return Some(i);
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Collision detection (ported from DanmakuFlameMaster's DanmakuUtils)
// ---------------------------------------------------------------------------

/// Check if two scroll entries will collide (1:1 port from DFM)
/// Ported from DanmakuUtils.willHitInDuration()
#[inline]
fn scroll_entries_collide(entry_a: &TrackEntry, entry_b: &TrackEntry, view_width: f32) -> bool {
    if entry_a.danmaku_type != entry_b.danmaku_type {
        return false;
    }

    let (d1, d2) = if entry_a.time_ms <= entry_b.time_ms {
        (entry_a, entry_b)
    } else {
        (entry_b, entry_a)
    };

    let d_time = d2.time_ms - d1.time_ms;

    if d_time <= 0 {
        return true;
    }

    if d_time >= d1.duration_ms as i64 {
        return false;
    }

    let d1_left_at_d2_start = entry_left_at(d1, d2.time_ms, view_width);
    let d1_right_at_d2_start = d1_left_at_d2_start + d1.paint_width;
    let d2_left_at_start = entry_left_at_start(d2, view_width);

    if check_hit_same_type(
        d1.danmaku_type,
        d1_left_at_d2_start,
        d1_right_at_d2_start,
        d2_left_at_start,
        d2_left_at_start + d2.paint_width,
    ) {
        return true;
    }

    let d1_left_at_d1_end = entry_left_at(d1, d1.end_ms(), view_width);
    let d1_right_at_d1_end = d1_left_at_d1_end + d1.paint_width;
    let d2_left_at_d1_end = entry_left_at(d2, d1.end_ms(), view_width);

    check_hit_same_type(
        d1.danmaku_type,
        d1_left_at_d1_end,
        d1_right_at_d1_end,
        d2_left_at_d1_end,
        d2_left_at_d1_end + d2.paint_width,
    )
}

#[inline]
fn check_hit_same_type(
    danmaku_type: DanmakuType,
    left1: f32,
    right1: f32,
    left2: f32,
    right2: f32,
) -> bool {
    debug_assert!(danmaku_type.is_scroll());
    match danmaku_type {
        DanmakuType::ScrollRL => left2 < right1,
        DanmakuType::ScrollLR => right2 > left1,
        _ => false,
    }
}

#[inline]
fn entry_left_at_start(entry: &TrackEntry, view_width: f32) -> f32 {
    match entry.danmaku_type {
        DanmakuType::ScrollRL => view_width,
        DanmakuType::ScrollLR => -entry.paint_width,
        _ => 0.0,
    }
}

#[inline]
fn entry_left_at(entry: &TrackEntry, time_ms: i64, view_width: f32) -> f32 {
    if entry.danmaku_type == DanmakuType::ScrollLR {
        return entry_x_at(entry, time_ms, view_width);
    }

    let elapsed = (time_ms - entry.time_ms).max(0) as f32;

    if entry.step_x <= 0.0 {
        return view_width;
    }

    if elapsed >= entry.duration_ms as f32 {
        return -entry.paint_width;
    }

    let pos = view_width - elapsed * entry.step_x;
    pos.max(-entry.paint_width)
}

#[inline]
#[allow(dead_code)]
fn entry_right_at(entry: &TrackEntry, time_ms: i64, view_width: f32) -> f32 {
    entry_left_at(entry, time_ms, view_width) + entry.paint_width
}

#[inline]
fn entry_x_at(entry: &TrackEntry, time_ms: i64, view_width: f32) -> f32 {
    let elapsed = (time_ms - entry.time_ms).max(0) as f32;
    if entry.step_x <= 0.0 {
        return match entry.danmaku_type {
            DanmakuType::ScrollRL => view_width,
            DanmakuType::ScrollLR => -entry.paint_width,
            _ => 0.0,
        };
    }
    match entry.danmaku_type {
        DanmakuType::ScrollRL => view_width - elapsed * entry.step_x,
        DanmakuType::ScrollLR => elapsed * entry.step_x - entry.paint_width,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::danmaku::dfm_core::model::DanmakuItem;

    fn calc_step_x(paint_width: f32, duration_ms: i64, view_width: f32) -> f32 {
        (view_width + paint_width) / duration_ms as f32
    }

    fn make_scroll_item(
        time_ms: i64,
        text: &str,
        paint_width: f32,
        danmaku_type: DanmakuType,
        duration_ms: i64,
        view_width: f32,
    ) -> DanmakuItem {
        let mut item = DanmakuItem::new(
            time_ms,
            text.into(),
            0xFFFFFFFF,
            25.0,
            danmaku_type,
            duration_ms,
        );
        item.paint_width = paint_width;
        item.paint_height = 30.0;
        item.step_x = calc_step_x(paint_width, duration_ms, view_width);
        item
    }

    fn make_fixed_item(
        time_ms: i64,
        text: &str,
        danmaku_type: DanmakuType,
        duration_ms: i64,
    ) -> DanmakuItem {
        let mut item = DanmakuItem::new(
            time_ms,
            text.into(),
            0xFFFFFFFF,
            25.0,
            danmaku_type,
            duration_ms,
        );
        item.paint_width = 100.0;
        item.paint_height = 30.0;
        item
    }

    #[test]
    fn test_first_item_placed_at_top() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut item = make_scroll_item(0, "test", 100.0, DanmakuType::ScrollRL, 5000, 1920.0);

        let (placed, _) = retainer.fix(&mut item, 1920.0, 1080.0, &flags, 1.0, false);
        assert!(placed);
        assert!(item.is_shown);
        assert!(
            (item.y - 2.0).abs() < 1.0,
            "first item y={} should be ~2.0",
            item.y
        );
    }

    #[test]
    fn test_same_time_items_different_tracks() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items = vec![
            make_scroll_item(0, "first", 100.0, DanmakuType::ScrollRL, 5000, 1920.0),
            make_scroll_item(0, "second", 100.0, DanmakuType::ScrollRL, 5000, 1920.0),
        ];

        let (placed1, _) = retainer.fix(&mut items[0], 1920.0, 1080.0, &flags, 1.0, false);
        assert!(placed1);
        let first_y = items[0].y;

        let (placed2, _) = retainer.fix(&mut items[1], 1920.0, 1080.0, &flags, 1.0, false);
        assert!(placed2);
        assert!(
            items[1].y > first_y,
            "same-time items should be on different tracks: first_y={}, second_y={}",
            first_y,
            items[1].y
        );
    }

    #[test]
    fn test_non_overlapping_items_same_track() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items = vec![
            make_scroll_item(0, "early", 100.0, DanmakuType::ScrollRL, 3000, 1920.0),
            make_scroll_item(10000, "late", 100.0, DanmakuType::ScrollRL, 3000, 1920.0),
        ];

        retainer.fix(&mut items[0], 1920.0, 1080.0, &flags, 1.0, false);
        let first_y = items[0].y;

        retainer.fix(&mut items[1], 1920.0, 1080.0, &flags, 1.0, false);
        assert!(
            (items[1].y - first_y).abs() < 1.0,
            "non-overlapping items should share track: first_y={}, second_y={}",
            first_y,
            items[1].y
        );
    }

    #[test]
    fn test_fixed_items_separate_tracks() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items = vec![
            make_fixed_item(0, "top1", DanmakuType::FixTop, 3800),
            make_fixed_item(0, "top2", DanmakuType::FixTop, 3800),
        ];

        retainer.fix(&mut items[0], 1920.0, 1080.0, &flags, 1.0, false);
        let first_y = items[0].y;

        retainer.fix(&mut items[1], 1920.0, 1080.0, &flags, 1.0, false);
        assert!(
            items[1].y > first_y,
            "same-time fixed items should be on different tracks"
        );
    }

    #[test]
    fn test_fixed_expired_item_replaced() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items = vec![
            make_fixed_item(0, "first", DanmakuType::FixTop, 3800),
            make_fixed_item(5000, "second", DanmakuType::FixTop, 3800),
        ];

        retainer.fix(&mut items[0], 1920.0, 1080.0, &flags, 1.0, false);
        let first_y = items[0].y;

        retainer.fix(&mut items[1], 1920.0, 1080.0, &flags, 1.0, false);
        assert!(
            (items[1].y - first_y).abs() < 1.0,
            "expired fixed item should be replaced: first_y={}, second_y={}",
            first_y,
            items[1].y
        );
    }

    #[test]
    fn test_scroll_collision_same_time() {
        let d1 = TrackEntry {
            time_ms: 0,
            duration_ms: 5000,
            paint_width: 100.0,
            step_x: calc_step_x(100.0, 5000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 0,
        };
        let d2 = TrackEntry {
            time_ms: 0,
            duration_ms: 5000,
            paint_width: 100.0,
            step_x: calc_step_x(100.0, 5000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 1,
        };
        assert!(scroll_entries_collide(&d1, &d2, 1920.0));
    }

    #[test]
    fn test_scroll_no_collision_far_apart() {
        let d1 = TrackEntry {
            time_ms: 0,
            duration_ms: 3000,
            paint_width: 100.0,
            step_x: calc_step_x(100.0, 3000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 0,
        };
        let d2 = TrackEntry {
            time_ms: 10000,
            duration_ms: 3000,
            paint_width: 100.0,
            step_x: calc_step_x(100.0, 3000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 1,
        };
        assert!(!scroll_entries_collide(&d1, &d2, 1920.0));
    }

    #[test]
    fn test_scroll_x_position() {
        let entry = TrackEntry {
            time_ms: 0,
            duration_ms: 5000,
            paint_width: 100.0,
            step_x: calc_step_x(100.0, 5000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 0,
        };
        let x0 = entry_x_at(&entry, 0, 1920.0);
        assert!((x0 - 1920.0).abs() < 1.0);
        let x5 = entry_x_at(&entry, 5000, 1920.0);
        assert!((x5 - (-100.0)).abs() < 1.0);
    }

    #[test]
    fn test_overflow_queues_item() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items = vec![
            make_fixed_item(0, "a", DanmakuType::FixTop, 3800),
            make_fixed_item(0, "b", DanmakuType::FixTop, 3800),
        ];

        // Only one track fits (view_height=60, track_height=45, track_count=1).
        let (placed0, _) = retainer.fix(&mut items[0], 1920.0, 60.0, &flags, 1.0, false);
        assert!(placed0, "first item should be placed in the only track");

        // Second item: track still occupied → dropped (Next2 behavior).
        let (placed1, _) = retainer.fix(&mut items[1], 1920.0, 60.0, &flags, 1.0, false);
        assert!(
            !placed1,
            "second item should be dropped when all tracks are full"
        );
    }

    #[test]
    fn test_different_width_same_time_different_tracks() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items = vec![
            make_scroll_item(0, "wide", 500.0, DanmakuType::ScrollRL, 5000, 1920.0),
            make_scroll_item(0, "narrow", 28.0, DanmakuType::ScrollRL, 5000, 1920.0),
        ];

        retainer.fix(&mut items[0], 1920.0, 1080.0, &flags, 1.0, false);
        retainer.fix(&mut items[1], 1920.0, 1080.0, &flags, 1.0, false);
        assert!(
            (items[0].y - items[1].y).abs() > 1.0,
            "different-width same-time items must be on different tracks: wide_y={}, narrow_y={}",
            items[0].y,
            items[1].y
        );
    }

    #[test]
    fn test_many_same_time_no_y_overlap() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);

        let texts: Vec<String> = (0..15).map(|i| format!("弹幕{}", i)).collect();
        let mut items: Vec<DanmakuItem> = texts
            .iter()
            .map(|text| make_scroll_item(1000, text, 150.0, DanmakuType::ScrollRL, 5000, 1920.0))
            .collect();

        let mut placed_ys = Vec::new();
        for item in &mut items {
            let (placed, _) = retainer.fix(item, 1920.0, 1080.0, &flags, 1.0, false);
            if placed {
                placed_ys.push(item.y);
            }
        }

        for i in 0..placed_ys.len() {
            for j in (i + 1)..placed_ys.len() {
                assert!(
                    (placed_ys[i] - placed_ys[j]).abs() > 1.0,
                    "items {} and {} share y={}",
                    i,
                    j,
                    placed_ys[i]
                );
            }
        }
    }

    #[test]
    fn test_scroll_overwrite_never_displaces_stable_zone() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(0.0, 0.0);
        let view_width = 640.0;
        let view_height = 150.0; // Five 30px tracks: two stable, three overflow.
        let mut displaced = SmallVec::<[usize; 4]>::new();
        let mut replacement_y = 0.0;

        for index in 0..6 {
            let mut item = make_scroll_item(
                0,
                &format!("dense-{index}"),
                160.0,
                DanmakuType::ScrollRL,
                5000,
                view_width,
            );
            item.index = index;
            let (placed, next_displaced) = retainer.fix_with_options(
                &mut item,
                view_width,
                view_height,
                &flags,
                1.0,
                false,
                false,
                true,
            );
            assert!(placed);
            if index == 5 {
                displaced = next_displaced;
                replacement_y = item.y;
            }
        }

        assert!(!displaced.is_empty());
        assert!(
            displaced.iter().all(|index| *index >= 2),
            "stable-zone entries must never be overwrite victims: {displaced:?}"
        );
        assert!(
            replacement_y >= 60.0,
            "replacement must stay in the lower overflow zone, got y={replacement_y}"
        );
    }

    #[test]
    fn test_chain_queue_fixed_items() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items: Vec<DanmakuItem> = (0..4)
            .map(|i| {
                let mut item = make_fixed_item(0, &format!("top{}", i), DanmakuType::FixTop, 3800);
                item.index = i as u32;
                item
            })
            .collect();

        // Only one track fits (view_height=60, track_height=45).
        // First item gets placed; subsequent items are dropped (Next2 behavior).
        for i in 0..items.len() {
            items[i].index = i as u32;
            let (placed, _) = retainer.fix(&mut items[i], 1920.0, 60.0, &flags, 1.0, false);
            if i == 0 {
                assert!(placed, "item 0 should be placed (first on empty track)");
                assert_eq!(items[0].time_ms, 0);
            } else {
                assert!(
                    !placed,
                    "item {} should be dropped when all tracks are full",
                    i
                );
            }
        }
    }

    #[test]
    fn test_staggered_items_tracks() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);

        let mut items: Vec<DanmakuItem> = (0..10)
            .map(|i| {
                make_scroll_item(
                    i * 100,
                    &format!("item_{}", i),
                    150.0,
                    DanmakuType::ScrollRL,
                    5000,
                    1920.0,
                )
            })
            .collect();

        for item in &mut items {
            retainer.fix(item, 1920.0, 1080.0, &flags, 1.0, false);
        }

        for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                let time_diff = (items[j].time_ms - items[i].time_ms).abs();
                if time_diff < 5000 {
                    let y_diff = (items[i].y - items[j].y).abs();
                    if y_diff < 1.0 && items[i].y >= 0.0 && items[j].y >= 0.0 {
                        println!(
                            "Same track: item_{} (t={}) and item_{} (t={}), y={}",
                            i, items[i].time_ms, j, items[j].time_ms, items[i].y
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_fixed_top_overlap_no_visual_overlap() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items: Vec<DanmakuItem> = (0..5)
            .map(|i| make_fixed_item(0, &format!("top{}", i), DanmakuType::FixTop, 3800))
            .collect();

        for i in 0..items.len() {
            let (_, displaced) = retainer.fix(&mut items[i], 1920.0, 1080.0, &flags, 1.0, false);
            for &d in &displaced {
                items[d].is_filtered = true;
                items[d].filter_param = 99;
            }
        }

        let mut visible_ys = Vec::new();
        for item in &items {
            if item.is_shown && !item.is_filtered {
                visible_ys.push(item.y);
            }
        }

        for i in 0..visible_ys.len() {
            for j in (i + 1)..visible_ys.len() {
                assert!(
                    (visible_ys[i] - visible_ys[j]).abs() > 1.0,
                    "visible items {} and {} share y={}, causing visual overlap",
                    i,
                    j,
                    visible_ys[i]
                );
            }
        }
    }

    #[test]
    fn test_fix_bottom_overflow_queues_correctly() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.5);
        let mut items: Vec<DanmakuItem> = (0..4)
            .map(|i| {
                let mut item =
                    make_fixed_item(0, &format!("bottom{}", i), DanmakuType::FixBottom, 3800);
                item.index = i as u32;
                item
            })
            .collect();

        // Only one track fits (view_height=60, track_height=45).
        // First item gets placed in the only track; rest are dropped.
        for i in 0..items.len() {
            items[i].index = i as u32;
            let (placed, _) = retainer.fix(&mut items[i], 1920.0, 60.0, &flags, 1.0, false);
            if i == 0 {
                assert!(placed, "item 0 should be placed");
                assert_eq!(items[0].time_ms, 0);
            } else {
                assert!(
                    !placed,
                    "item {} should be dropped when all tracks are full",
                    i
                );
            }
        }
    }

    #[test]
    fn test_long_danmaku_catches_short() {
        let short = TrackEntry {
            time_ms: 0,
            duration_ms: 8000,
            paint_width: 50.0,
            step_x: calc_step_x(50.0, 8000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 0,
        };
        let long = TrackEntry {
            time_ms: 1000,
            duration_ms: 8000,
            paint_width: 400.0,
            step_x: calc_step_x(400.0, 8000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 1,
        };
        assert!(
            scroll_entries_collide(&short, &long, 1920.0),
            "long danmaku starting later should catch up to short danmaku on same track"
        );
    }

    #[test]
    fn test_no_false_positive_at_start() {
        let short = TrackEntry {
            time_ms: 0,
            duration_ms: 8000,
            paint_width: 50.0,
            step_x: calc_step_x(50.0, 8000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 0,
        };
        let long = TrackEntry {
            time_ms: 1000,
            duration_ms: 8000,
            paint_width: 400.0,
            step_x: calc_step_x(400.0, 8000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 1,
        };
        let short_left = entry_left_at(&short, 1000, 1920.0);
        let short_right = short_left + short.paint_width;
        let long_left = entry_left_at(&long, 1000, 1920.0);
        assert!(
            long_left >= short_right,
            "at d2 start: long.left={} should be >= short.right={} (no overlap yet)",
            long_left,
            short_right
        );
    }

    #[test]
    fn test_catch_up_at_end() {
        let short = TrackEntry {
            time_ms: 0,
            duration_ms: 8000,
            paint_width: 50.0,
            step_x: calc_step_x(50.0, 8000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 0,
        };
        let long = TrackEntry {
            time_ms: 1000,
            duration_ms: 8000,
            paint_width: 400.0,
            step_x: calc_step_x(400.0, 8000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 1,
        };
        let short_left = entry_left_at(&short, 8000, 1920.0);
        let short_right = short_left + short.paint_width;
        let long_left = entry_left_at(&long, 8000, 1920.0);
        assert!(
            long_left < short_right,
            "at d1 end: long.left={} should be < short.right={} (catch-up happened)",
            long_left,
            short_right
        );
    }

    #[test]
    fn test_no_collision_when_far_apart_in_time() {
        let short = TrackEntry {
            time_ms: 0,
            duration_ms: 8000,
            paint_width: 50.0,
            step_x: calc_step_x(50.0, 8000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 0,
        };
        let long = TrackEntry {
            time_ms: 7000,
            duration_ms: 8000,
            paint_width: 400.0,
            step_x: calc_step_x(400.0, 8000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 1,
        };
        assert!(
            !scroll_entries_collide(&short, &long, 1920.0),
            "long danmaku starting 7s later should not catch short (short ends at 8s, only 1s overlap window not enough)"
        );
    }

    #[test]
    fn test_long_short_different_tracks() {
        let flags = GlobalFlags::default();
        let mut retainer = DanmakuRetainer::new(2.0, 0.15);
        let mut short_item = make_scroll_item(0, "短", 50.0, DanmakuType::ScrollRL, 8000, 1920.0);
        let mut long_item = make_scroll_item(
            1000,
            "很长很长的弹幕内容在这里",
            400.0,
            DanmakuType::ScrollRL,
            8000,
            1920.0,
        );

        retainer.fix(&mut short_item, 1920.0, 1080.0, &flags, 1.0, false);
        retainer.fix(&mut long_item, 1920.0, 1080.0, &flags, 1.0, false);

        assert!(
            (short_item.y - long_item.y).abs() > 1.0,
            "long danmaku (y={}) should be on different track from short (y={}) to avoid catch-up",
            long_item.y,
            short_item.y
        );
    }

    #[test]
    fn test_check_hit_direction_rtl() {
        let d1 = TrackEntry {
            time_ms: 0,
            duration_ms: 5000,
            paint_width: 100.0,
            step_x: calc_step_x(100.0, 5000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 0,
        };
        let d2 = TrackEntry {
            time_ms: 500,
            duration_ms: 5000,
            paint_width: 100.0,
            step_x: calc_step_x(100.0, 5000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 1,
        };

        assert!(
            !scroll_entries_collide(&d1, &d2, 1920.0),
            "same-speed danmaku 500ms apart should NOT collide (they maintain distance)"
        );

        let d1_fast = TrackEntry {
            time_ms: 0,
            duration_ms: 8000,
            paint_width: 50.0,
            step_x: calc_step_x(50.0, 8000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 0,
        };
        let d2_slow = TrackEntry {
            time_ms: 1000,
            duration_ms: 8000,
            paint_width: 400.0,
            step_x: calc_step_x(400.0, 8000, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 1,
        };

        let t_start = 1000;
        let d1_right_at_start = entry_left_at(&d1_fast, t_start, 1920.0) + d1_fast.paint_width;
        let d2_left_at_start = entry_left_at(&d2_slow, t_start, 1920.0);
        assert!(
            d2_left_at_start >= d1_right_at_start,
            "at d2 start: d2.left={} should be >= d1.right={} (no overlap yet, d2 just entered)",
            d2_left_at_start,
            d1_right_at_start
        );

        let t_end = 8000;
        let d1_right_at_end = entry_left_at(&d1_fast, t_end, 1920.0) + d1_fast.paint_width;
        let d2_left_at_end = entry_left_at(&d2_slow, t_end, 1920.0);
        assert!(
            d2_left_at_end < d1_right_at_end,
            "at d1 end: d2.left={} should be < d1.right={} (long danmaku caught up!)",
            d2_left_at_end,
            d1_right_at_end
        );

        assert!(
            scroll_entries_collide(&d1_fast, &d2_slow, 1920.0),
            "long fast danmaku should collide with short slow danmaku"
        );
    }

    /// `compact` 的两条路径（升序时的队首前缀弹出 / 顺序不可信时的全量过滤）
    /// 必须给出同一个结果集：优化只允许改成本，不允许改行为。
    #[test]
    fn compact_fast_path_matches_full_filter() {
        let entry = |time_ms: i64, duration_ms: i64| TrackEntry {
            time_ms,
            duration_ms,
            paint_width: 100.0,
            step_x: calc_step_x(100.0, duration_ms, 1920.0),
            danmaku_type: DanmakuType::ScrollRL,
            danmaku_index: 0,
        };

        // 同一 duration（`dfm.rs` 的真实场景）：结束时间随发射时间同步递增
        let mut sorted = TrackData::new();
        sorted.ensure_track_count(2);
        for (i, t) in [0i64, 1000, 2000, 4000].into_iter().enumerate() {
            sorted.push_entry(i % 2, entry(t, 3000));
        }
        assert!(sorted.end_ms_sorted, "同一 duration 的轨道必须判定为有序");
        let mut sorted_counters = PlacementCounters::default();
        sorted.compact(5000, 3000, &mut sorted_counters);
        // 轨道 0：t=0/2000 的结束时间 3000/5000 都已到期 → 整条清空
        assert!(sorted.tracks[0].is_empty());
        // 轨道 1：t=1000（结束 4000）过期，t=4000（结束 7000）保留
        assert_eq!(sorted.tracks[1].len(), 1);
        assert_eq!(sorted.tracks[1][0].time_ms, 4000);
        // 计数器说的是真事：这轮清理只看了 3 个条目（弹出的前缀），弹出 3 个
        assert_eq!(sorted_counters.compact_scanned, 3);
        assert_eq!(sorted_counters.compact_popped, 3);

        // 同一轨道混入不同 duration：结束时间不再单调 → 必须退回全量过滤
        let mut mixed = TrackData::new();
        mixed.ensure_track_count(1);
        mixed.push_entry(0, entry(0, 8000)); // 结束 8000
        mixed.push_entry(0, entry(1000, 1000)); // 结束 2000，破坏了结束时间单调性
        assert!(!mixed.end_ms_sorted);
        let mut mixed_counters = PlacementCounters::default();
        mixed.compact(3000, 0, &mut mixed_counters);
        // 全量过滤按真实结束时间判断：t=1000（结束 2000）被清，t=0（结束 8000）留下
        assert_eq!(mixed.tracks[0].len(), 1);
        assert_eq!(mixed.tracks[0][0].time_ms, 0);
        // 兜底路径扫描整条轨道（2 条），只弹出 1 条
        assert_eq!(mixed_counters.compact_scanned, 2);
        assert_eq!(mixed_counters.compact_popped, 1);
    }

    /// 性能探针（不做断言，只是把排布成本打印出来）。
    ///
    /// 用 `cargo test -p erika --lib -- --nocapture probe_placement_cost` **单跑**
    /// 这一个：进程级计数器是所有排布共用的，并行跑别的测试会污染它。
    #[test]
    fn probe_placement_cost() {
        run_probe("稀疏 120ms/条", 30_000, 120);
        run_probe("密集  20ms/条", 30_000, 20);
        run_probe("刷屏   8ms/条", 30_000, 8);
    }

    fn run_probe(label: &str, count: usize, interval_ms: i64) {
        let flags = GlobalFlags::default();
        let width = 1920.0f32;
        let duration_ms = 8000i64;

        let mut items: Vec<DanmakuItem> = (0..count)
            .map(|i| {
                let time_ms = i as i64 * interval_ms;
                let paint_width = 120.0 + (i % 7) as f32 * 40.0;
                make_scroll_item(
                    time_ms,
                    "测试弹幕",
                    paint_width,
                    DanmakuType::ScrollRL,
                    duration_ms,
                    width,
                )
            })
            .collect();

        let started = std::time::Instant::now();
        {
            let mut retainer = DanmakuRetainer::new(2.0, 0.15);
            for item in items.iter_mut() {
                retainer.fix(item, width, 1080.0, &flags, 1.0, false);
            }
        } // retainer 在这里析构 ⇒ 本轮读数已经汇总进进程级统计
        let wall = started.elapsed();
        let round = placement_report().last_round.expect("刚跑完一轮排布");
        println!(
            "probe[{label}]: 输入 {count} 条 / 放置 {} 条、丢弃 {} 条、覆盖 {} 条 / \
             排布耗时 {}ms（外部计时 {wall:?}） / 碰撞判定 {} 次 / \
             轨道清理扫描 {} 条目、弹出 {} 个",
            round.counters.placed,
            round.counters.dropped,
            round.counters.displaced,
            round.elapsed_ms,
            round.counters.collide_checks,
            round.counters.compact_scanned,
            round.counters.compact_popped,
        );
    }
}
