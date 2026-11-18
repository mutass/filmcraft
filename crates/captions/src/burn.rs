//! Caption layout and burn-in.
//!
//! [`overlay`] lays out the caption showing on a track at a time with the track's
//! [`CaptionStyle`] (size relative to a 1080-line frame, colour, background box, outline,
//! alignment, anchor and margin; WebVTT `line:N%` and `align:` cue settings override the style)
//! and rasterises it into a small premultiplied linear-light RGBA [`Overlay`] positioned in the
//! frame. Renderers composite overlays over the finished picture.

use std::ops::Range;

use filmcraft_color::srgb_to_linear;
use filmcraft_project::{Caption, CaptionAlign, CaptionAnchor, CaptionAnimation, CaptionStyle, CaptionTrack, Sequence};
use filmcraft_time::{TICKS_PER_SECOND, Tick};
use unicode_segmentation::UnicodeSegmentation;

use filmcraft_text::{ParagraphStyle, TextStyle, layout, render};

/// Caption overlays are transient render resources, not source images. Keep one cue from
/// materialising an unbounded mask or RGBA buffer.
const MAX_OVERLAY_PIXELS: usize = 16 << 20;
const MAX_CAPTION_SIZE_1080: f32 = 256.0;
const MAX_OUTLINE_RADIUS: f32 = 64.0;
const MAX_ANIMATED_WORDS: usize = 8192;

/// A rendered caption: premultiplied linear-light RGBA f32 pixels at `(x, y)` in the frame.
#[derive(Clone, Debug, PartialEq)]
pub struct Overlay {
    pub x: i32,
    pub y: i32,
    pub w: usize,
    pub h: usize,
    pub px: Vec<f32>,
}

impl Overlay {
    /// Composite over a premultiplied RGBA f32 canvas of `cw`×`ch` pixels.
    pub fn composite_onto(&self, canvas: &mut [f32], cw: usize, ch: usize) {
        for y in 0..self.h {
            let cy = self.y + y as i32;
            if cy < 0 || cy >= ch as i32 {
                continue;
            }
            for x in 0..self.w {
                let cx = self.x + x as i32;
                if cx < 0 || cx >= cw as i32 {
                    continue;
                }
                let s = &self.px[(y * self.w + x) * 4..(y * self.w + x) * 4 + 4];
                if s[3] <= 0.0 {
                    continue;
                }
                let i = (cy as usize * cw + cx as usize) * 4;
                let k = 1.0 - s[3];
                for c in 0..4 {
                    canvas[i + c] = s[c] + canvas[i + c] * k;
                }
            }
        }
    }
}

/// The caption face: the track's font family (Inter when empty or not installed) in SemiBold at `px`.
fn caption_style(font: &str, px: f32) -> TextStyle {
    let family = if font.trim().is_empty() { filmcraft_text::fonts::DEFAULT_FAMILY } else { font.trim() };
    TextStyle { family: family.into(), style: "SemiBold".into(), size: px, ..Default::default() }
}

fn lin(c: [u8; 4]) -> [f32; 4] {
    let a = c[3] as f32 / 255.0;
    [srgb_to_linear(c[0] as f32 / 255.0) * a, srgb_to_linear(c[1] as f32 / 255.0) * a, srgb_to_linear(c[2] as f32 / 255.0) * a, a]
}

/// Word-wrap one line to `max_w` pixels.
fn wrap(line: &str, ts: &TextStyle, max_w: f32) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in line.split(' ') {
        let cand = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
        if !cur.is_empty() && filmcraft_text::measure(&cand, ts) > max_w {
            out.push(std::mem::take(&mut cur));
            cur = word.to_string();
        } else {
            cur = cand;
        }
    }
    out.push(cur);
    out
}

fn fit_caption_lines(text: &str, font: &str, px: f32, max_w: f32, max_lines: usize) -> (TextStyle, Vec<String>) {
    let style_at = |size| caption_style(font, size);
    let wrapped_at = |size| {
        let style = style_at(size);
        let lines = wrap(text, &style, max_w);
        (style, lines)
    };
    let (style, lines) = wrapped_at(px);
    if lines.len() <= max_lines {
        return (style, lines);
    }

    let min_size = 4.0_f32.min(px);
    let (min_style, min_lines) = wrapped_at(min_size);
    if min_lines.len() > max_lines {
        return (min_style, min_lines);
    }
    let (mut low, mut high) = (min_size, px);
    for _ in 0..12 {
        let mid = (low + high) * 0.5;
        if wrap(text, &style_at(mid), max_w).len() <= max_lines {
            low = mid;
        } else {
            high = mid;
        }
    }
    let style = style_at(low);
    let lines = wrap(text, &style, max_w);
    (style, lines)
}

fn blur_mask(mask: &[f32], w: usize, h: usize, radius: usize) -> Vec<f32> {
    if radius == 0 || w == 0 || h == 0 || w.checked_mul(h) != Some(mask.len()) {
        return mask.to_vec();
    }
    let mut horizontal = vec![0.0; mask.len()];
    for y in 0..h {
        let mut start = 0usize;
        let mut end = radius.min(w - 1);
        let mut sum = (start..=end).map(|x| mask[y * w + x]).sum::<f32>();
        for x in 0..w {
            horizontal[y * w + x] = sum / (end - start + 1) as f32;
            let next_start = x.saturating_add(1).saturating_sub(radius).min(w - 1);
            let next_end = x.saturating_add(1).saturating_add(radius).min(w - 1);
            if next_start > start {
                sum -= mask[y * w + start];
                start = next_start;
            }
            if next_end > end {
                end += 1;
                sum += mask[y * w + end];
            }
        }
    }
    let mut blurred = vec![0.0; mask.len()];
    for x in 0..w {
        let mut start = 0usize;
        let mut end = radius.min(h - 1);
        let mut sum = (start..=end).map(|y| horizontal[y * w + x]).sum::<f32>();
        for y in 0..h {
            blurred[y * w + x] = sum / (end - start + 1) as f32;
            let next_start = y.saturating_add(1).saturating_sub(radius).min(h - 1);
            let next_end = y.saturating_add(1).saturating_add(radius).min(h - 1);
            if next_start > start {
                sum -= horizontal[start * w + x];
                start = next_start;
            }
            if next_end > end {
                end += 1;
                sum += horizontal[end * w + x];
            }
        }
    }
    blurred
}

