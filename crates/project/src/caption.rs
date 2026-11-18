//! Caption tracks.
//!
//! Premiere keeps captions in their own track area above the video tracks. A [`CaptionTrack`] has a
//! format (Subtitle, CEA-608, CEA-708, Teletext), a track style (font, size, colour, background box,
//! position) and a list of [`Caption`] blocks with exact in/out times in sequence ticks. Captions on
//! one track never overlap (like track items); [`CaptionTrack::check`] verifies it.
//!
//! Caption text is kept as written (lines separated by `\n`, inline `<i>`/`<b>`/`<u>` tags and
//! WebVTT cue settings preserved) so files round-trip; renderers strip markup for display.

use filmcraft_time::{Tick, TimeRange};
use serde::{Deserialize, Serialize};

use crate::{ClipId, TrackId};

/// Caption stream format of a track (Premiere's "Caption Format" choices).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CaptionFormat {
    /// Open/sidecar subtitles (SRT, WebVTT).
    #[default]
    Subtitle,
    /// CEA-608 (line 21) closed captions.
    Cea608,
    /// CEA-708 (DTVCC) closed captions.
    Cea708,
    /// EBU Teletext subtitles.
    Teletext,
}

impl CaptionFormat {
    pub const ALL: [CaptionFormat; 4] = [CaptionFormat::Subtitle, CaptionFormat::Cea608, CaptionFormat::Cea708, CaptionFormat::Teletext];
    pub fn label(self) -> &'static str {
        match self {
            CaptionFormat::Subtitle => "Subtitle",
            CaptionFormat::Cea608 => "CEA-608",
            CaptionFormat::Cea708 => "CEA-708",
            CaptionFormat::Teletext => "Teletext",
        }
    }
    pub fn from_name(s: &str) -> Option<CaptionFormat> {
        let n = s.to_ascii_lowercase().replace(['-', ' ', '_'], "");
        Self::ALL.iter().copied().find(|f| f.label().to_ascii_lowercase().replace(['-', ' '], "") == n)
    }
    /// Characters per line limit used when wrapping (608 is fixed at 32 columns).
    pub fn max_columns(self) -> Option<usize> {
        match self {
            CaptionFormat::Cea608 => Some(32),
            CaptionFormat::Teletext => Some(40),
            CaptionFormat::Cea708 => Some(42),
            CaptionFormat::Subtitle => None,
        }
    }
}

/// Horizontal text alignment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CaptionAlign {
    Left,
    #[default]
    Center,
    Right,
}

/// Vertical placement of the caption block in the frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CaptionAnchor {
    Top,
    Middle,
    #[default]
    Bottom,
}

/// Built-in caption looks, selected on a caption track and saved with the project.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptionTemplate {
    #[default]
    Classic,
    ColorPop,
    Shine,
    #[serde(alias = "type_on_four")]
    TypeOnTwo,
    WordReveal,
    Karaoke,
    PopWord,
    WordMarkerBox,
    WordMarkerUnderline,
    Custom,
}

impl CaptionTemplate {
    pub const BUILT_IN: [Self; 9] = [
        Self::Classic,
        Self::ColorPop,
        Self::Shine,
        Self::TypeOnTwo,
        Self::WordReveal,
        Self::Karaoke,
        Self::PopWord,
        Self::WordMarkerBox,
        Self::WordMarkerUnderline,
    ];

    pub fn id(self) -> &'static str {
        match self {
            Self::Classic => "classic",
            Self::ColorPop => "color_pop",
            Self::Shine => "shine",
            Self::TypeOnTwo => "type_on_two",
            Self::WordReveal => "word_reveal",
            Self::Karaoke => "karaoke",
            Self::PopWord => "pop_word",
            Self::WordMarkerBox => "word_marker_box",
            Self::WordMarkerUnderline => "word_marker_underline",
            Self::Custom => "custom",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Classic => "Classic",
            Self::ColorPop => "Color Pop",
            Self::Shine => "Shine",
            Self::TypeOnTwo => "Type on (2 characters)",
            Self::WordReveal => "Word by word",
            Self::Karaoke => "Karaoke highlight",
            Self::PopWord => "Pop word",
            Self::WordMarkerBox => "Word marker (box)",
            Self::WordMarkerUnderline => "Word marker (underline)",
            Self::Custom => "Custom",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::BUILT_IN
            .into_iter()
            .find(|template| template.id().eq_ignore_ascii_case(name) || template.label().eq_ignore_ascii_case(name))
            .or_else(|| name.eq_ignore_ascii_case("type_on_four").then_some(Self::TypeOnTwo))
    }

