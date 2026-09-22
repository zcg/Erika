//! 弹幕屏蔽规则引擎。
//!
//! `FilterSystem` 原先只接受一个 `Vec<String>` 屏蔽词表，并且其中唯一被真正实现
//! 的维度是「关键词」和「`名称/表达式/` 正则」，用户维度只是一个从未被任何判定
//! 读取的死字段。本模块把它补成一套**结构化规则集**：用户维度的判定由本模块自己
//! 的集合承担（`FilterSystem` 上那个同名、写入后从不被读取的字段已随之删除——
//! 留两份等于两份真相），并保持对既有输入格式完全兼容。
//!
//! # 规则文本语法
//!
//! 每条规则是一个字符串，按下面的顺序识别（先匹配到的形态生效）：
//!
//! | 写法 | 含义 | 依据 |
//! | --- | --- | --- |
//! | `re:表达式` | 正则屏蔽（Rust `regex` 语法） | 本模块扩展 |
//! | `名称/表达式/` | 正则屏蔽，等价 `re:表达式`；`名称` 只作人读说明 | Erika 既有格式 |
//! | `ci:文本` | 关键词，ASCII 大小写不敏感 | 本模块扩展 |
//! | `<标识>` | 屏蔽用户：匹配来源提供的用户标识（B 站 XML 的 `user_hash`，录播姬 XML 的 uid） | DanmakuFactory `--blacklist` 的 `<>` 约定 |
//! | `[用户名]` | 屏蔽用户：用户名**子串**匹配 | DanmakuFactory `--blacklist` 的 `[]` 约定 |
//! | `type:名称` | 按类型屏蔽，见下 | DanmakuFactory `-b/--blockmode` |
//! | `len:>N` `len:<N` `len:N-M` | 按文本字符数屏蔽 | DanmakuFactory 无此能力，本模块扩展 |
//! | `time:60-120` `time:>60` `time:<10` | 按时间区间屏蔽（秒，闭区间） | DanmakuFactory 无此能力，本模块扩展 |
//! | `level:<N` `level:>N` | 按用户等级屏蔽（仅来源提供等级时生效） | DanmakuFactory 无此能力，本模块扩展 |
//! | 其它任意文本 | 关键词，大小写敏感的**子串**匹配 | 与旧行为一致 |
//!
//! `type:名称` 支持的类型名（不分大小写）：
//!
//! - `scroll` / `r2l` / `1` —— 普通滚动（右向左）
//! - `reverse` / `l2r` / `6` —— 逆向滚动（左向右）
//! - `top` / `5` —— 顶部固定
//! - `bottom` / `4` —— 底部固定
//! - `special` / `advanced` / `7` —— 高级弹幕（同时覆盖代码弹幕 mode=8 与 BAS 弹幕 mode=9）
//! - `color` —— 彩色弹幕（非白色）
//! - `repeat` —— 同屏重复弹幕（等价于打开重复合并过滤，见 `FilterSystem::duplicate_merge`）
//!
//! # 与 DanmakuFactory 的差异（都是**刻意**的）
//!
//! - 关键词走 Aho-Corasick 自动机一次扫描，而不是 DanmakuFactory 的
//!   「每条弹幕 × 每个词 `strstr`」（`src/List/DanmakuFactoryList.c:283-288`）。
//! - 正则走 `RegexSet`：全部表达式在同一次扫描里判定，而不是每个 pattern
//!   每条弹幕新建一次 `pcre2_match_data`。
//! - 正则编译失败会作为 [`RuleError`] 上报，而不是像 DanmakuFactory 那样把失败
//!   的 pattern 跳过、导致 `regCodes[i]` 与 `keyStrings[i]` 索引错位
//!   （`src/List/DanmakuFactoryList.c:213-228` vs `:283`）。
//! - `<标识>` 用字符串比较，不做数值转换——DanmakuFactory 用 `atoi` 把 uid 截成
//!   `int` 再与 `unsigned long long` 比（`src/List/DanmakuFactoryList.c:336`），
//!   大 uid 会被截断而永远匹配不上。