fn in_rounded_rect(x: f32, y: f32, rect: [f32; 4], radius: f32) -> bool {
    let [x0, y0, x1, y1] = rect;
    if x < x0 || x >= x1 || y < y0 || y >= y1 {
        return false;
    }
    let radius = radius.min((x1 - x0) * 0.5).min((y1 - y0) * 0.5);
    if radius <= 0.0 {
        return true;
    }
    let (cx, cy) = (x.clamp(x0 + radius, x1 - radius), y.clamp(y0 + radius, y1 - radius));
    let (dx, dy) = (x - cx, y - cy);
    dx * dx + dy * dy <= radius * radius
}

fn add_word_marker(
    mask: &mut [f32],
    mut border: Option<&mut [f32]>,
    w: usize,
    h: usize,
    rect: [f32; 4],
    x: f32,
    y: f32,
    box_marker: bool,
    px: f32,
    corner_radius: f32,
    border_width: f32,
) {
    let pad = if box_marker { (px * 0.08).clamp(2.0, 8.0) } else { 0.0 };
    let thickness = (px * 0.08).clamp(2.0, 8.0);
    let (x0, x1) = (x + rect[0] - pad, x + rect[2] + pad);
    let (y0, y1) = if box_marker {
        (y + rect[1] - pad, y + rect[3] + pad)
    } else {
        let bottom = y + rect[3] + (px * 0.04).clamp(1.0, 4.0);
        (bottom - thickness, bottom)
    };
    if ![x0, x1, y0, y1].iter().all(|v| v.is_finite()) || x1 <= x0 || y1 <= y0 {
        return;
    }
    let outer = [x0, y0, x1, y1];
    let radius = if box_marker { corner_radius.clamp(0.0, 40.0).min((x1 - x0) * 0.5).min((y1 - y0) * 0.5) } else { 0.0 };
    let border_width = if box_marker { border_width.clamp(0.0, 20.0).min((x1 - x0) * 0.5).min((y1 - y0) * 0.5) } else { 0.0 };
    let inner = [x0 + border_width, y0 + border_width, x1 - border_width, y1 - border_width];
    let inner_radius = (radius - border_width).max(0.0);
    let start_x = (x0.floor() as i32).max(0);
    let end_x = (x1.ceil() as i32).min(w as i32);
    let start_y = (y0.floor() as i32).max(0);
    let end_y = (y1.ceil() as i32).min(h as i32);
    for py in start_y..end_y {
        for px_i in start_x..end_x {
            let fx = px_i as f32 + 0.5;
            let fy = py as f32 + 0.5;
            if in_rounded_rect(fx, fy, outer, radius) {
                let Some(index) = (py as usize).checked_mul(w).and_then(|row| row.checked_add(px_i as usize)) else { continue };
                if let Some(pixel) = mask.get_mut(index) {
                    *pixel = 1.0;
                }
                if border_width > 0.0
                    && !in_rounded_rect(fx, fy, inner, inner_radius)
                    && let Some(border_mask) = border.as_deref_mut()
                    && let Some(pixel) = border_mask.get_mut(index)
                {
                    *pixel = 1.0;
                }
            }
        }
    }
}

fn word_ranges(line: &str) -> Option<Vec<Range<usize>>> {
    let mut ranges: Vec<Range<usize>> = Vec::new();
    let mut pending_start = None;
    let mut separated = true;
    for (start, part) in line.split_word_bound_indices() {
        let end = start.checked_add(part.len())?;
        if part.chars().all(char::is_whitespace) {
            pending_start = None;
            separated = true;
            continue;
        }
        if part.chars().any(char::is_alphanumeric) {
            if ranges.len() >= MAX_ANIMATED_WORDS {
                return None;
            }
            ranges.push(pending_start.take().unwrap_or(start)..end);
        } else if !separated {
            if let Some(last) = ranges.last_mut() {
                last.end = end;
            } else {
                pending_start = Some(start);
            }
        } else {
            pending_start = Some(start);
        }
        separated = false;
    }
    if ranges.is_empty() {
        for (start, grapheme) in line.grapheme_indices(true) {
            if grapheme.chars().all(char::is_whitespace) {
                continue;
            }
            if ranges.len() >= MAX_ANIMATED_WORDS {
                return None;
            }
            ranges.push(start..start.checked_add(grapheme.len())?);
        }
    }
    Some(ranges)
}

fn lines_word_ranges(lines: &[String]) -> Option<Vec<Vec<Range<usize>>>> {
    let mut count = 0usize;
    let mut result = Vec::with_capacity(lines.len());
    for line in lines {
        let ranges = word_ranges(line)?;
        count = count.checked_add(ranges.len())?;
        if count > MAX_ANIMATED_WORDS {
            return None;
        }
        result.push(ranges);
    }
    Some(result)
}

fn truncate_to_words(lines: Vec<String>, ranges: &[Vec<Range<usize>>], visible_words: usize) -> Vec<String> {
    let mut remaining = visible_words;
    lines
        .into_iter()
        .zip(ranges)
        .map(|(line, words)| {
            if remaining >= words.len() {
                remaining = remaining.saturating_sub(words.len());
                return line;
            }
            let Some(last) = remaining.checked_sub(1).and_then(|i| words.get(i)) else {
                return String::new();
            };
            remaining = 0;
            line.get(..last.end).unwrap_or("").to_string()
        })
        .collect()
}

#[derive(Clone, Debug, Default)]
struct WordAnimationState {
    revealed_words: usize,
    active_words: Option<Range<usize>>,
    pop_scale: f32,
}

fn animation_speed_milli(style: &CaptionStyle) -> i128 {
    let speed = if style.animation_speed.is_finite() { style.animation_speed.clamp(0.25, 4.0) } else { 1.0 };
    i128::from((speed * 1000.0).round() as i64)
}

fn pop_scale(age: i64, speed_milli: i128) -> f32 {
    let window = (i128::from(TICKS_PER_SECOND) * 18 / 100 * 1000 / speed_milli.max(1)).max(1);
    let progress = (i128::from(age.max(0)) as f32 / window as f32).clamp(0.0, 1.0);
    let remaining = 1.0 - progress;
    1.0 + 0.12 * remaining * remaining
}

