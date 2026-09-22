/// Danmaku filtering system.
/// Ported from DanmakuFilters.java with support for:
/// - Type blocking
/// - Quantity density control
/// - Elapsed time protection
/// - Maximum lines
/// - Duplicate merging
/// - Overlapping detection
/// - Keyword/user blocking
use rustc_hash::{FxHashMap, FxHashSet};

use crate::danmaku::dfm_core::model::{DanmakuItem, DanmakuType, Duration, GlobalFlags};
use crate::danmaku::rules::{BlockReason, DanmakuRuleSet, RuleError};

/// Context passed to filters during the rendering loop.
#[derive(Debug)]
pub struct FilterContext {
    pub timer_ms: i64,
    pub index_in_screen: usize,
    pub screen_size: usize,
    pub frame_elapsed_ms: u64,
    pub global_flags: GlobalFlags,
    pub scroll_duration: Duration,
}

/// Secondary filter state from the retainer.
#[derive(Debug, Clone)]
pub struct RetainerState {
    pub will_hit: bool,
    pub line_number: u32,
}

/// `filter_param` 诊断码：命中屏蔽时写进 `DanmakuItem::filter_param`，供统计浮窗
/// 与测试区分「为什么这条被屏蔽了」。1~5 沿用 DFM+ 既有编号，6 起是本模块新增的
/// 规则维度（见 `crate::danmaku::rules`）。
pub mod filter_param {
    /// 类型屏蔽（`blocked_types` 字段，DFM+ 既有）
    pub const TYPE: u32 = 1;
    /// 同屏密度上限（DFM+ 既有）
    pub const QUANTITY: u32 = 2;
    /// 帧耗时保护（DFM+ 既有）
    pub const ELAPSED: u32 = 3;
    /// 关键词屏蔽（DFM+ 既有）
    pub const KEYWORD: u32 = 4;
    /// 正则屏蔽（从 DFM+ 的关键词码里拆出来，便于诊断行分别显示"关键词/正则"）
    pub const REGEX: u32 = 12;
    /// 重复合并（DFM+ 既有）
    pub const DUPLICATE: u32 = 5;
    /// 用户标识屏蔽（`<标识>` 规则）
    pub const USER: u32 = 6;
    /// 用户名屏蔽（`[用户名]` 规则）
    pub const NAME: u32 = 7;
    /// 用户等级屏蔽（`level:` 规则）
    pub const LEVEL: u32 = 8;
    /// 文本长度屏蔽（`len:` 规则）
    pub const TEXT_LENGTH: u32 = 9;
    /// 时间区间屏蔽（`time:` 规则）
    pub const TIME_RANGE: u32 = 10;
    /// 彩色弹幕屏蔽（`type:color` 规则）
    pub const COLOR: u32 = 11;
    /// 轨道分配失败 / 超出最大行数（DFM+ 既有留白码）
    pub const OVERFLOW: u32 = 99;
}

/// 规则命中原因 → `filter_param` 诊断码。
fn param_for_reason(reason: BlockReason) -> u32 {
    match reason {
        BlockReason::Type => filter_param::TYPE,
        BlockReason::Color => filter_param::COLOR,
        BlockReason::Keyword => filter_param::KEYWORD,
        BlockReason::Regex => filter_param::REGEX,
        BlockReason::User => filter_param::USER,
        BlockReason::Name => filter_param::NAME,
        BlockReason::Level => filter_param::LEVEL,
        BlockReason::TextLength => filter_param::TEXT_LENGTH,
        BlockReason::TimeRange => filter_param::TIME_RANGE,
    }
}

/// Complete filter system.
#[derive(Debug)]
pub struct FilterSystem {
    pub blocked_types: FxHashSet<DanmakuType>,
    pub max_quantity: Option<u32>,
    pub max_lines: FxHashMap<DanmakuType, u32>,
    pub overlapping_filter: FxHashMap<DanmakuType, bool>,
    /// 结构化屏蔽规则（关键词 / 正则 / 类型 / 颜色 / 长度 / 时间 / 等级 / 用户名）。
    /// 判定入口是 [`DanmakuRuleSet::matches`]，规则集为空时零开销短路。
    pub rules: DanmakuRuleSet,
    /// 最近一次 [`FilterSystem::set_block_words`] 的编译诊断。
    ///
    /// 与「写错了就悄悄不生效」相对：写错的正则/类型名会留在这里，调用方可以
    /// 打日志或回显给用户。DanmakuFactory 的对应缺陷是编译失败后
    /// `regCodes[i]` 与 `keyStrings[i]` 索引错位，更难查。
    pub rule_errors: Vec<RuleError>,
    pub duplicate_merge: bool,
    pub elapsed_time_limit_ms: u64,
    last_skipped_time: Option<i64>,
    current_duplicates: FxHashMap<String, i64>,
    blocked_duplicates: FxHashSet<String>,
    passed_duplicates: FxHashSet<String>,
}