use aho_corasick::AhoCorasick;
use regex::RegexSet;
use rustc_hash::FxHashSet;

use crate::danmaku::dfm_core::model::{DanmakuItem, DanmakuType};

/// 规则编译诊断。
///
/// 与「编译不出就悄悄丢掉」相对：调用方能看到哪条规则没生效，以及原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleError {
    /// 正则表达式编译失败（`spec` 是原始规则文本，`message` 是编译器报错）。
    InvalidRegex { spec: String, message: String },
    /// `type:` 后面的类型名不认识。
    UnknownType { spec: String, name: String },
    /// `len:` / `time:` / `level:` 后面的数值（区间）写法非法。
    InvalidNumber { spec: String },
    /// 规则文本为空（只有空白）。
    Empty,
}

impl std::fmt::Display for RuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRegex { spec, message } => {
                write!(f, "正则规则编译失败：{spec:?}（{message}）")
            }
            Self::UnknownType { spec, name } => {
                write!(f, "类型屏蔽规则 {spec:?} 的类型名不认识：{name:?}")
            }
            Self::InvalidNumber { spec } => write!(f, "规则的数值写法非法：{spec:?}"),
            Self::Empty => write!(f, "规则文本为空"),
        }
    }
}

impl std::error::Error for RuleError {}

/// 命中屏蔽的原因。用于 `FilterSystem` 的 `filter_param` 诊断码。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockReason {
    /// 类型屏蔽（`type:`）
    Type,
    /// 彩色弹幕屏蔽（`type:color`）
    Color,
    /// 关键词屏蔽（纯文本 / `ci:`）
    Keyword,
    /// 正则屏蔽（`re:` / `名称/表达式/`）
    Regex,
    /// 用户标识屏蔽（`<标识>`）
    User,
    /// 用户名屏蔽（`[用户名]`）
    Name,
    /// 用户等级屏蔽（`level:`）
    Level,
    /// 文本长度屏蔽（`len:`）
    TextLength,
    /// 时间区间屏蔽（`time:`）
    TimeRange,
}

/// 一条「数值屏蔽区间」。
///
/// `text_len` / `level` / `time` 三类规则共用。`min` / `max` 各自可缺省
/// （缺省 = 该侧无界），边界默认是**闭**的；`exclusive_min` / `exclusive_max`
/// 标记 `len:>N` / `len:<N` 这种严格不等号（`>=` / `<=` 与 `N-M` 形式两端都是闭的）。
///
/// 一条规则一个区间、同类多条规则各存一条，而不是把同类规则并成一个区间：
/// 并集要把边界与开闭一起合并，很容易在"一端相等、开闭不同"时算错；分开存
/// 之后判定就是 `iter().any(blocks)`，规则条数本来就是个位数。
#[derive(Debug, Clone, Copy, PartialEq)]
struct Bound {
    min: Option<f64>,
    max: Option<f64>,
    exclusive_min: bool,
    exclusive_max: bool,
}

impl Bound {
    /// 值落在屏蔽区间内（= 这条规则要求屏蔽它）。
    ///
    /// 关键：**只有落在区间内才屏蔽**。判据返回 `false` 的两种情况是"值在区间外"
    /// 和"这条规则两端都不设限"——后者不该出现（`parse_bounds` 会拒绝空区间），
    /// 这里再兜一次，避免一条空规则把全部弹幕都判掉。
    fn blocks(&self, value: f64) -> bool {
        if let Some(min) = self.min {
            if value < min || (self.exclusive_min && value == min) {
                return false;
            }
        }
        if let Some(max) = self.max {
            if value > max || (self.exclusive_max && value == max) {
                return false;
            }
        }
        self.min.is_some() || self.max.is_some()
    }
}

