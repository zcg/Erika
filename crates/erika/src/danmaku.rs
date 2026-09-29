//! Native Erika danmaku pipeline.
//!
//! The DFM-style track retainer and filters are adapted from the MIT-licensed
//! NipaPlay-Reload DFM+ Rust implementation:
//! Copyright (c) 2025 MCDFsteve.

mod dfm;
pub mod dfm_core;
mod outline;
mod rules;
mod typography;

pub use rules::{BlockReason, DanmakuRuleSet, RuleError};

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ab_glyph::{
    CodepointIdIter, Font, FontArc, FontRef, FontVec, Glyph, GlyphId, GlyphSvg, Outline, ScaleFont,
    v2,
};
use serde_json::Value;
use thiserror::Error;

use crate::NIPAPLAY_FALLBACK_FONT;
use crate::subtitle::SubtitleFontAttachment;
use crate::text::TextShaper;
use outline::{raster_radius, resolve_width_px};
use typography::{
    DEFAULT_CONFIG_FONT_SIZE, DEFAULT_NATIVE_FONT_SIZE, DEFAULT_SOURCE_FONT_SIZE,
    effective_config_font_size, effective_font_size, px_scale_for_em,
};

const DEFAULT_SCROLL_DURATION: Duration = Duration::from_millis(9000);
const DEFAULT_STATIC_DURATION: Duration = Duration::from_millis(3800);
const DEFAULT_GENERATION: u64 = 1;
const GLYPH_ATLAS_WIDTH: u32 = 2048;
const GLYPH_ATLAS_INITIAL_HEIGHT: u32 = 256;
/// 自发言弹幕背景框专用：atlas 第 0 行前 N 列预留为纯白像素（fill=255，
/// outline=0）。渲染自发言弹幕时用这块 1×N 区域作 fill 纹理源，拉伸成一条
/// 深色半透明背景框（三个渲染后端无需改动——填色 pass 采样该 texel 即可）。
const SELF_BG_ATLAS_RESERVE: u32 = 4;
const SCROLL_DURATION_REFERENCE_LOGICAL_WIDTH: f32 = 1280.0;
const SCROLL_DURATION_MIN_WIDTH_SCALE: f32 = 0.9;
const SCROLL_DURATION_MAX_WIDTH_SCALE: f32 = 1.3;
const DEFAULT_DANMAKU_TRACK_ID: u64 = 1;
const MAX_CUSTOM_DANMAKU_FONT_BYTES: u64 = 64 * 1024 * 1024;
pub const DANMAKU_DEBUG_BUCKETS: usize = 16;

// Memory bounds for the danmaku caches. Each is a hard cap with LRU-style
// eviction: a cache hit is moved to the back of the order queue, and when the
// cap is exceeded the front (least-recently-used) entry is dropped. Eviction
// only forces a re-rasterize / re-layout on the next hit, never changes what
// is drawn — so it trades CPU for memory without affecting functionality.
// A live danmaku stream produces unbounded unique text lines, and the system
// font fallback keeps a full copy of every font face it loads; without caps
// both grow without limit for the lifetime of the session.
const MAX_LOADED_SYSTEM_FACES: usize = 24; // faces, not bytes: budget on count

/// 常驻**字体字节**上限（不是 face 数）。
///
/// 只按 face 数封顶是不够的：一个 face 背后的字体文件可以是上百 MB
/// （实测一台装了 Source Han Sans 的机器上单个 `.ttc` 就有 111.9 MB，
/// 且 `fontdb` 会把它的 45 个 face 登记成 45 个条目）。24 个 face 各自
/// 读一份完整文件就是几百 MB —— 这是"进播放后内存起飞"的主因。
///
/// 这里按**不同源文件**累加：同一个 `.ttc` 的全部 face 共享一份字节，
/// 所以多 face 不会重复计数。超限时按 LRU 逐出 face，被逐出的 face 下次
/// 命中会重新读入文件——只换 CPU，不改变任何字形选择结果。
///
/// 128 MiB 的取值：足够常驻一个"最坏情况下单个大字体 + 若干小字体"的组合，
/// 又能在面数上限之外兜住总量。低于单个最大字体文件时该字体会被反复读入，
/// 因此不能再小。
const MAX_LOADED_SYSTEM_FONT_BYTES: usize = 128 * 1024 * 1024;

/// **探测缓存**上限：只为"判断某个 face 是否含某字符"读进来的字体字节。
///
/// 未命中的 face 不进 `loaded`（见 `SystemFontFallback::resolve`），它读过的
/// 字节只留在这个 LRU 里，让同一个字体文件在一轮字形回退里只读一次。
const MAX_SYSTEM_FONT_PROBE_BYTES: usize = 24 * 1024 * 1024;
const TEXT_LAYOUT_CACHE_CAPACITY: usize = 4096; // distinct danmaku line layouts
const GLYPH_CACHE_CAPACITY: usize = 8192; // (font × char × size) bitmaps
// Atlas alpha buffers are width × height × 2 channels and only ever grow via
// ensure_height. Cap the height; when exceeded the atlas resets (packed
// coordinates are re-derived on demand, so a reset is a re-pack, not a
// functional change).
const GLYPH_ATLAS_MAX_HEIGHT: u32 = 4096;

#[derive(Debug, Error)]
pub enum DanmakuError {
    #[error("invalid danmaku field: {0}")]
    InvalidField(String),
    #[error("missing danmaku text")]
    MissingText,
    #[error("danmaku parse error: {0}")]
    Parse(String),
    #[error("danmaku io error: {0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, DanmakuError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DanmakuMode {
    Scroll,
    ScrollReverse,
    Top,
    Bottom,
    Special,
}

impl DanmakuMode {
    pub fn from_bilibili_mode(value: u32) -> Self {
        match value {
            6 => Self::ScrollReverse,
            5 => Self::Top,
            4 => Self::Bottom,
            7 => Self::Special,
            _ => Self::Scroll,
        }
    }