fn word_animation_state(c: &Caption, style: &CaptionStyle, elapsed: i64, word_count: usize) -> WordAnimationState {
    if word_count == 0 {
        return WordAnimationState::default();
    }
    let group_size = usize::from(style.words_per_group.clamp(1, 5));
    let duration = c.duration.0.max(0);
    let elapsed = elapsed.max(0);
    let speed_milli = animation_speed_milli(style);
    let timed = c.word_timings.len() == word_count
        && c.word_timings.iter().all(|timing| timing.end.0 > timing.start.0 && timing.start.0 >= 0 && timing.end.0 <= duration);
    if timed {
        let revealed = c.word_timings.iter().filter(|timing| timing.start.0 <= elapsed).count();
        let active = c.word_timings.iter().position(|timing| timing.start.0 <= elapsed && elapsed < timing.end.0);
        let active_words = active.map(|index| {
            let first = index / group_size * group_size;
            first..first.saturating_add(group_size).min(word_count)
        });
        let revealed_words = revealed.checked_add(group_size - 1).map(|n| n / group_size * group_size).unwrap_or(word_count).min(word_count);
        let scale = if style.animation == CaptionAnimation::PopWord {
            active.and_then(|index| c.word_timings.get(index)).map(|timing| pop_scale(elapsed.saturating_sub(timing.start.0), speed_milli)).unwrap_or(1.0)
        } else {
            1.0
        };
        return WordAnimationState { revealed_words, active_words, pop_scale: scale };
    }

    let group_count = word_count / group_size + usize::from(!word_count.is_multiple_of(group_size));
    let group_index = if duration > 0 {
        let scaled_elapsed = (i128::from(elapsed).saturating_mul(speed_milli) / 1000).min(i128::from(duration));
        let count = i128::try_from(group_count).unwrap_or(i128::MAX);
        usize::try_from((scaled_elapsed.saturating_mul(count) / i128::from(duration)).max(0))
            .unwrap_or(group_count.saturating_sub(1))
            .min(group_count.saturating_sub(1))
    } else {
        0
    };
    let first = group_index.saturating_mul(group_size).min(word_count);
    let last = first.saturating_add(group_size).min(word_count);
    let group_start = if duration > 0 {
        i64::try_from(i128::from(duration) * i128::try_from(group_index).unwrap_or(i128::MAX) / i128::try_from(group_count).unwrap_or(i128::MAX))
            .unwrap_or(duration)
    } else {
        0
    };
    let pop_scale = if style.animation == CaptionAnimation::PopWord { pop_scale(elapsed.saturating_sub(group_start), speed_milli) } else { 1.0 };
    WordAnimationState { revealed_words: last, active_words: Some(first..last), pop_scale }
}

/// Placement overrides from WebVTT cue settings.
fn vtt_overrides(settings: &str) -> (Option<f32>, Option<CaptionAlign>) {
    let mut line = None;
    let mut align = None;
    for kv in settings.split_whitespace() {
        let Some((k, v)) = kv.split_once(':') else { continue };
        match k {
            "line" => {
                let v = v.split(',').next().unwrap_or("");
                if let Some(p) = v.strip_suffix('%').and_then(|p| p.parse::<f32>().ok()) {
                    line = Some((p / 100.0).clamp(0.0, 1.0));
                }
            }
            "align" => {
                align = match v {
                    "start" | "left" => Some(CaptionAlign::Left),
                    "end" | "right" => Some(CaptionAlign::Right),
                    "center" | "middle" => Some(CaptionAlign::Center),
                    _ => None,
                }
            }
            _ => {}
        }
    }
    (line, align)
}

/// Lay out and rasterise one caption for a `w`×`h` frame.
pub fn render_caption(c: &Caption, style: &CaptionStyle, w: usize, h: usize) -> Option<Overlay> {
    render_caption_at(c, style, w, h, Tick::ZERO)
}