/// 编译后的屏蔽规则集。
///
/// 构造入口是 [`DanmakuRuleSet::parse`]（带诊断）与 [`DanmakuRuleSet::parse_lossy`]
/// （丢掉诊断，给只需要行为、不关心报错的调用方）。规则集本身不可变，判定是
/// `&self`，因此可以被多条弹幕共享、也可以缓存起来复用。
#[derive(Debug, Default)]
pub struct DanmakuRuleSet {
    /// 类型屏蔽。`Special` 同时代表高级弹幕（mode 7）与代码/BAS 弹幕（mode 8/9）。
    blocked_types: FxHashSet<DanmakuType>,
    /// 是否屏蔽彩色弹幕（非白色）。DanmakuFactory 的 `BLK_COLOR`。
    block_color: bool,
    /// 是否屏蔽同屏重复弹幕。DanmakuFactory 的 `BLK_REPEAT`；本内核里落到
    /// `FilterSystem::duplicate_merge` 上，不由规则集自己判定。
    block_repeat: bool,
    /// 大小写敏感关键词自动机（纯文本规则）。
    keywords: Option<AhoCorasick>,
    /// ASCII 大小写不敏感关键词自动机（`ci:` 规则）。
    ///
    /// 单独一个自动机而不是把所有文本转小写：`AhoCorasick` 的
    /// `ascii_case_insensitive` 是**自动机级**开关，两种语义无法共用一个自动机；
    /// 而给每条弹幕做一次全文小写化会在热路径上产生一次分配，代价更大。
    keywords_ci: Option<AhoCorasick>,
    /// 全部正则一次扫描（`re:` 与 `名称/表达式/` 规则）。
    regexes: Option<RegexSet>,
    /// 被屏蔽的用户标识（`<标识>`），原样字符串比较。
    blocked_users: FxHashSet<String>,
    /// 被屏蔽的用户名（`[用户名]`），子串匹配；同样走自动机，避免
    /// DanmakuFactory 那种「每条弹幕 × 每个名字 `strstr`」的平方级开销。
    names: Option<AhoCorasick>,
    /// 用户等级区间（`level:`，同类多条规则各存一条）。
    level: Vec<Bound>,
    /// 文本字符数区间（`len:`）。
    text_len: Vec<Bound>,
    /// 时间区间（`time:`，单位秒）。
    time_range: Vec<Bound>,
    /// 生效的规则条数（用于诊断与缓存键）。
    rule_count: usize,
}