impl Default for FilterSystem {
    fn default() -> Self {
        Self {
            blocked_types: FxHashSet::default(),
            max_quantity: None,
            max_lines: FxHashMap::default(),
            overlapping_filter: FxHashMap::default(),
            rules: DanmakuRuleSet::default(),
            rule_errors: Vec::new(),
            duplicate_merge: false,
            elapsed_time_limit_ms: 20,
            last_skipped_time: None,
            current_duplicates: FxHashMap::default(),
            blocked_duplicates: FxHashSet::default(),
            passed_duplicates: FxHashSet::default(),
        }
    }
}

impl FilterSystem {
    /// Primary filter: runs before layout during the main rendering loop.
    /// Returns true if the danmaku should be filtered (hidden).
    /// Ported from DanmakuFilters.filter().
    pub fn filter_primary(&mut self, item: &mut DanmakuItem, ctx: &FilterContext) -> bool {
        // Type filter
        if self.blocked_types.contains(&item.danmaku_type) {
            item.is_filtered = true;
            item.filter_param = filter_param::TYPE;
            item.flags.filter = ctx.global_flags.filter_flag;
            return true;
        }

        // Quantity density filter (only for scroll types)
        if item.danmaku_type.is_scroll() {
            if self.filter_quantity(item, ctx) {
                item.is_filtered = true;
                item.filter_param = filter_param::QUANTITY;
                item.flags.filter = ctx.global_flags.filter_flag;
                return true;
            }
        }

        // Elapsed time filter (performance protection)
        if ctx.frame_elapsed_ms >= self.elapsed_time_limit_ms
            && item.is_outside(ctx.timer_ms, &ctx.global_flags)
        {
            item.is_filtered = true;
            item.filter_param = filter_param::ELAPSED;
            item.flags.filter = ctx.global_flags.filter_flag;
            return true;
        }

        // 结构化屏蔽规则（关键词 / 正则 / 类型 / 颜色 / 长度 / 时间 / 用户 / 等级）。
        // 规则集为空时 `matches` 在第一行就返回 None，不进任何自动机。
        if let Some(reason) = self
            .rules
            .matches(item, item.get_actual_time(&ctx.global_flags))
        {
            item.is_filtered = true;
            item.filter_param = param_for_reason(reason);
            item.flags.filter = ctx.global_flags.filter_flag;
            return true;
        }

        // Duplicate merging filter
        if self.duplicate_merge && self.filter_duplicate(item, ctx) {
            item.is_filtered = true;
            item.filter_param = filter_param::DUPLICATE;
            item.flags.filter = ctx.global_flags.filter_flag;
            return true;
        }

        item.is_filtered = false;
        item.filter_param = 0;
        false
    }

    /// Secondary filter: runs after collision avoidance.
    /// Returns true if the danmaku should be filtered.
    /// Ported from DanmakuFilters.filterSecondary().
    pub fn filter_secondary(
        &self,
        item: &mut DanmakuItem,
        state: &RetainerState,
        _flags: &GlobalFlags,
    ) -> bool {
        // Maximum lines filter
        if let Some(&max) = self.max_lines.get(&item.danmaku_type) {
            if state.line_number >= max {
                return true;
            }
        }

        // Overlapping filter
        if let Some(&enabled) = self.overlapping_filter.get(&item.danmaku_type) {
            if enabled && state.will_hit {
                return true;
            }
        }

        false
    }

    /// Quantity density filter.
    /// Ported from QuantityDanmakuFilter.needFilter().
    fn filter_quantity(&mut self, item: &DanmakuItem, ctx: &FilterContext) -> bool {
        let Some(max_size) = self.max_quantity else {
            return false;
        };

        if max_size == 0 {
            return false;
        }

        let filter_factor = 1.0 / (max_size as f64 + max_size as f64 / 5.0);

        if let Some(last_time) = self.last_skipped_time {
            let gap = item.get_actual_time(&ctx.global_flags) - last_time;
            let max_duration = ctx.scroll_duration.value;
            if gap >= 0 && (gap as f64) < (max_duration as f64 * filter_factor) {
                return true;
            }
        }

        if ctx.index_in_screen as u32 > max_size + max_size / 5 {
            return true;
        }

        self.last_skipped_time = Some(item.get_actual_time(&ctx.global_flags));
        false
    }