/// Render one caption at its elapsed timeline time, for animated caption templates.
pub fn render_caption_at(c: &Caption, style: &CaptionStyle, w: usize, h: usize, elapsed: Tick) -> Option<Overlay> {
    if w == 0 || h == 0 {
        return None;
    }
    let scale = h as f32 / 1080.0;
    let elapsed = elapsed.0.max(0);
    let speed_milli = animation_speed_milli(style);
    let animation_elapsed = i64::try_from((i128::from(elapsed).saturating_mul(speed_milli) / 1000).max(0)).unwrap_or(i64::MAX);
    let grow = match style.animation {
        CaptionAnimation::Grow => {
            let progress = (animation_elapsed as f32 / (TICKS_PER_SECOND as f32 * 0.28)).clamp(0.0, 1.0);
            let eased = progress * progress * (3.0 - 2.0 * progress);
            0.82 + 0.18 * eased
        }
        _ => 1.0,
    };
    let px = (style.size.clamp(4.0, MAX_CAPTION_SIZE_1080) * grow * scale).clamp(4.0, h as f32);
    let max_w = w as f32 * 0.9;
    let mut plain_lines = vec![c.plain_lines().join(" ")];
    let (ts, _) = fit_caption_lines(&plain_lines[0], &style.font, px, max_w, usize::from(style.max_lines.clamp(1, 5)));
    let px = ts.size;
    let word_ranges = if matches!(
        style.animation,
        CaptionAnimation::WordReveal
            | CaptionAnimation::Karaoke
            | CaptionAnimation::PopWord
            | CaptionAnimation::WordMarkerBox
            | CaptionAnimation::WordMarkerUnderline
    ) {
        lines_word_ranges(&plain_lines)
    } else {
        None
    };
    let word_count = word_ranges.as_ref().map(|all| all.iter().map(Vec::len).sum()).unwrap_or(0);
    let word_state = word_ranges.as_ref().map(|_| word_animation_state(c, style, elapsed, word_count)).unwrap_or_default();
    if style.animation == CaptionAnimation::WordReveal
        && let Some(ranges) = &word_ranges
    {
        plain_lines = truncate_to_words(plain_lines, ranges, word_state.revealed_words);
    }
    let mut lines: Vec<String> = Vec::new();
    // (speaker names are metadata; like Premiere, they are not burned in)
    let mut remaining_chars = match style.animation {
        CaptionAnimation::TypeOnTwo => {
            let interval = i128::from(TICKS_PER_SECOND) * 18 / 100;
            let groups = i128::from(animation_elapsed) / interval.max(1) + 1;
            usize::try_from(groups.saturating_mul(2)).unwrap_or(usize::MAX)
        }
        _ => usize::MAX,
    };
    for l in plain_lines {
        let l = if style.animation == CaptionAnimation::TypeOnTwo {
            let visible: String = l.chars().take(remaining_chars).collect();
            remaining_chars = remaining_chars.saturating_sub(visible.chars().count());
            visible
        } else {
            l
        };
        lines.extend(wrap(l.trim(), &ts, max_w));
    }
    lines.retain(|l| !l.is_empty());
    if lines.is_empty() {
        return None;
    }
    let rendered_word_ranges = if matches!(
        style.animation,
        CaptionAnimation::Karaoke | CaptionAnimation::PopWord | CaptionAnimation::WordMarkerBox | CaptionAnimation::WordMarkerUnderline
    ) {
        lines_word_ranges(&lines)
    } else {
        None
    };
    let vm = filmcraft_text::fonts::face(filmcraft_text::resolve(&ts.family, &ts.style).face).metrics(px);
    let (asc, desc) = (vm.ascent, vm.descent);
    let lh = px * style.line_spacing.clamp(0.8, 4.0);
    let pad_x = (px * 0.3).round();
    let block_h = lh * lines.len() as f32;
    let (line_override, align_override) = vtt_overrides(&c.settings);
    let align = align_override.unwrap_or(style.align);
    let margin = style.margin.clamp(0.0, 0.45) * h as f32;
    let top = match (line_override, style.anchor) {
        (Some(f), _) => (f * h as f32).min(h as f32 - block_h),
        (None, CaptionAnchor::Top) => margin,
        (None, CaptionAnchor::Middle) => (h as f32 - block_h) / 2.0,
        (None, CaptionAnchor::Bottom) => h as f32 - margin - block_h,
    }
    .max(0.0)
    .round();
    let side = w as f32 * 0.05;
    let widths: Vec<f32> = lines.iter().map(|l| filmcraft_text::measure(l, &ts)).collect();
    let box_width = widths.iter().copied().fold(0.0, f32::max) + 2.0 * pad_x;
    let box_align = align_override.unwrap_or(style.box_align);
    let box_x = match box_align {
        CaptionAlign::Left => side,
        CaptionAlign::Right => w as f32 - side - box_width,
        CaptionAlign::Center => (w as f32 - box_width) / 2.0,
    };
    let xs: Vec<f32> = widths
        .iter()
        .map(|&lw| {
            if style.background {
                match align {
                    CaptionAlign::Left => box_x + pad_x,
                    CaptionAlign::Right => box_x + box_width - pad_x - lw,
                    CaptionAlign::Center => box_x + (box_width - lw) / 2.0,
                }
            } else {
                match align {
                    CaptionAlign::Left => side + pad_x,
                    CaptionAlign::Right => w as f32 - side - pad_x - lw,
                    CaptionAlign::Center => (w as f32 - lw) / 2.0,
                }
            }
        })
        .collect();
    let outline = (style.outline * scale).clamp(0.0, MAX_OUTLINE_RADIUS);
    let outline_pad = outline.ceil() + 1.0;
    let animation_pad = if style.animation == CaptionAnimation::PopWord { px * 0.12 } else { 0.0 };
    let shadow_blur = if style.shadow_blur.is_finite() { style.shadow_blur.clamp(0.0, 40.0) * scale } else { 0.0 };
    let shadow_radius = shadow_blur.round().clamp(0.0, 32.0) as usize;
    let shadow_offset_x = if style.shadow_offset_x.is_finite() { (style.shadow_offset_x.clamp(-40.0, 40.0) * scale).clamp(-(w as f32), w as f32) } else { 0.0 };
    let shadow_offset_y = if style.shadow_offset_y.is_finite() { (style.shadow_offset_y.clamp(-40.0, 40.0) * scale).clamp(-(h as f32), h as f32) } else { 0.0 };
    let shadow_pad = if style.shadow { shadow_radius as f32 + shadow_offset_x.abs().max(shadow_offset_y.abs()) + 1.0 } else { 0.0 };
    let grow = outline_pad + animation_pad + shadow_pad;
    let text_bx0 = xs.iter().zip(&widths).map(|(x, _)| x - pad_x).fold(f32::MAX, f32::min);
    let text_bx1 = xs.iter().zip(&widths).map(|(x, lw)| x + lw + pad_x).fold(f32::MIN, f32::max);
    let (bx0, bx1) = if style.background { (box_x.min(text_bx0) - grow, (box_x + box_width).max(text_bx1) + grow) } else { (text_bx0 - grow, text_bx1 + grow) };
    let by0 = (top - grow).floor();
    let by1 = (top + block_h + grow).ceil();
    // Text can be wider than the frame (an unbroken imported WebVTT word, for example). Raster
    // only the visible intersection; `draw` receives the original coordinates and clips into it.
    let (left, top) = (bx0.clamp(0.0, w as f32), by0.clamp(0.0, h as f32));
    let (right, bottom) = (bx1.clamp(0.0, w as f32), by1.clamp(0.0, h as f32));
    if right <= left || bottom <= top {
        return None;
    }
    let (ox, oy) = (left as i32, top as i32);
    let (ow, oh) = ((right - left) as usize, (bottom - top) as usize);
    if ow.checked_mul(oh).is_none_or(|pixels| pixels > MAX_OVERLAY_PIXELS) {
        return None;
    }
    let mut cover = vec![0.0f32; ow * oh];
    let mut active_cover = if word_state.active_words.is_some() && rendered_word_ranges.is_some() { Some(vec![0.0f32; ow * oh]) } else { None };
    let is_box_marker = style.animation == CaptionAnimation::WordMarkerBox;
    let is_word_marker = is_box_marker || style.animation == CaptionAnimation::WordMarkerUnderline;
    let mut word_marker = if is_word_marker { Some(vec![0.0f32; ow * oh]) } else { None };
    let mut word_marker_border = if is_box_marker { Some(vec![0.0f32; ow * oh]) } else { None };
    let word_marker_radius = if style.word_marker_radius.is_finite() { style.word_marker_radius.clamp(0.0, 40.0) * scale } else { 5.5 * scale };
    let word_marker_border_width =
        if style.word_marker_border_width.is_finite() { style.word_marker_border_width.clamp(0.0, 20.0) * scale } else { 0.0 };
    let mut bg = vec![0.0f32; ow * oh];
    let mut word_base = 0usize;
    for (i, line) in lines.iter().enumerate() {
        let lt = top + lh * i as f32;
        if style.background && style.background_color[3] > 0 {
            let (x0, x1) = (box_x, box_x + box_width);
            let (y0, y1) = (lt, lt + lh);
            for y in (y0.floor() as i32).max(oy)..(y1.ceil() as i32).min(oy + oh as i32) {
                let fy = ((y as f32 + 1.0).min(y1) - (y as f32).max(y0)).clamp(0.0, 1.0);
                for x in (x0.floor() as i32).max(ox)..(x1.ceil() as i32).min(ox + ow as i32) {
                    let fx = ((x as f32 + 1.0).min(x1) - (x as f32).max(x0)).clamp(0.0, 1.0);
                    let j = (y - oy) as usize * ow + (x - ox) as usize;
                    bg[j] = (bg[j] + fx * fy).min(1.0);
                }
            }
        }
        let baseline = lt + (lh - (asc + desc)) / 2.0 + asc;
        let l = layout(line, &ts, &ParagraphStyle::default());
        let mut m = filmcraft_text::Mask { w: ow, h: oh, a: std::mem::take(&mut cover) };
        render::draw(&l, &render::at(xs[i], baseline.round()), &mut m, (ox, oy));
        cover = m.a;
        if let (Some(active), Some(spans), Some(word_range)) = (&mut active_cover, rendered_word_ranges.as_ref(), word_state.active_words.as_ref())
            && let Some(line_words) = spans.get(i)
        {
            for span in line_words {
                if word_range.contains(&word_base) {
                    let mut word_layout = l.as_ref().clone();
                    word_layout.glyphs.retain(|glyph| span.contains(&glyph.cluster));
                    if is_word_marker && let Some(marker) = &mut word_marker {
                        for rect in l.selection_rects(span.start, span.end) {
                            add_word_marker(
                                marker,
                                word_marker_border.as_deref_mut(),
                                ow,
                                oh,
                                rect,
                                xs[i] - ox as f32,
                                baseline.round() - oy as f32,
                                is_box_marker,
                                px,
                                word_marker_radius,
                                word_marker_border_width,
                            );
                        }
                    }
                    let (mut tx, mut ty) = (xs[i], baseline.round());
                    if style.animation == CaptionAnimation::PopWord && word_state.pop_scale > 1.0 {
                        let rects = l.selection_rects(span.start, span.end);
                        if let Some((x0, y0, x1, y1)) = rects.iter().fold(None::<(f32, f32, f32, f32)>, |bounds, rect| {
                            Some(match bounds {
                                Some((x0, y0, x1, y1)) => (x0.min(rect[0]), y0.min(rect[1]), x1.max(rect[2]), y1.max(rect[3])),
                                None => (rect[0], rect[1], rect[2], rect[3]),
                            })
                        }) {
                            let (cx, cy) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
                            tx += cx * (1.0 - word_state.pop_scale);
                            ty += cy * (1.0 - word_state.pop_scale);
                        }
                    }
                    let transform = if style.animation == CaptionAnimation::PopWord {
                        [word_state.pop_scale, 0.0, 0.0, word_state.pop_scale, tx, ty]
                    } else {
                        render::at(tx, ty)
                    };
                    let mut mask = filmcraft_text::Mask { w: ow, h: oh, a: std::mem::take(active) };
                    render::draw(&word_layout, &transform, &mut mask, (ox, oy));
                    *active = mask.a;
                }
                word_base = word_base.saturating_add(1);
            }
        }
    }
    let stroke = if outline > 0.0 { dilate(&cover, ow, oh, outline) } else { Vec::new() };
    let shadow = if style.shadow {
        let mut shape = cover.clone();
        for (coverage, background) in shape.iter_mut().zip(&bg) {
            *coverage = coverage.max(*background);
        }
        let blurred = blur_mask(&shape, ow, oh, shadow_radius);
        let mut shifted = vec![0.0; blurred.len()];
        let (dx, dy) = (shadow_offset_x.round() as i32, shadow_offset_y.round() as i32);
        for y in 0..oh {
            for x in 0..ow {
                let (sx, sy) = (x as i32 + dx, y as i32 + dy);
                if sx >= 0 && sy >= 0 && sx < ow as i32 && sy < oh as i32 {
                    shifted[sy as usize * ow + sx as usize] = blurred[y * ow + x];
                }
            }
        }
        Some(shifted)
    } else {
        None
    };
    let (tc, bc, oc, sc) = (lin(style.color), lin(style.background_color), lin(style.outline_color), lin(style.shadow_color));
    let active_color = lin(style.highlight_color);
    let marker_alpha = ((u16::from(style.word_marker_opacity.min(100)) * 255) / 100) as u8;
    let marker_color = if is_box_marker { lin([style.highlight_color[0], style.highlight_color[1], style.highlight_color[2], marker_alpha]) } else { active_color };
    let marker_border_color = lin(style.word_marker_border_color);
    let shine = style.animation == CaptionAnimation::Shine && c.duration.0 > 0;
    let shine_band = (ow as f32 * 0.16).max(4.0);
    let shine_progress = if shine { (animation_elapsed as f32 / c.duration.0 as f32).clamp(0.0, 1.0) } else { 0.0 };
    let shine_center = -shine_band + (ow as f32 + 2.0 * shine_band) * shine_progress;
    let highlight = lin([255, 255, 255, 255]);
    let mut out = vec![0.0f32; ow * oh * 4];
    for (j, coverage) in cover.iter().enumerate() {
        let mut p = [0.0f32; 4];
        let over = |p: &mut [f32; 4], c: [f32; 4], a: f32| {
            if a > 0.0 {
                for k in 0..4 {
                    p[k] = c[k] * a + p[k] * (1.0 - c[3] * a);
                }
            }
        };
        if let Some(shadow) = &shadow {
            over(&mut p, sc, shadow[j]);
        }
        over(&mut p, bc, bg[j]);
        let marker_coverage = word_marker.as_ref().and_then(|mask| mask.get(j)).copied().unwrap_or(0.0);
        over(&mut p, marker_color, marker_coverage);
        let marker_border_coverage = word_marker_border.as_ref().and_then(|mask| mask.get(j)).copied().unwrap_or(0.0);
        over(&mut p, marker_border_color, marker_border_coverage);
        if !stroke.is_empty() {
            over(&mut p, oc, stroke[j]);
        }
        let active_coverage = if is_word_marker { 0.0 } else { active_cover.as_ref().and_then(|mask| mask.get(j)).copied().unwrap_or(0.0) };
        let base_coverage = coverage * (1.0 - active_coverage.clamp(0.0, 1.0));
        let color = if shine {
            let x = (j % ow) as f32;
            let weight = ((shine_band - (x - shine_center).abs()) / shine_band).clamp(0.0, 1.0);
            let amount = weight * 0.9;
            [
                tc[0] * (1.0 - amount) + highlight[0] * amount,
                tc[1] * (1.0 - amount) + highlight[1] * amount,
                tc[2] * (1.0 - amount) + highlight[2] * amount,
                tc[3],
            ]
        } else {
            tc
        };
        over(&mut p, color, base_coverage);
        over(&mut p, active_color, active_coverage);
        out[j * 4..j * 4 + 4].copy_from_slice(&p);
    }
    Some(Overlay { x: ox, y: oy, w: ow, h: oh, px: out })
}