impl DanmakuRuleSet {
    /// 解析规则文本列表，返回规则集与全部编译诊断。
    ///
    /// 单条规则出错不会让整份规则失效：出错的规则被跳过并记录，其余规则照常生效。
    pub fn parse(spec: &[String]) -> (Self, Vec<RuleError>) {
        let mut rules = Self::default();
        let mut errors = Vec::new();
        let mut keywords = Vec::new();
        let mut keywords_ci = Vec::new();
        let mut names = Vec::new();
        let mut regex_patterns = Vec::new();

        for raw in spec {
            let text = raw.trim();
            if text.is_empty() {
                errors.push(RuleError::Empty);
                continue;
            }

            if let Some(rest) = strip_prefix_ci(text, "re:") {
                match Self::push_regex(&mut regex_patterns, raw, rest) {
                    Ok(()) => rules.rule_count += 1,
                    Err(error) => errors.push(error),
                }
            } else if let Some(rest) = strip_prefix_ci(text, "ci:") {
                if rest.is_empty() {
                    errors.push(RuleError::Empty);
                } else {
                    keywords_ci.push(rest.to_string());
                    rules.rule_count += 1;
                }
            } else if let Some(rest) = strip_prefix_ci(text, "type:") {
                match apply_type_rule(&mut rules, raw, rest) {
                    Ok(()) => rules.rule_count += 1,
                    Err(error) => errors.push(error),
                }
            } else if let Some(rest) = strip_prefix_ci(text, "len:") {
                match parse_bound(rest) {
                    Some(bound) => {
                        rules.text_len.push(bound);
                        rules.rule_count += 1;
                    }
                    None => errors.push(RuleError::InvalidNumber { spec: raw.clone() }),
                }
            } else if let Some(rest) = strip_prefix_ci(text, "time:") {
                match parse_bound(rest) {
                    Some(bound) => {
                        rules.time_range.push(bound);
                        rules.rule_count += 1;
                    }
                    None => errors.push(RuleError::InvalidNumber { spec: raw.clone() }),
                }
            } else if let Some(rest) = strip_prefix_ci(text, "level:") {
                match parse_bound(rest) {
                    Some(bound) => {
                        rules.level.push(bound);
                        rules.rule_count += 1;
                    }
                    None => errors.push(RuleError::InvalidNumber { spec: raw.clone() }),
                }
            } else if let Some(inner) = text.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
                let inner = inner.trim();
                if inner.is_empty() {
                    errors.push(RuleError::Empty);
                } else {
                    rules.blocked_users.insert(inner.to_string());
                    rules.rule_count += 1;
                }
            } else if let Some(inner) = text.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                let inner = inner.trim();
                if inner.is_empty() {
                    errors.push(RuleError::Empty);
                } else {
                    names.push(inner.to_string());
                    rules.rule_count += 1;
                }
            } else if let Some(pattern) = legacy_regex_pattern(text) {
                match Self::push_regex(&mut regex_patterns, raw, pattern) {
                    Ok(()) => rules.rule_count += 1,
                    Err(error) => errors.push(error),
                }
            } else {
                keywords.push(text.to_string());
                rules.rule_count += 1;
            }
        }

        rules.keywords = build_automaton(keywords, false);
        rules.keywords_ci = build_automaton(keywords_ci, true);
        rules.names = build_automaton(names, false);
        if !regex_patterns.is_empty() {
            match RegexSet::new(&regex_patterns) {
                Ok(set) => rules.regexes = Some(set),
                // 单个 pattern 已经用 `Regex::new` 验过，走到这里说明是 RegexSet
                // 自身的限制（例如表达式总数超过它的大小上限）。退化成逐条匹配
                // 会静默改变语义，所以这里报错而不是降级。
                Err(error) => errors.push(RuleError::InvalidRegex {
                    spec: regex_patterns.join(" | "),
                    message: format!("RegexSet 构建失败：{error}"),
                }),
            }
        }

        (rules, errors)
    }

    /// 解析规则文本列表并丢弃诊断（只需要行为时使用）。
    pub fn parse_lossy(spec: &[String]) -> Self {
        Self::parse(spec).0
    }

    /// 规则集里是否一条有效规则都没有。
    ///
    /// 判定入口用它做快速短路：默认配置（没有屏蔽规则）下每条弹幕只多一次
    /// 分支判断，不进任何自动机。
    pub fn is_empty(&self) -> bool {
        self.rule_count == 0
    }

    /// 是否需要打开「同屏重复弹幕」过滤（`type:repeat`）。
    pub fn wants_repeat_filter(&self) -> bool {
        self.block_repeat
    }

    /// 被屏蔽的用户标识集合（只读，供诊断/序列化）。
    pub fn blocked_users(&self) -> &FxHashSet<String> {
        &self.blocked_users
    }

    /// 判定一条弹幕是否应被屏蔽；命中时返回原因。
    ///
    /// `actual_time_ms` 是**已应用时间轴偏移**后、弹幕在视频时间轴上的落点（毫秒），
    /// `time:` 规则按它判定——用户说「屏蔽片头 90 秒」指的是视频时间轴，而不是
    /// 弹幕文件里的原始时间。
    ///
    /// 判定顺序按「便宜的先做」排列：类型/颜色/长度是常数时间比较，
    /// 关键词与正则各是一次自动机/DFA 扫描，用户相关判定放最后。
    pub fn matches(&self, item: &DanmakuItem, actual_time_ms: i64) -> Option<BlockReason> {
        if self.is_empty() {
            return None;
        }
        if self.blocked_types.contains(&item.danmaku_type) {
            return Some(BlockReason::Type);
        }
        if self.block_color && item.text_color & 0x00FF_FFFF != 0x00FF_FFFF {
            return Some(BlockReason::Color);
        }
        if !self.text_len.is_empty() {
            let length = item.text.chars().count() as f64;
            if self.text_len.iter().any(|bound| bound.blocks(length)) {
                return Some(BlockReason::TextLength);
            }
        }
        if !self.time_range.is_empty() {
            let seconds = actual_time_ms as f64 / 1000.0;
            if self.time_range.iter().any(|bound| bound.blocks(seconds)) {
                return Some(BlockReason::TimeRange);
            }
        }
        if let Some(automaton) = &self.keywords {
            if automaton.is_match(&item.text) {
                return Some(BlockReason::Keyword);
            }
        }
        if let Some(automaton) = &self.keywords_ci {
            if automaton.is_match(&item.text) {
                return Some(BlockReason::Keyword);
            }
        }
        if let Some(set) = &self.regexes {
            if set.is_match(&item.text) {
                return Some(BlockReason::Regex);
            }
        }
        if !self.blocked_users.is_empty() && !item.user_id.is_empty() {
            if self.blocked_users.contains(&item.user_id) {
                return Some(BlockReason::User);
            }
        }
        if let Some(automaton) = &self.names {
            if !item.user_name.is_empty() && automaton.is_match(&item.user_name) {
                return Some(BlockReason::Name);
            }
        }
        if !self.level.is_empty() && item.user_level > 0 {
            if self
                .level
                .iter()
                .any(|bound| bound.blocks(f64::from(item.user_level)))
            {
                return Some(BlockReason::Level);
            }
        }
        None
    }

    fn push_regex(patterns: &mut Vec<String>, spec: &str, pattern: &str) -> Result<(), RuleError> {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return Err(RuleError::InvalidRegex {
                spec: spec.to_string(),
                message: "表达式为空".to_string(),
            });
        }
        // 先逐条用 `Regex` 验证：`RegexSet::new` 是全有或全无的，一条写错会让
        // 整份正则规则一起失效；逐条验证后只丢掉写错的那一条。
        match regex::Regex::new(pattern) {
            Ok(_) => {
                patterns.push(pattern.to_string());
                Ok(())
            }
            Err(error) => Err(RuleError::InvalidRegex {
                spec: spec.to_string(),
                message: error.to_string(),
            }),
        }
    }
}