    /// The look and animation for this built-in template.
    pub fn style(self) -> CaptionStyle {
        let mut style = CaptionStyle { template: self, ..CaptionStyle::default() };
        match self {
            Self::Classic => {}
            Self::ColorPop => {
                style.color = [255, 224, 112, 255];
                style.background_color = [20, 25, 70, 224];
                style.outline = 1.5;
                style.outline_color = [18, 30, 78, 255];
                style.animation = CaptionAnimation::Grow;
            }
            Self::Shine => {
                style.color = [176, 236, 255, 255];
                style.background = false;
                style.outline = 2.0;
                style.outline_color = [10, 48, 86, 255];
                style.animation = CaptionAnimation::Shine;
            }
            Self::TypeOnTwo => {
                style.color = [255, 255, 255, 255];
                style.background_color = [12, 45, 80, 224];
                style.animation = CaptionAnimation::TypeOnTwo;
            }
            Self::WordReveal => {
                style.color = [255, 255, 255, 255];
                style.animation = CaptionAnimation::WordReveal;
            }
            Self::Karaoke => {
                style.color = [255, 255, 255, 255];
                style.background = false;
                style.outline = 1.5;
                style.outline_color = [12, 28, 48, 255];
                style.animation = CaptionAnimation::Karaoke;
            }
            Self::PopWord => {
                style.color = [255, 255, 255, 255];
                style.animation = CaptionAnimation::PopWord;
            }
            Self::WordMarkerBox => {
                style.color = [255, 255, 255, 255];
                style.animation = CaptionAnimation::WordMarkerBox;
                style.background = false;
            }
            Self::WordMarkerUnderline => {
                style.color = [255, 255, 255, 255];
                style.animation = CaptionAnimation::WordMarkerUnderline;
                style.background = false;
            }
            Self::Custom => {}
        }
        style
    }
}

/// Per-caption animation used by burn-in rendering. Caption sidecar exports remain text-only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptionAnimation {
    #[default]
    None,
    Grow,
    Shine,
    #[serde(alias = "type_on_four")]
    TypeOnTwo,
    WordReveal,
    Karaoke,
    PopWord,
    WordMarkerBox,
    WordMarkerUnderline,
}

impl CaptionAnimation {
    pub fn id(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Grow => "grow",
            Self::Shine => "shine",
            Self::TypeOnTwo => "type_on_two",
            Self::WordReveal => "word_reveal",
            Self::Karaoke => "karaoke",
            Self::PopWord => "pop_word",
            Self::WordMarkerBox => "word_marker_box",
            Self::WordMarkerUnderline => "word_marker_underline",
        }
    }
}

/// Track-level caption style (Premiere's caption track style / Essential Graphics text settings).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptionStyle {
    /// The selected built-in look, or `Custom` after manually changing style properties.
    pub template: CaptionTemplate,
    /// Burn-in animation; ignored by caption sidecar export.
    pub animation: CaptionAnimation,
    /// Animation rate multiplier. Transcript-synced word boundaries are not retimed.
    pub animation_speed: f32,
    /// Number of words revealed or highlighted together (1–5).
    pub words_per_group: u8,
    /// Word-marker box fill opacity as a percentage (0–100).
    pub word_marker_opacity: u8,
    /// Word-marker box border colour, sRGB + alpha.
    pub word_marker_border_color: [u8; 4],
    /// Word-marker box border width in pixels at 1080-line frame height.
    pub word_marker_border_width: f32,
    /// Word-marker box corner radius in pixels at 1080-line frame height.
    pub word_marker_radius: f32,
    /// Maximum rendered lines; text size is reduced as needed so all caption text remains visible (1–5).
    pub max_lines: u8,
    /// Highlight colour for karaoke and pop-word animations.
    pub highlight_color: [u8; 4],
    /// Font family name (bundled, craft-fonts or system); unknown or empty names fall back to Inter.
    pub font: String,
    /// Font size in pixels for a 1080-line frame (scaled with the frame height).
    pub size: f32,
    /// Text colour, sRGB + alpha.
    pub color: [u8; 4],
    /// Draw a background box behind each line.
    pub background: bool,
    /// Background box colour, sRGB + alpha.
    pub background_color: [u8; 4],
    /// Horizontal alignment of the background box, independent from text alignment.
    pub box_align: CaptionAlign,
    pub align: CaptionAlign,
    pub anchor: CaptionAnchor,
    /// Distance from the anchored frame edge, as a fraction of the frame height.
    pub margin: f32,
    /// Line height as a multiple of the font size.
    pub line_spacing: f32,
    /// Text outline width in pixels at 1080 lines (0 = none).
    pub outline: f32,
    pub outline_color: [u8; 4],
    /// Draw a soft shadow behind the caption and its background box.
    pub shadow: bool,
    /// Shadow colour, sRGB + alpha.
    pub shadow_color: [u8; 4],
    /// Shadow blur radius in pixels at 1080-line frame height.
    pub shadow_blur: f32,
    /// Horizontal shadow offset in pixels at 1080-line frame height.
    pub shadow_offset_x: f32,
    /// Vertical shadow offset in pixels at 1080-line frame height.
    pub shadow_offset_y: f32,
}