    /// Duplicate merging filter.
    /// Ported from DuplicateMergingFilter.needFilter().
    fn filter_duplicate(&mut self, item: &DanmakuItem, ctx: &FilterContext) -> bool {
        let text = &item.text;

        if self.current_duplicates.len() > 128 {
            self.current_duplicates
                .retain(|_, &mut time| ctx.timer_ms - time < 10000);
        }

        if self.blocked_duplicates.contains(text) {
            return true;
        }

        if self.passed_duplicates.contains(text) {
            return false;
        }

        if self.current_duplicates.contains_key(text) {
            self.blocked_duplicates.insert(text.clone());
            return true;
        }

        self.current_duplicates
            .insert(text.clone(), item.get_actual_time(&ctx.global_flags));
        self.passed_duplicates.insert(text.clone());
        false
    }

    /// Reset filter state (e.g., on seek).
    pub fn reset(&mut self) {
        self.last_skipped_time = None;
        self.current_duplicates.clear();
        self.blocked_duplicates.clear();
        self.passed_duplicates.clear();
    }

    /// 用屏蔽词表重建规则集。
    ///
    /// 规则文本语法见 `crate::danmaku::rules`（兼容既有的 `规则名称/表达式/`
    /// 正则写法，以及 DanmakuFactory `--blacklist` 的 `<uid>` / `[用户名]` 约定）。
    /// 编译诊断留在 [`FilterSystem::rule_errors`]，不会静默丢弃写错的规则。
    ///
    /// 所有维度（含 `<标识>` / `[用户名]` 的用户级屏蔽）都由规则集自己在
    /// [`DanmakuRuleSet::matches`] 里判定，`FilterSystem` 不再保存第二份
    /// 用户集合——曾经那个 `blocked_users` 字段**写入但从不被读取**，白付一次
    /// 每次改配置的 `HashSet` 克隆，还会与规则集形成两份真相。
    ///
    /// `type:repeat` 规则会顺带打开 [`FilterSystem::duplicate_merge`]：同屏重复
    /// 屏蔽在本内核里是由重复合并过滤器执行的，规则集本身不做这件事。
    pub fn set_block_words(&mut self, words: &[String]) {
        let (rules, errors) = DanmakuRuleSet::parse(words);
        if rules.wants_repeat_filter() {
            self.duplicate_merge = true;
        }
        self.rules = rules;
        self.rule_errors = errors;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::danmaku::dfm_core::model::Duration;

    fn make_ctx(timer_ms: i64) -> FilterContext {
        FilterContext {
            timer_ms,
            index_in_screen: 0,
            screen_size: 10,
            frame_elapsed_ms: 0,
            global_flags: GlobalFlags::default(),
            scroll_duration: Duration::new(5000),
        }
    }

    #[test]
    fn test_type_filter() {
        let mut filters = FilterSystem::default();
        filters.blocked_types.insert(DanmakuType::FixTop);
        let ctx = make_ctx(0);
        let mut item = DanmakuItem::new(
            0,
            "test".into(),
            0xFFFFFFFF,
            25.0,
            DanmakuType::FixTop,
            3800,
        );
        assert!(filters.filter_primary(&mut item, &ctx));
    }

    #[test]
    fn test_keyword_filter() {
        let mut filters = FilterSystem::default();
        filters.set_block_words(&["bad".to_string()]);
        let ctx = make_ctx(0);
        let mut item = DanmakuItem::new(
            0,
            "this is bad content".into(),
            0xFFFFFFFF,
            25.0,
            DanmakuType::ScrollRL,
            5000,
        );
        assert!(filters.filter_primary(&mut item, &ctx));
    }

    #[test]
    fn test_elapsed_time_filter() {
        let mut filters = FilterSystem::default();
        filters.elapsed_time_limit_ms = 20;
        let mut ctx = make_ctx(0);
        ctx.frame_elapsed_ms = 25; // exceeded limit
        let mut item = DanmakuItem::new(
            10000,
            "future".into(),
            0xFFFFFFFF,
            25.0,
            DanmakuType::ScrollRL,
            5000,
        );
        // item.is_outside(0) = true (time 10000 > 0, dtime > 0 and dtime < duration → not outside)
        // Actually is_outside checks dtime <= 0 || dtime >= duration
        // dtime = 0 - 10000 = -10000 <= 0 → outside
        assert!(filters.filter_primary(&mut item, &ctx));
    }
}