/// 大小写不敏感地剥掉规则前缀（`type:` 与 `Type:` 等价）。
///
/// 用 `str::get(..n)` 而不是 `&text[..n]`：规则文本可能是中文关键词（如"广告"
/// 是 6 字节），一律按 `prefix.len()` 切片会在字节中间切断而 **panic**；
/// `get` 在非字符边界上返回 `None`，正是这里想要的结果（中文不可能等于 ASCII 前缀）。
fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    if head.eq_ignore_ascii_case(prefix) {
        text.get(prefix.len()..)
    } else {
        None
    }
}

/// 兼容 Erika 既有的 `名称/表达式/` 正则写法。
///
/// 判据与旧实现 `parse_regex_rule` 一致：含 `/`、第一个与最后一个 `/` 不同、
/// 且最后一个 `/` 在字符串末尾。返回其中的表达式。
fn legacy_regex_pattern(text: &str) -> Option<&str> {
    let first = text.find('/')?;
    let last = text.rfind('/')?;
    if first == last || last != text.len() - 1 {
        return None;
    }
    let pattern = &text[first + 1..last];
    if pattern.is_empty() {
        None
    } else {
        Some(pattern)
    }
}

/// 应用到规则集上的 `type:` 规则。
fn apply_type_rule(rules: &mut DanmakuRuleSet, spec: &str, name: &str) -> Result<(), RuleError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(RuleError::UnknownType {
            spec: spec.to_string(),
            name: name.to_string(),
        });
    }
    let lowered = name.to_ascii_lowercase();
    let matched = match lowered.as_str() {
        "scroll" | "scroll_rl" | "scrollrl" | "r2l" | "1" | "2" | "3" => {
            rules.blocked_types.insert(DanmakuType::ScrollRL);
            true
        }
        "reverse" | "scroll_lr" | "scrolllr" | "l2r" | "6" => {
            rules.blocked_types.insert(DanmakuType::ScrollLR);
            true
        }
        "top" | "fix_top" | "fixtop" | "5" => {
            rules.blocked_types.insert(DanmakuType::FixTop);
            true
        }
        "bottom" | "fix_bottom" | "fixbottom" | "4" => {
            rules.blocked_types.insert(DanmakuType::FixBottom);
            true
        }
        // 高级弹幕（mode 7）与代码/BAS 弹幕（mode 8/9）在内核里是同一个
        // `DanmakuType::Special`，屏蔽 `special` 会一起屏蔽掉——这正是 B 站
        // 「屏蔽特殊弹幕」的语义。
        "special" | "advanced" | "7" | "code" | "8" | "bas" | "9" => {
            rules.blocked_types.insert(DanmakuType::Special);
            true
        }
        "color" | "colour" => {
            rules.block_color = true;
            true
        }
        "repeat" | "duplicate" => {
            rules.block_repeat = true;
            true
        }
        _ => false,
    };
    if matched {
        Ok(())
    } else {
        Err(RuleError::UnknownType {
            spec: spec.to_string(),
            name: name.to_string(),
        })
    }
}