impl Default for CaptionStyle {
    fn default() -> Self {
        Self {
            template: CaptionTemplate::Classic,
            animation: CaptionAnimation::None,
            animation_speed: 1.0,
            words_per_group: 1,
            word_marker_opacity: 38,
            word_marker_border_color: [255, 208, 64, 255],
            word_marker_border_width: 0.0,
            word_marker_radius: 5.5,
            max_lines: 2,
            highlight_color: [255, 208, 64, 255],
            font: "Inter".into(),
            size: 54.0,
            color: [255, 255, 255, 255],
            background: true,
            background_color: [0, 0, 0, 191],
            box_align: CaptionAlign::Center,
            align: CaptionAlign::Center,
            anchor: CaptionAnchor::Bottom,
            margin: 0.08,
            line_spacing: 1.25,
            outline: 0.0,
            outline_color: [0, 0, 0, 255],
            shadow: false,
            shadow_color: [0, 0, 0, 200],
            shadow_blur: 8.0,
            shadow_offset_x: 0.0,
            shadow_offset_y: 4.0,
        }
    }
}

/// One transcript word's timing relative to the caption's start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptionWordTiming {
    pub start: Tick,
    pub end: Tick,
}

/// One caption block on a caption track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Caption {
    pub id: ClipId,
    /// Timeline start (sequence ticks).
    pub start: Tick,
    pub duration: Tick,
    /// Caption text; lines separated by `\n`.
    pub text: String,
    /// Optional transcript word timings relative to `start`; empty for imported or edited captions.
    #[serde(default)]
    pub word_timings: Vec<CaptionWordTiming>,
    /// Speaker name (WebVTT `<v Name>` voice span).
    #[serde(default)]
    pub speaker: Option<String>,
    /// WebVTT cue identifier, kept for round trips.
    #[serde(default)]
    pub cue_id: Option<String>,
    /// WebVTT cue settings (`line:90% align:start`…), kept verbatim.
    #[serde(default)]
    pub settings: String,
}

impl Caption {
    pub fn end(&self) -> Tick {
        self.start + self.duration
    }
    pub fn range(&self) -> TimeRange {
        TimeRange::new(self.start, self.duration)
    }
    /// The caption's text with markup removed and entities decoded (what is drawn).
    pub fn plain_lines(&self) -> Vec<String> {
        plain_text(&self.text).lines().map(str::to_string).collect()
    }
}

/// A caption track (shown in the caption area above the video tracks).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptionTrack {
    pub id: TrackId,
    pub name: String,
    pub format: CaptionFormat,
    /// BCP-47 language tag (`en`, `fr-CA`…).
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub style: CaptionStyle,
    /// Track output (eye): shown in the Program monitor and burned in on export.
    pub enabled: bool,
    pub locked: bool,
    /// Follows inserts / extracts on other tracks.
    #[serde(default = "yes")]
    pub sync_lock: bool,
    /// Captions sorted by start, non-overlapping.
    pub captions: Vec<Caption>,
    /// WebVTT header blocks (STYLE / REGION / NOTE) kept for round trips.
    #[serde(default)]
    pub vtt_blocks: Vec<String>,
}