/// Max-filter a coverage mask with a disc of radius `r`.
fn dilate(m: &[f32], w: usize, h: usize, r: f32) -> Vec<f32> {
    let ri = r.ceil() as i32;
    let mut offs = Vec::new();
    for dy in -ri..=ri {
        for dx in -ri..=ri {
            let d = ((dx * dx + dy * dy) as f32).sqrt();
            if d <= r + 0.5 {
                offs.push((dx, dy, (r + 0.5 - d).clamp(0.0, 1.0)));
            }
        }
    }
    let mut out = vec![0.0f32; w * h];
    for y in 0..h as i32 {
        for x in 0..w as i32 {
            let mut v = 0.0f32;
            for &(dx, dy, wgt) in &offs {
                let (sx, sy) = (x + dx, y + dy);
                if sx >= 0 && sy >= 0 && sx < w as i32 && sy < h as i32 {
                    v = v.max(m[sy as usize * w + sx as usize] * wgt);
                }
            }
            out[y as usize * w + x as usize] = v;
        }
    }
    out
}

/// The overlay for a caption track at timeline time `t` (None when nothing shows).
pub fn track_overlay(track: &CaptionTrack, t: Tick, w: usize, h: usize) -> Option<Overlay> {
    if !track.enabled {
        return None;
    }
    let c = track.caption_at(t)?;
    render_caption_at(c, &track.style, w, h, Tick(t.0.saturating_sub(c.start.0)))
}