/// 解析 `>N` / `>=N` / `<N` / `<=N` / `N-M` / `N` 形式的屏蔽区间。
///
/// `>N` / `<N` 保留**严格不等号**语义（`len:>60` 不屏蔽正好 60 字的弹幕）——
/// 用户写的就是"大于"，把边界偷偷放宽会让人以为规则算错了。
fn parse_bound(text: &str) -> Option<Bound> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Some(rest) = text.strip_prefix('>') {
        let (rest, exclusive) = match rest.strip_prefix('=') {
            Some(rest) => (rest, false),
            None => (rest, true),
        };
        return Some(Bound {
            min: Some(parse_number(rest)?),
            max: None,
            exclusive_min: exclusive,
            exclusive_max: false,
        });
    }
    if let Some(rest) = text.strip_prefix('<') {
        let (rest, exclusive) = match rest.strip_prefix('=') {
            Some(rest) => (rest, false),
            None => (rest, true),
        };
        return Some(Bound {
            min: None,
            max: Some(parse_number(rest)?),
            exclusive_min: false,
            exclusive_max: exclusive,
        });
    }
    let mut parts = text.splitn(2, '-');
    let first = parse_number(parts.next()?)?;
    match parts.next() {
        // `len:3` = 只屏蔽正好 3 个字
        None => Some(Bound {
            min: Some(first),
            max: Some(first),
            exclusive_min: false,
            exclusive_max: false,
        }),
        Some(rest) => {
            let second = parse_number(rest)?;
            // 写反的区间（`time:120-60`）取小者为下界，而不是默默失效。
            Some(Bound {
                min: Some(first.min(second)),
                max: Some(first.max(second)),
                exclusive_min: false,
                exclusive_max: false,
            })
        }
    }
}

fn parse_number(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let value: f64 = text.parse().ok()?;
    value.is_finite().then_some(value)
}