fn yes() -> bool {
    true
}

impl CaptionTrack {
    pub fn new(id: TrackId, name: String, format: CaptionFormat) -> Self {
        Self {
            id,
            name,
            format,
            language: "en".into(),
            style: CaptionStyle::default(),
            enabled: true,
            locked: false,
            sync_lock: true,
            captions: Vec::new(),
            vtt_blocks: Vec::new(),
        }
    }
    pub fn end(&self) -> Tick {
        self.captions.iter().map(Caption::end).max().unwrap_or(Tick::ZERO)
    }
    /// Caption covering `t`.
    pub fn caption_at(&self, t: Tick) -> Option<&Caption> {
        let idx = self.captions.partition_point(|c| c.start <= t);
        idx.checked_sub(1).map(|i| &self.captions[i]).filter(|c| t < c.end())
    }
    pub fn caption(&self, id: ClipId) -> Option<&Caption> {
        self.captions.iter().find(|c| c.id == id)
    }
    pub fn caption_mut(&mut self, id: ClipId) -> Option<&mut Caption> {
        self.captions.iter_mut().find(|c| c.id == id)
    }
    pub fn sort(&mut self) {
        self.captions.sort_by_key(|c| c.start);
    }
    /// Invariant check: captions sorted, positive duration, no overlaps.
    pub fn check(&self) -> Result<(), String> {
        self.check_bounds()?;
        for c in &self.captions {
            if c.duration.0 <= 0 {
                return Err(format!("{}: caption {:?} has non-positive duration", self.name, c.id));
            }
        }
        for w in self.captions.windows(2) {
            if w[0].end() > w[1].start {
                return Err(format!("{}: captions {:?} and {:?} overlap", self.name, w[0].id, w[1].id));
            }
        }
        Ok(())
    }
    /// Only the time bounds that keep caption arithmetic from overflowing (see `Track::check_bounds`).
    pub fn check_bounds(&self) -> Result<(), String> {
        for c in &self.captions {
            if !crate::bounded_time_range(c.start, c.duration) {
                return Err(format!("{}: caption {:?} is outside supported time bounds", self.name, c.id));
            }
        }
        Ok(())
    }
}