/// Overlays of every visible caption track of a sequence at `t`, bottom track first.
pub fn sequence_overlays(seq: &Sequence, t: Tick, w: usize, h: usize) -> Vec<Overlay> {
    seq.caption_tracks.iter().rev().filter_map(|tr| track_overlay(tr, t, w, h)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use filmcraft_project::{CaptionFormat, CaptionTemplate, CaptionWordTiming, ClipId, TrackId};

    fn cap(text: &str) -> Caption {
        Caption {
            id: ClipId(1),
            start: Tick(0),
            duration: Tick(100),
            text: text.into(),
            word_timings: Vec::new(),
            speaker: None,
            cue_id: None,
            settings: String::new(),
        }
    }

    #[test]
    fn bottom_centre_with_box() {
        let st = CaptionStyle::default();
        let o = render_caption(&cap("Hello world"), &st, 1920, 1080).unwrap();
        // centred horizontally, in the lower part of the frame
        let cx = o.x as f32 + o.w as f32 / 2.0;
        assert!((cx - 960.0).abs() < 4.0, "centre {cx}");
        assert!(o.y > 800 && (o.y as usize + o.h) < 1080, "y {} h {}", o.y, o.h);
        // white text pixels exist and the box is dark translucent
        let mut white = 0;
        let mut boxy = 0;
        for p in o.px.chunks(4) {
            if p[3] > 0.99 && p[0] > 0.9 {
                white += 1;
            } else if p[3] > 0.7 && p[0] < 0.01 {
                boxy += 1;
            }
        }
        assert!(white > 500, "{white}");
        assert!(boxy > 1000, "{boxy}");
    }

    #[test]
    fn top_anchor_and_vtt_line() {
        let st = CaptionStyle { anchor: CaptionAnchor::Top, ..Default::default() };
        let o = render_caption(&cap("Top"), &st, 1280, 720).unwrap();
        assert!(o.y < 100);
        let mut c = cap("Line");
        c.settings = "line:50% align:start".into();
        let o = render_caption(&c, &CaptionStyle::default(), 1280, 720).unwrap();
        assert!((o.y - 360).abs() < 4, "{}", o.y);
        assert!(o.x < 120, "{}", o.x);
    }

    #[test]
    fn margin_is_a_fraction_of_frame_height() {
        // #1542: on a portrait sequence, a bottom-anchored caption with margin 0.31 sits with
        // its bottom edge 0.31 of the frame height above the bottom; the top anchor mirrors it.
        let st = CaptionStyle { anchor: CaptionAnchor::Bottom, margin: 0.31, size: 78.0, ..Default::default() };
        let o = render_caption(&cap("Hello world"), &st, 1080, 1920).unwrap();
        // the overlay pads the content by the outline grow (the default outline is 0, so by 1 px)
        let bottom = (o.y as usize + o.h) as f32;
        let expected = 1920.0 - 0.31 * 1920.0;
        assert!((bottom - expected).abs() <= 2.0, "bottom edge {bottom}, expected {expected}");
        let st = CaptionStyle { anchor: CaptionAnchor::Top, margin: 0.31, size: 78.0, ..Default::default() };
        let o = render_caption(&cap("Hello world"), &st, 1080, 1920).unwrap();
        assert!((o.y as f32 - 0.31 * 1920.0).abs() <= 2.0, "top edge {}, expected {}", o.y, 0.31 * 1920.0);
        // margin 0 and the 0.45 clamp stay inside the frame
        for margin in [0.0, 0.31, 0.45, 9.0] {
            let st = CaptionStyle { anchor: CaptionAnchor::Bottom, margin, size: 78.0, ..Default::default() };
            let o = render_caption(&cap("Hello world"), &st, 1080, 1920).unwrap();
            // the outline-grow padding plus ceil rounding may reach 2 px past the edge; the
            // composite clips it
            assert!(o.y >= 0 && o.y as usize + o.h <= 1922, "margin {margin}: y {} h {}", o.y, o.h);
        }
    }

    #[test]
    fn wraps_and_composites() {
        let long = "word ".repeat(60);
        let o = render_caption(&cap(&long), &CaptionStyle::default(), 640, 360).unwrap();
        assert!(o.w <= 640 && o.h > 0, "{}x{}", o.w, o.h);
        let mut canvas = vec![0.0f32; 640 * 360 * 4];
        o.composite_onto(&mut canvas, 640, 360);
        assert!(canvas.chunks(4).any(|p| p[0] > 0.9));
    }

    #[test]
    fn overlay_is_clipped_to_the_frame_for_an_unbroken_word() {
        let o = render_caption(&cap(&"W".repeat(10_000)), &CaptionStyle::default(), 640, 360).unwrap();
        assert!(o.x >= 0 && o.y >= 0);
        assert!(o.w <= 640 && o.h <= 360, "{}x{}", o.w, o.h);
        assert!(o.w.checked_mul(o.h).unwrap() <= MAX_OVERLAY_PIXELS);
    }

    #[test]
    fn hostile_outline_is_bounded() {
        let st = CaptionStyle { outline: 1_000_000.0, ..Default::default() };
        let o = render_caption(&cap("Safe"), &st, 640, 360).unwrap();
        assert!(o.w <= 640 && o.h <= 360, "{}x{}", o.w, o.h);
    }

    #[test]
    fn outline_draws_around_text() {
        let st = CaptionStyle { background: false, outline: 4.0, outline_color: [255, 0, 0, 255], ..Default::default() };
        let o = render_caption(&cap("O"), &st, 1920, 1080).unwrap();
        assert!(o.px.chunks(4).any(|p| p[0] > 0.9 && p[1] < 0.05 && p[3] > 0.9), "red outline pixels");
    }

    #[test]
    fn track_font_is_used() {
        // the style's font used to be ignored (always Inter)
        let inter = render_caption(&cap("Hello world"), &CaptionStyle::default(), 1920, 1080).unwrap();
        let serif = CaptionStyle { font: "Noto Serif".into(), ..Default::default() };
        let serif = render_caption(&cap("Hello world"), &serif, 1920, 1080).unwrap();
        assert_ne!((inter.w, &inter.px), (serif.w, &serif.px), "Noto Serif renders like Inter");
        let mono = CaptionStyle { font: "jetbrains mono".into(), ..Default::default() };
        let mono = render_caption(&cap("Hello world"), &mono, 1920, 1080).unwrap();
        assert_ne!((inter.w, &inter.px), (mono.w, &mono.px), "JetBrains Mono renders like Inter");
    }

    #[test]
    fn unknown_or_empty_font_falls_back_to_inter() {
        let inter = render_caption(&cap("Hello"), &CaptionStyle::default(), 1280, 720).unwrap();
        for font in ["", "  ", "No Such Font 123"] {
            let st = CaptionStyle { font: font.into(), ..Default::default() };
            assert_eq!(render_caption(&cap("Hello"), &st, 1280, 720), Some(inter.clone()), "font {font:?}");
        }
    }

    #[test]
    fn hidden_track_draws_nothing() {
        let mut t = CaptionTrack::new(TrackId(1), "S".into(), CaptionFormat::Subtitle);
        t.captions.push(cap("x"));
        assert!(track_overlay(&t, Tick(5), 100, 100).is_some());
        t.enabled = false;
        assert!(track_overlay(&t, Tick(5), 100, 100).is_none());
    }

    #[test]
    fn type_on_two_reveals_two_unicode_characters_per_step() {
        let c = cap("A界é🙂BCDEFGHI");
        let style = CaptionStyle { animation: CaptionAnimation::TypeOnTwo, ..Default::default() };
        let first = render_caption_at(&c, &style, 640, 360, Tick::ZERO).expect("first group renders");
        let next = render_caption_at(&c, &style, 640, 360, Tick(TICKS_PER_SECOND * 18 / 100)).expect("second group renders");
        let complete = render_caption_at(&c, &style, 640, 360, Tick(TICKS_PER_SECOND)).expect("full text renders");
        assert_eq!(first, render_caption(&cap("A界"), &CaptionStyle::default(), 640, 360).expect("two characters render"));
        assert_eq!(next, render_caption(&cap("A界é🙂"), &CaptionStyle::default(), 640, 360).expect("four characters render"));
        assert_eq!(complete, render_caption(&c, &CaptionStyle::default(), 640, 360).expect("full caption renders"));
    }

    #[test]
    fn grow_and_shine_templates_change_the_rendered_caption_over_time() {
        let c = cap("Caption");
        let grow = CaptionTemplate::ColorPop.style();
        let small = render_caption_at(&c, &grow, 640, 360, Tick::ZERO).expect("growing caption");
        let full = render_caption_at(&c, &grow, 640, 360, Tick(TICKS_PER_SECOND)).expect("grown caption");
        assert!(full.w > small.w, "grow animation increases the caption size");

        let shine = CaptionTemplate::Shine.style();
        let before = render_caption_at(&c, &shine, 640, 360, Tick::ZERO).expect("caption before shine");
        let during = render_caption_at(&c, &shine, 640, 360, Tick(c.duration.0 / 2)).expect("caption during shine");
        assert_ne!(before.px, during.px, "shine moves across the text over the caption duration");
    }

    #[test]
    fn transcript_word_timings_drive_reveal_and_highlight_groups() {
        let mut c = cap("one two three");
        c.duration = Tick(TICKS_PER_SECOND * 3);
        c.word_timings = vec![
            CaptionWordTiming { start: Tick(0), end: Tick(TICKS_PER_SECOND / 2) },
            CaptionWordTiming { start: Tick(TICKS_PER_SECOND / 2), end: Tick(TICKS_PER_SECOND * 3 / 2) },
            CaptionWordTiming { start: Tick(TICKS_PER_SECOND * 3 / 2), end: Tick(TICKS_PER_SECOND * 3) },
        ];
        let style = CaptionStyle { animation: CaptionAnimation::WordReveal, words_per_group: 2, ..Default::default() };
        assert_eq!(word_animation_state(&c, &style, 0, 3).revealed_words, 2);
        assert_eq!(word_animation_state(&c, &style, TICKS_PER_SECOND * 2, 3).revealed_words, 3);

        let karaoke = CaptionStyle { animation: CaptionAnimation::Karaoke, words_per_group: 2, ..Default::default() };
        assert_eq!(word_animation_state(&c, &karaoke, TICKS_PER_SECOND * 2, 3).active_words, Some(2..3));
    }

    #[test]
    fn untimed_words_are_evenly_distributed_and_speed_adjusts_fallback() {
        let mut c = cap("one two three four");
        c.duration = Tick(TICKS_PER_SECOND * 4);
        let style = CaptionStyle { animation: CaptionAnimation::WordReveal, animation_speed: 2.0, ..Default::default() };
        assert_eq!(word_animation_state(&c, &style, TICKS_PER_SECOND / 2, 4).revealed_words, 2);
        assert_eq!(word_animation_state(&c, &style, TICKS_PER_SECOND * 4, 4).revealed_words, 4);
        let hostile = CaptionStyle { animation: CaptionAnimation::PopWord, animation_speed: f32::NAN, words_per_group: 0, ..Default::default() };
        let state = word_animation_state(&c, &hostile, 0, 4);
        assert_eq!(state.active_words, Some(0..1));
        assert!(state.pop_scale.is_finite());
    }

    #[test]
    fn caption_line_limit_fits_text_without_dropping_words() {
        let text = "that all that sugar would ferment and give the whole colony diabetes and by day 12";
        let (style, lines) = fit_caption_lines(text, "Inter", 54.0, 360.0, 2);
        assert!(lines.len() <= 2, "{lines:?}");
        assert_eq!(lines.join(" "), text);
        assert!(style.size < 54.0, "font shrinks only when needed: {}", style.size);
    }

    #[test]
    fn caption_background_box_alignment_is_independent_of_text_alignment() {
        let text = "a longer caption line";
        let left = CaptionStyle { box_align: CaptionAlign::Left, align: CaptionAlign::Center, ..Default::default() };
        let right = CaptionStyle { box_align: CaptionAlign::Right, align: CaptionAlign::Center, ..Default::default() };
        let left_overlay = render_caption(&cap(text), &left, 640, 360).expect("left-aligned box");
        let right_overlay = render_caption(&cap(text), &right, 640, 360).expect("right-aligned box");
        assert!(left_overlay.x < right_overlay.x);
    }

    #[test]
    fn drop_shadow_adds_a_soft_offset_behind_the_caption_box() {
        let caption = cap("shadow");
        let plain = render_caption(&caption, &CaptionStyle::default(), 640, 360).expect("plain caption");
        let shadow_style = CaptionStyle { shadow: true, shadow_blur: 6.0, shadow_offset_x: 5.0, shadow_offset_y: 5.0, ..Default::default() };
        let shadow = render_caption(&caption, &shadow_style, 640, 360).expect("shadow caption");
        assert_ne!(plain.px, shadow.px);
        assert!(shadow.w > plain.w && shadow.h > plain.h);
    }

    #[test]
    fn word_markers_follow_the_active_word_in_box_and_underline_styles() {
        let mut caption = cap("one two");
        caption.duration = Tick(TICKS_PER_SECOND);
        caption.word_timings = vec![
            filmcraft_project::CaptionWordTiming { start: Tick::ZERO, end: Tick(TICKS_PER_SECOND / 2) },
            filmcraft_project::CaptionWordTiming { start: Tick(TICKS_PER_SECOND / 2), end: Tick(TICKS_PER_SECOND) },
        ];
        let box_style =
            CaptionStyle { animation: CaptionAnimation::WordMarkerBox, background: false, highlight_color: [255, 128, 0, 255], ..Default::default() };
        let underline_style =
            CaptionStyle { animation: CaptionAnimation::WordMarkerUnderline, background: false, highlight_color: [255, 128, 0, 255], ..Default::default() };
        let box_first = render_caption_at(&caption, &box_style, 640, 360, Tick(TICKS_PER_SECOND / 4)).expect("box marker");
        let box_second = render_caption_at(&caption, &box_style, 640, 360, Tick(TICKS_PER_SECOND * 3 / 4)).expect("box marker on second word");
        let underline = render_caption_at(&caption, &underline_style, 640, 360, Tick(TICKS_PER_SECOND / 4)).expect("underline marker");
        assert_ne!(box_first.px, box_second.px, "the box tracks the active timed word");
        assert_ne!(box_first.px, underline.px, "box and underline have distinct shapes");
        assert!(box_first.px.chunks(4).any(|p| p[0] > 0.5 && p[1] > 0.1 && p[1] < 0.4), "box marker uses the caption highlight colour");
        assert!(underline.px.chunks(4).any(|p| p[0] > 0.5 && p[1] > 0.1 && p[1] < 0.4), "underline uses the caption highlight colour");
    }

    #[test]
    fn word_marker_box_opacity_border_and_radius_are_independently_configurable() {
        let mut caption = cap("one two");
        caption.duration = Tick(TICKS_PER_SECOND);
        caption.word_timings = vec![
            CaptionWordTiming { start: Tick::ZERO, end: Tick(TICKS_PER_SECOND / 2) },
            CaptionWordTiming { start: Tick(TICKS_PER_SECOND / 2), end: Tick(TICKS_PER_SECOND) },
        ];
        let base = CaptionStyle {
            animation: CaptionAnimation::WordMarkerBox,
            background: false,
            word_marker_opacity: 0,
            word_marker_border_color: [0, 255, 0, 255],
            word_marker_border_width: 0.0,
            word_marker_radius: 0.0,
            ..Default::default()
        };
        let transparent = render_caption_at(&caption, &base, 640, 360, Tick(TICKS_PER_SECOND / 4)).expect("transparent marker");
        let opaque = render_caption_at(
            &caption,
            &CaptionStyle { word_marker_opacity: 100, ..base.clone() },
            640,
            360,
            Tick(TICKS_PER_SECOND / 4),
        )
        .expect("opaque marker");
        let rounded = render_caption_at(
            &caption,
            &CaptionStyle { word_marker_radius: 16.0, ..base.clone() },
            640,
            360,
            Tick(TICKS_PER_SECOND / 4),
        )
        .expect("rounded marker");
        let bordered = render_caption_at(
            &caption,
            &CaptionStyle { word_marker_border_width: 4.0, ..base.clone() },
            640,
            360,
            Tick(TICKS_PER_SECOND / 4),
        )
        .expect("bordered marker");
        assert_ne!(transparent.px, opaque.px, "fill opacity changes the marker");
        assert_ne!(transparent.px, rounded.px, "corner radius changes the marker");
        assert!(
            bordered.px.chunks(4).any(|p| p[1] > 0.9 && p[0] < 0.1 && p[2] < 0.1),
            "separate border colour is rendered"
        );
    }

    #[test]
    fn karaoke_uses_the_configured_highlight_colour() {
        let mut c = cap("hello world");
        c.duration = Tick(TICKS_PER_SECOND);
        let style = CaptionStyle { animation: CaptionAnimation::Karaoke, highlight_color: [0, 255, 0, 255], ..Default::default() };
        let rendered = render_caption_at(&c, &style, 640, 360, Tick::ZERO).expect("karaoke caption");
        assert!(rendered.px.chunks(4).any(|p| p[1] > 0.9 && p[0] < 0.1), "active word uses the configured green");
    }

    #[test]
    fn word_reveal_shows_only_the_currently_revealed_words() {
        let mut c = cap("one two");
        c.duration = Tick(TICKS_PER_SECOND);
        c.word_timings = vec![
            CaptionWordTiming { start: Tick(0), end: Tick(TICKS_PER_SECOND / 2) },
            CaptionWordTiming { start: Tick(TICKS_PER_SECOND / 2), end: Tick(TICKS_PER_SECOND) },
        ];
        let style = CaptionStyle { animation: CaptionAnimation::WordReveal, ..Default::default() };
        let first = render_caption_at(&c, &style, 640, 360, Tick::ZERO).expect("first word");
        let expected = render_caption(&cap("one"), &CaptionStyle::default(), 640, 360).expect("static first word");
        assert_eq!(first, expected);
    }
}