/// 把关键词/用户名列表编译成自动机；空列表返回 `None`。
fn build_automaton(patterns: Vec<String>, ascii_case_insensitive: bool) -> Option<AhoCorasick> {
    if patterns.is_empty() {
        return None;
    }
    AhoCorasick::builder()
        .ascii_case_insensitive(ascii_case_insensitive)
        .build(&patterns)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn item(text: &str) -> DanmakuItem {
        DanmakuItem::new(
            1000,
            text.to_string(),
            0xFFFF_FFFF,
            25.0,
            DanmakuType::ScrollRL,
            5000,
        )
    }

    fn rules(items: &[&str]) -> DanmakuRuleSet {
        let (rules, errors) = DanmakuRuleSet::parse(&spec(items));
        assert!(
            errors.is_empty(),
            "规则应全部编译成功，实际报错：{errors:?}"
        );
        rules
    }

    #[test]
    fn plain_text_is_a_case_sensitive_substring_keyword() {
        // 局部变量**不能**叫 `rules`：本模块的测试辅助函数就叫 `rules`，
        // 同名局部变量会把它遮蔽掉，于是下面 `rules(&["ad"])` 变成"把一个
        // `DanmakuRuleSet` 值当函数调用" —— `cargo check`（非测试目标）看不见，
        // 但 lib test 目标会以 E0618 直接编不过。
        let ad = rules(&["广告"]);
        assert_eq!(
            ad.matches(&item("这里是广告君"), 1000),
            Some(BlockReason::Keyword)
        );
        // 大小写敏感：`AD` 不该命中 `ad`（与 DanmakuFactory 的 strstr 一致）
        assert_eq!(rules(&["ad"]).matches(&item("AD"), 1000), None);
    }

    #[test]
    fn ci_prefix_matches_ascii_case_insensitively() {
        let rules = rules(&["ci:ad"]);
        assert_eq!(rules.matches(&item("AD"), 1000), Some(BlockReason::Keyword));
        assert_eq!(
            rules.matches(&item("aD!"), 1000),
            Some(BlockReason::Keyword)
        );
    }

    #[test]
    fn legacy_slash_form_and_re_prefix_are_both_regex() {
        // 旧格式 `名称/表达式/`（Erika 既有）与新增的 `re:` 必须等价
        let legacy = rules(&["spoiler/[0-9]+ spoiler/"]);
        let explicit = rules(&["re:[0-9]+ spoiler"]);
        let probe = item("episode 123 spoiler ahead");
        assert_eq!(legacy.matches(&probe, 0), Some(BlockReason::Regex));
        assert_eq!(explicit.matches(&probe, 0), Some(BlockReason::Regex));
        assert_eq!(legacy.matches(&item("no match here"), 0), None);
    }

    #[test]
    fn regex_compile_error_is_reported_and_does_not_kill_other_rules() {
        // 一条写错不能让整份规则失效（DanmakuFactory 的索引错位缺陷正是这里）
        let (rules, errors) = DanmakuRuleSet::parse(&spec(&["re:[unclosed", "广告"]));
        assert_eq!(errors.len(), 1, "应只报一条：{errors:?}");
        assert!(matches!(errors[0], RuleError::InvalidRegex { .. }));
        assert_eq!(
            rules.matches(&item("广告君"), 0),
            Some(BlockReason::Keyword),
            "写错的正则不该带走合法的关键词规则"
        );
    }

    #[test]
    fn type_rules_cover_every_bilibili_mode() {
        let mut top = item("顶部");
        top.danmaku_type = DanmakuType::FixTop;
        assert_eq!(
            rules(&["type:top"]).matches(&top, 0),
            Some(BlockReason::Type)
        );
        // 数值编号与英文别名等价（B 站 mode：4/5/6/7）
        assert_eq!(rules(&["type:5"]).matches(&top, 0), Some(BlockReason::Type));

        let mut special = item("高级");
        special.danmaku_type = DanmakuType::Special;
        assert_eq!(
            rules(&["type:special"]).matches(&special, 0),
            Some(BlockReason::Type)
        );
        // 代码/BAS 弹幕在内核里也是 Special，`type:code` 应能屏蔽它
        assert_eq!(
            rules(&["type:code"]).matches(&special, 0),
            Some(BlockReason::Type)
        );
    }

    #[test]
    fn unknown_type_name_is_reported() {
        let (_, errors) = DanmakuRuleSet::parse(&spec(&["type:sideways"]));
        assert!(
            matches!(errors[0], RuleError::UnknownType { .. }),
            "{errors:?}"
        );
    }

    #[test]
    fn color_rule_blocks_non_white_only() {
        let rules = rules(&["type:color"]);
        let mut colored = item("彩色");
        colored.text_color = 0xFF00_00FF; // 一点红
        assert_eq!(rules.matches(&colored, 0), Some(BlockReason::Color));
        // 白色（含 alpha）不该被彩色规则命中
        assert_eq!(rules.matches(&item("白色"), 0), None);
    }

    #[test]
    fn user_and_name_rules_use_source_metadata() {
        let mut target = item("无视我");
        target.user_id = "f11e5b6c-abc".to_string();
        target.user_name = "爱刷屏的人".to_string();

        assert_eq!(
            rules(&["<f11e5b6c-abc>"]).matches(&target, 0),
            Some(BlockReason::User)
        );
        // 数值 uid 也走同一条通道（录播姬来源给的是 uid 而不是 hash）
        let mut uid_item = item("无视我");
        uid_item.user_id = "123456789012345678".to_string();
        assert_eq!(
            rules(&["<123456789012345678>"]).matches(&uid_item, 0),
            Some(BlockReason::User),
            "大 uid 必须原样匹配，不能像 DanmakuFactory 那样被 atoi 截断"
        );
        // 用户名是子串匹配
        assert_eq!(
            rules(&["[刷屏]"]).matches(&target, 0),
            Some(BlockReason::Name)
        );
        // 元数据缺失（空串）时用户规则不生效，而不是全部命中
        assert_eq!(
            rules(&["<f11e5b6c-abc>"]).matches(&item("普通弹幕"), 0),
            None
        );
    }

    #[test]
    fn length_time_and_level_rules() {
        let long = item("一二三四五六");
        assert_eq!(
            rules(&["len:>5"]).matches(&long, 0),
            Some(BlockReason::TextLength)
        );
        assert_eq!(rules(&["len:>6"]).matches(&long, 0), None);
        // 区间写法（含写反的区间）
        assert_eq!(
            rules(&["len:3-8"]).matches(&long, 0),
            Some(BlockReason::TextLength)
        );
        assert_eq!(
            rules(&["len:8-3"]).matches(&long, 0),
            Some(BlockReason::TextLength)
        );

        // 时间区间按**已应用偏移**的毫秒判定（秒为单位书写）
        assert_eq!(
            rules(&["time:<2"]).matches(&item("片头"), 1000),
            Some(BlockReason::TimeRange)
        );
        assert_eq!(rules(&["time:<2"]).matches(&item("正片"), 5000), None);
        assert_eq!(
            rules(&["time:60-120"]).matches(&item("片尾"), 90_000),
            Some(BlockReason::TimeRange)
        );

        let mut veteran = item("老用户");
        veteran.user_level = 6;
        assert_eq!(
            rules(&["level:>=6"]).matches(&veteran, 0),
            Some(BlockReason::Level)
        );
        assert_eq!(rules(&["level:>=6"]).matches(&item("无等级信息"), 0), None);
    }

    #[test]
    fn invalid_number_rule_is_reported() {
        let (_, errors) = DanmakuRuleSet::parse(&spec(&["len:>abc"]));
        assert!(
            matches!(errors[0], RuleError::InvalidNumber { .. }),
            "{errors:?}"
        );
    }

    #[test]
    fn repeat_rule_only_asks_for_the_duplicate_filter() {
        let rules = rules(&["type:repeat"]);
        assert!(rules.wants_repeat_filter());
        // 重复屏蔽由重复合并过滤器执行，规则集本身不该把每条都判掉
        assert_eq!(rules.matches(&item("重复"), 0), None);
    }

    #[test]
    fn empty_rule_set_never_blocks() {
        let rules = DanmakuRuleSet::parse_lossy(&[]);
        assert!(rules.is_empty());
        // 空规则集时关键词/正则/用户全不生效，且 `matches` 在第一步就短路
        assert_eq!(rules.matches(&item("广告 <foo> [bar]"), 0), None);
    }
}