/// Strip `<…>` tags and decode the common HTML/WebVTT entities.
pub fn plain_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '<' => {
                // A tag runs to the next '>'.  Scan forward once; preserving an unterminated
                // tag as literal text must not rescan every later opening bracket.
                let mut literal = String::new();
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == '>' {
                        closed = true;
                        break;
                    }
                    literal.push(next);
                }
                if !closed {
                    out.push('<');
                    out.push_str(&literal);
                }
            }
            '{' if chars.peek() == Some(&'\\') => {
                // SSA-style override blocks some SRT files carry: {\an8}
                let mut literal = String::new();
                let mut closed = false;
                for next in chars.by_ref() {
                    if next == '}' {
                        closed = true;
                        break;
                    }
                    literal.push(next);
                }
                if !closed {
                    out.push('{');
                    out.push_str(&literal);
                }
            }
            '&' => {
                let rest: String = chars.clone().take(8).collect();
                let ents = [
                    ("amp;", '&'),
                    ("lt;", '<'),
                    ("gt;", '>'),
                    ("nbsp;", '\u{a0}'),
                    ("quot;", '"'),
                    ("apos;", '\''),
                    ("lrm;", '\u{200e}'),
                    ("rlm;", '\u{200f}'),
                ];
                if let Some((name, ch)) = ents.iter().find(|(n, _)| rest.starts_with(n)) {
                    for _ in 0..name.len() {
                        chars.next();
                    }
                    out.push(*ch);
                } else {
                    out.push('&');
                }
            }
            '\r' => {}
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain() {
        assert_eq!(plain_text("<i>Hello</i> &amp; <b>bye</b>"), "Hello & bye");
        assert_eq!(plain_text("{\\an8}Top"), "Top");
        assert_eq!(plain_text("a < b"), "a < b");
        assert_eq!(plain_text("<v Bob>Hi</v>"), "Hi");
    }

    #[test]
    fn malformed_caption_markup_is_preserved_without_repeated_suffix_scans() {
        let angle = "<".repeat(10_000);
        let ssa = format!("{{\\{}", "{".repeat(10_000));
        assert_eq!(plain_text(&angle), angle);
        assert_eq!(plain_text(&ssa), ssa);
    }

    #[test]
    fn caption_at() {
        let mut t = CaptionTrack::new(TrackId(1), "Subtitle".into(), CaptionFormat::Subtitle);
        for (i, s) in [0i64, 100, 300].iter().enumerate() {
            t.captions.push(Caption {
                id: ClipId(i as u64),
                start: Tick(*s),
                duration: Tick(50),
                text: format!("c{i}"),
                word_timings: Vec::new(),
                speaker: None,
                cue_id: None,
                settings: String::new(),
            });
        }
        assert_eq!(t.caption_at(Tick(120)).unwrap().text, "c1");
        assert!(t.caption_at(Tick(160)).is_none());
        assert!(t.check().is_ok());
        assert_eq!(CaptionFormat::from_name("cea608"), Some(CaptionFormat::Cea608));
        assert_eq!(CaptionFormat::from_name("CEA-708"), Some(CaptionFormat::Cea708));
    }

    #[test]
    fn previous_type_on_four_values_load_as_two_character_template() {
        assert_eq!(CaptionTemplate::from_name("type_on_four"), Some(CaptionTemplate::TypeOnTwo));
        assert_eq!(serde_json::from_str::<CaptionTemplate>(r#""type_on_four""#).expect("legacy template parses"), CaptionTemplate::TypeOnTwo);
        assert_eq!(serde_json::from_str::<CaptionAnimation>(r#""type_on_four""#).expect("legacy animation parses"), CaptionAnimation::TypeOnTwo);
    }

    #[test]
    fn older_caption_data_defaults_new_word_animation_fields() {
        let caption = Caption {
            id: ClipId(1),
            start: Tick::ZERO,
            duration: Tick(100),
            text: "hello".into(),
            word_timings: Vec::new(),
            speaker: None,
            cue_id: None,
            settings: String::new(),
        };
        let mut caption_json = serde_json::to_value(caption).expect("serialize caption");
        caption_json.as_object_mut().expect("caption object").remove("word_timings");
        let caption: Caption = serde_json::from_value(caption_json).expect("legacy caption loads");
        assert!(caption.word_timings.is_empty());

        let mut style_json = serde_json::to_value(CaptionStyle::default()).expect("serialize style");
        let style_map = style_json.as_object_mut().expect("style object");
        style_map.remove("animation_speed");
        style_map.remove("words_per_group");
        style_map.remove("word_marker_opacity");
        style_map.remove("word_marker_border_color");
        style_map.remove("word_marker_border_width");
        style_map.remove("word_marker_radius");
        style_map.remove("max_lines");
        style_map.remove("highlight_color");
        style_map.remove("box_align");
        style_map.remove("shadow");
        style_map.remove("shadow_color");
        style_map.remove("shadow_blur");
        style_map.remove("shadow_offset_x");
        style_map.remove("shadow_offset_y");
        let style: CaptionStyle = serde_json::from_value(style_json).expect("legacy style loads");
        assert_eq!(style.animation_speed, 1.0);
        assert_eq!(style.words_per_group, 1);
        assert_eq!(style.word_marker_opacity, 38);
        assert_eq!(style.word_marker_border_color, [255, 208, 64, 255]);
        assert_eq!(style.word_marker_border_width, 0.0);
        assert_eq!(style.word_marker_radius, 5.5);
        assert_eq!(style.max_lines, 2);
        assert_eq!(style.highlight_color, [255, 208, 64, 255]);
        assert_eq!(style.box_align, CaptionAlign::Center);
        assert!(!style.shadow);
    }

    #[test]
    fn corrupt_caption_times_do_not_overflow_the_overlap_check() {
        let mut track = CaptionTrack::new(TrackId(1), "Subtitles".into(), CaptionFormat::Subtitle);
        for (id, start) in [(1, i64::MAX), (2, 10)] {
            track.captions.push(Caption {
                id: ClipId(id),
                start: Tick(start),
                duration: Tick(50),
                text: String::new(),
                word_timings: Vec::new(),
                speaker: None,
                cue_id: None,
                settings: String::new(),
            });
        }
        let result = std::panic::catch_unwind(|| track.check());
        assert!(result.is_ok());
        assert!(result.unwrap().is_err());
    }
}