    fn from_text(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "top" | "fix_top" | "fixed_top" | "5" => Self::Top,
            "bottom" | "fix_bottom" | "fixed_bottom" | "4" => Self::Bottom,
            "reverse" | "scroll_reverse" | "left" | "l2r" | "6" => Self::ScrollReverse,
            "special" | "7" => Self::Special,
            _ => Self::Scroll,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DanmakuColor {
    pub red: f32,
    pub green: f32,
    pub blue: f32,
    pub alpha: f32,
}

impl DanmakuColor {
    pub const WHITE: Self = Self::rgb_u8(255, 255, 255);

    pub const fn rgb_u8(red: u8, green: u8, blue: u8) -> Self {
        Self {
            red: red as f32 / 255.0,
            green: green as f32 / 255.0,
            blue: blue as f32 / 255.0,
            alpha: 1.0,
        }
    }

    pub fn rgba(self, opacity: f32) -> [f32; 4] {
        [
            self.red.clamp(0.0, 1.0),
            self.green.clamp(0.0, 1.0),
            self.blue.clamp(0.0, 1.0),
            (self.alpha * opacity).clamp(0.0, 1.0),
        ]
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuItem {
    pub id: u64,
    pub pts: Duration,
    pub text: String,
    pub mode: DanmakuMode,
    pub font_size: f32,
    pub color: DanmakuColor,
    pub opacity: f32,
    pub is_self: bool,
    /// 发送者标识。
    ///
    /// B 站 XML 的 `p` 属性第 7 段（`user_hash`，形如 `f11e5b6c-...`）落在这里；
    /// JSON 来源认 `user_hash` / `uid` 键。空串 = 来源未提供，此时 `<标识>`
    /// 屏蔽规则对它不生效。
    pub user_id: String,
    /// 发送者用户名。
    ///
    /// B 站点播 XML **不含**这一项（`p` 里只有 hash），只有直播/录播姬来源、
    /// 或 JSON 来源显式给出 `user_name` / `name` 时才有值。
    pub user_name: String,
    /// 发送者等级；0 = 未知。
    ///
    /// 同样只有部分来源提供（B 站点播 XML 没有），`level:` 屏蔽规则在 0 时跳过。
    pub user_level: u32,
}

impl DanmakuItem {
    fn normalized(mut self, fallback_id: u64) -> Result<Self> {
        if self.text.trim().is_empty() {
            return Err(DanmakuError::MissingText);
        }
        if self.id == 0 {
            self.id = fallback_id;
        }
        // A non-positive source font size is the **"use the app's configured
        // size" sentinel**, and the only place that policy lives is
        // `typography::effective_font_size` (`if source > 0.0 { scale relative to
        // DEFAULT_SOURCE_FONT_SIZE } else { configured size }`).
        //
        // It used to be clamped with `.max(1.0)` here, which silently destroyed
        // that sentinel: `sanitize_f32(0.0, …)` returns 0.0 (0 is finite), so
        // every "use the configured size" item arrived downstream as **1.0** —
        // and 1.0 is *positive*, so `effective_font_size` read it as a relative
        // size and computed `configured × (1.0 / 25.0)`, i.e. 1/25 of the
        // configured size. Self-sent danmaku (bili-rs sends `font_size: 0.0`
        // precisely to mean "follow the user's setting") therefore rendered at
        // **1 pixel**: the glyphs were real, but a 1px box is all you see.
        //
        // Keep the sentinel intact here; `effective_font_size` resolves it.
        // NaN and negatives both land on 0.0 = the same sentinel, which is
        // also what they resolved to before (NaN → 25 → ×1.0 → configured).
        self.font_size = sanitize_f32(self.font_size, 0.0).max(0.0);
        self.opacity = sanitize_f32(self.opacity, 1.0).clamp(0.0, 1.0);
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DanmakuViewport {
    pub width: u32,
    pub height: u32,
    pub scale_factor: f32,
}

impl DanmakuViewport {
    pub fn new(width: u32, height: u32) -> Self {
        Self::with_scale(width, height, 1.0)
    }

    pub fn with_scale(width: u32, height: u32, scale_factor: f32) -> Self {
        let scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
            scale_factor
        } else {
            1.0
        };
        Self {
            width: width.max(1),
            height: height.max(1),
            scale_factor,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuLayoutConfig {
    pub font_size: f32,
    pub opacity: f32,
    pub display_area: f32,
    pub scroll_duration_seconds: f32,
    pub scroll_speed_factor: f32,
    pub track_gap_ratio: f32,
    pub outline_width: f32,
    pub shadow_offset: [f32; 2],
    pub shadow_style: DanmakuShadowStyle,
    pub custom_font_family: String,
    pub custom_font_file_path: String,
    #[doc(hidden)]
    pub custom_font_face_index: u32,
    pub merge_duplicates: bool,
    pub allow_stacking: bool,
    pub allow_scroll_overwrite: bool,
    pub max_quantity: Option<u32>,
    pub max_lines_per_mode: Option<u32>,
    pub block_top: bool,
    pub block_bottom: bool,
    pub block_scroll: bool,
    pub block_words: Vec<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DanmakuShadowStyle {
    None,
    Soft,
    Medium,
    #[default]
    Strong,
}

impl DanmakuShadowStyle {
    pub fn from_code(value: i32) -> Self {
        match value {
            0 => Self::None,
            1 => Self::Soft,
            2 => Self::Medium,
            3 => Self::Strong,
            _ => Self::Strong,
        }
    }

    pub fn code(self) -> i32 {
        match self {
            Self::None => 0,
            Self::Soft => 1,
            Self::Medium => 2,
            Self::Strong => 3,
        }
    }
}

impl Default for DanmakuLayoutConfig {
    fn default() -> Self {
        Self {
            font_size: DEFAULT_CONFIG_FONT_SIZE,
            opacity: 1.0,
            display_area: 1.0,
            scroll_duration_seconds: 10.0,
            scroll_speed_factor: 1.0,
            track_gap_ratio: 0.15,
            outline_width: 1.0,
            shadow_offset: [1.0, 1.0],
            shadow_style: DanmakuShadowStyle::Strong,
            custom_font_family: String::new(),
            custom_font_file_path: String::new(),
            custom_font_face_index: 0,
            merge_duplicates: false,
            allow_stacking: false,
            allow_scroll_overwrite: false,
            max_quantity: None,
            max_lines_per_mode: None,
            block_top: false,
            block_bottom: false,
            block_scroll: false,
            block_words: Vec::new(),
            enabled: true,
        }
    }
}

impl DanmakuLayoutConfig {
    fn sanitized(&self) -> Self {
        let mut config = self.clone();
        config.font_size = sanitize_f32(config.font_size, DEFAULT_CONFIG_FONT_SIZE).max(1.0);
        config.opacity = sanitize_f32(config.opacity, 1.0).clamp(0.0, 1.0);
        config.display_area = sanitize_f32(config.display_area, 1.0).clamp(0.1, 1.0);
        config.scroll_duration_seconds =
            sanitize_f32(config.scroll_duration_seconds, 10.0).clamp(1.0, 60.0);
        config.scroll_speed_factor = sanitize_f32(config.scroll_speed_factor, 1.0).clamp(0.2, 4.0);
        config.track_gap_ratio = sanitize_f32(config.track_gap_ratio, 0.15).clamp(0.0, 2.0);
        config.outline_width = sanitize_f32(config.outline_width, 1.0).clamp(0.0, 4.0);
        config.shadow_offset = [
            sanitize_f32(config.shadow_offset[0], 1.0),
            sanitize_f32(config.shadow_offset[1], 1.0),
        ];
        config.custom_font_family = config.custom_font_family.trim().to_string();
        config.custom_font_file_path = config.custom_font_file_path.trim().to_string();
        config
    }

    /// Clears the fields that only affect painting, so two configs that differ
    /// solely in those fields compare equal.
    ///
    /// Written as a subtractive normalization rather than a list of
    /// layout-affecting fields on purpose: a field added later is treated as
    /// layout-affecting until it is explicitly listed here. That costs a
    /// needless relayout at worst, whereas forgetting to extend an additive
    /// list would silently drop the setting ("changing it does nothing").
    fn without_paint_only_fields(&self) -> Self {
        let mut config = self.clone();
        config.opacity = 0.0;
        config.shadow_offset = [0.0, 0.0];
        config.shadow_style = DanmakuShadowStyle::default();
        config.enabled = false;
        config
    }

    fn layout_equivalent(&self, other: &Self) -> bool {
        self.without_paint_only_fields() == other.without_paint_only_fields()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DanmakuConfigChange {
    Unchanged,
    PaintOnly,
    Layout,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct DanmakuTimeline {
    items: Vec<DanmakuItem>,
}

impl DanmakuTimeline {
    pub fn new(items: Vec<DanmakuItem>) -> Result<Self> {
        let mut timeline = Self { items: Vec::new() };
        timeline.extend(items)?;
        Ok(timeline)
    }

    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let input =
            fs::read_to_string(path).map_err(|error| DanmakuError::Io(error.to_string()))?;
        match path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_ascii_lowercase)
        {
            Some(ext) if ext == "xml" => Self::from_bilibili_xml(&input),
            Some(ext) if ext == "jsonl" || ext == "ndjson" => Self::from_json_lines(&input),
            Some(ext) if ext == "json" => Self::from_json(&input),
            _ => Self::parse_auto(&input),
        }
    }

    pub fn parse_auto(input: &str) -> Result<Self> {
        let trimmed = input.trim_start();
        if trimmed.starts_with('<') {
            Self::from_bilibili_xml(input)
        } else if trimmed.starts_with('{') {
            Self::from_json(input).or_else(|_| Self::from_json_lines(input))
        } else if trimmed.starts_with('[') {
            Self::from_json(input)
        } else {
            Self::from_json_lines(input)
        }
    }

    pub fn from_json(input: &str) -> Result<Self> {
        let value: Value =
            serde_json::from_str(input).map_err(|error| DanmakuError::Parse(error.to_string()))?;
        let comments = match value {
            Value::Array(items) => items,
            Value::Object(mut object) => object
                .remove("comments")
                .or_else(|| object.remove("danmaku"))
                .or_else(|| object.remove("items"))
                .and_then(|value| value.as_array().cloned())
                .ok_or_else(|| DanmakuError::InvalidField("comments".to_string()))?,
            _ => return Err(DanmakuError::InvalidField("json root".to_string())),
        };
        let mut items = Vec::new();
        for (index, value) in comments.iter().enumerate() {
            match item_from_json_value(value, index as u64 + 1) {
                Ok(item) => items.push(item),
                Err(error) if should_skip_item_error(&error) => continue,
                Err(error) => return Err(error),
            }
        }
        Self::new(items)
    }

    pub fn from_json_lines(input: &str) -> Result<Self> {
        let mut items = Vec::new();
        for (line_index, line) in input.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let value: Value = serde_json::from_str(line).map_err(|error| {
                DanmakuError::Parse(format!("line {}: {error}", line_index + 1))
            })?;
            match item_from_json_value(&value, items.len() as u64 + 1) {
                Ok(item) => items.push(item),
                Err(error) if should_skip_item_error(&error) => continue,
                Err(error) => return Err(error),
            }
        }
        Self::new(items)
    }

    pub fn from_bilibili_xml(input: &str) -> Result<Self> {
        Self::new(parse_bilibili_xml(input)?)
    }

    pub fn push(&mut self, item: DanmakuItem) -> Result<()> {
        let fallback = self.items.len() as u64 + 1;
        self.items.push(item.normalized(fallback)?);
        self.items.sort_by_key(|item| item.pts);
        Ok(())
    }

    pub fn extend<I>(&mut self, items: I) -> Result<()>
    where
        I: IntoIterator<Item = DanmakuItem>,
    {
        for item in items {
            let fallback = self.items.len() as u64 + 1;
            match item.normalized(fallback) {
                Ok(item) => self.items.push(item),
                Err(DanmakuError::MissingText) => continue,
                Err(error) => return Err(error),
            }
        }
        self.items.sort_by_key(|item| item.pts);
        Ok(())
    }

    pub fn items(&self) -> &[DanmakuItem] {
        &self.items
    }

    pub fn window(&self, start: Duration, end: Duration) -> Self {
        if end < start || self.items.is_empty() {
            return Self::default();
        }
        let items = self
            .items
            .iter()
            .filter(|item| item.pts >= start && item.pts <= end)
            .cloned()
            .collect();
        Self { items }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DanmakuTrackSource {
    Unknown,
    File(PathBuf),
    Json,
    Remote(String),
    Manual,
}

impl DanmakuTrackSource {
    pub fn label(&self) -> String {
        match self {
            Self::Unknown => String::new(),
            Self::File(path) => path.to_string_lossy().into_owned(),
            Self::Json => "json".to_string(),
            Self::Remote(value) => value.clone(),
            Self::Manual => "manual".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuTrack {
    id: u64,
    name: String,
    source: DanmakuTrackSource,
    timeline: DanmakuTimeline,
    enabled: bool,
    offset: i64,
}

impl DanmakuTrack {
    pub fn new(
        id: u64,
        name: impl Into<String>,
        source: DanmakuTrackSource,
        timeline: DanmakuTimeline,
    ) -> Self {
        Self {
            id: id.max(DEFAULT_DANMAKU_TRACK_ID),
            name: name.into(),
            source,
            timeline,
            enabled: true,
            offset: 0,
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn source(&self) -> &DanmakuTrackSource {
        &self.source
    }

    pub fn timeline(&self) -> &DanmakuTimeline {
        &self.timeline
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn offset(&self) -> i64 {
        self.offset
    }

    pub fn offset_duration(&self) -> Duration {
        Duration::from_micros(self.offset.max(0) as u64)
    }

    pub fn item_count(&self) -> usize {
        self.timeline.len()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuTrackInfo {
    pub id: u64,
    pub name: String,
    pub source: String,
    pub enabled: bool,
    pub offset_micros: i64,
    pub item_count: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuSession {
    tracks: Vec<DanmakuTrack>,
    active_timeline: DanmakuTimeline,
    next_track_id: u64,
    global_offset: i64,
    version: u64,
    dirty: bool,
}

impl Default for DanmakuSession {
    fn default() -> Self {
        Self::new()
    }
}

impl DanmakuSession {
    pub fn new() -> Self {
        Self {
            tracks: Vec::new(),
            active_timeline: DanmakuTimeline::default(),
            next_track_id: DEFAULT_DANMAKU_TRACK_ID,
            global_offset: 0,
            version: 1,
            dirty: false,
        }
    }

    pub fn from_timeline(timeline: DanmakuTimeline) -> Self {
        let mut session = Self::new();
        session.replace_default_track(timeline, "default", DanmakuTrackSource::Unknown);
        session
    }

    pub fn add_track(
        &mut self,
        timeline: DanmakuTimeline,
        name: impl Into<String>,
        source: DanmakuTrackSource,
    ) -> u64 {
        let id = self.allocate_track_id();
        self.add_track_with_id(id, timeline, name, source)
    }

    pub fn add_track_with_offset(
        &mut self,
        timeline: DanmakuTimeline,
        name: impl Into<String>,
        source: DanmakuTrackSource,
        offset_micros: i64,
    ) -> u64 {
        let id = self.add_track(timeline, name, source);
        let _ = self.set_track_offset(id, offset_micros);
        id
    }

    pub fn replace_default_track(
        &mut self,
        timeline: DanmakuTimeline,
        name: impl Into<String>,
        source: DanmakuTrackSource,
    ) -> u64 {
        self.tracks.clear();
        self.next_track_id = DEFAULT_DANMAKU_TRACK_ID + 1;
        self.add_track_with_id(DEFAULT_DANMAKU_TRACK_ID, timeline, name, source)
    }

    pub fn clear(&mut self) {
        if self.tracks.is_empty() && self.active_timeline.is_empty() {
            return;
        }
        self.tracks.clear();
        self.active_timeline = DanmakuTimeline::default();
        self.next_track_id = DEFAULT_DANMAKU_TRACK_ID;
        self.mark_changed();
    }

    pub fn remove_track(&mut self, track_id: u64) -> bool {
        let before = self.tracks.len();
        self.tracks.retain(|track| track.id != track_id);
        let removed = self.tracks.len() != before;
        if removed {
            self.mark_dirty();
        }
        removed
    }

    pub fn set_track_enabled(&mut self, track_id: u64, enabled: bool) -> bool {
        let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) else {
            return false;
        };
        if track.enabled != enabled {
            track.enabled = enabled;
            self.mark_dirty();
        }
        true
    }

    pub fn set_track_offset(&mut self, track_id: u64, offset_micros: i64) -> bool {
        let Some(track) = self.tracks.iter_mut().find(|track| track.id == track_id) else {
            return false;
        };
        if track.offset != offset_micros {
            track.offset = offset_micros;
            self.mark_dirty();
        }
        true
    }

    pub fn set_global_offset(&mut self, offset_micros: i64) {
        if self.global_offset != offset_micros {
            self.global_offset = offset_micros;
            self.mark_dirty();
        }
    }

    pub fn global_offset(&self) -> i64 {
        self.global_offset
    }

    pub fn tracks(&self) -> &[DanmakuTrack] {
        &self.tracks
    }

    pub fn track_infos(&self) -> Vec<DanmakuTrackInfo> {
        self.tracks
            .iter()
            .map(|track| DanmakuTrackInfo {
                id: track.id,
                name: track.name.clone(),
                source: track.source.label(),
                enabled: track.enabled,
                offset_micros: track.offset,
                item_count: track.item_count(),
            })
            .collect()
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn active_timeline(&mut self) -> &DanmakuTimeline {
        self.rebuild_if_dirty();
        &self.active_timeline
    }

    pub fn active_timeline_clone(&mut self) -> DanmakuTimeline {
        self.active_timeline().clone()
    }

    pub fn is_empty(&mut self) -> bool {
        self.active_timeline().is_empty()
    }

    fn add_track_with_id(
        &mut self,
        id: u64,
        timeline: DanmakuTimeline,
        name: impl Into<String>,
        source: DanmakuTrackSource,
    ) -> u64 {
        let track = DanmakuTrack::new(id, name, source, timeline);
        self.tracks.push(track);
        self.next_track_id = self.next_track_id.max(id.saturating_add(1));
        self.mark_dirty();
        id
    }

    fn allocate_track_id(&mut self) -> u64 {
        let id = self.next_track_id.max(DEFAULT_DANMAKU_TRACK_ID);
        self.next_track_id = id.saturating_add(1).max(DEFAULT_DANMAKU_TRACK_ID + 1);
        id
    }

    fn mark_dirty(&mut self) {
        self.dirty = true;
        self.mark_changed();
    }

    fn mark_changed(&mut self) {
        self.version = self.version.saturating_add(1).max(1);
    }

    fn rebuild_if_dirty(&mut self) {
        if !self.dirty {
            return;
        }
        let mut items = Vec::new();
        let mut layout_item_id = 1u64;
        for track in &self.tracks {
            if !track.enabled {
                continue;
            }
            for item in track.timeline.items() {
                let offset = track.offset.saturating_add(self.global_offset);
                let Some(pts) = apply_track_offset(item.pts, offset) else {
                    continue;
                };
                let mut item = item.clone();
                // Source IDs belong to the host's data model and may be large or
                // duplicated across sources. Layout identity only has to be
                // unique for this immutable session version, so allocate it here
                // instead of packing two arbitrary u64 values into one.
                item.id = layout_item_id;
                layout_item_id += 1;
                item.pts = pts;
                items.push(item);
            }
        }
        items.sort_by_key(|item| item.pts);
        self.active_timeline = DanmakuTimeline { items };
        self.dirty = false;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PreparedDanmakuItem {
    pub id: u64,
    pub time: Duration,
    pub text: Arc<str>,
    pub mode: DanmakuMode,
    pub color: DanmakuColor,
    pub opacity: f32,
    pub font_size: f32,
    pub width: f32,
    pub height: f32,
    pub y: f32,
    pub track_index: usize,
    pub scroll_speed: f32,
    pub duration: Duration,
    pub duplicate_count: u32,
    /// 用户自己发送的弹幕（渲染时加背景框标识）
    pub is_self: bool,
    text_layout: Arc<PreparedTextLayout>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DfmPreparedLayout {
    viewport: DanmakuViewport,
    config: DanmakuLayoutConfig,
    dfm_layout: dfm::PreparedLayout,
    stats: DanmakuPreparedStats,
    scroll_duration: Duration,
    static_duration: Duration,
    item_times: Vec<f64>,
    items: Vec<PreparedDanmakuItem>,
}

impl DfmPreparedLayout {
    pub fn items(&self) -> &[PreparedDanmakuItem] {
        &self.items
    }

    pub fn stats(&self) -> DanmakuPreparedStats {
        self.stats
    }

    /// 本次准备中编译失败的屏蔽规则（写错的正则/类型名）。
    ///
    /// 空 = 全部规则都已生效；非空说明这些规则**没有生效**，调用方应当把
    /// 数量或全文回显给用户，而不是让用户以为"我明明配了却不生效"。
    pub fn rule_errors(&self) -> &[RuleError] {
        &self.dfm_layout.rule_errors
    }

    /// 编译失败的屏蔽规则条数（给只显示一个数的界面用）。
    pub fn rule_error_count(&self) -> usize {
        self.dfm_layout.rule_errors.len()
    }

    pub(crate) fn apply_paint_config(&mut self, config: &DanmakuLayoutConfig) {
        let config = config.sanitized();
        // A layout replacement is prepared asynchronously. Its previous
        // geometry can remain on screen as a moving fallback in the meantime,
        // so paint-only fields must also be applicable across layout revisions.
        self.config.opacity = config.opacity;
        self.config.shadow_offset = config.shadow_offset;
        self.config.shadow_style = config.shadow_style;
        self.config.enabled = config.enabled;
    }

    pub fn frame_layout(&self, media_time: Duration, generation: u64) -> DanmakuFrameLayout {
        if !self.config.enabled {
            return DanmakuFrameLayout::empty(media_time, generation, self.viewport);
        }
        let current = media_time.as_secs_f64();
        let mut items = Vec::with_capacity(dfm::frame_candidate_count(&self.dfm_layout, current));
        dfm::for_each_frame_item(&self.dfm_layout, current, |frame_item| {
            let Some(item) = self.items.get(frame_item.item_index) else {
                return;
            };
            if item.mode == DanmakuMode::Special {
                return;
            }
            items.push(DanmakuPlacedItem {
                item_id: item.id,
                text: Arc::clone(&item.text),
                mode: item.mode,
                track_index: frame_item.track_index.max(0) as usize,
                x: frame_item.x as f32,
                y: frame_item.y as f32,
                width: item.width,
                height: item.height,
                font_size: item.font_size,
                color: item.color,
                opacity: item.opacity * self.config.opacity,
                outline_width: resolve_width_px(item.font_size, self.config.outline_width),
                shadow_offset: scale_offset(self.config.shadow_offset, self.viewport.scale_factor),
                shadow_alpha: shadow_style_alpha(self.config.shadow_style),
                duplicate_count: item.duplicate_count,
                is_self: item.is_self,
                text_layout: Arc::clone(&item.text_layout),
            });
        });
        DanmakuFrameLayout {
            media_time,
            generation,
            viewport: self.viewport,
            prepared_stats: self.stats,
            items,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuFrameLayout {
    pub media_time: Duration,
    pub generation: u64,
    pub viewport: DanmakuViewport,
    pub prepared_stats: DanmakuPreparedStats,
    pub items: Vec<DanmakuPlacedItem>,
}

impl DanmakuFrameLayout {
    pub fn empty(media_time: Duration, generation: u64, viewport: DanmakuViewport) -> Self {
        Self {
            media_time,
            generation,
            viewport,
            prepared_stats: DanmakuPreparedStats::default(),
            items: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuPlacedItem {
    pub item_id: u64,
    pub text: Arc<str>,
    pub mode: DanmakuMode,
    pub track_index: usize,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    pub font_size: f32,
    pub color: DanmakuColor,
    pub opacity: f32,
    pub outline_width: f32,
    pub shadow_offset: [f32; 2],
    pub shadow_alpha: f32,
    pub duplicate_count: u32,
    /// 用户自己发送的弹幕（渲染时在文字后垫一条背景框）
    pub is_self: bool,
    text_layout: Arc<PreparedTextLayout>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuRenderPlan {
    pub media_time: Duration,
    pub generation: u64,
    pub viewport: DanmakuViewport,
    pub atlas: Option<Arc<DanmakuGlyphAtlas>>,
    pub items: Vec<DanmakuGlyphInstance>,
    pub frame_stats: DanmakuFrameStats,
}

impl DanmakuRenderPlan {
    pub fn empty(media_time: Duration, generation: u64, viewport: DanmakuViewport) -> Self {
        Self {
            media_time,
            generation,
            viewport,
            atlas: None,
            items: Vec::new(),
            frame_stats: DanmakuFrameStats::default(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DanmakuFrameStats {
    pub prepared: DanmakuPreparedStats,
    pub placed_items: usize,
    pub scroll_items: usize,
    pub top_items: usize,
    pub bottom_items: usize,
    pub scroll_rows: usize,
    pub scroll_track_min: usize,
    pub scroll_track_max: usize,
    pub scroll_min_y: f32,
    pub scroll_max_y: f32,
    pub scroll_bucket_count: usize,
    pub scroll_buckets: [DanmakuDebugBucket; DANMAKU_DEBUG_BUCKETS],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DanmakuDebugBucket {
    pub key: i32,
    pub count: u32,
}

/// 屏蔽原因分类计数。
///
/// 诊断码（`dfm_core::filters::filter_param`）是内核内部编号，这里是给界面读的
/// **分类桶**。两者一一对应，见 [`DanmakuFilterBreakdown::add_filter_param`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DanmakuFilterBreakdown {
    /// 类型屏蔽（滚动/顶部/底部/逆向/特殊）
    pub type_blocked: u32,
    /// 同屏密度上限
    pub quantity: u32,
    /// 帧耗时保护
    pub elapsed: u32,
    /// 关键词屏蔽
    pub keyword: u32,
    /// 正则屏蔽
    pub regex: u32,
    /// 用户标识屏蔽（`<标识>`）
    pub user: u32,
    /// 用户名屏蔽（`[用户名]`）
    pub name: u32,
    /// 用户等级屏蔽（`level:`）
    pub level: u32,
    /// 文本长度屏蔽（`len:`）
    pub text_length: u32,
    /// 时间区间屏蔽（`time:`）
    pub time_range: u32,
    /// 彩色弹幕屏蔽（`type:color`）
    pub color: u32,
    /// 重复弹幕合并 / 轨道装不下
    pub duplicate: u32,
}

impl DanmakuFilterBreakdown {
    /// 按 `filter_param` 诊断码累加一条被屏蔽的弹幕。
    ///
    /// 编号来源是 `danmaku::dfm_core::filters::filter_param`；这里重复一份常量是
    /// 因为 `dfm_core` 是移植层（对外公开的是本模块的类型），不想把内部编号
    /// 变成公开契约。**两边改动必须同步**，测试 `filter_breakdown_matches_param_codes`
    /// 会盯住这件事。
    pub fn add_filter_param(&mut self, param: u32) {
        match param {
            0 => {}
            1 => self.type_blocked += 1,
            2 => self.quantity += 1,
            3 => self.elapsed += 1,
            4 => self.keyword += 1,
            5 => self.duplicate += 1,
            6 => self.user += 1,
            7 => self.name += 1,
            8 => self.level += 1,
            9 => self.text_length += 1,
            10 => self.time_range += 1,
            11 => self.color += 1,
            12 => self.regex += 1,
            _ => self.duplicate += 1,
        }
    }

    /// 分类计数之和。
    pub fn total(&self) -> u32 {
        self.type_blocked
            + self.quantity
            + self.elapsed
            + self.keyword
            + self.regex
            + self.user
            + self.name
            + self.level
            + self.text_length
            + self.time_range
            + self.color
            + self.duplicate
    }

    /// 非零分类的中文摘要（如 `类型 12 · 关键词 20 · 正则 5`）；全零返回空串。
    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut push = |label: &str, value: u32| {
            if value > 0 {
                parts.push(format!("{label} {value}"));
            }
        };
        push("类型", self.type_blocked);
        push("密度", self.quantity);
        push("耗时保护", self.elapsed);
        push("关键词", self.keyword);
        push("正则", self.regex);
        push("用户", self.user);
        push("用户名", self.name);
        push("等级", self.level);
        push("长度", self.text_length);
        push("时间区间", self.time_range);
        push("彩色", self.color);
        push("合并/装不下", self.duplicate);
        parts.join(" · ")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DanmakuPreparedStats {
    pub source_items: usize,
    pub supported_items: usize,
    pub prepared_items: usize,
    pub filtered_items: usize,
    /// 被屏蔽条目按原因分类的计数（分类之和 == `filtered_items`）。
    pub filter_breakdown: DanmakuFilterBreakdown,
    pub prepared_scroll_items: usize,
    pub prepared_top_items: usize,
    pub prepared_bottom_items: usize,
    pub prepared_scroll_rows: usize,
    pub prepared_scroll_min_y: f32,
    pub prepared_scroll_max_y: f32,
    pub expected_scroll_tracks: usize,
    pub dfm_track_count: usize,
    pub display_area_height: f32,
    pub scroll_area_height: f32,
    pub track_height: f32,
    pub scroll_bucket_count: usize,
    pub scroll_buckets: [DanmakuDebugBucket; DANMAKU_DEBUG_BUCKETS],
}

impl DanmakuPreparedStats {
    /// 一行中文摘要，给诊断浮窗直接显示。
    ///
    /// 形如 `屏蔽 37 / 1200（类型 12 · 关键词 20 · 正则 5）`。没有任何屏蔽时显示
    /// `屏蔽 0 / 1200`——**仍然显示**，因为"这一栏是 0"本身就是用户想知道的信息
    /// （规则没生效时最容易看出来）。`source_items` 为 0（没加载弹幕）时返回 `—`。
    pub fn filter_summary(&self) -> String {
        if self.source_items == 0 {
            return "—".to_string();
        }
        let detail = self.filter_breakdown.describe();
        if detail.is_empty() {
            format!("屏蔽 0 / {}", self.source_items)
        } else {
            format!(
                "屏蔽 {} / {}（{detail}）",
                self.filter_breakdown.total(),
                self.source_items
            )
        }
    }
}

impl DanmakuFrameStats {
    fn from_layout(layout: &DanmakuFrameLayout) -> Self {
        let mut stats = Self::default();
        stats.prepared = layout.prepared_stats;
        let mut scroll_rows = BTreeSet::new();
        let mut scroll_track_min = usize::MAX;
        let mut scroll_track_max = 0usize;
        let mut bucket_counts = BTreeMap::<i32, u32>::new();
        let mut scroll_min_y = f32::MAX;
        let mut scroll_max_y = f32::MIN;
        for item in &layout.items {
            stats.placed_items += 1;
            match item.mode {
                DanmakuMode::Scroll | DanmakuMode::ScrollReverse => {
                    stats.scroll_items += 1;
                    let row_key = item.y.round() as i32;
                    scroll_rows.insert(row_key);
                    *bucket_counts.entry(row_key).or_default() += 1;
                    scroll_track_min = scroll_track_min.min(item.track_index);
                    scroll_track_max = scroll_track_max.max(item.track_index);
                    scroll_min_y = scroll_min_y.min(item.y);
                    scroll_max_y = scroll_max_y.max(item.y + item.height);
                }
                DanmakuMode::Top => stats.top_items += 1,
                DanmakuMode::Bottom => stats.bottom_items += 1,
                DanmakuMode::Special => {}
            }
        }
        stats.scroll_rows = scroll_rows.len();
        if stats.scroll_items > 0 {
            stats.scroll_track_min = scroll_track_min;
            stats.scroll_track_max = scroll_track_max;
            stats.scroll_min_y = scroll_min_y;
            stats.scroll_max_y = scroll_max_y;
        }
        stats.scroll_bucket_count = fill_debug_buckets(&bucket_counts, &mut stats.scroll_buckets);
        stats
    }
}

impl DanmakuPreparedStats {
    fn from_items(
        source_items: usize,
        supported_items: usize,
        prepared: &[PreparedDanmakuItem],
        viewport: DanmakuViewport,
        config: &DanmakuLayoutConfig,
        dfm_track_count: usize,
        filter_breakdown: DanmakuFilterBreakdown,
    ) -> Self {
        let mut stats = Self {
            source_items,
            supported_items,
            prepared_items: prepared.len(),
            filtered_items: supported_items.saturating_sub(prepared.len()),
            filter_breakdown,
            dfm_track_count,
            display_area_height: viewport.height as f32 * config.display_area.clamp(0.1, 1.0),
            scroll_area_height: viewport.height as f32 * config.display_area.clamp(0.1, 1.0),
            ..Self::default()
        };
        let mut scroll_rows = BTreeSet::new();
        let mut bucket_counts = BTreeMap::<i32, u32>::new();
        let mut scroll_min_y = f32::MAX;
        let mut scroll_max_y = f32::MIN;
        let mut first_scroll_height = 0.0f32;
        for item in prepared {
            match item.mode {
                DanmakuMode::Scroll | DanmakuMode::ScrollReverse => {
                    stats.prepared_scroll_items += 1;
                    let row_key = item.y.round() as i32;
                    scroll_rows.insert(row_key);
                    *bucket_counts.entry(row_key).or_default() += 1;
                    scroll_min_y = scroll_min_y.min(item.y);
                    scroll_max_y = scroll_max_y.max(item.y + item.height);
                    if first_scroll_height <= 0.0 {
                        first_scroll_height = item.height;
                    }
                }
                DanmakuMode::Top => stats.prepared_top_items += 1,
                DanmakuMode::Bottom => stats.prepared_bottom_items += 1,
                DanmakuMode::Special => {}
            }
        }
        stats.prepared_scroll_rows = scroll_rows.len();
        if stats.prepared_scroll_items > 0 {
            stats.prepared_scroll_min_y = scroll_min_y;
            stats.prepared_scroll_max_y = scroll_max_y;
        }
        let track_height =
            (first_scroll_height + first_scroll_height * config.track_gap_ratio).max(1.0);
        stats.track_height = track_height;
        stats.expected_scroll_tracks =
            (stats.scroll_area_height / track_height).floor().max(1.0) as usize;
        stats.scroll_bucket_count = fill_debug_buckets(&bucket_counts, &mut stats.scroll_buckets);
        stats
    }
}

fn fill_debug_buckets(
    source: &BTreeMap<i32, u32>,
    target: &mut [DanmakuDebugBucket; DANMAKU_DEBUG_BUCKETS],
) -> usize {
    for bucket in target.iter_mut() {
        *bucket = DanmakuDebugBucket::default();
    }
    for (slot, (&key, &count)) in target.iter_mut().zip(source.iter()) {
        *slot = DanmakuDebugBucket { key, count };
    }
    source.len()
}

#[derive(Debug, Clone, PartialEq)]
pub struct DanmakuGlyphInstance {
    pub item_id: u64,
    pub rect: [f32; 4],
    pub tex_rect: [f32; 4],
    pub color_rgba: [f32; 4],
    pub outline_rgba: [f32; 4],
    pub shadow_rgba: [f32; 4],
    pub shadow_offset: [f32; 2],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanmakuAtlasUpdate {
    pub from_version: u64,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DanmakuGlyphAtlas {
    pub width: u32,
    pub height: u32,
    pub stride: usize,
    pub fill_alpha: Vec<u8>,
    pub outline_alpha: Vec<u8>,
    pub version: u64,
    pub update: Option<DanmakuAtlasUpdate>,
}

impl DanmakuGlyphAtlas {
    pub fn is_valid(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.stride >= self.width as usize
            && self.fill_alpha.len() >= self.required_len()
            && self.outline_alpha.len() >= self.required_len()
    }

    pub fn required_len(&self) -> usize {
        self.stride.saturating_mul(self.height as usize)
    }

    pub fn incremental_update_from(
        &self,
        version: u64,
        width: u32,
        height: u32,
        stride: usize,
    ) -> Option<&DanmakuAtlasUpdate> {
        let update = self.update.as_ref()?;
        (update.from_version == version
            && self.width == width
            && self.height == height
            && self.stride == stride
            && update.width > 0
            && update.height > 0
            && update.x.saturating_add(update.width) <= self.width
            && update.y.saturating_add(update.height) <= self.height)
            .then_some(update)
    }
}

#[derive(Debug, Clone, PartialEq)]
struct PreparedTextLayout {
    metrics: TextMeasure,
    glyphs: Vec<PreparedGlyphPlacement>,
}

#[derive(Debug, Clone, PartialEq)]
struct PreparedGlyphPlacement {
    glyph: Arc<RasterizedGlyph>,
    packed: PackedGlyph,
    pen_x: f32,
}

#[derive(Debug, Clone)]
struct DanmakuFontFace {
    id: u32,
    font: Arc<FontArc>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DanmakuFontSelection {
    pub generation: u64,
    pub fonts: Arc<[SubtitleFontAttachment]>,
}

impl DanmakuFontSelection {
    pub fn new(generation: u64, fonts: impl Into<Arc<[SubtitleFontAttachment]>>) -> Self {
        Self {
            generation,
            fonts: fonts.into(),
        }
    }
}

impl DanmakuFontFace {
    fn new(id: u32, font: FontArc) -> Self {
        Self {
            id,
            font: Arc::new(font),
        }
    }

    fn glyph_id(&self, ch: char) -> GlyphId {
        self.font.glyph_id(ch)
    }

    fn has_glyph(&self, ch: char) -> bool {
        ch.is_whitespace() || self.glyph_id(ch).0 != 0
    }
}

/// 字体**源文件**的标识（一个 `.ttf`/`.ttc`/`.otf` 文件）。
///
/// `fontdb` 把 `.ttc`（字体集合）里的每个 face 登记成独立的 `fontdb::ID`，
/// 但它们在内存里读到的是**同一个文件的同一份字节**。以源文件为单位共享
/// 字体字节的所有权载体。
///
/// 进程内的 `Arc<[u8]>`（内嵌子集字体、自定义字体）与 `fontdb` 的共享源
/// （磁盘文件、或 memmap 特性下的文件映射）都归一到这一个类型，使
/// [`SharedFont`] 无论字节从哪来都能**零拷贝**共享，不需要像旧实现那样
/// `data.to_vec()` 复制一份整文件。
type FontBytes = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// 取字体文件字节：优先**只读文件映射**，失败才退回整份读入。
///
/// 字体文件（尤其 CJK 集合）动辄几十上百 MB，而渲染一次只访问 cmap / 表目录
/// 那几页。`fs::read` 会把整个文件变成**进程私有内存**；文件映射后它只是
/// **文件支持的只读页**：按需分页、系统在内存压力下可直接回收，同一文件被
/// 多个 face 或多次加载也只对应同一份物理页。
///
/// 用裸 kernel32 FFI 而不是引依赖：本仓的 `windows` crate 没有开
/// `Win32_Storage_FileSystem`/`Win32_System_Memory` feature，为这一处改
/// Cargo.toml 不划算；映射失败时照旧走 `fs::read`，功能不受影响。
fn font_file_bytes(path: &Path) -> Option<FontBytes> {
    #[cfg(target_os = "windows")]
    if let Some(mapped) = font_file_map::MappedFontFile::try_map(path) {
        let handle: FontBytes = Arc::new(mapped);
        return Some(handle);
    }
    let data = fs::read(path).ok()?;
    Some(owned_font_bytes(Arc::from(data)))
}

/// 只读内存映射的字体文件视图（Windows）。
#[cfg(target_os = "windows")]
mod font_file_map {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 0x0000_0001;
    const FILE_SHARE_WRITE: u32 = 0x0000_0002;
    const FILE_SHARE_DELETE: u32 = 0x0000_0004;
    const OPEN_EXISTING: u32 = 3;
    const PAGE_READONLY: u32 = 0x02;
    const FILE_MAP_READ: u32 = 0x0004;

    unsafe extern "system" {
        fn CreateFileW(
            file_name: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: *mut c_void,
        ) -> *mut c_void;
        fn CreateFileMappingW(
            file: *mut c_void,
            security_attributes: *mut c_void,
            protect: u32,
            maximum_size_high: u32,
            maximum_size_low: u32,
            name: *const u16,
        ) -> *mut c_void;
        fn MapViewOfFile(
            file_mapping: *mut c_void,
            desired_access: u32,
            file_offset_high: u32,
            file_offset_low: u32,
            number_of_bytes_to_map: usize,
        ) -> *mut c_void;
        fn UnmapViewOfFile(base_address: *const c_void) -> i32;
        fn CloseHandle(object: *mut c_void) -> i32;
    }

    pub(super) struct MappedFontFile {
        base: *const u8,
        len: usize,
        mapping: *mut c_void,
        file: *mut c_void,
    }

    // SAFETY: 映射视图只读，且在自身 `Drop` 之前一直有效；`*const u8` 指向
    // 文件内容，不构成可变别名。
    unsafe impl Send for MappedFontFile {}
    unsafe impl Sync for MappedFontFile {}

    impl MappedFontFile {
        pub(super) fn try_map(path: &Path) -> Option<Self> {
            let len = usize::try_from(std::fs::metadata(path).ok()?.len()).ok()?;
            if len == 0 {
                return None;
            }
            let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0u16]).collect();
            // SAFETY: 参数按 Win32 约定传入；每一步都检查返回值，失败路径按
            // 相反顺序释放已经取得的句柄（不泄漏、不重复释放）。
            let (file, mapping, base) = unsafe {
                let file = CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                );
                if file.is_null() || file as isize == -1 {
                    return None;
                }
                let mapping = CreateFileMappingW(
                    file,
                    std::ptr::null_mut(),
                    PAGE_READONLY,
                    0,
                    0,
                    std::ptr::null(),
                );
                if mapping.is_null() {
                    CloseHandle(file);
                    return None;
                }
                let base = MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0);
                if base.is_null() {
                    CloseHandle(mapping);
                    CloseHandle(file);
                    return None;
                }
                (file, mapping, base)
            };
            Some(Self {
                base: base.cast::<u8>(),
                len,
                mapping,
                file,
            })
        }
    }

    impl AsRef<[u8]> for MappedFontFile {
        fn as_ref(&self) -> &[u8] {
            // SAFETY: `base`/`len` 来自一次成功的 `MapViewOfFile`，视图在
            // `Drop` 之前一直有效，且映射是只读的。
            unsafe { std::slice::from_raw_parts(self.base, self.len) }
        }
    }

    impl Drop for MappedFontFile {
        fn drop(&mut self) {
            // SAFETY: 三个句柄都由本值独占持有，且只会在这里释放一次。
            unsafe {
                UnmapViewOfFile(self.base.cast::<c_void>());
                CloseHandle(self.mapping);
                CloseHandle(self.file);
            }
        }
    }
}

/// 取 [`FontBytes`] 底层的字节切片。
fn font_bytes_slice(bytes: &FontBytes) -> &[u8] {
    AsRef::<[u8]>::as_ref(&**bytes)
}

/// 把进程内的 `Arc<[u8]>` 归一成 [`FontBytes`]。
///
/// 显式包一层 `Arc<Arc<[u8]>>`（`Arc<[u8]>` 自身实现 `AsRef<[u8]>`），
/// 而不是直接写 unsized coercion——后者在 `let`/`match` 位置会被推断歧义。
/// 这一层只是多一次间接寻址，**不复制任何字节**。
fn owned_font_bytes(data: Arc<[u8]>) -> FontBytes {
    Arc::new(data)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum FontSourceKey {
    /// 磁盘上的字体文件（`Source::File` / `Source::SharedFile`）。
    Path(PathBuf),
    /// 进程内字节源（`Source::Binary`，例如内嵌的子集字体）：
    /// 以数据地址区分，同一个 `Arc` 只算一个源。
    Inline(usize),
}

impl FontSourceKey {
    fn from_source(source: &fontdb::Source) -> Self {
        match source {
            fontdb::Source::File(path) | fontdb::Source::SharedFile(path, _) => {
                Self::Path(path.clone())
            }
            fontdb::Source::Binary(bytes) => {
                Self::Inline(font_bytes_slice(bytes).as_ptr() as usize)
            }
        }
    }
}

struct SystemFontFallback {
    db: fontdb::Database,
    faces: Vec<fontdb::ID>,
    loaded: HashMap<fontdb::ID, Option<DanmakuFontFace>>,
    /// LRU order of `loaded` keys; front is the least-recently-used face.
    loaded_order: VecDeque<fontdb::ID>,
    char_cache: HashMap<char, Option<u32>>,
    next_font_id: u32,
    /// face → 它所属的字体文件（同一 `.ttc` 的全部 face 同源）。
    source_keys: HashMap<fontdb::ID, FontSourceKey>,
    /// 源文件字节：同一个字体文件的全部 face 共享这一份，[`SharedFont`]
    /// 零拷贝借用它。
    sources: HashMap<FontSourceKey, FontBytes>,
    /// 被 `loaded` 引用的源：常驻，字节计入 `MAX_LOADED_SYSTEM_FONT_BYTES`。
    pinned_sources: HashSet<FontSourceKey>,
    /// 只被"探测"过、未被 `loaded` 引用的源的 LRU 顺序（front = 最久未用）。
    probe_order: VecDeque<FontSourceKey>,
    /// `probe_order` 里源的字节总量。
    probe_bytes: usize,
}

/// 手写 `Debug`：`sources` 里装的是 trait object（没有 `Debug`），而调试输出
/// 只需要看数量与预算用量，不需要把字体字节本身打印出来。
impl std::fmt::Debug for SystemFontFallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SystemFontFallback")
            .field("faces", &self.faces.len())
            .field("loaded", &self.loaded.len())
            .field("sources", &self.sources.len())
            .field("pinned_sources", &self.pinned_sources.len())
            .field("probe_bytes", &self.probe_bytes)
            .finish_non_exhaustive()
    }
}

impl SystemFontFallback {
    fn new(next_font_id: u32) -> Self {
        let mut db = fontdb::Database::new();
        db.load_system_fonts();
        // 回退候选：**Windows 走显式白名单，一个都不多**。
        //
        // 原先的候选来自两个"越大越糟"的来源：下面的 preferred 列表（含
        // "Source Han Sans SC" / "Noto Sans CJK SC" 这类第三方字体名）× 再把
        // `db.faces()`（机器上已安装的**全部** face）整个追加。两者都会把用户
        // 自装的字体拉进候选，而回退解析对候选逐个探测字形、命中即整份读入
        // 常驻：实测一台装了 Source Han Sans（单个 `.ttc` 111.9 MB）外加一批
        // 20+ MB 第三方字体的机器上，仅字体一项就常驻数百 MB。
        //
        // **按路径区分"系统自带 vs 用户自装"行不通**：本机 `%WINDIR%\Fonts`
        // 实测 205 个文件、844 MB，其中大量是用户自己装进系统目录的第三方字体
        // （MapleMono/Noto 等，单个 20 MB 上下），与自带字体同目录。所以改成
        // **显式白名单**——"系统里装了什么"从此不影响常驻内存。
        //
        // 白名单里**故意不含雅黑**：首选字体就是雅黑（宿主
        // `DEFAULT_DANMAKU_FONT_FAMILY`），`resolve_font` 每次都先问它，能走到
        // 这里说明雅黑已确认没有该字形，再列一遍只会白读一份 18.8 MB。留下的
        // 两个都是"首选缺字"的兜底，且**只在真的用到时才被读入**：
        // - `Segoe UI Symbol`：箭头/数学/几何等符号（有轮廓，能栅格化）；
        // - `SimSun`：宋体，GBK 覆盖面比雅黑更宽，中文生僻字的最后兜底。
        //
        // 不列 `Segoe UI Emoji`：它是彩色字体，本渲染路径只走轮廓，读进来
        // 12.3 MB 也画不出彩色 emoji。
        #[cfg(target_os = "windows")]
        const WINDOWS_FALLBACK_FONT_FAMILIES: &[&str] = &["Segoe UI Symbol", "SimSun"];

        let mut faces = Vec::new();

        #[cfg(target_os = "windows")]
        for family in WINDOWS_FALLBACK_FONT_FAMILIES {
            let query = fontdb::Query {
                families: &[fontdb::Family::Name(family)],
                // 只取常规字重 / 直立 / 标准宽度：回退永远只用同一个字体实例，
                // 没有必要把 Bold/Light/Semibold 变体也读进来。雅黑在本机就有
                // msyhbd/msyhl/msyhsb/msyhsl/msyhhv 五个变体，合计 82 MB。
                weight: fontdb::Weight::NORMAL,
                stretch: fontdb::Stretch::Normal,
                style: fontdb::Style::Normal,
            };
            if let Some(id) = db.query(&query) {
                push_unique_face(&mut faces, id);
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            let preferred = [
                "PingFang SC",
                "Hiragino Sans GB",
                "Hiragino Sans",
                "Apple SD Gothic Neo",
                "Songti SC",
                "STHeiti",
                "Microsoft YaHei",
                "SimHei",
                "Noto Sans CJK SC",
                "Noto Sans CJK JP",
                "Noto Sans CJK",
                "Source Han Sans SC",
                "Source Han Sans",
                "Arial Unicode MS",
                "Arial Unicode",
                "Segoe UI Symbol",
            ];
            for family in preferred {
                let query = fontdb::Query {
                    families: &[fontdb::Family::Name(family)],
                    weight: fontdb::Weight::NORMAL,
                    stretch: fontdb::Stretch::Normal,
                    style: fontdb::Style::Normal,
                };
                if let Some(id) = db.query(&query) {
                    push_unique_face(&mut faces, id);
                }
            }
            for family in [fontdb::Family::SansSerif, fontdb::Family::Serif] {
                let query = fontdb::Query {
                    families: &[family],
                    weight: fontdb::Weight::NORMAL,
                    stretch: fontdb::Stretch::Normal,
                    style: fontdb::Style::Normal,
                };
                if let Some(id) = db.query(&query) {
                    push_unique_face(&mut faces, id);
                }
            }
            for face in db.faces() {
                push_unique_face(&mut faces, face.id);
            }
        }
        // face → 源文件：`resolve` 要在**不复制字体字节**的前提下定位具体
        // face（见 `probe_face_has_glyph`），这里一次性建表。
        let mut source_keys = HashMap::new();
        for face in db.faces() {
            source_keys.insert(face.id, FontSourceKey::from_source(&face.source));
        }
        Self {
            db,
            faces,
            loaded: HashMap::new(),
            loaded_order: VecDeque::new(),
            char_cache: HashMap::new(),
            next_font_id,
            source_keys,
            sources: HashMap::new(),
            pinned_sources: HashSet::new(),
            probe_order: VecDeque::new(),
            probe_bytes: 0,
        }
    }

    fn resolve(&mut self, ch: char) -> Option<DanmakuFontFace> {
        if let Some(font_id) = self.char_cache.get(&ch).copied() {
            if let Some(face) = font_id.and_then(|font_id| self.loaded_font_by_id(font_id)) {
                return Some(face);
            }
            // char_cache hit but the face it pointed at was evicted by the
            // face-count budget. Drop the stale entry and fall through to the
            // full scan so the character re-resolves. A `None` hit means the
            // char was already determined to have no font — keep that result.
            if font_id.is_some() {
                self.char_cache.remove(&ch);
            } else {
                return None;
            }
        }
        let faces = self.faces.clone();
        for face_id in faces {
            match self.loaded.get(&face_id).cloned() {
                // 已加载且可用：直接查，零额外开销。
                Some(Some(face)) => {
                    self.touch_loaded(face_id);
                    if face.has_glyph(ch) {
                        self.char_cache.insert(ch, Some(face.id));
                        return Some(face);
                    }
                    continue;
                }
                // 已确认"这个 face 读不出来"：不再重复尝试。
                Some(None) => continue,
                None => {}
            }
            // 未加载的 face：先**零拷贝探测**它是否含该字符。
            //
            // 这里曾经无条件 `load_face`，于是每遇到一个字形就要把沿途所有
            // 候选字体整份读入并常驻——一台装了大型 CJK 字体集合的机器上，
            // 单单这一项实测就吃掉几百 MB，而其中绝大多数 face 与这个字符
            // 无关。改成"探测命中才加载"后，常驻的只剩真正参与渲染的字体。
            if !self.probe_face_has_glyph(face_id, ch) {
                continue;
            }
            let Some(face) = self.load_face(face_id) else {
                continue;
            };
            if face.has_glyph(ch) {
                self.char_cache.insert(ch, Some(face.id));
                return Some(face);
            }
        }
        self.char_cache.insert(ch, None);
        None
    }

    /// 该 face 是否含 `ch` 的字形——**不把字体字节注册为常驻**。
    ///
    /// 依赖 `source_bytes` 的探测 LRU：同一个字体文件在一轮字形回退里只读
    /// 一次盘，且未被真正用到的字体不会常驻。
    fn probe_face_has_glyph(&mut self, id: fontdb::ID, ch: char) -> bool {
        let Some((bytes, index)) = self.source_bytes(id) else {
            return false;
        };
        // SAFETY: `bytes` 在本次调用期间一直存活，`FontRef` 不逃逸出函数。
        let slice: &[u8] = bytes.as_ref().as_ref();
        FontRef::try_from_slice_and_index(slice, index)
            .map(|font| font.glyph_id(ch).0 != 0)
            .unwrap_or(false)
    }

    /// 取某个 face 所属**字体文件**的字节与它的 face 索引。
    ///
    /// 同一个文件的全部 face 共享同一份 [`FontBytes`]：`fontdb` 在 memmap
    /// 特性下给的是已经映射好的共享源，直接复用即可（旧实现走
    /// `with_face_data` + `to_vec()`，每加载一个 face 就整份复制一次文件）。
    /// 未被 `loaded` 引用的源放在有上限的 LRU 里，超限即淘汰——淘汰只会让
    /// 下次探测重新取一次源，不改变字形选择结果。
    fn source_bytes(&mut self, id: fontdb::ID) -> Option<(FontBytes, u32)> {
        let key = self.source_keys.get(&id)?.clone();
        let (source, index) = self.db.face_source(id)?;
        if let Some(bytes) = self.sources.get(&key).cloned() {
            if !self.pinned_sources.contains(&key) {
                self.touch_probe(&key);
            }
            return Some((bytes, index));
        }
        let bytes: FontBytes = match source {
            fontdb::Source::SharedFile(_, bytes) | fontdb::Source::Binary(bytes) => bytes,
            fontdb::Source::File(path) => font_file_bytes(&path)?,
        };
        let len = font_bytes_slice(&bytes).len();
        self.sources.insert(key.clone(), bytes.clone());
        self.probe_order.push_back(key);
        self.probe_bytes = self.probe_bytes.saturating_add(len);
        self.enforce_probe_budget();
        Some((bytes, index))
    }

    /// 把某个源移出探测 LRU（它已被 `loaded` 引用，字节改由面预算管）。
    fn pin_source(&mut self, id: fontdb::ID) {
        let Some(key) = self.source_keys.get(&id).cloned() else {
            return;
        };
        if self.pinned_sources.insert(key.clone()) {
            if let Some(pos) = self.probe_order.iter().position(|k| *k == key) {
                self.probe_order.remove(pos);
            }
            if let Some(bytes) = self.sources.get(&key) {
                let len = font_bytes_slice(bytes).len();
                self.probe_bytes = self.probe_bytes.saturating_sub(len);
            }
        }
    }

    fn touch_probe(&mut self, key: &FontSourceKey) {
        if let Some(pos) = self.probe_order.iter().position(|k| k == key) {
            self.probe_order.remove(pos);
            self.probe_order.push_back(key.clone());
        }
    }

    fn enforce_probe_budget(&mut self) {
        while self.probe_bytes > MAX_SYSTEM_FONT_PROBE_BYTES && !self.probe_order.is_empty() {
            let Some(old_key) = self.probe_order.pop_front() else {
                break;
            };
            if self.pinned_sources.contains(&old_key) {
                continue;
            }
            let Some(bytes) = self.sources.remove(&old_key) else {
                continue;
            };
            let len = font_bytes_slice(&bytes).len();
            self.probe_bytes = self.probe_bytes.saturating_sub(len);
        }
    }

    fn loaded_font_by_id(&mut self, font_id: u32) -> Option<DanmakuFontFace> {
        let cached_id = self
            .loaded
            .iter()
            .find_map(|(id, face)| face.as_ref().filter(|face| face.id == font_id).map(|_| *id));
        let face = cached_id
            .and_then(|id| self.loaded.get(&id))
            .and_then(|f| f.clone());
        if face.is_some() {
            if let Some(id) = cached_id {
                self.touch_loaded(id);
            }
        }
        face
    }

    fn load_face(&mut self, id: fontdb::ID) -> Option<DanmakuFontFace> {
        if let Some(face) = self.loaded.get(&id).cloned() {
            self.touch_loaded(id);
            return face;
        }
        // 共享源字节：同一个 `.ttc` 的多个 face 只保留**一份**文件字节，
        // `SharedFont`（`FontRef`）零拷贝借用它。旧实现是
        // `with_face_data(... data.to_vec() ...)`——每加载一个 face 就整份
        // 复制一次字体文件，45-face 的 CJK 集合就是 45 份上百 MB 的副本。
        let (bytes, index) = self.source_bytes(id)?;
        let face = load_shared_font_data(bytes, index).map(|font| {
            let id = self.next_font_id;
            self.next_font_id = self.next_font_id.saturating_add(1).max(1);
            DanmakuFontFace::new(id, font)
        });
        self.loaded.insert(id, face.clone());
        self.loaded_order.push_back(id);
        // 该源现在被 `loaded` 引用：字节计入面预算，并移出探测 LRU。
        self.pin_source(id);
        self.enforce_loaded_budget();
        face
    }

    /// Move an already-loaded face to the back of the LRU order.
    fn touch_loaded(&mut self, id: fontdb::ID) {
        if let Some(pos) = self
            .loaded_order
            .iter()
            .position(|candidate| *candidate == id)
        {
            self.loaded_order.remove(pos);
            self.loaded_order.push_back(id);
        }
    }

    /// Cap the number of retained font faces **and** the retained font bytes.
    /// Eviction releases the face's full font bytes (unless a rasterized glyph
    /// elsewhere still holds an Arc to it). Any char_cache entry that pointed at
    /// an evicted face is dropped so the next lookup re-resolves through the
    /// full scan.
    ///
    /// 面数上限挡不住"只加载了一个 face、但它是上百 MB 的 CJK 集合"这种
    /// 情况（面数只算 1），所以字节上限必须同时成立；两者都只换 CPU，
    /// 不改变字形选择结果。
    fn enforce_loaded_budget(&mut self) {
        while self.loaded_order.len() > MAX_LOADED_SYSTEM_FACES
            || self.pinned_source_bytes() > MAX_LOADED_SYSTEM_FONT_BYTES
        {
            let Some(old_id) = self.loaded_order.pop_front() else {
                break;
            };
            let internal_id = self
                .loaded
                .remove(&old_id)
                .and_then(|face| face.map(|face| face.id));
            if let Some(internal_id) = internal_id {
                self.char_cache
                    .retain(|_, cached| *cached != Some(internal_id));
            }
            self.unpin_if_unused(old_id);
        }
    }

    /// `loaded` 里各 face 引用的**不同源文件**的字节总量。
    ///
    /// 同一个 `.ttc` 的多个 face 只算一次，所以"一个字体集合被加载了多个
    /// face"不会把预算放大成 N 倍。
    fn pinned_source_bytes(&self) -> usize {
        let mut total = 0usize;
        let mut seen: Vec<&FontSourceKey> = Vec::new();
        for id in self.loaded.keys() {
            let Some(key) = self.source_keys.get(id) else {
                continue;
            };
            if seen.iter().any(|candidate| *candidate == key) {
                continue;
            }
            seen.push(key);
            if let Some(bytes) = self.sources.get(key) {
                total = total.saturating_add(font_bytes_slice(bytes).len());
            }
        }
        total
    }

    /// 某个 face 被逐出后，若它的源已不再被任何 `loaded` face 引用，
    /// 就从常驻集合降级回探测 LRU（下次用到再取一次源）。
    fn unpin_if_unused(&mut self, id: fontdb::ID) {
        let Some(key) = self.source_keys.get(&id).cloned() else {
            return;
        };
        let still_used = self
            .loaded
            .keys()
            .any(|other| self.source_keys.get(other).is_some_and(|k| *k == key));
        if still_used {
            return;
        }
        if self.pinned_sources.remove(&key) {
            if let Some(bytes) = self.sources.get(&key) {
                let len = font_bytes_slice(bytes).len();
                self.probe_bytes = self.probe_bytes.saturating_add(len);
            }
            self.probe_order.push_back(key);
            self.enforce_probe_budget();
        }
    }
}

fn push_unique_face(faces: &mut Vec<fontdb::ID>, id: fontdb::ID) {
    if !faces.contains(&id) {
        faces.push(id);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TextLayoutCacheKey {
    text: Arc<str>,
    font_size_milli: u32,
    outline_radius: u16,
}

impl TextLayoutCacheKey {
    fn new(text: Arc<str>, font_size: f32, outline_width: f32) -> Self {
        Self {
            text,
            font_size_milli: (sanitize_f32(font_size, DEFAULT_NATIVE_FONT_SIZE).max(1.0) * 1000.0)
                .round() as u32,
            outline_radius: raster_radius(outline_width),
        }
    }

    fn font_size(&self) -> f32 {
        self.font_size_milli as f32 / 1000.0
    }

    fn outline_width(&self) -> f32 {
        f32::from(self.outline_radius)
    }
}

/// Bounded text-layout cache. A live danmaku stream generates unbounded
/// unique line texts; without a cap every distinct line would stay resident
/// for the whole session. Evicted entries are simply re-laid-out on the next
/// hit (CPU cost only — the rendered result is identical).
#[derive(Debug)]
struct TextLayoutCache {
    map: HashMap<TextLayoutCacheKey, Arc<PreparedTextLayout>>,
    order: VecDeque<TextLayoutCacheKey>,
    capacity: usize,
}

impl TextLayoutCache {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    fn get(&mut self, key: &TextLayoutCacheKey) -> Option<Arc<PreparedTextLayout>> {
        let hit = self.map.get(key).cloned();
        if hit.is_some() {
            self.touch(key);
        }
        hit
    }

    fn insert(&mut self, key: TextLayoutCacheKey, layout: Arc<PreparedTextLayout>) {
        if let Some(pos) = self.order.iter().position(|candidate| candidate == &key) {
            self.order.remove(pos);
        }
        self.map.insert(key.clone(), layout);
        self.order.push_back(key);
        while self.map.len() > self.capacity {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&old);
        }
    }

    fn touch(&mut self, key: &TextLayoutCacheKey) {
        if let Some(pos) = self.order.iter().position(|candidate| candidate == key) {
            self.order.remove(pos);
            self.order.push_back(key.clone());
        }
    }
}

#[derive(Debug, Clone)]
pub struct DanmakuTextRasterizer {
    shaper: TextShaper,
    primary_font: Option<DanmakuFontFace>,
    selected_fonts: Arc<[DanmakuFontFace]>,
    selected_before_primary: bool,
    fallback_fonts: Arc<Mutex<SystemFontFallback>>,
    glyph_cache: Arc<Mutex<GlyphCache>>,
    text_layout_cache: Arc<Mutex<TextLayoutCache>>,
    glyph_atlas: Arc<Mutex<PersistentGlyphAtlas>>,
}

impl Default for DanmakuTextRasterizer {
    fn default() -> Self {
        Self::new(TextShaper::default())
    }
}

impl DanmakuTextRasterizer {
    pub fn new(shaper: TextShaper) -> Self {
        Self {
            shaper,
            primary_font: load_default_font().map(|font| DanmakuFontFace::new(0, font)),
            selected_fonts: Arc::from([]),
            selected_before_primary: false,
            fallback_fonts: Arc::new(Mutex::new(SystemFontFallback::new(1))),
            glyph_cache: Arc::new(Mutex::new(GlyphCache::with_capacity(GLYPH_CACHE_CAPACITY))),
            text_layout_cache: Arc::new(Mutex::new(TextLayoutCache::with_capacity(
                TEXT_LAYOUT_CACHE_CAPACITY,
            ))),
            glyph_atlas: Arc::new(Mutex::new(PersistentGlyphAtlas::new())),
        }
    }

    pub fn for_config(config: &DanmakuLayoutConfig) -> Self {
        Self::for_config_and_selection(config, &DanmakuFontSelection::default())
    }

    pub fn for_config_and_selection(
        config: &DanmakuLayoutConfig,
        selection: &DanmakuFontSelection,
    ) -> Self {
        let shaper = TextShaper::default();
        let primary_font = load_configured_font(
            &config.custom_font_family,
            &config.custom_font_file_path,
            config.custom_font_face_index,
        )
        .map(|font| DanmakuFontFace::new(0, font));
        let first_selected_id = u32::from(primary_font.is_some());
        let selected_fonts = load_selected_fonts(&selection.fonts, first_selected_id);
        let next_font_id = first_selected_id.saturating_add(selected_fonts.len() as u32);
        Self {
            shaper,
            primary_font,
            selected_fonts: Arc::from(selected_fonts),
            selected_before_primary: config.custom_font_family.is_empty()
                && config.custom_font_file_path.is_empty(),
            fallback_fonts: Arc::new(Mutex::new(SystemFontFallback::new(next_font_id))),
            glyph_cache: Arc::new(Mutex::new(GlyphCache::with_capacity(GLYPH_CACHE_CAPACITY))),
            text_layout_cache: Arc::new(Mutex::new(TextLayoutCache::with_capacity(
                TEXT_LAYOUT_CACHE_CAPACITY,
            ))),
            glyph_atlas: Arc::new(Mutex::new(PersistentGlyphAtlas::new())),
        }
    }

    pub fn measure(&self, text: &str, font_size: f32) -> TextMeasure {
        if self.primary_font.is_none() {
            let metrics = self.shaper.measure(text, font_size);
            return TextMeasure {
                width: metrics.width.max(1.0),
                height: metrics.height.max(1.0),
                ascent: metrics.ascent,
                descent: metrics.descent,
            };
        }
        self.measure_with_fallback(text, font_size)
    }

    fn measure_with_fallback(&self, text: &str, font_size: f32) -> TextMeasure {
        let mut width = 0.0f32;
        let mut max_ascent = 0.0f32;
        let mut max_descent = 0.0f32;
        let mut max_line_gap = 0.0f32;
        let mut previous: Option<(u32, GlyphId)> = None;
        for ch in text.chars() {
            let Some(face) = self.resolve_font(ch) else {
                let metrics = self.shaper.measure(&ch.to_string(), font_size);
                width += metrics.width.max(font_size * 0.5);
                max_ascent = max_ascent.max(metrics.ascent);
                max_descent = max_descent.max(metrics.descent);
                previous = None;
                continue;
            };
            let scale = px_scale_for_em(face.font.as_ref(), font_size);
            let scaled = face.font.as_scaled(scale);
            let glyph_id = scaled.glyph_id(ch);
            if let Some((previous_font_id, previous_glyph_id)) = previous {
                if previous_font_id == face.id {
                    width += scaled.kern(previous_glyph_id, glyph_id);
                }
            }
            width += scaled.h_advance(glyph_id).max(0.0);
            max_ascent = max_ascent.max(scaled.ascent());
            max_descent = max_descent.max(scaled.descent().abs());
            max_line_gap = max_line_gap.max(scaled.line_gap());
            previous = Some((face.id, glyph_id));
        }
        let ascent = max_ascent.max(font_size * 0.8);
        let descent = max_descent.max(font_size * 0.2);
        let height = (ascent + descent + max_line_gap).max(font_size * 1.2);
        TextMeasure {
            width: width.max(1.0),
            height,
            ascent,
            descent,
        }
    }

    fn prepare_text_layout_with_metrics(
        &self,
        text: Arc<str>,
        font_size: f32,
        outline_width: f32,
        metrics: TextMeasure,
    ) -> Arc<PreparedTextLayout> {
        let key = TextLayoutCacheKey::new(text, font_size, outline_width);
        {
            let mut cache = self
                .text_layout_cache
                .lock()
                .expect("danmaku text layout cache lock");
            if let Some(layout) = cache.get(&key) {
                return layout;
            }
        }
        let layout = Arc::new(self.build_text_layout(&key, metrics));
        self.text_layout_cache
            .lock()
            .expect("danmaku text layout cache lock")
            .insert(key, Arc::clone(&layout));
        layout
    }

    fn build_text_layout(
        &self,
        key: &TextLayoutCacheKey,
        metrics: TextMeasure,
    ) -> PreparedTextLayout {
        let font_size = key.font_size();
        let outline_width = key.outline_width();
        let mut glyphs = Vec::new();
        let mut pen_x = 0.0f32;
        let mut previous: Option<(u32, GlyphId)> = None;
        for ch in key.text.chars() {
            let face = self.resolve_font(ch);
            let glyph_id = face.as_ref().map(|face| face.glyph_id(ch));
            if let (Some(face), Some((previous_font_id, previous_glyph_id)), Some(current)) =
                (face.as_ref(), previous, glyph_id)
            {
                if previous_font_id == face.id {
                    pen_x += face
                        .font
                        .as_scaled(px_scale_for_em(face.font.as_ref(), font_size))
                        .kern(previous_glyph_id, current);
                }
            }
            let glyph_key = GlyphCacheKey::new(
                face.as_ref().map(|face| face.id).unwrap_or(u32::MAX),
                ch,
                font_size,
                outline_width,
            );
            let glyph = self.cached_glyph_by_key(glyph_key);
            let packed = if glyph.has_bitmap() {
                self.pack_glyph(&glyph)
            } else {
                PackedGlyph::default()
            };
            let advance = glyph.advance;
            glyphs.push(PreparedGlyphPlacement {
                glyph,
                packed,
                pen_x,
            });
            pen_x += advance;
            previous = face
                .as_ref()
                .and_then(|face| glyph_id.map(|glyph_id| (face.id, glyph_id)));
        }
        PreparedTextLayout { metrics, glyphs }
    }

    fn pack_glyph(&self, glyph: &RasterizedGlyph) -> PackedGlyph {
        self.glyph_atlas
            .lock()
            .expect("danmaku glyph atlas lock")
            .pack(glyph)
    }

    fn resolve_font(&self, ch: char) -> Option<DanmakuFontFace> {
        if self.selected_before_primary {
            if let Some(font) = self.selected_fonts.iter().find(|font| font.has_glyph(ch)) {
                return Some(font.clone());
            }
        }
        if let Some(font) = &self.primary_font {
            if font.has_glyph(ch) {
                return Some(font.clone());
            }
        }
        if !self.selected_before_primary {
            if let Some(font) = self.selected_fonts.iter().find(|font| font.has_glyph(ch)) {
                return Some(font.clone());
            }
        }
        self.fallback_fonts
            .lock()
            .expect("danmaku system font fallback lock")
            .resolve(ch)
    }

    pub fn render_plan(&self, layout: &DanmakuFrameLayout) -> DanmakuRenderPlan {
        let frame_stats = DanmakuFrameStats::from_layout(layout);
        if layout.items.is_empty() {
            return DanmakuRenderPlan::empty(layout.media_time, layout.generation, layout.viewport);
        }
        let Some(atlas_snapshot) = self
            .glyph_atlas
            .lock()
            .expect("danmaku glyph atlas lock")
            .snapshot()
        else {
            return DanmakuRenderPlan::empty(layout.media_time, layout.generation, layout.viewport);
        };
        let atlas_w = atlas_snapshot.width.max(1) as f32;
        let atlas_h = atlas_snapshot.height.max(1) as f32;

        let capacity = layout
            .items
            .iter()
            .map(|item| item.text_layout.glyphs.len())
            .sum();
        let mut items = Vec::with_capacity(capacity);
        for item in &layout.items {
            let color = item.color.rgba(item.opacity);
            if color[3] <= 0.0 {
                continue;
            }
            self.append_item_glyphs(item, color, atlas_w, atlas_h, &mut items);
        }
        DanmakuRenderPlan {
            media_time: layout.media_time,
            generation: layout.generation,
            viewport: layout.viewport,
            atlas: Some(atlas_snapshot),
            items,
            frame_stats,
        }
    }

    fn append_item_glyphs(
        &self,
        item: &DanmakuPlacedItem,
        color: [f32; 4],
        atlas_w: f32,
        atlas_h: f32,
        items: &mut Vec<DanmakuGlyphInstance>,
    ) {
        if item.is_self {
            // 自发言弹幕：先垫背景框，再画文字。同一 item_id 的字形保持连续
            //（metal 按 item_id 分组 layering 的假设不变）。
            self.append_self_background(item, color, atlas_w, atlas_h, items);
        }
        let baseline = item.y + item.text_layout.metrics.ascent;
        for placement in &item.text_layout.glyphs {
            let glyph = &placement.glyph;
            if glyph.has_bitmap() {
                let packed = placement.packed;
                items.push(DanmakuGlyphInstance {
                    item_id: item.item_id,
                    rect: [
                        item.x + placement.pen_x + glyph.offset_x,
                        baseline + glyph.offset_y,
                        glyph.width as f32,
                        glyph.height as f32,
                    ],
                    tex_rect: [
                        packed.x as f32 / atlas_w,
                        packed.y as f32 / atlas_h,
                        packed.width as f32 / atlas_w,
                        packed.height as f32 / atlas_h,
                    ],
                    color_rgba: color,
                    // Keep the outline as opaque as the fill. Capping it at
                    // 0.75 made an otherwise opaque danmaku look washed out.
                    outline_rgba: [0.0, 0.0, 0.0, color[3]],
                    shadow_rgba: [0.0, 0.0, 0.0, (color[3] * item.shadow_alpha).min(1.0)],
                    shadow_offset: item.shadow_offset,
                });
            }
        }
    }

    /// 自发言弹幕背景框：把整条文字的所有字形包围盒向外扩一点，用 atlas 预留的
    /// 纯白像素作 fill 纹理源，垫一层深色半透明底（B站自发言弹幕样式）。只走
    /// fill pass（outline/shadow alpha=0，各后端自动跳过），无需改渲染后端。
    fn append_self_background(
        &self,
        item: &DanmakuPlacedItem,
        color: [f32; 4],
        atlas_w: f32,
        atlas_h: f32,
        items: &mut Vec<DanmakuGlyphInstance>,
    ) {
        let baseline = item.y + item.text_layout.metrics.ascent;
        let mut min_x = f32::MAX;
        let mut min_y = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_y = f32::MIN;
        for placement in &item.text_layout.glyphs {
            let glyph = &placement.glyph;
            if !glyph.has_bitmap() {
                continue;
            }
            let x0 = item.x + placement.pen_x + glyph.offset_x;
            let y0 = baseline + glyph.offset_y;
            min_x = min_x.min(x0);
            min_y = min_y.min(y0);
            max_x = max_x.max(x0 + glyph.width as f32);
            max_y = max_y.max(y0 + glyph.height as f32);
        }
        if min_x > max_x || min_y > max_y {
            return; // 无有效字形（纯空白等）
        }
        let pad = (item.font_size * 0.14).max(2.0);
        // 白色边界框：比深色底向外扩 border_w。同一 item_id 连续 push → 分组
        // layering 保持 push 顺序（白框最底、深底中间、字形最上），视觉即
        // "深色底 + 白色描边框住"（用户要求自发言弹幕白框突出）。只走 fill
        // pass（outline/shadow alpha=0，各后端自动跳过）。
        let border_w = (item.font_size * 0.07).max(1.5);
        let reserve = SELF_BG_ATLAS_RESERVE as f32;
        let white_alpha = (color[3] * 0.85).clamp(0.5, 0.95);
        items.push(DanmakuGlyphInstance {
            item_id: item.item_id,
            rect: [
                min_x - pad - border_w,
                min_y - pad - border_w,
                (max_x - min_x) + pad * 2.0 + border_w * 2.0,
                (max_y - min_y) + pad * 2.0 + border_w * 2.0,
            ],
            tex_rect: [0.0, 0.0, reserve / atlas_w, 1.0 / atlas_h],
            color_rgba: [1.0, 1.0, 1.0, white_alpha],
            outline_rgba: [0.0, 0.0, 0.0, 0.0],
            shadow_rgba: [0.0, 0.0, 0.0, 0.0],
            shadow_offset: [0.0, 0.0],
        });
        items.push(DanmakuGlyphInstance {
            item_id: item.item_id,
            rect: [
                min_x - pad,
                min_y - pad,
                (max_x - min_x) + pad * 2.0,
                (max_y - min_y) + pad * 2.0,
            ],
            tex_rect: [0.0, 0.0, reserve / atlas_w, 1.0 / atlas_h],
            // 深色半透明底：透明度跟随文字整体透明度，但不随文字颜色变化
            color_rgba: [0.06, 0.07, 0.09, (color[3] * 0.5).clamp(0.15, 0.6)],
            outline_rgba: [0.0, 0.0, 0.0, 0.0],
            shadow_rgba: [0.0, 0.0, 0.0, 0.0],
            shadow_offset: [0.0, 0.0],
        });
    }

    fn cached_glyphs<I>(&self, keys: I) -> Vec<Arc<RasterizedGlyph>>
    where
        I: IntoIterator<Item = GlyphCacheKey>,
    {
        let keys = keys.into_iter().collect::<Vec<_>>();
        if keys.is_empty() {
            return Vec::new();
        }
        let mut results = Vec::with_capacity(keys.len());
        let mut missing = Vec::new();
        {
            let mut cache = self.glyph_cache.lock().expect("danmaku glyph cache lock");
            for key in keys {
                if let Some(glyph) = cache.get(&key) {
                    results.push(glyph);
                } else {
                    results.push(Arc::new(RasterizedGlyph::placeholder(key)));
                    missing.push((results.len() - 1, key));
                }
            }
        }
        if missing.is_empty() {
            return results;
        }
        for (index, key) in missing {
            let glyph = Arc::new(self.rasterize_glyph(key));
            let cached = self
                .glyph_cache
                .lock()
                .expect("danmaku glyph cache lock")
                .get_or_insert(key, glyph);
            results[index] = cached;
        }
        results
    }

    fn cached_glyph_by_key(&self, key: GlyphCacheKey) -> Arc<RasterizedGlyph> {
        self.cached_glyphs([key])
            .into_iter()
            .next()
            .expect("one glyph returned")
    }

    fn rasterize_glyph(&self, key: GlyphCacheKey) -> RasterizedGlyph {
        if let Some(font) = self.resolve_font(key.ch) {
            if font.id == key.font_id {
                return rasterize_font_glyph(&font.font, key);
            }
        }
        rasterize_fallback_glyph(&self.shaper, key)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct GlyphCacheKey {
    font_id: u32,
    ch: char,
    font_size_milli: u32,
    outline_radius: u16,
}

impl GlyphCacheKey {
    fn new(font_id: u32, ch: char, font_size: f32, outline_width: f32) -> Self {
        Self {
            font_id,
            ch,
            font_size_milli: (sanitize_f32(font_size, DEFAULT_NATIVE_FONT_SIZE).max(1.0) * 1000.0)
                .round() as u32,
            outline_radius: raster_radius(outline_width),
        }
    }

    fn font_size(self) -> f32 {
        self.font_size_milli as f32 / 1000.0
    }
}

/// Bounded glyph-bitmap cache. Under a fixed danmaku font size this is
/// naturally bounded by the charset, but dynamic font sizes / atlas resets can
/// multiply distinct (font × char × size) keys without limit. The cap keeps
/// it flat; an evicted glyph is re-rasterized on the next hit.
#[derive(Debug)]
struct GlyphCache {
    map: HashMap<GlyphCacheKey, Arc<RasterizedGlyph>>,
    order: VecDeque<GlyphCacheKey>,
    capacity: usize,
}

impl GlyphCache {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
            capacity,
        }
    }

    fn get(&mut self, key: &GlyphCacheKey) -> Option<Arc<RasterizedGlyph>> {
        let hit = self.map.get(key).cloned();
        if hit.is_some() {
            self.touch(key);
        }
        hit
    }

    /// Insert `glyph` for `key`, or return the existing entry if a concurrent
    /// rasterizer already inserted one (never overwrites, matching the old
    /// `entry().or_insert_with` behaviour).
    fn get_or_insert(
        &mut self,
        key: GlyphCacheKey,
        glyph: Arc<RasterizedGlyph>,
    ) -> Arc<RasterizedGlyph> {
        if let Some(existing) = self.map.get(&key).cloned() {
            self.touch(&key);
            return existing;
        }
        self.map.insert(key.clone(), glyph.clone());
        self.order.push_back(key.clone());
        while self.map.len() > self.capacity {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            self.map.remove(&old);
        }
        glyph
    }

    fn touch(&mut self, key: &GlyphCacheKey) {
        if let Some(pos) = self.order.iter().position(|candidate| candidate == key) {
            self.order.remove(pos);
            self.order.push_back(key.clone());
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct RasterizedGlyph {
    key: GlyphCacheKey,
    width: u32,
    height: u32,
    stride: usize,
    fill_alpha: Vec<u8>,
    outline_alpha: Vec<u8>,
    offset_x: f32,
    offset_y: f32,
    advance: f32,
}

impl RasterizedGlyph {
    fn placeholder(key: GlyphCacheKey) -> Self {
        Self::empty(key, 0.0)
    }

    fn empty(key: GlyphCacheKey, advance: f32) -> Self {
        Self {
            key,
            width: 0,
            height: 0,
            stride: 0,
            fill_alpha: Vec::new(),
            outline_alpha: Vec::new(),
            offset_x: 0.0,
            offset_y: 0.0,
            advance,
        }
    }

    fn has_bitmap(&self) -> bool {
        self.width > 0
            && self.height > 0
            && self.stride >= self.width as usize
            && self.fill_alpha.len() >= self.required_len()
            && self.outline_alpha.len() >= self.required_len()
    }

    fn required_len(&self) -> usize {
        self.stride.saturating_mul(self.height as usize)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct PackedGlyph {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[derive(Debug)]
struct PersistentGlyphAtlas {
    width: u32,
    height: u32,
    stride: usize,
    cursor_x: u32,
    cursor_y: u32,
    row_height: u32,
    fill_alpha: Vec<u8>,
    outline_alpha: Vec<u8>,
    packed: HashMap<GlyphCacheKey, PackedGlyph>,
    version: u64,
    dirty: bool,
    dirty_bounds: Option<[u32; 4]>,
    snapshot: Option<Arc<DanmakuGlyphAtlas>>,
}

impl PersistentGlyphAtlas {
    fn new() -> Self {
        let width = GLYPH_ATLAS_WIDTH;
        let height = GLYPH_ATLAS_INITIAL_HEIGHT;
        let stride = width as usize;
        let mut atlas = Self {
            width,
            height,
            stride,
            // 从预留区之后开始排字，纯白像素区永远不被字形覆盖
            cursor_x: SELF_BG_ATLAS_RESERVE,
            cursor_y: 0,
            row_height: 0,
            fill_alpha: vec![0; stride * height as usize],
            outline_alpha: vec![0; stride * height as usize],
            packed: HashMap::new(),
            version: 1,
            dirty: true,
            dirty_bounds: None,
            snapshot: None,
        };
        atlas.reserve_self_bg_pixel();
        atlas
    }

    fn pack(&mut self, glyph: &RasterizedGlyph) -> PackedGlyph {
        if let Some(packed) = self.packed.get(&glyph.key).copied() {
            return packed;
        }
        let glyph_width = glyph.width.min(self.width);
        let glyph_height = glyph.height;
        if self.cursor_x + glyph_width > self.width {
            // 换行必须回到**预留区之后**，不是 0。
            //
            // `[0, SELF_BG_ATLAS_RESERVE)` 是自发言弹幕背景框的纯白像素来源
            // （`reserve_self_bg_pixel` 填充、`append_self_background` 以
            // `tex_rect = [0, 0, reserve/w, 1/h]` 采样）。`new()` 里
            // `cursor_x` 从预留区之后起排就是为了"纯白像素区永远不被字形覆盖"
            // （见本字段初始化处的注释），而这里原来写的是 `cursor_x = 0`
            // —— 图集一旦换行，字形就从 x=0 开始排，**直接把预留区覆盖掉**。
            // 后果：自发言弹幕的背景框采样到的不是纯白条带，而是字形像素，
            // 框渲染成一块半透明、脏兮兮的矩形（实测现象）。
            self.cursor_x = SELF_BG_ATLAS_RESERVE;
            self.cursor_y += self.row_height;
            self.row_height = 0;
        }
        self.ensure_height(self.cursor_y + glyph_height);
        let packed = PackedGlyph {
            x: self.cursor_x,
            y: self.cursor_y,
            width: glyph_width,
            height: glyph_height,
        };
        for row in 0..glyph_height as usize {
            let src = row * glyph.stride;
            let dst = (packed.y as usize + row) * self.stride + packed.x as usize;
            let len = glyph_width as usize;
            self.fill_alpha[dst..dst + len].copy_from_slice(&glyph.fill_alpha[src..src + len]);
            self.outline_alpha[dst..dst + len]
                .copy_from_slice(&glyph.outline_alpha[src..src + len]);
        }
        self.cursor_x += glyph_width;
        self.row_height = self.row_height.max(glyph_height);
        self.packed.insert(glyph.key, packed);
        self.version = self.version.saturating_add(1).max(1);
        self.dirty = true;
        if packed.width > 0 && packed.height > 0 {
            let right = packed.x.saturating_add(packed.width);
            let bottom = packed.y.saturating_add(packed.height);
            self.dirty_bounds = Some(match self.dirty_bounds {
                Some([left, top, old_right, old_bottom]) => [
                    left.min(packed.x),
                    top.min(packed.y),
                    old_right.max(right),
                    old_bottom.max(bottom),
                ],
                None => [packed.x, packed.y, right, bottom],
            });
        }
        packed
    }

    fn ensure_height(&mut self, required: u32) {
        if required > GLYPH_ATLAS_MAX_HEIGHT {
            // A single glyph taller than the cap (extreme font sizes). Restart
            // the atlas rather than grow past the limit; the glyph is
            // re-packed on the next call. Consumers always read the current
            // buffers through the latest snapshot, so this is a re-pack, not a
            // rendering change.
            self.reset_atlas();
            return;
        }
        if required <= self.height {
            return;
        }
        while required > self.height {
            self.height *= 2;
        }
        let new_len = self.stride * self.height as usize;
        self.fill_alpha.resize(new_len, 0);
        self.outline_alpha.resize(new_len, 0);
        self.dirty = true;
    }

    /// 把 atlas 第 0 行前 `SELF_BG_ATLAS_RESERVE` 列写成纯白（fill=255）并
    /// 记录进 dirty_bounds，确保增量上传/全量重建都能把这块区域送进 GPU。
    /// 必须在字形开始排入之前调用（`new` 与 `reset_atlas`）。
    fn reserve_self_bg_pixel(&mut self) {
        let n = (SELF_BG_ATLAS_RESERVE as usize).min(self.stride);
        self.fill_alpha[..n].fill(255);
        self.outline_alpha[..n].fill(0);
        self.dirty_bounds = Some(match self.dirty_bounds {
            Some([l, t, r, b]) => [l.min(0), t.min(0), r.max(SELF_BG_ATLAS_RESERVE), b.max(1)],
            None => [0, 0, SELF_BG_ATLAS_RESERVE, 1],
        });
        self.dirty = true;
    }

    /// Drop all packed glyphs and reset the buffers to the initial height.
    /// Called when the atlas would otherwise grow past `GLYPH_ATLAS_MAX_HEIGHT`
    /// and whenever a fresh rasterizer generation starts, so the atlas memory
    /// stays bounded for the session.
    fn reset_atlas(&mut self) {
        self.height = GLYPH_ATLAS_INITIAL_HEIGHT;
        let new_len = self.stride * self.height as usize;
        self.fill_alpha.clear();
        self.fill_alpha.resize(new_len, 0);
        self.outline_alpha.clear();
        self.outline_alpha.resize(new_len, 0);
        self.cursor_x = SELF_BG_ATLAS_RESERVE;
        self.cursor_y = 0;
        self.row_height = 0;
        self.packed.clear();
        self.version = self.version.saturating_add(1).max(1);
        self.dirty = true;
        self.dirty_bounds = None;
        self.snapshot = None;
        self.reserve_self_bg_pixel();
    }

    fn snapshot(&mut self) -> Option<Arc<DanmakuGlyphAtlas>> {
        if self.packed.is_empty() {
            return None;
        }
        if self.dirty || self.snapshot.is_none() {
            let update = self.snapshot.as_ref().and_then(|snapshot| {
                self.dirty_bounds
                    .map(|[left, top, right, bottom]| DanmakuAtlasUpdate {
                        from_version: snapshot.version,
                        x: left,
                        y: top,
                        width: right.saturating_sub(left),
                        height: bottom.saturating_sub(top),
                    })
            });
            self.snapshot = Some(Arc::new(DanmakuGlyphAtlas {
                width: self.width,
                height: self.height,
                stride: self.stride,
                fill_alpha: self.fill_alpha.clone(),
                outline_alpha: self.outline_alpha.clone(),
                version: self.version,
                update,
            }));
            self.dirty = false;
            self.dirty_bounds = None;
        }
        self.snapshot.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextMeasure {
    pub width: f32,
    pub height: f32,
    pub ascent: f32,
    pub descent: f32,
}

#[derive(Debug, Clone)]
pub struct DfmLayoutEngine {
    timeline: DanmakuTimeline,
    config: DanmakuLayoutConfig,
    rasterizer: DanmakuTextRasterizer,
    font_selection: DanmakuFontSelection,
    prepared: Option<DfmPreparedLayout>,
    stable_tracks: HashMap<u64, usize>,
    rejected_items: BTreeSet<u64>,
    stable_viewport: Option<DanmakuViewport>,
}

impl DfmLayoutEngine {
    pub fn new(timeline: DanmakuTimeline, config: DanmakuLayoutConfig) -> Self {
        let config = config.sanitized();
        let rasterizer = DanmakuTextRasterizer::for_config(&config);
        Self {
            timeline,
            config,
            rasterizer,
            font_selection: DanmakuFontSelection::default(),
            prepared: None,
            stable_tracks: HashMap::new(),
            rejected_items: BTreeSet::new(),
            stable_viewport: None,
        }
    }

    pub fn set_timeline(&mut self, timeline: DanmakuTimeline) {
        self.timeline = timeline;
        self.prepared = None;
        self.invalidate_placement_history();
    }

    pub fn sync_timeline(&mut self, timeline: &DanmakuTimeline) {
        if self.timeline != *timeline {
            self.timeline = timeline.clone();
            self.prepared = None;
        }
    }

    pub fn clear_timeline(&mut self) {
        self.timeline = DanmakuTimeline::default();
        self.prepared = None;
        self.invalidate_placement_history();
    }

    pub fn set_config(&mut self, config: DanmakuLayoutConfig) -> bool {
        self.apply_config(config) != DanmakuConfigChange::Unchanged
    }

    pub(crate) fn apply_config(&mut self, config: DanmakuLayoutConfig) -> DanmakuConfigChange {
        let config = config.sanitized();
        if self.config == config {
            return DanmakuConfigChange::Unchanged;
        }
        // Hiding can reuse the prepared geometry and suppress it immediately.
        // Showing cannot assume that geometry still exists because planner
        // windows prepared while disabled intentionally contain no items.
        let re_enabling = !self.config.enabled && config.enabled;
        let layout_changed = re_enabling || !self.config.layout_equivalent(&config);
        let font_changed = self.config.custom_font_family != config.custom_font_family
            || self.config.custom_font_file_path != config.custom_font_file_path
            || self.config.custom_font_face_index != config.custom_font_face_index;
        self.config = config;
        if font_changed {
            self.rasterizer =
                DanmakuTextRasterizer::for_config_and_selection(&self.config, &self.font_selection);
        }
        if layout_changed {
            self.prepared = None;
            self.rejected_items.clear();
            // Track assignments are preferences, not hard constraints. Keeping
            // them across font-size and other layout changes lets overlapping
            // items stay in the same logical lane; the retainer will reject a
            // preference when the new geometry no longer fits.
            DanmakuConfigChange::Layout
        } else {
            if let Some(prepared) = &mut self.prepared {
                prepared.apply_paint_config(&self.config);
            }
            DanmakuConfigChange::PaintOnly
        }
    }

    pub fn set_font_selection(&mut self, selection: DanmakuFontSelection) -> bool {
        if self.font_selection.generation == selection.generation {
            return false;
        }
        self.rasterizer = DanmakuTextRasterizer::for_config_and_selection(&self.config, &selection);
        self.font_selection = selection;
        self.prepared = None;
        self.invalidate_placement_history();
        true
    }

    pub fn config(&self) -> &DanmakuLayoutConfig {
        &self.config
    }

    pub(crate) fn rasterizer_clone(&self) -> DanmakuTextRasterizer {
        self.rasterizer.clone()
    }

    pub(crate) fn set_config_with_rasterizer(
        &mut self,
        config: DanmakuLayoutConfig,
        rasterizer: DanmakuTextRasterizer,
    ) {
        if self.apply_config(config) == DanmakuConfigChange::Layout {
            // The async planner must pack glyphs into the same atlas that the
            // presenter later snapshots. This is especially important after a
            // custom-font change creates a fresh rasterizer generation.
            self.rasterizer = rasterizer;
        }
    }

    pub fn prepare(&mut self, viewport: DanmakuViewport, _generation: u64) -> DfmPreparedLayout {
        if self.stable_viewport != Some(viewport) {
            self.invalidate_placement_history();
            self.stable_viewport = Some(viewport);
        }
        let config = self.config.sanitized();
        let planned_ids = self
            .timeline
            .items()
            .iter()
            .filter(|item| item.mode != DanmakuMode::Special)
            .filter(|item| !self.rejected_items.contains(&item.id))
            .map(|item| item.id)
            .collect::<BTreeSet<_>>();
        let prepared = prepare_layout(
            &self.timeline,
            viewport,
            &config,
            &self.rasterizer,
            &self.stable_tracks,
            &self.rejected_items,
        );
        let active_ids = prepared
            .items()
            .iter()
            .map(|item| item.id)
            .collect::<BTreeSet<_>>();
        if config.enabled {
            // A sliding window may forget older track occupancy, but it must not
            // revise an item's original admission decision. Otherwise an item
            // dropped at birth can reappear halfway across the screen.
            self.rejected_items
                .extend(planned_ids.difference(&active_ids).copied());
        }
        self.stable_tracks
            .retain(|item_id, _| active_ids.contains(item_id));
        for item in prepared.items() {
            self.stable_tracks.insert(item.id, item.track_index);
        }
        self.prepared = Some(prepared.clone());
        prepared
    }

    pub fn invalidate_placement_history(&mut self) {
        self.stable_tracks.clear();
        self.rejected_items.clear();
        self.stable_viewport = None;
    }

    pub fn frame_layout(
        &mut self,
        media_time: Duration,
        viewport: DanmakuViewport,
        generation: u64,
    ) -> DanmakuFrameLayout {
        let generation = generation.max(DEFAULT_GENERATION);
        let needs_prepare = self.prepared.as_ref().is_none_or(|prepared| {
            prepared.viewport != viewport || prepared.config != self.config.sanitized()
        });
        if needs_prepare {
            self.prepare(viewport, generation);
        }
        self.prepared
            .as_ref()
            .expect("prepared layout exists")
            .frame_layout(media_time, generation)
    }

    pub fn render_prepared_plan(
        &self,
        prepared: &DfmPreparedLayout,
        media_time: Duration,
        generation: u64,
    ) -> DanmakuRenderPlan {
        let layout = prepared.frame_layout(media_time, generation);
        self.rasterizer.render_plan(&layout)
    }

    pub fn render_plan(
        &mut self,
        media_time: Duration,
        viewport: DanmakuViewport,
        generation: u64,
    ) -> DanmakuRenderPlan {
        let layout = self.frame_layout(media_time, viewport, generation);
        self.rasterizer.render_plan(&layout)
    }
}

fn prepare_layout(
    timeline: &DanmakuTimeline,
    viewport: DanmakuViewport,
    config: &DanmakuLayoutConfig,
    rasterizer: &DanmakuTextRasterizer,
    stable_tracks: &HashMap<u64, usize>,
    rejected_items: &BTreeSet<u64>,
) -> DfmPreparedLayout {
    if timeline.is_empty() || !config.enabled {
        let dfm_layout = dfm::PreparedLayout {
            width: viewport.width as f64,
            height: viewport.height as f64,
            scroll_duration_seconds: config.scroll_duration_seconds.max(1.0) as f64,
            static_duration_seconds: DEFAULT_STATIC_DURATION.as_secs_f64(),
            items: Vec::new(),
            item_times: Vec::new(),
            track_count: 0,
            rule_errors: Vec::new(),
            filter_breakdown: DanmakuFilterBreakdown::default(),
        };
        return DfmPreparedLayout {
            viewport,
            config: config.clone(),
            dfm_layout,
            stats: DanmakuPreparedStats::default(),
            scroll_duration: DEFAULT_SCROLL_DURATION,
            static_duration: DEFAULT_STATIC_DURATION,
            item_times: Vec::new(),
            items: Vec::new(),
        };
    }
    let scroll_duration = scroll_duration_for_viewport(config, viewport);
    let base_font_size = effective_config_font_size(config.font_size, viewport.scale_factor);
    let source_count = timeline.items().len();
    let mut source_items = Vec::new();
    for source in timeline.items() {
        if source.mode == DanmakuMode::Special || rejected_items.contains(&source.id) {
            continue;
        }
        let font_size =
            effective_font_size(source.font_size, config.font_size, viewport.scale_factor);
        let measure = rasterizer.measure(&source.text, font_size);
        source_items.push(dfm::PrepareItem {
            source_id: source.id,
            time_seconds: source.pts.as_secs_f64(),
            text: source.text.clone(),
            type_code: mode_to_dfm_type_code(source.mode),
            color_argb: color_to_argb(source.color, source.opacity),
            opacity: source.opacity,
            is_me: source.is_self,
            font_size,
            paint_width: measure.width as f64,
            paint_height: measure.height.max(font_size * 1.2) as f64,
            user_id: source.user_id.clone(),
            user_name: source.user_name.clone(),
            user_level: source.user_level,
        });
    }
    let supported_count = source_items.len();

    let dfm_layout = dfm::prepare_layout(dfm::PrepareRequest {
        items: source_items,
        width: viewport.width as f64,
        height: viewport.height as f64,
        font_size: base_font_size as f64,
        display_area: config.display_area as f64,
        scroll_duration_seconds: scroll_duration.as_secs_f64(),
        allow_stacking: config.allow_stacking,
        allow_scroll_overwrite: config.allow_scroll_overwrite,
        merge_danmaku: config.merge_duplicates,
        max_quantity: config.max_quantity,
        max_lines_per_type: config.max_lines_per_mode,
        track_gap_ratio: config.track_gap_ratio as f64,
        outline_width: config.outline_width as f64,
        preferred_tracks: stable_tracks,
        block_words: config.block_words.clone(),
        block_top: config.block_top,
        block_bottom: config.block_bottom,
        block_scroll: config.block_scroll,
    })
    .unwrap_or_else(|_| dfm::PreparedLayout {
        width: viewport.width as f64,
        height: viewport.height as f64,
        scroll_duration_seconds: scroll_duration.as_secs_f64(),
        static_duration_seconds: DEFAULT_STATIC_DURATION.as_secs_f64(),
        items: Vec::new(),
        item_times: Vec::new(),
        track_count: 0,
        rule_errors: Vec::new(),
        filter_breakdown: DanmakuFilterBreakdown::default(),
    });

    let prepared = dfm_layout
        .items
        .iter()
        .map(|item| {
            let text: Arc<str> = Arc::from(item.text.as_str());
            let font_size = item.font_size as f32;
            let metrics = rasterizer.measure(&item.text, font_size);
            let outline_width = resolve_width_px(font_size, config.outline_width);
            let text_layout = rasterizer.prepare_text_layout_with_metrics(
                Arc::clone(&text),
                font_size,
                outline_width,
                metrics,
            );
            PreparedDanmakuItem {
                id: item.source_id,
                time: Duration::from_secs_f64(item.time_seconds.max(0.0)),
                text,
                mode: dfm_type_code_to_mode(item.type_code),
                color: DanmakuColor::from_argb(item.color_argb as u32),
                opacity: item.opacity,
                font_size,
                width: item.width as f32,
                height: item.height as f32,
                y: item.y_position as f32,
                track_index: item.track_index.max(0) as usize,
                scroll_speed: item.scroll_speed as f32,
                duration: Duration::from_secs_f64(item.duration_seconds.max(0.0)),
                duplicate_count: item.duplicate_count,
                is_self: item.is_me,
                text_layout,
            }
        })
        .collect::<Vec<_>>();

    let item_times = prepared
        .iter()
        .map(|item| item.time.as_secs_f64())
        .collect();
    let stats = DanmakuPreparedStats::from_items(
        source_count,
        supported_count,
        &prepared,
        viewport,
        config,
        dfm_layout.track_count.max(0) as usize,
        dfm_layout.filter_breakdown,
    );
    DfmPreparedLayout {
        viewport,
        config: config.clone(),
        dfm_layout,
        stats,
        scroll_duration,
        static_duration: DEFAULT_STATIC_DURATION,
        item_times,
        items: prepared,
    }
}

pub fn parse_bilibili_xml(input: &str) -> Result<Vec<DanmakuItem>> {
    let mut items = Vec::new();
    for (index, segment) in input.split("<d ").skip(1).enumerate() {
        let Some(p_start) = segment.find("p=\"") else {
            continue;
        };
        let p_rest = &segment[p_start + 3..];
        let Some(p_end) = p_rest.find('"') else {
            continue;
        };
        let fields = p_rest[..p_end].split(',').collect::<Vec<_>>();
        let Some(text_start) = segment.find('>') else {
            continue;
        };
        let Some(text_end) = segment[text_start + 1..].find("</d>") else {
            continue;
        };
        let text = decode_xml_entities(&segment[text_start + 1..text_start + 1 + text_end]);
        if text.trim().is_empty() {
            continue;
        }
        let mode_code = fields
            .get(1)
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(1);
        let mode = DanmakuMode::from_bilibili_mode(mode_code);
        // mode 7（高级弹幕）的文本体是「[起点X,起点Y,"透明度",生存时间,"正文",…]」，
        // 把它当普通文字写出来就是一段乱码。Erika 目前没有高级弹幕的路径渲染
        // （`prepare_layout` 同样跳过 `DanmakuMode::Special`，`dfm.rs` 的轨道分配
        // 也没有 Special 分支），因此这里丢弃整条。代价是这一条不会进入任何统计
        // 计数（解析阶段就没了），调试时只能从「XML 条数 - 时间线条数」反推。
        if mode == DanmakuMode::Special {
            continue;
        }
        let Ok(pts) = parse_float_field(fields.first(), "pts") else {
            continue;
        };
        items.push(DanmakuItem {
            id: fields
                .get(7)
                .and_then(|value| value.parse().ok())
                .unwrap_or(index as u64 + 1),
            pts: Duration::from_secs_f64(pts.max(0.0)),
            mode,
            font_size: fields
                .get(2)
                .and_then(|value| value.parse().ok())
                .unwrap_or(DEFAULT_SOURCE_FONT_SIZE),
            color: DanmakuColor::from_decimal(
                fields
                    .get(3)
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(0xffffff),
            ),
            opacity: 1.0,
            is_self: false,
            // `p` 第 7 段（下标 6）是发送者 hash。Erika 早期实现整体丢弃了它，
            // 于是 `<标识>` 屏蔽规则在 XML 来源上永远匹配不到任何东西。
            user_id: fields
                .get(6)
                .map(|value| value.trim().to_string())
                .unwrap_or_default(),
            // B 站点播 XML 的 `p` 里没有用户名与等级。
            user_name: String::new(),
            user_level: 0,
            text,
        });
    }
    Ok(items)
}

fn should_skip_item_error(error: &DanmakuError) -> bool {
    matches!(
        error,
        DanmakuError::MissingText | DanmakuError::InvalidField(_)
    )
}

impl DanmakuColor {
    fn from_decimal(value: u32) -> Self {
        Self::rgb_u8(
            ((value >> 16) & 0xff) as u8,
            ((value >> 8) & 0xff) as u8,
            (value & 0xff) as u8,
        )
    }

    fn from_argb(value: u32) -> Self {
        Self {
            red: ((value >> 16) & 0xff) as f32 / 255.0,
            green: ((value >> 8) & 0xff) as f32 / 255.0,
            blue: (value & 0xff) as f32 / 255.0,
            alpha: ((value >> 24) & 0xff) as f32 / 255.0,
        }
    }
}

fn mode_to_dfm_type_code(mode: DanmakuMode) -> i32 {
    match mode {
        DanmakuMode::Scroll => 1,
        DanmakuMode::ScrollReverse => 6,
        DanmakuMode::Top => 5,
        DanmakuMode::Bottom => 4,
        DanmakuMode::Special => 7,
    }
}

fn dfm_type_code_to_mode(type_code: i32) -> DanmakuMode {
    DanmakuMode::from_bilibili_mode(type_code.max(0) as u32)
}

fn color_to_argb(color: DanmakuColor, opacity: f32) -> i32 {
    let alpha = (color.alpha * opacity)
        .clamp(0.0, 1.0)
        .mul_add(255.0, 0.0)
        .round() as u32;
    let red = (color.red.clamp(0.0, 1.0) * 255.0).round() as u32;
    let green = (color.green.clamp(0.0, 1.0) * 255.0).round() as u32;
    let blue = (color.blue.clamp(0.0, 1.0) * 255.0).round() as u32;
    ((alpha << 24) | (red << 16) | (green << 8) | blue) as i32
}

fn item_from_json_value(value: &Value, fallback_id: u64) -> Result<DanmakuItem> {
    let object = value
        .as_object()
        .ok_or_else(|| DanmakuError::InvalidField("comment".to_string()))?;
    let text = string_field(object, &["content", "text", "c"]).ok_or(DanmakuError::MissingText)?;
    let mode = string_field(object, &["type", "mode", "y"])
        .map(|value| DanmakuMode::from_text(&value))
        .or_else(|| {
            numeric_field(object, &["type_code", "mode_code"])
                .map(|v| DanmakuMode::from_bilibili_mode(v as u32))
        })
        .unwrap_or(DanmakuMode::Scroll);
    let color = object
        .get("color")
        .or_else(|| object.get("r"))
        .and_then(parse_color_value)
        .unwrap_or(DanmakuColor::WHITE);
    Ok(DanmakuItem {
        id: u64_field(object, &["id"]).unwrap_or(fallback_id),
        pts: Duration::from_secs_f64(
            numeric_field(object, &["time", "t"])
                .unwrap_or(0.0)
                .max(0.0),
        ),
        text,
        mode,
        font_size: numeric_field(object, &["font_size", "fontSize", "size", "s"])
            .unwrap_or(DEFAULT_SOURCE_FONT_SIZE as f64) as f32,
        color,
        opacity: numeric_field(object, &["opacity", "alpha", "a"]).unwrap_or(1.0) as f32,
        is_self: bool_field(object, &["is_me", "isMe", "self", "mine"]).unwrap_or(false),
        // 发送者元数据：JSON 来源可以带，键名对齐 B 站与录播姬两套叫法。
        // `uid` 与 `user_hash` 都落进同一个 `user_id`——内核只做原样字符串比较，
        // 不关心来源给的是 hash 还是数值 uid。
        user_id: string_field(object, &["user_hash", "userHash", "uid", "mid", "user_id"])
            .unwrap_or_default(),
        user_name: string_field(object, &["user_name", "userName", "uname", "name"])
            .unwrap_or_default(),
        user_level: u64_field(object, &["user_level", "userLevel", "level"])
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32,
    })
}

fn string_field(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .and_then(|value| match value {
            Value::String(value) => Some(value.clone()),
            Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
}

fn numeric_field(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<f64> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .and_then(|value| match value {
            Value::Number(value) => value.as_f64(),
            Value::String(value) => value.parse().ok(),
            _ => None,
        })
}

fn u64_field(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .and_then(|value| match value {
            Value::Number(value) => value.as_u64(),
            Value::String(value) => value.parse().ok(),
            _ => None,
        })
}

fn bool_field(object: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<bool> {
    keys.iter()
        .find_map(|key| object.get(*key))
        .and_then(|value| match value {
            Value::Bool(value) => Some(*value),
            Value::Number(value) => Some(value.as_u64().unwrap_or(0) != 0),
            Value::String(value) => matches!(value.as_str(), "true" | "1" | "yes").then_some(true),
            _ => None,
        })
}

fn parse_color_value(value: &Value) -> Option<DanmakuColor> {
    match value {
        Value::Number(value) => value
            .as_u64()
            .map(|value| DanmakuColor::from_decimal(value as u32)),
        Value::String(value) => parse_color_string(value),
        _ => None,
    }
}

fn parse_color_string(value: &str) -> Option<DanmakuColor> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        let hex = hex.strip_prefix("0x").unwrap_or(hex);
        let rgb = u32::from_str_radix(hex, 16).ok()?;
        return Some(DanmakuColor::from_decimal(rgb));
    }
    if let Some(inner) = value.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let parts = inner
            .split(',')
            .map(|part| part.trim().parse::<u8>().ok())
            .collect::<Option<Vec<_>>>()?;
        if parts.len() >= 3 {
            return Some(DanmakuColor::rgb_u8(parts[0], parts[1], parts[2]));
        }
    }
    value.parse::<u32>().ok().map(DanmakuColor::from_decimal)
}

fn parse_float_field(field: Option<&&str>, name: &'static str) -> Result<f64> {
    field
        .ok_or_else(|| DanmakuError::InvalidField(name.to_string()))?
        .parse::<f64>()
        .map_err(|_| DanmakuError::InvalidField(name.to_string()))
}

fn decode_xml_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

pub(crate) fn scroll_duration_for_viewport(
    config: &DanmakuLayoutConfig,
    viewport: DanmakuViewport,
) -> Duration {
    let duration_seconds = sanitize_f32(config.scroll_duration_seconds, 10.0).max(1.0);
    let speed_factor = sanitize_f32(config.scroll_speed_factor, 1.0).max(0.001);
    let logical_width = viewport.width as f32 / viewport.scale_factor.max(0.001);
    let width_scale = (logical_width / SCROLL_DURATION_REFERENCE_LOGICAL_WIDTH).clamp(
        SCROLL_DURATION_MIN_WIDTH_SCALE,
        SCROLL_DURATION_MAX_WIDTH_SCALE,
    );
    let millis = duration_seconds * width_scale * 1000.0 / speed_factor;
    Duration::from_millis(millis.max(1.0).round() as u64)
}

fn scale_offset(offset: [f32; 2], scale_factor: f32) -> [f32; 2] {
    let scale_factor = sanitize_f32(scale_factor, 1.0).max(0.001);
    [offset[0] * scale_factor, offset[1] * scale_factor]
}

fn shadow_style_alpha(style: DanmakuShadowStyle) -> f32 {
    match style {
        DanmakuShadowStyle::None => 0.0,
        DanmakuShadowStyle::Soft => 0.34,
        DanmakuShadowStyle::Medium => 0.44,
        DanmakuShadowStyle::Strong => 0.55,
    }
}

fn apply_track_offset(pts: Duration, offset_micros: i64) -> Option<Duration> {
    if offset_micros >= 0 {
        return Some(pts.saturating_add(Duration::from_micros(offset_micros as u64)));
    }
    pts.checked_sub(Duration::from_micros(offset_micros.unsigned_abs()))
}

fn sanitize_f32(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

fn load_configured_font(family: &str, file_path: &str, face_index: u32) -> Option<FontArc> {
    let file_path = file_path.trim();
    if !file_path.is_empty() {
        match load_font_from_path(Path::new(file_path), face_index) {
            Ok(font) => return Some(font),
            Err(error) => {
                crate::trace::diagnostic(
                    serde_json::json!({
                        "event": "danmaku_custom_font",
                        "stage": "rejected",
                        "path": file_path,
                        "faceIndex": face_index,
                        "bytes": error.bytes,
                        "maxBytes": MAX_CUSTOM_DANMAKU_FONT_BYTES,
                        "reason": error.reason,
                        "detail": error.detail,
                        "fallback": if family.trim().is_empty() { "default" } else { "family_then_default" },
                    })
                    .to_string(),
                );
            }
        }
    }

    let family = family.trim();
    if !family.is_empty() {
        if let Some(font) = load_font_family(family) {
            return Some(font);
        }
    }

    load_default_font()
}

fn load_selected_fonts(
    attachments: &[SubtitleFontAttachment],
    first_font_id: u32,
) -> Vec<DanmakuFontFace> {
    let mut fonts = Vec::new();
    for attachment in attachments {
        let mut database = fontdb::Database::new();
        database.load_font_source(fontdb::Source::Binary(Arc::new(attachment.data.clone())));
        let face_indices = database.faces().map(|face| face.index).collect::<Vec<_>>();
        for face_index in face_indices {
            let bytes = owned_font_bytes(attachment.data.clone());
            if let Some(font) = load_shared_font_data(bytes, face_index) {
                let id = first_font_id.saturating_add(fonts.len() as u32);
                fonts.push(DanmakuFontFace::new(id, font));
            }
        }
    }
    fonts
}

#[derive(Debug, PartialEq, Eq)]
struct CustomDanmakuFontError {
    reason: &'static str,
    detail: String,
    bytes: u64,
}

fn load_font_from_path(
    path: &Path,
    face_index: u32,
) -> std::result::Result<FontArc, CustomDanmakuFontError> {
    let metadata = fs::metadata(path).map_err(|error| CustomDanmakuFontError {
        reason: "metadata_failed",
        detail: error.to_string(),
        bytes: 0,
    })?;
    let bytes = metadata.len();
    if !metadata.is_file() {
        return Err(CustomDanmakuFontError {
            reason: "not_regular_file",
            detail: "custom font path is not a regular file".to_string(),
            bytes,
        });
    }
    if bytes > MAX_CUSTOM_DANMAKU_FONT_BYTES {
        return Err(CustomDanmakuFontError {
            reason: "file_too_large",
            detail: format!(
                "custom font exceeds {} byte limit",
                MAX_CUSTOM_DANMAKU_FONT_BYTES
            ),
            bytes,
        });
    }

    let file = File::open(path).map_err(|error| CustomDanmakuFontError {
        reason: "read_failed",
        detail: error.to_string(),
        bytes,
    })?;
    let mut data = Vec::with_capacity(usize::try_from(bytes).unwrap_or(0));
    file.take(MAX_CUSTOM_DANMAKU_FONT_BYTES + 1)
        .read_to_end(&mut data)
        .map_err(|error| CustomDanmakuFontError {
            reason: "read_failed",
            detail: error.to_string(),
            bytes,
        })?;
    if data.len() as u64 > MAX_CUSTOM_DANMAKU_FONT_BYTES {
        return Err(CustomDanmakuFontError {
            reason: "file_too_large",
            detail: format!(
                "custom font exceeds {} byte limit",
                MAX_CUSTOM_DANMAKU_FONT_BYTES
            ),
            bytes: data.len() as u64,
        });
    }
    if let Some(font) = load_font_data(data.clone(), face_index) {
        return Ok(font);
    }

    let mut db = fontdb::Database::new();
    db.load_font_data(data);
    db.faces()
        .find(|face| face.index == face_index)
        .and_then(|face| load_fontdb_face(&db, face.id))
        .ok_or_else(|| CustomDanmakuFontError {
            reason: "invalid_font",
            detail: format!("font contains no loadable face at index {face_index}"),
            bytes,
        })
}

fn load_font_family(family: &str) -> Option<FontArc> {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let query = fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight: fontdb::Weight::NORMAL,
        stretch: fontdb::Stretch::Normal,
        style: fontdb::Style::Normal,
    };
    db.query(&query).and_then(|id| load_fontdb_face(&db, id))
}

fn load_fontdb_face(db: &fontdb::Database, id: fontdb::ID) -> Option<FontArc> {
    db.with_face_data(id, |data, face_index| {
        load_font_data(data.to_vec(), face_index)
    })
    .flatten()
}

fn load_font_data(data: Vec<u8>, face_index: u32) -> Option<FontArc> {
    FontVec::try_from_vec_and_index(data, face_index)
        .map(FontArc::new)
        .ok()
}

#[derive(Clone)]
struct SharedFont {
    // This field must be dropped before `data`, because its internal references
    // point into the allocation kept alive by `data`.
    font: FontRef<'static>,
    data: FontBytes,
}

impl SharedFont {
    fn try_new(data: FontBytes, face_index: u32) -> Option<Self> {
        // SAFETY: `bytes` points into the allocation owned by `data`. The handle
        // is stored in the same value and is declared after `font`, so it
        // remains alive until all references held by FontRef have been dropped.
        let source: &[u8] = font_bytes_slice(&data);
        let bytes = unsafe { std::slice::from_raw_parts(source.as_ptr(), source.len()) };
        let font = FontRef::try_from_slice_and_index(bytes, face_index).ok()?;
        Some(Self { font, data })
    }
}

impl Font for SharedFont {
    fn units_per_em(&self) -> Option<f32> {
        self.font.units_per_em()
    }

    fn ascent_unscaled(&self) -> f32 {
        self.font.ascent_unscaled()
    }

    fn descent_unscaled(&self) -> f32 {
        self.font.descent_unscaled()
    }

    fn line_gap_unscaled(&self) -> f32 {
        self.font.line_gap_unscaled()
    }

    fn italic_angle(&self) -> f32 {
        self.font.italic_angle()
    }

    fn glyph_id(&self, c: char) -> GlyphId {
        self.font.glyph_id(c)
    }

    fn h_advance_unscaled(&self, id: GlyphId) -> f32 {
        self.font.h_advance_unscaled(id)
    }

    fn h_side_bearing_unscaled(&self, id: GlyphId) -> f32 {
        self.font.h_side_bearing_unscaled(id)
    }

    fn v_advance_unscaled(&self, id: GlyphId) -> f32 {
        self.font.v_advance_unscaled(id)
    }

    fn v_side_bearing_unscaled(&self, id: GlyphId) -> f32 {
        self.font.v_side_bearing_unscaled(id)
    }

    fn kern_unscaled(&self, first: GlyphId, second: GlyphId) -> f32 {
        self.font.kern_unscaled(first, second)
    }

    fn outline(&self, id: GlyphId) -> Option<Outline> {
        self.font.outline(id)
    }

    fn glyph_count(&self) -> usize {
        self.font.glyph_count()
    }

    fn codepoint_ids(&self) -> CodepointIdIter<'_> {
        self.font.codepoint_ids()
    }

    fn glyph_raster_image2(&self, id: GlyphId, pixel_size: u16) -> Option<v2::GlyphImage<'_>> {
        self.font.glyph_raster_image2(id, pixel_size)
    }

    fn glyph_svg_image(&self, id: GlyphId) -> Option<GlyphSvg<'_>> {
        self.font.glyph_svg_image(id)
    }

    fn font_data(&self) -> &[u8] {
        font_bytes_slice(&self.data)
    }
}

fn load_shared_font_data(data: FontBytes, face_index: u32) -> Option<FontArc> {
    SharedFont::try_new(data, face_index).map(FontArc::new)
}

fn load_default_font() -> Option<FontArc> {
    if let Ok(font) = FontArc::try_from_slice(NIPAPLAY_FALLBACK_FONT) {
        return Some(font);
    }

    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    let families = [
        fontdb::Family::Name("PingFang SC"),
        fontdb::Family::Name("Hiragino Sans GB"),
        fontdb::Family::Name("Microsoft YaHei"),
        fontdb::Family::Name("Noto Sans CJK SC"),
        fontdb::Family::Name("Noto Sans CJK"),
        fontdb::Family::Name("Source Han Sans SC"),
        fontdb::Family::SansSerif,
    ];
    for family in families {
        let query = fontdb::Query {
            families: &[family],
            weight: fontdb::Weight::NORMAL,
            stretch: fontdb::Stretch::Normal,
            style: fontdb::Style::Normal,
        };
        if let Some(font) = db.query(&query).and_then(|id| load_fontdb_face(&db, id)) {
            return Some(font);
        }
    }
    const CANDIDATES: &[&str] = &[
        "/System/Library/Fonts/Supplemental/Arial Unicode.ttf",
        "/Library/Fonts/Arial Unicode.ttf",
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/Supplemental/Arial.ttf",
        "/System/Library/Fonts/SFNS.ttf",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
        "C:\\Windows\\Fonts\\arial.ttf",
    ];
    for path in CANDIDATES {
        if let Ok(font) = load_font_from_path(Path::new(path), 0) {
            return Some(font);
        }
    }
    None
}

fn rasterize_font_glyph(font: &FontArc, key: GlyphCacheKey) -> RasterizedGlyph {
    let font_size = key.font_size();
    let scale = px_scale_for_em(font, font_size);
    let scaled = font.as_scaled(scale);
    let glyph_id = scaled.glyph_id(key.ch);
    let advance = scaled.h_advance(glyph_id).max(0.0);
    if key.ch.is_whitespace() {
        return RasterizedGlyph::empty(key, advance.max(font_size * 0.35));
    }
    let glyph = Glyph {
        id: glyph_id,
        scale,
        position: ab_glyph::point(0.0, 0.0),
    };
    let Some(outlined) = font.outline_glyph(glyph) else {
        return RasterizedGlyph::empty(key, advance.max(font_size * 0.5));
    };
    let bounds = outlined.px_bounds();
    let pad = i32::from(key.outline_radius) + 2;
    let width = (bounds.width().ceil() as i32 + pad * 2).max(1) as u32;
    let height = (bounds.height().ceil() as i32 + pad * 2).max(1) as u32;
    let stride = width as usize;
    let mut fill_alpha = vec![0u8; stride * height as usize];
    outlined.draw(|x, y, coverage| {
        let px = x as i32 + pad;
        let py = y as i32 + pad;
        if px < 0 || py < 0 || px >= width as i32 || py >= height as i32 {
            return;
        }
        let index = py as usize * stride + px as usize;
        let value = (coverage.clamp(0.0, 1.0) * 255.0).round() as u8;
        fill_alpha[index] = fill_alpha[index].max(value);
    });
    let outline_alpha = if key.outline_radius > 0 {
        dilate_alpha(
            &fill_alpha,
            width,
            height,
            stride,
            i32::from(key.outline_radius),
        )
    } else {
        fill_alpha.clone()
    };
    RasterizedGlyph {
        key,
        width,
        height,
        stride,
        fill_alpha,
        outline_alpha,
        offset_x: bounds.min.x.floor() - pad as f32,
        offset_y: bounds.min.y.floor() - pad as f32,
        advance,
    }
}

fn rasterize_fallback_glyph(shaper: &TextShaper, key: GlyphCacheKey) -> RasterizedGlyph {
    let font_size = key.font_size();
    if key.ch.is_whitespace() {
        return RasterizedGlyph::empty(key, font_size * 0.35);
    }
    let text = key.ch.to_string();
    let metrics = shaper.measure(&text, font_size);
    let pad = u32::from(key.outline_radius) + 2;
    let width = metrics.width.ceil().max(1.0) as u32 + pad * 2;
    let height = metrics.height.ceil().max(1.0) as u32 + pad * 2;
    let stride = width as usize;
    let mut fill_alpha = vec![0u8; stride * height as usize];
    for y in pad..height.saturating_sub(pad) {
        for x in pad..width.saturating_sub(pad) {
            let border = x == pad
                || y == pad
                || x + 1 == width.saturating_sub(pad)
                || y + 1 == height.saturating_sub(pad);
            fill_alpha[y as usize * stride + x as usize] = if border { 180 } else { 230 };
        }
    }
    let outline_alpha = if key.outline_radius > 0 {
        dilate_alpha(
            &fill_alpha,
            width,
            height,
            stride,
            i32::from(key.outline_radius),
        )
    } else {
        fill_alpha.clone()
    };
    RasterizedGlyph {
        key,
        width,
        height,
        stride,
        fill_alpha,
        outline_alpha,
        offset_x: -(pad as f32),
        offset_y: -metrics.ascent - pad as f32,
        advance: metrics.width.max(font_size * 0.5),
    }
}

fn dilate_alpha(input: &[u8], width: u32, height: u32, stride: usize, radius: i32) -> Vec<u8> {
    if radius <= 0 {
        return input.to_vec();
    }
    let mut output = input.to_vec();
    for y in 0..height as i32 {
        for x in 0..width as i32 {
            let mut max_value = input[y as usize * stride + x as usize];
            for oy in -radius..=radius {
                for ox in -radius..=radius {
                    let nx = x + ox;
                    let ny = y + oy;
                    if nx < 0 || ny < 0 || nx >= width as i32 || ny >= height as i32 {
                        continue;
                    }
                    max_value = max_value.max(input[ny as usize * stride + nx as usize]);
                }
            }
            output[y as usize * stride + x as usize] = max_value;
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ttc() -> Vec<u8> {
        let source = NIPAPLAY_FALLBACK_FONT;
        let face_count = 2u32;
        let header_len = 12 + face_count as usize * 4;
        let first_offset = (header_len + 3) & !3;
        let second_offset = (first_offset + source.len() + 3) & !3;
        let mut collection = vec![0; second_offset + source.len()];
        collection[0..4].copy_from_slice(b"ttcf");
        collection[4..8].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        collection[8..12].copy_from_slice(&face_count.to_be_bytes());
        collection[12..16].copy_from_slice(&(first_offset as u32).to_be_bytes());
        collection[16..20].copy_from_slice(&(second_offset as u32).to_be_bytes());

        for (face_index, face_offset) in [first_offset, second_offset].into_iter().enumerate() {
            collection[face_offset..face_offset + source.len()].copy_from_slice(source);
            let table_count = u16::from_be_bytes([source[4], source[5]]) as usize;
            for table_index in 0..table_count {
                let record = 12 + table_index * 16;
                let source_offset =
                    u32::from_be_bytes(source[record + 8..record + 12].try_into().unwrap())
                        as usize;
                let collection_record = face_offset + record + 8;
                collection[collection_record..collection_record + 4]
                    .copy_from_slice(&((face_offset + source_offset) as u32).to_be_bytes());
                if face_index == 1 && &source[record..record + 4] == b"head" {
                    collection[face_offset + source_offset + 18..face_offset + source_offset + 20]
                        .copy_from_slice(&512u16.to_be_bytes());
                }
            }
        }
        collection
    }

    fn item(time: f64, text: &str, mode: DanmakuMode) -> DanmakuItem {
        DanmakuItem {
            id: 0,
            pts: Duration::from_secs_f64(time),
            text: text.to_string(),
            mode,
            font_size: 24.0,
            color: DanmakuColor::WHITE,
            opacity: 1.0,
            is_self: false,
            user_id: String::new(),
            user_name: String::new(),
            user_level: 0,
        }
    }

    /// 带发送者元数据的条目，用于屏蔽规则测试。
    ///
    /// 当前无调用点（屏蔽规则的用例在 `danmaku::rules` 里自带构造），保留待用。
    #[allow(dead_code)]
    fn item_with_user(
        time: f64,
        text: &str,
        mode: DanmakuMode,
        user_id: &str,
        user_name: &str,
        user_level: u32,
    ) -> DanmakuItem {
        DanmakuItem {
            user_id: user_id.to_string(),
            user_name: user_name.to_string(),
            user_level,
            ..item(time, text, mode)
        }
    }

    fn assert_close(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 0.001,
            "expected {actual} to be close to {expected}"
        );
    }

    fn selected_font(data: Vec<u8>) -> SubtitleFontAttachment {
        SubtitleFontAttachment::new("selected", None, Vec::new(), Arc::<[u8]>::from(data))
    }

    #[test]
    fn parses_json_long_and_short_fields() {
        let input = r##"{
            "comments": [
                {"time": 1.0, "content": "hello", "type": "scroll", "color": "#ff0000"},
                {"t": 2.0, "c": "top", "y": "top", "r": "rgb(0,255,0)"}
            ]
        }"##;
        let timeline = DanmakuTimeline::from_json(input).unwrap();

        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline.items()[0].mode, DanmakuMode::Scroll);
        assert_eq!(timeline.items()[1].mode, DanmakuMode::Top);
        assert_eq!(timeline.items()[0].color.red, 1.0);
        assert_eq!(timeline.items()[1].color.green, 1.0);
    }

    #[test]
    fn parses_nipaplay_camel_case_item_fields() {
        let input = r#"[{"t":1,"c":"phone","y":"scroll","r":16777215,"fontSize":30,"isMe":true}]"#;
        let timeline = DanmakuTimeline::from_json(input).unwrap();

        assert_eq!(timeline.items()[0].font_size, 30.0);
        assert!(timeline.items()[0].is_self);
    }

    #[test]
    fn parses_json_ids_without_floating_point_rounding() {
        let input = r#"[
            {"id":9007199254740993,"time":1,"content":"first"},
            {"id":"18446744073709551615","time":2,"content":"second"}
        ]"#;
        let timeline = DanmakuTimeline::from_json(input).unwrap();

        assert_eq!(timeline.items()[0].id, 9_007_199_254_740_993);
        assert_eq!(timeline.items()[1].id, u64::MAX);
    }

    #[test]
    fn session_allocates_layout_ids_independently_of_source_ids() {
        let mut first = item(1.0, "first", DanmakuMode::Scroll);
        first.id = u64::MAX;
        let mut second = item(2.0, "second", DanmakuMode::Scroll);
        second.id = u64::MAX;
        let timeline = DanmakuTimeline::new(vec![first, second]).unwrap();
        let mut session = DanmakuSession::from_timeline(timeline);

        let ids = session
            .active_timeline()
            .items()
            .iter()
            .map(|item| item.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn parses_json_lines_and_xml() {
        let jsonl = r#"{"t":1,"c":"a","y":"bottom","r":16777215}
{"t":2,"c":"b","y":"scroll"}"#;
        let timeline = DanmakuTimeline::from_json_lines(jsonl).unwrap();
        assert_eq!(timeline.items()[0].mode, DanmakuMode::Bottom);

        let xml = r#"<i><d p="1.0,1,25,16777215,0,0,0,42">滚动&amp;测试</d><d p="2.0,5,25,16711680,0,0,0,0">顶部</d></i>"#;
        let timeline = DanmakuTimeline::from_bilibili_xml(xml).unwrap();
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline.items()[0].text, "滚动&测试");
        assert_eq!(timeline.items()[1].mode, DanmakuMode::Top);
    }

    #[test]
    fn dfm_layout_avoids_same_time_scroll_collision() {
        let timeline = DanmakuTimeline::new(vec![
            item(1.0, "wide comment", DanmakuMode::Scroll),
            item(1.0, "another comment", DanmakuMode::Scroll),
        ])
        .unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let prepared = engine.prepare(DanmakuViewport::new(640, 360), 7);

        assert_eq!(prepared.items().len(), 2);
        assert_ne!(prepared.items()[0].y, prepared.items()[1].y);
    }

    #[test]
    fn default_scroll_layout_uses_full_display_area_when_configured() {
        let timeline = DanmakuTimeline::new(
            (0..18)
                .map(|index| item(1.0, &format!("line {index}"), DanmakuMode::Scroll))
                .collect(),
        )
        .unwrap();
        let config = DanmakuLayoutConfig {
            track_gap_ratio: 0.0,
            merge_duplicates: false,
            allow_scroll_overwrite: false,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let prepared = engine.prepare(DanmakuViewport::new(640, 360), 7);
        let max_y = prepared
            .items()
            .iter()
            .map(|item| item.y)
            .fold(0.0, f32::max);

        assert!(
            max_y > 270.0,
            "display_area=1.0 should allow scroll rows into the lower viewport, got max y {max_y}"
        );
        assert!(
            max_y < 360.0,
            "scroll rows should still stay inside the viewport, got max y {max_y}"
        );
    }

    #[test]
    fn dense_default_layout_keeps_using_the_full_display_area() {
        let timeline = DanmakuTimeline::new(
            (0..960)
                .map(|index| {
                    let mut entry = item(
                        index as f64 / 80.0,
                        &format!("dense default {index}"),
                        DanmakuMode::Scroll,
                    );
                    entry.id = index + 1;
                    entry
                })
                .collect(),
        )
        .unwrap();
        let config = DanmakuLayoutConfig {
            merge_duplicates: false,
            display_area: 1.0,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let viewport = DanmakuViewport::new(640, 360);
        let prepared = engine.prepare(viewport, 7);
        let frame = prepared.frame_layout(Duration::from_secs_f64(10.0), 7);
        let max_y = frame
            .items
            .iter()
            .filter(|item| item.mode == DanmakuMode::Scroll)
            .map(|item| item.y)
            .fold(0.0, f32::max);

        assert!(
            max_y > viewport.height as f32 * 0.75,
            "dense default layout must keep using the full display area; max y={max_y}"
        );
    }

    #[test]
    fn frame_layout_uses_generation_and_media_time() {
        let timeline = DanmakuTimeline::new(vec![item(1.0, "sync", DanmakuMode::Scroll)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(640, 360);
        let layout = engine.frame_layout(Duration::from_secs_f64(1.5), viewport, 10);
        let stale = engine.frame_layout(Duration::from_secs_f64(1.5), viewport, 11);

        assert_eq!(layout.generation, 10);
        assert_eq!(layout.items.len(), 1);
        assert_eq!(stale.generation, 11);
    }

    #[test]
    fn generation_change_reuses_prepared_layout() {
        let timeline = DanmakuTimeline::new(vec![item(1.0, "seek", DanmakuMode::Scroll)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(640, 360);

        let first = engine.prepare(viewport, 1);
        let first_text = Arc::clone(&first.items()[0].text);
        let first_text_layout = Arc::clone(&first.items()[0].text_layout);
        let layout = engine.frame_layout(Duration::from_secs_f64(1.5), viewport, 2);
        let prepared = engine.prepared.as_ref().expect("prepared layout exists");

        assert_eq!(layout.generation, 2);
        assert!(Arc::ptr_eq(&prepared.items()[0].text, &first_text));
        assert!(Arc::ptr_eq(
            &prepared.items()[0].text_layout,
            &first_text_layout
        ));
    }

    #[test]
    fn sliding_timeline_window_preserves_tracks_for_overlapping_items() {
        let timeline = DanmakuTimeline::new(
            (0..80)
                .map(|index| {
                    let mut entry = item(
                        index as f64 * 0.25,
                        &format!("scroll {index}"),
                        DanmakuMode::Scroll,
                    );
                    entry.id = index + 1;
                    entry
                })
                .collect(),
        )
        .unwrap();
        let config = DanmakuLayoutConfig {
            allow_stacking: true,
            merge_duplicates: false,
            ..DanmakuLayoutConfig::default()
        };
        let viewport = DanmakuViewport::new(640, 360);
        let first_window = timeline.window(Duration::ZERO, Duration::from_secs(12));
        let second_window = timeline.window(Duration::from_secs(4), Duration::from_secs(16));
        let mut engine = DfmLayoutEngine::new(first_window, config);

        let first = engine.prepare(viewport, 1);
        let first_tracks = first
            .items()
            .iter()
            .map(|item| (item.id, item.track_index))
            .collect::<HashMap<_, _>>();
        engine.sync_timeline(&second_window);
        let second = engine.prepare(viewport, 1);

        let overlap = second
            .items()
            .iter()
            .filter_map(|item| {
                first_tracks
                    .get(&item.id)
                    .map(|track| (*track, item.track_index))
            })
            .collect::<Vec<_>>();
        assert!(!overlap.is_empty());
        assert!(overlap.iter().all(|(before, after)| before == after));
        assert_eq!(engine.stable_tracks.len(), second.items().len());
        assert!(
            engine
                .stable_tracks
                .keys()
                .all(|item_id| second.items().iter().any(|item| item.id == *item_id))
        );
    }

    #[test]
    fn sliding_timeline_window_does_not_revive_rejected_items() {
        let mut entries = vec![
            item(0.0, "occupy", DanmakuMode::Scroll),
            item(0.0, "occupy", DanmakuMode::Scroll),
            item(
                1.0,
                "this rejected danmaku is deliberately very wide",
                DanmakuMode::Scroll,
            ),
            item(4.0, "already visible", DanmakuMode::Scroll),
        ];
        for (index, entry) in entries.iter_mut().enumerate() {
            entry.id = index as u64 + 1;
        }
        let timeline = DanmakuTimeline::new(entries).unwrap();
        let config = DanmakuLayoutConfig {
            track_gap_ratio: 0.0,
            merge_duplicates: false,
            ..DanmakuLayoutConfig::default()
        };
        let viewport = DanmakuViewport::new(320, 58);
        let first_window = timeline.window(Duration::ZERO, Duration::from_secs(8));
        let second_window = timeline.window(Duration::from_secs(1), Duration::from_secs(9));
        let mut engine = DfmLayoutEngine::new(first_window, config);

        let first = engine.prepare(viewport, 1);
        assert!(!first.items().iter().any(|item| item.id == 3));
        let original_track = first
            .items()
            .iter()
            .find(|item| item.id == 4)
            .expect("later item is placed after the first pair expires")
            .track_index;

        engine.sync_timeline(&second_window);
        let second = engine.prepare(viewport, 1);

        assert!(!second.items().iter().any(|item| item.id == 3));
        assert_eq!(
            second
                .items()
                .iter()
                .find(|item| item.id == 4)
                .expect("visible item remains prepared")
                .track_index,
            original_track
        );
    }

    #[test]
    fn render_plan_contains_glyph_atlas() {
        let timeline = DanmakuTimeline::new(vec![item(0.0, "GPU", DanmakuMode::Top)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let plan = engine.render_plan(
            Duration::from_millis(500),
            DanmakuViewport::new(640, 360),
            3,
        );

        assert_eq!(plan.generation, 3);
        assert!(!plan.items.is_empty());
        let atlas = plan.atlas.as_ref().expect("glyph atlas exists");
        assert!(atlas.is_valid());
        assert!(plan.items.iter().all(|item| item.tex_rect[2] > 0.0));
    }

    #[test]
    fn render_plan_reuses_glyph_atlas_snapshot_for_stable_glyph_set() {
        let timeline = DanmakuTimeline::new(vec![item(0.0, "GPU", DanmakuMode::Scroll)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(640, 360);

        let first = engine.render_plan(Duration::from_millis(500), viewport, 3);
        let first_atlas = first.atlas.as_ref().expect("glyph atlas exists").clone();
        let first_x = first.items[0].rect[0];
        let second = engine.render_plan(Duration::from_millis(600), viewport, 4);
        let second_atlas = second.atlas.as_ref().expect("glyph atlas exists");

        assert_eq!(second.generation, 4);
        assert_ne!(first_x, second.items[0].rect[0]);
        assert_eq!(first_atlas.version, second_atlas.version);
        assert!(Arc::ptr_eq(&first_atlas, second_atlas));
    }

    #[test]
    fn glyph_atlas_snapshot_describes_only_the_new_dirty_region() {
        let first_timeline = DanmakuTimeline::new(vec![item(0.0, "A", DanmakuMode::Top)]).unwrap();
        let mut engine = DfmLayoutEngine::new(first_timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(640, 360);
        let first = engine.render_plan(Duration::from_millis(500), viewport, 1);
        let first_atlas = first.atlas.as_ref().expect("first glyph atlas exists");
        assert!(first_atlas.update.is_none());

        engine.set_timeline(DanmakuTimeline::new(vec![item(0.0, "AB", DanmakuMode::Top)]).unwrap());
        let second = engine.render_plan(Duration::from_millis(500), viewport, 2);
        let second_atlas = second.atlas.as_ref().expect("second glyph atlas exists");
        let update = second_atlas
            .update
            .as_ref()
            .expect("new glyph produces an incremental update");

        assert_eq!(update.from_version, first_atlas.version);
        assert!(update.width > 0 && update.height > 0);
        assert!(update.width < second_atlas.width);
        assert!(
            second_atlas
                .incremental_update_from(
                    first_atlas.version,
                    first_atlas.width,
                    first_atlas.height,
                    first_atlas.stride,
                )
                .is_some()
        );
    }

    #[test]
    fn text_layout_cache_evicts_least_recently_used() {
        let mut cache = TextLayoutCache::with_capacity(2);
        let layout = || {
            Arc::new(PreparedTextLayout {
                metrics: TextMeasure {
                    width: 1.0,
                    height: 1.0,
                    ascent: 1.0,
                    descent: 1.0,
                },
                glyphs: Vec::new(),
            })
        };
        let key = |text: &str| TextLayoutCacheKey::new(Arc::<str>::from(text), 24.0, 1.0);
        cache.insert(key("a"), layout());
        cache.insert(key("b"), layout());
        cache.insert(key("c"), layout());
        // Over capacity: the least-recently-used ("a") is evicted.
        assert!(cache.get(&key("a")).is_none());
        assert!(cache.get(&key("b")).is_some());
        assert!(cache.get(&key("c")).is_some());

        // Touching "b" makes it most-recent; the next insert evicts "c".
        assert!(cache.get(&key("b")).is_some());
        cache.insert(key("d"), layout());
        assert!(cache.get(&key("b")).is_some());
        assert!(cache.get(&key("c")).is_none());
        assert!(cache.get(&key("d")).is_some());
    }

    #[test]
    fn glyph_cache_evicts_lru_and_get_or_insert_preserves_existing() {
        let mut cache = GlyphCache::with_capacity(2);
        let key = |id: u32, ch: char| GlyphCacheKey::new(id, ch, 24.0, 1.0);
        let glyph = |k: GlyphCacheKey| Arc::new(RasterizedGlyph::empty(k, 1.0));
        cache.get_or_insert(key(0, 'a'), glyph(key(0, 'a')));
        cache.get_or_insert(key(0, 'b'), glyph(key(0, 'b')));
        cache.get_or_insert(key(0, 'c'), glyph(key(0, 'c')));
        assert!(cache.get(&key(0, 'a')).is_none());
        assert!(cache.get(&key(0, 'b')).is_some());
        assert!(cache.get(&key(0, 'c')).is_some());

        // A duplicate insert must keep the existing entry, never overwrite.
        let replacement = Arc::new(RasterizedGlyph::empty(key(0, 'c'), 99.0));
        let got = cache.get_or_insert(key(0, 'c'), replacement);
        assert_eq!(got.advance, 1.0);
    }

    #[test]
    fn glyph_atlas_resets_instead_of_growing_past_max_height() {
        let mut atlas = PersistentGlyphAtlas::new();
        let key = GlyphCacheKey::new(0, 'a', 24.0, 1.0);
        let glyph = RasterizedGlyph {
            key,
            width: 1,
            height: 1,
            stride: 1,
            fill_alpha: vec![255],
            outline_alpha: vec![255],
            offset_x: 0.0,
            offset_y: 0.0,
            advance: 1.0,
        };
        let packed = atlas.pack(&glyph);
        // 第 0 行前 SELF_BG_ATLAS_RESERVE 列预留给自发言弹幕背景框（纯白 texel），
        // 字形打包从 x=reserve 开始。
        assert_eq!(
            packed,
            PackedGlyph {
                x: SELF_BG_ATLAS_RESERVE as u32,
                y: 0,
                width: 1,
                height: 1
            }
        );

        atlas.ensure_height(GLYPH_ATLAS_MAX_HEIGHT + 1);
        assert_eq!(atlas.height, GLYPH_ATLAS_INITIAL_HEIGHT);
        assert!(atlas.packed.is_empty());
        assert!(atlas.snapshot.is_none());
        assert!(atlas.dirty);

        let repacked = atlas.pack(&glyph);
        assert_eq!(
            repacked,
            PackedGlyph {
                x: SELF_BG_ATLAS_RESERVE as u32,
                y: 0,
                width: 1,
                height: 1
            }
        );
        assert!(atlas.packed.contains_key(&key));
    }

    /// 回退候选必须是**显式白名单**，而不是"遍历字体库"。
    ///
    /// 这条断言守住的是内存：候选集一旦来自 `db.faces()`（机器上装了什么就是
    /// 什么），用户自装的第三方字体会全部进来——本机实测 `%WINDIR%\Fonts` 有
    /// 205 个文件 844 MB，其中单个 CJK 集合 111.9 MB，且多个 face 各自一份。
    /// 回退首次缺字时遍历到它们就会把它们整份读入常驻。
    #[test]
    #[cfg(target_os = "windows")]
    fn system_font_fallback_candidates_stay_a_small_explicit_whitelist() {
        let fb = SystemFontFallback::new(1);
        assert!(
            fb.faces.len() <= 4,
            "回退候选应保持为显式白名单（Windows 上至多 4 个），实际 {} 个——\
             候选里出现大量 face 通常意味着又退回成遍历字体库了",
            fb.faces.len()
        );
    }

    #[test]
    fn system_font_fallback_evicts_oldest_faces_over_budget() {
        let font = FontArc::new(
            FontVec::try_from_vec(NIPAPLAY_FALLBACK_FONT.to_vec()).expect("fallback font parses"),
        );
        let mut db = fontdb::Database::new();
        let ttc = test_ttc();
        for _ in 0..20 {
            db.load_font_data(ttc.clone());
        }
        let mut db_ids: Vec<fontdb::ID> = db.faces().map(|face| face.id).collect();
        assert!(db_ids.len() > MAX_LOADED_SYSTEM_FACES);
        db_ids.truncate(MAX_LOADED_SYSTEM_FACES + 1);

        let mut fb = SystemFontFallback {
            db,
            faces: Vec::new(),
            loaded: HashMap::new(),
            loaded_order: VecDeque::new(),
            char_cache: HashMap::new(),
            next_font_id: 1,
            source_keys: HashMap::new(),
            sources: HashMap::new(),
            pinned_sources: HashSet::new(),
            probe_order: VecDeque::new(),
            probe_bytes: 0,
        };
        for (index, db_id) in db_ids.iter().copied().enumerate() {
            fb.loaded.insert(
                db_id,
                Some(DanmakuFontFace::new(index as u32 + 1, font.clone())),
            );
            fb.loaded_order.push_back(db_id);
        }
        // char_cache pointing at the evicted (oldest) face is dropped; entries
        // for surviving faces are kept.
        fb.char_cache.insert('x', Some(1));
        fb.char_cache.insert('y', Some(2));

        fb.enforce_loaded_budget();

        assert_eq!(fb.loaded_order.len(), MAX_LOADED_SYSTEM_FACES);
        assert_eq!(fb.loaded.len(), MAX_LOADED_SYSTEM_FACES);
        assert!(!fb.loaded.contains_key(&db_ids[0]));
        assert!(fb.loaded.contains_key(&db_ids[1]));
        assert!(!fb.char_cache.contains_key(&'x'));
        assert!(fb.char_cache.contains_key(&'y'));
    }

    #[test]
    fn layout_equivalence_only_ignores_the_declared_paint_only_fields() {
        let base = DanmakuLayoutConfig::default();

        let mut paint_only = base.clone();
        paint_only.opacity = 0.25;
        paint_only.shadow_offset = [4.0, 4.0];
        paint_only.shadow_style = DanmakuShadowStyle::None;
        paint_only.enabled = !base.enabled;
        assert!(base.layout_equivalent(&paint_only));

        // Anything outside that set must force a relayout; the normalization
        // is subtractive so a field added later is layout-affecting by default.
        let mut layout = base.clone();
        layout.font_size += 1.0;
        assert!(!base.layout_equivalent(&layout));

        let mut blocked = base.clone();
        blocked.block_words = vec!["blocked".to_string()];
        assert!(!base.layout_equivalent(&blocked));
    }

    #[test]
    fn paint_only_config_change_reuses_prepared_layout() {
        let timeline = DanmakuTimeline::new(vec![item(0.0, "paint", DanmakuMode::Top)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(640, 360);
        let first = engine.prepare(viewport, 1);
        let first_text_layout = Arc::clone(&first.items()[0].text_layout);

        let mut config = engine.config().clone();
        config.opacity = 0.4;
        config.shadow_style = DanmakuShadowStyle::None;
        assert_eq!(engine.apply_config(config), DanmakuConfigChange::PaintOnly);

        let prepared = engine.prepared.as_ref().expect("prepared layout retained");
        assert!(Arc::ptr_eq(
            &prepared.items()[0].text_layout,
            &first_text_layout
        ));
        let frame = prepared.frame_layout(Duration::from_millis(500), 2);
        assert_close(frame.items[0].opacity, 0.4);
        assert_close(frame.items[0].shadow_alpha, 0.0);
    }

    #[test]
    fn font_size_change_preserves_stable_track_preferences() {
        let timeline = DanmakuTimeline::new(vec![
            item(0.0, "first", DanmakuMode::Top),
            item(0.1, "second", DanmakuMode::Top),
            item(0.2, "third", DanmakuMode::Top),
        ])
        .unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(1280, 720);
        let first = engine.prepare(viewport, 1);
        let first_tracks = first
            .items()
            .iter()
            .map(|item| (item.id, item.track_index))
            .collect::<HashMap<_, _>>();

        let mut config = engine.config().clone();
        config.font_size += 2.0;
        assert_eq!(engine.apply_config(config), DanmakuConfigChange::Layout);
        assert_eq!(engine.stable_tracks, first_tracks);

        let second = engine.prepare(viewport, 2);
        for item in second.items() {
            assert_eq!(
                Some(&item.track_index),
                first_tracks.get(&item.id),
                "overlapping item {} changed logical track",
                item.id
            );
        }
    }

    #[test]
    fn disabling_is_immediate_but_re_enabling_invalidates_empty_layouts() {
        let timeline = DanmakuTimeline::new(vec![item(0.0, "toggle", DanmakuMode::Top)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(640, 360);

        let enabled = engine.prepare(viewport, 1);
        assert!(!enabled.items().is_empty());

        let mut config = engine.config().clone();
        config.enabled = false;
        assert_eq!(
            engine.apply_config(config.clone()),
            DanmakuConfigChange::PaintOnly
        );
        let hidden_frame = engine
            .prepared
            .as_ref()
            .expect("disabling retains the prepared geometry")
            .frame_layout(Duration::from_millis(500), 2);
        assert!(hidden_frame.items.is_empty());

        // A later planner window prepared while disabled is intentionally
        // empty, which is the state that must not survive re-enabling.
        assert!(engine.prepare(viewport, 2).items().is_empty());

        config.enabled = true;
        assert_eq!(engine.apply_config(config), DanmakuConfigChange::Layout);
        assert!(
            engine.prepared.is_none(),
            "the disabled empty layout must not be reused after enabling danmaku"
        );
        assert!(!engine.prepare(viewport, 3).items().is_empty());
    }

    #[test]
    fn scroll_duration_uses_logical_width_and_retina_scale() {
        let config = DanmakuLayoutConfig::default();
        let narrow = scroll_duration_for_viewport(&config, DanmakuViewport::new(640, 360));
        let reference = scroll_duration_for_viewport(&config, DanmakuViewport::new(1280, 720));
        let wide = scroll_duration_for_viewport(&config, DanmakuViewport::new(1920, 1080));
        let retina_reference =
            scroll_duration_for_viewport(&config, DanmakuViewport::with_scale(2560, 1440, 2.0));

        assert_eq!(narrow, Duration::from_secs(9));
        assert_eq!(reference, Duration::from_secs(10));
        assert_eq!(wide, Duration::from_secs(13));
        assert_eq!(retina_reference, reference);
    }

    #[test]
    fn selected_memory_fonts_fallback_in_selection_order() {
        let first = selected_font(NIPAPLAY_FALLBACK_FONT.to_vec());
        let second = selected_font(test_ttc());
        let selection = DanmakuFontSelection::new(1, Arc::from(vec![first, second]));
        let rasterizer = DanmakuTextRasterizer::for_config_and_selection(
            &DanmakuLayoutConfig::default(),
            &selection,
        );

        let resolved = rasterizer
            .resolve_font('A')
            .expect("selected font resolves");

        assert_eq!(resolved.id, 1);
    }

    #[test]
    fn font_selection_generation_atomically_invalidates_rasterizer_caches() {
        let timeline = DanmakuTimeline::new(vec![item(0.0, "cache", DanmakuMode::Top)]).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let viewport = DanmakuViewport::new(640, 360);
        let first = engine.render_plan(Duration::from_millis(500), viewport, 1);
        let first_atlas = first.atlas.expect("first atlas exists");
        let selection = DanmakuFontSelection::new(
            2,
            Arc::from(vec![selected_font(NIPAPLAY_FALLBACK_FONT.to_vec())]),
        );

        assert!(engine.set_font_selection(selection.clone()));
        assert!(!engine.set_font_selection(selection));
        assert!(engine.prepared.is_none());
        let second = engine.render_plan(Duration::from_millis(500), viewport, 2);
        let second_atlas = second.atlas.expect("second atlas exists");

        assert!(!Arc::ptr_eq(&first_atlas, &second_atlas));
    }

    #[test]
    fn layout_font_size_uses_nipaplay_logical_units_across_resize() {
        let timeline = DanmakuTimeline::new(vec![DanmakuItem {
            font_size: DEFAULT_SOURCE_FONT_SIZE,
            ..item(0.0, "scale", DanmakuMode::Scroll)
        }])
        .unwrap();
        let config = DanmakuLayoutConfig {
            font_size: 32.0,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);

        let small = engine.prepare(DanmakuViewport::new(640, 360), 1);
        let large = engine.prepare(DanmakuViewport::new(1920, 1080), 2);
        let expected = 32.0;

        assert_close(small.items()[0].font_size, expected);
        assert_close(large.items()[0].font_size, expected);
        assert!(large.items()[0].scroll_speed > small.items()[0].scroll_speed);
    }

    #[test]
    fn layout_font_size_uses_surface_scale_for_physical_pixels() {
        let timeline = DanmakuTimeline::new(vec![DanmakuItem {
            font_size: DEFAULT_SOURCE_FONT_SIZE,
            ..item(0.0, "scale", DanmakuMode::Top)
        }])
        .unwrap();
        let config = DanmakuLayoutConfig {
            font_size: 32.0,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);

        let one_x = engine.prepare(DanmakuViewport::with_scale(800, 450, 1.0), 1);
        let two_x = engine.prepare(DanmakuViewport::with_scale(1600, 900, 2.0), 2);
        let expected = 32.0;

        assert_close(one_x.items()[0].font_size, expected);
        assert_close(two_x.items()[0].font_size, expected * 2.0);
    }

    #[test]
    fn nipaplay_phone_font_size_uses_fractional_android_density() {
        let timeline = DanmakuTimeline::new(vec![DanmakuItem {
            font_size: DEFAULT_SOURCE_FONT_SIZE,
            ..item(0.0, "android density", DanmakuMode::Top)
        }])
        .unwrap();
        let config = DanmakuLayoutConfig {
            font_size: 20.0,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);

        let prepared = engine.prepare(DanmakuViewport::with_scale(1080, 2400, 2.625), 1);

        assert_close(prepared.items()[0].font_size, 52.5);
    }

    #[test]
    fn source_font_size_is_relative_to_configured_reference_size() {
        let timeline = DanmakuTimeline::new(vec![DanmakuItem {
            font_size: 50.0,
            ..item(0.0, "large", DanmakuMode::Top)
        }])
        .unwrap();
        let config = DanmakuLayoutConfig {
            font_size: 30.0,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let prepared = engine.prepare(DanmakuViewport::new(1920, 1080), 1);

        assert_close(prepared.items()[0].font_size, 60.0);
    }

    /// 回归：`font_size: 0.0` 是"跟随应用设置字号"的哨兵值，必须能活着到达
    /// `effective_font_size`。
    ///
    /// 曾经 `DanmakuItem::normalized` 在入口 `.max(1.0)`，把 0 变成 1。而 1.0 是
    /// **正数**，`effective_font_size` 于是把它当成"相对字号"而不是"未指定"，
    /// 算出 `配置字号 × (1.0 / 25.0)` —— 自己发的弹幕因此以 **1 像素**渲染，
    /// 画面上只剩一个 1px 字形撑起来的小方块。普通弹幕因为 `source = 25.0`
    /// 恰好落在基准值上，所以一直是好的，这个 bug 才只打在自发弹幕身上。
    ///
    /// 断言两件事：哨兵解析出的是**配置字号本身**；且与"显式给出默认基准字号"
    /// 的普通弹幕**完全一致**（不是接近，是一致——差一个系数就是回归）。
    #[test]
    fn zero_source_font_size_follows_configured_size_instead_of_collapsing() {
        let viewport = DanmakuViewport::new(1920, 1080);
        let config = DanmakuLayoutConfig {
            font_size: 30.0,
            ..DanmakuLayoutConfig::default()
        };

        let sentinel_timeline = DanmakuTimeline::new(vec![DanmakuItem {
            font_size: 0.0,
            is_self: true,
            ..item(0.0, "self", DanmakuMode::Scroll)
        }])
        .unwrap();
        let reference_timeline = DanmakuTimeline::new(vec![DanmakuItem {
            font_size: DEFAULT_SOURCE_FONT_SIZE,
            is_self: true,
            ..item(0.0, "self", DanmakuMode::Scroll)
        }])
        .unwrap();

        let mut sentinel_engine = DfmLayoutEngine::new(sentinel_timeline, config.clone());
        let mut reference_engine = DfmLayoutEngine::new(reference_timeline, config);
        let sentinel = sentinel_engine.prepare(viewport, 1);
        let reference = reference_engine.prepare(viewport, 1);

        assert_close(sentinel.items()[0].font_size, 30.0);
        assert_close(
            sentinel.items()[0].font_size,
            reference.items()[0].font_size,
        );
    }

    /// 回归：`0.0` 必须在 `DanmakuTimeline::new` 之后**仍然是 0.0**。
    ///
    /// 这是上面那条测试的因——`normalized()` 里的钳制一旦改回去，这条会先红。
    /// 单独钉住入口行为，是为了让失败时能一眼看出是"哨兵被入口吃掉"还是
    /// "下游解析策略变了"。
    #[test]
    fn zero_source_font_size_survives_timeline_normalization() {
        let timeline = DanmakuTimeline::new(vec![DanmakuItem {
            font_size: 0.0,
            ..item(0.0, "self", DanmakuMode::Scroll)
        }])
        .unwrap();

        assert_eq!(timeline.items()[0].font_size, 0.0);
    }

    #[test]
    fn render_plan_keeps_fill_and_outline_masks_separate() {
        let timeline = DanmakuTimeline::new(vec![item(0.0, "Outline", DanmakuMode::Top)]).unwrap();
        let config = DanmakuLayoutConfig {
            outline_width: 2.0,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let plan = engine.render_plan(
            Duration::from_millis(500),
            DanmakuViewport::new(640, 360),
            3,
        );
        let atlas = plan.atlas.as_ref().expect("glyph atlas exists");
        let fill_sum = atlas.fill_alpha.iter().map(|&v| u64::from(v)).sum::<u64>();
        let outline_sum = atlas
            .outline_alpha
            .iter()
            .map(|&v| u64::from(v))
            .sum::<u64>();

        assert!(outline_sum > fill_sum);
        assert_ne!(atlas.fill_alpha, atlas.outline_alpha);
    }

    #[test]
    fn keyword_filter_blocks_items() {
        let timeline = DanmakuTimeline::new(vec![
            item(0.0, "visible", DanmakuMode::Scroll),
            item(0.1, "blocked bad", DanmakuMode::Scroll),
        ])
        .unwrap();
        let config = DanmakuLayoutConfig {
            block_words: vec!["bad".to_string()],
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let prepared = engine.prepare(DanmakuViewport::new(640, 360), 1);

        assert_eq!(prepared.items().len(), 1);
        assert_eq!(prepared.items()[0].text.as_ref(), "visible");
    }

    #[test]
    fn duplicate_merge_matches_dfm_prepare_scope() {
        let timeline = DanmakuTimeline::new(vec![
            item(0.0, "same", DanmakuMode::Top),
            item(0.1, "same", DanmakuMode::Top),
            item(11.0, "same", DanmakuMode::Top),
        ])
        .unwrap();
        let config = DanmakuLayoutConfig {
            merge_duplicates: true,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let prepared = engine.prepare(DanmakuViewport::new(640, 360), 1);

        assert_eq!(prepared.items().len(), 1);
        assert_eq!(prepared.items()[0].duplicate_count, 3);
        assert_eq!(prepared.items()[0].text.as_ref(), "same x3");
    }

    #[test]
    fn parser_skips_bad_individual_items() {
        let input = r#"{"comments":[
            {"time":1,"content":"ok"},
            {"time":2,"content":""},
            {"bad":"shape"},
            {"time":3,"content":"still ok"}
        ]}"#;
        let timeline = DanmakuTimeline::from_json(input).unwrap();

        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline.items()[0].text, "ok");
        assert_eq!(timeline.items()[1].text, "still ok");
    }

    #[test]
    fn dfm_overwrite_marks_displaced_scroll_items_filtered() {
        let mut first = item(0.0, "first wide", DanmakuMode::Scroll);
        first.font_size = 24.0;
        let mut second = item(0.0, "second wide", DanmakuMode::Scroll);
        second.font_size = 24.0;
        let timeline = DanmakuTimeline::new(vec![first, second]).unwrap();
        let config = DanmakuLayoutConfig {
            display_area: 0.1,
            track_gap_ratio: 0.0,
            allow_scroll_overwrite: true,
            merge_duplicates: false,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let prepared = engine.prepare(DanmakuViewport::new(320, 24), 1);

        assert_eq!(prepared.items().len(), 1);
        assert_eq!(prepared.items()[0].text.as_ref(), "second wide");
    }

    #[test]
    fn max_lines_caps_each_danmaku_mode_tracks() {
        let timeline = DanmakuTimeline::new(vec![
            item(0.0, "a", DanmakuMode::Top),
            item(0.0, "b", DanmakuMode::Top),
            item(0.0, "c", DanmakuMode::Bottom),
            item(0.0, "d", DanmakuMode::Bottom),
        ])
        .unwrap();
        let config = DanmakuLayoutConfig {
            max_lines_per_mode: Some(1),
            merge_duplicates: false,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let prepared = engine.prepare(DanmakuViewport::new(640, 360), 1);

        let top_count = prepared
            .items()
            .iter()
            .filter(|item| item.mode == DanmakuMode::Top)
            .count();
        let bottom_count = prepared
            .items()
            .iter()
            .filter(|item| item.mode == DanmakuMode::Bottom)
            .count();

        assert!(!prepared.items().is_empty());
        assert!(top_count <= 1);
        assert!(bottom_count <= 1);
        assert!(prepared.items().iter().all(|item| item.track_index == 0));
    }

    #[test]
    fn regex_filter_blocks_matching_items() {
        let timeline = DanmakuTimeline::new(vec![
            item(0.0, "visible", DanmakuMode::Scroll),
            item(0.1, "episode 123 spoiler", DanmakuMode::Scroll),
        ])
        .unwrap();
        let config = DanmakuLayoutConfig {
            block_words: vec!["spoiler/[0-9]+ spoiler/".to_string()],
            merge_duplicates: false,
            ..DanmakuLayoutConfig::default()
        };
        let mut engine = DfmLayoutEngine::new(timeline, config);
        let prepared = engine.prepare(DanmakuViewport::new(640, 360), 1);

        assert_eq!(prepared.items().len(), 1);
        assert_eq!(prepared.items()[0].text.as_ref(), "visible");
    }

    #[test]
    fn json_special_danmaku_is_filtered_from_layout() {
        let input = r#"{"comments":[{"time":1,"content":"path","type":"special"}]}"#;
        let timeline = DanmakuTimeline::from_json(input).unwrap();
        let mut engine = DfmLayoutEngine::new(timeline, DanmakuLayoutConfig::default());
        let prepared = engine.prepare(DanmakuViewport::new(640, 360), 1);

        assert_eq!(prepared.items().len(), 0);
    }

    #[test]
    fn custom_font_rejects_oversized_file_from_metadata() {
        let path = std::env::temp_dir().join(format!(
            "erika_danmaku_oversized_font_{}.ttf",
            std::process::id()
        ));
        let file = File::create(&path).unwrap();
        file.set_len(MAX_CUSTOM_DANMAKU_FONT_BYTES + 1).unwrap();

        let error = load_font_from_path(&path, 0).unwrap_err();

        assert_eq!(error.reason, "file_too_large");
        assert_eq!(error.bytes, MAX_CUSTOM_DANMAKU_FONT_BYTES + 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn custom_font_rejects_non_regular_file_from_metadata() {
        let error = load_font_from_path(std::env::temp_dir().as_path(), 0).unwrap_err();

        assert_eq!(error.reason, "not_regular_file");
    }

    #[test]
    fn custom_font_reports_invalid_font_structure() {
        let path = std::env::temp_dir().join(format!(
            "erika_danmaku_invalid_font_{}.ttf",
            std::process::id()
        ));
        std::fs::write(&path, b"not a font").unwrap();

        let error = load_font_from_path(&path, 0).unwrap_err();

        assert_eq!(error.reason, "invalid_font");
        assert_eq!(error.bytes, 10);
        assert!(!error.detail.is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalid_custom_font_keeps_default_fallback() {
        let path = std::env::temp_dir().join(format!(
            "erika_danmaku_fallback_font_{}.ttf",
            std::process::id()
        ));
        std::fs::write(&path, b"not a font").unwrap();

        let font = load_configured_font("", path.to_str().unwrap(), 0);

        assert!(font.is_some());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn custom_font_path_uses_configured_collection_face_index() {
        let path = std::env::temp_dir().join(format!(
            "erika_danmaku_collection_font_{}.ttc",
            std::process::id()
        ));
        std::fs::write(&path, test_ttc()).unwrap();

        let first = load_font_from_path(&path, 0).unwrap();
        let second = load_font_from_path(&path, 1).unwrap();

        assert_ne!(first.units_per_em(), second.units_per_em());
        assert_eq!(second.units_per_em(), Some(512.0));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn fontdb_face_loader_uses_queried_collection_face_index() {
        let mut db = fontdb::Database::new();
        db.load_font_data(test_ttc());
        let second_id = db.faces().find(|face| face.index == 1).unwrap().id;

        let second = load_fontdb_face(&db, second_id).unwrap();

        assert_eq!(second.units_per_em(), Some(512.0));
    }

    #[test]
    fn selected_collection_faces_share_the_attachment_allocation() {
        let attachment = selected_font(test_ttc());
        let fonts = load_selected_fonts(std::slice::from_ref(&attachment), 1);

        assert_eq!(fonts.len(), 2);
        assert_eq!(fonts[0].font.font_data().as_ptr(), attachment.data.as_ptr());
        assert_eq!(fonts[1].font.font_data().as_ptr(), attachment.data.as_ptr());
    }

    /// 分类桶与内核诊断码必须一一对应。
    ///
    /// `DanmakuFilterBreakdown::add_filter_param` 里为了不让 `dfm_core` 的内部编号
    /// 变成公开契约，把编号抄了一份；这个测试盯住两边不要漂移——它直接用诊断码
    /// 常量（而不是字面数字）驱动分类，所以任何一侧改编号都会在这里失败。
    #[test]
    fn filter_breakdown_matches_param_codes() {
        use crate::danmaku::dfm_core::filters::filter_param as code;

        let mut breakdown = DanmakuFilterBreakdown::default();
        for param in [
            code::TYPE,
            code::QUANTITY,
            code::ELAPSED,
            code::KEYWORD,
            code::REGEX,
            code::USER,
            code::NAME,
            code::LEVEL,
            code::TEXT_LENGTH,
            code::TIME_RANGE,
            code::COLOR,
            code::DUPLICATE,
        ] {
            breakdown.add_filter_param(param);
        }
        // 未屏蔽（0）与轨道装不下（99）都算进"合并/装不下"桶，所以 99 再加一次
        breakdown.add_filter_param(0);
        breakdown.add_filter_param(code::OVERFLOW);

        assert_eq!(breakdown.type_blocked, 1);
        assert_eq!(breakdown.quantity, 1);
        assert_eq!(breakdown.elapsed, 1);
        assert_eq!(breakdown.keyword, 1);
        assert_eq!(breakdown.regex, 1);
        assert_eq!(breakdown.user, 1);
        assert_eq!(breakdown.name, 1);
        assert_eq!(breakdown.level, 1);
        assert_eq!(breakdown.text_length, 1);
        assert_eq!(breakdown.time_range, 1);
        assert_eq!(breakdown.color, 1);
        assert_eq!(breakdown.duplicate, 2, "重复合并与轨道装不下共用一个桶");
        // 诊断码 0 是"没被屏蔽"，不该计入任何桶
        assert_eq!(breakdown.total(), 13);
    }

    /// 诊断行文字：**没有任何屏蔽时也要给出数字**，而不是让那一行消失或留空。
    #[test]
    fn filter_summary_always_reports_a_number() {
        // 没加载弹幕：显示占位符（不是 "屏蔽 0 / 0"）
        assert_eq!(DanmakuPreparedStats::default().filter_summary(), "—");

        // 加载了 1200 条、一条没屏蔽：仍然给出 0 与总数
        let none_filtered = DanmakuPreparedStats {
            source_items: 1200,
            ..DanmakuPreparedStats::default()
        };
        assert_eq!(none_filtered.filter_summary(), "屏蔽 0 / 1200");

        // 有屏蔽：按原因分类，且分类之和等于总数
        let mut breakdown = DanmakuFilterBreakdown::default();
        breakdown.type_blocked = 12;
        breakdown.keyword = 20;
        breakdown.regex = 5;
        let some_filtered = DanmakuPreparedStats {
            source_items: 1200,
            filtered_items: 37,
            filter_breakdown: breakdown,
            ..DanmakuPreparedStats::default()
        };
        assert_eq!(
            some_filtered.filter_summary(),
            "屏蔽 37 / 1200（类型 12 · 关键词 20 · 正则 5）"
        );
    }
}
