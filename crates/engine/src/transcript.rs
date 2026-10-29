//! Text-based editing commands (`transcript.*`): the Text panel ▸ Transcript tab.
//!
//! Transcripts belong to media items (`Project::transcripts`, media time); the sequence transcript
//! is derived from them ([`filmcraft_edit::transcript::sequence_words`]). Words of the sequence
//! transcript are addressed by index (`from`, `to`, inclusive), as `transcript.inspect` lists them.
//!
//! Speech recognition goes through a [`Transcriber`]: [`Session::transcriber`] when a host or a
//! test installed one, else the Whisper model named by `model` from `<data dir>/models` (needs the
//! engine feature `whisper`; without it `transcript.generate` fails with a clear error, and agents
//! can still bring their own transcript with `transcript.set`).

use std::sync::Arc;

use serde_json::{Value, json};

use filmcraft_edit as edit;
use filmcraft_edit::transcript::{self as tx, CaptionRules, SeqWord};
use filmcraft_project::{CaptionFormat, CaptionTrack, ItemId, ItemKind, TrackId, Transcript};
use filmcraft_speech::{Options, SpeechError, Transcriber};
use filmcraft_time::{TICKS_PER_SECOND, Tick, TimeRange};

use crate::commands::{CommandSpec, always, bad, bool_p, f64_p, has_seq, str_p, u64_p};
use crate::{EngineError, Result, Session};

type Run = fn(&mut Session, &Value) -> Result<Value>;
type Enabled = fn(&Session) -> std::result::Result<(), String>;

fn spec(id: &'static str, label: &'static str, menu: &'static [&'static str], params: &'static str, enabled: Enabled, run: Run, journal: bool) -> CommandSpec {
    CommandSpec { id, label, menu, shortcut: None, params, enabled, run, journal }
}

/// Where downloaded speech models live (`<data dir>/models`).
pub fn models_dir() -> Option<std::path::PathBuf> {
    crate::autosave::default_data_dir().map(|d| d.join("models"))
}

/// Whether this build can transcribe with Whisper (feature `whisper`).
pub fn speech_available() -> bool {
    filmcraft_speech::available()
}

/// The words of the active sequence's transcript.
pub fn sequence_words(s: &Session) -> Vec<SeqWord> {
    match s.active_sequence() {
        Some(q) => tx::sequence_words(q, &s.project.transcripts),
        None => Vec::new(),
    }
}

fn has_transcript(s: &Session) -> std::result::Result<(), String> {
    has_seq(s)?;
    if sequence_words(s).is_empty() { Err("the sequence has no transcript (Transcribe first)".into()) } else { Ok(()) }
}

fn has_transcripts(s: &Session) -> std::result::Result<(), String> {
    if s.project.transcripts.is_empty() { Err("there are no transcripts".into()) } else { Ok(()) }
}

/// Why speech-to-text can't run in this build (no installed transcriber, built without `whisper`).
pub(crate) const NO_SPEECH: &str =
    "speech-to-text is not available in this build (built without the `whisper` feature); import a transcript with transcript.set instead";

/// `transcript.generate` can run: a host installed a transcriber or the build has speech-to-text
/// (#97: it reported enabled and then always failed).
pub(crate) fn can_transcribe(s: &Session) -> std::result::Result<(), String> {
    if s.transcriber.is_some() || speech_available() { Ok(()) } else { Err(NO_SPEECH.into()) }
}

/// `transcript.downloadModel` can run: built with `speech-download` (#98).
fn can_download(_: &Session) -> std::result::Result<(), String> {
    if cfg!(feature = "speech-download") {
        Ok(())
    } else {
        Err("model downloads are not available in this build (built without the `speech-download` feature)".into())
    }
}

/// The media item behind a project item (subclips resolve to their parent).
fn media_item(s: &Session, item: ItemId) -> Option<ItemId> {
    s.project.resolve_media(item).map(|(root, _, _)| root)
}

fn ids_p(p: &Value, k: &str) -> Option<Vec<ItemId>> {
    p.get(k).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_u64).map(ItemId).collect())
}

/// Items to transcribe: `items` / `item`, else the Project panel selection, else the media of the
/// active sequence's enabled audio clips. Subclips resolve to their media; duplicates are removed.
fn targets(s: &Session, p: &Value) -> Vec<ItemId> {
    let mut raw = ids_p(p, "items").or_else(|| u64_p(p, "item").map(|i| vec![ItemId(i)])).unwrap_or_default();
    if raw.is_empty() {
        raw = s.state.project_selection.clone();
    }
    if raw.is_empty()
        && let Some(q) = s.active_sequence()
    {
        raw = q.audio_tracks.iter().flat_map(|t| t.items.iter()).filter(|it| it.enabled).map(|it| it.item).collect();
    }
    let mut out = Vec::new();
    for i in raw {
        if let Some(m) = media_item(s, i)
            && !out.contains(&m)
        {
            out.push(m);
        }
    }
    out
}

/// Mono 16 kHz audio of a media item (None: no audio).
fn item_audio(s: &Session, item: ItemId) -> Option<Vec<f32>> {
    let dur = match &s.project.item(item)?.kind {
        ItemKind::Media(m) => m.duration(),
        _ => return None,
    };
    let src = s.source(item)?;
    if !src.info().has_audio() {
        return None;
    }
    let sr = filmcraft_speech::SAMPLE_RATE;
    let len = dur.to_units_floor(sr as i64).max(0) as usize;
    let buf = src.audio(0, len, sr).ok()?;
    Some(filmcraft_speech::downmix(&buf.channels))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TimelineAudioTarget {
    item: ItemId,
    source_item: ItemId,
    source_start: Tick,
    source_end: Tick,
    clip_start: Tick,
    stream: usize,
}

const MAX_TIMELINE_TRANSCRIPT_SECONDS: i64 = 30 * 60;

fn timeline_targets(s: &Session, p: &Value) -> Result<Vec<TimelineAudioTarget>> {
    let selected_track = match p.get("track") {
        None => None,
        Some(Value::String(name)) if name.eq_ignore_ascii_case("mix") => None,
        Some(_) => crate::commands::track_p(s, p, "track", "transcript.generate")?,
    };
    let q = s.active_sequence().ok_or(EngineError::NoSequence)?;
    let mut targets = Vec::new();
    for clip in q
        .audio_tracks
        .iter()
        .filter(|audio_track| audio_track.enabled && selected_track.is_none_or(|selected| selected == audio_track.id))
        .flat_map(|audio_track| &audio_track.items)
        .filter(|clip| clip.enabled)
    {
        if clip.reverse || clip.frame_hold.is_some() || !clip.speed.is_finite() || clip.speed <= 0.0 || clip.duration.0 <= 0 {
            continue;
        }
        let consumed = clip.duration.0 as f64 * clip.speed;
        if !consumed.is_finite() || consumed <= 0.0 || consumed > i64::MAX as f64 {
            return Err(EngineError::Other(format!("audio clip {} has an invalid source duration", clip.id.0)));
        }
        let requested_end = clip
            .source_in
            .0
            .checked_add(consumed.round() as i64)
            .map(Tick)
            .ok_or_else(|| EngineError::Other(format!("audio clip {} source range overflows", clip.id.0)))?;
        let max_duration = MAX_TIMELINE_TRANSCRIPT_SECONDS * TICKS_PER_SECOND;
        if clip.source_in.0 < 0 || requested_end <= clip.source_in || requested_end.0.saturating_sub(clip.source_in.0) > max_duration {
            return Err(EngineError::Other(format!("audio clip {} source range is invalid or longer than 30 minutes", clip.id.0)));
        }
        let Some((source_item, media, subclip_range)) = s.project.resolve_media(clip.item) else {
            return Err(EngineError::Other(format!("audio clip {} does not refer to media that can be transcribed", clip.id.0)));
        };
        let offset = subclip_range.map_or(Tick::ZERO, |range| range.start);
        let available = subclip_range.map_or(media.duration(), |range| range.duration);
        if clip.source_in >= available {
            continue;
        }
        let source_end = Tick(requested_end.0.min(available.0));
        let source_start =
            offset.0.checked_add(clip.source_in.0).map(Tick).ok_or_else(|| EngineError::Other(format!("audio clip {} source start overflows", clip.id.0)))?;
        let source_end_absolute =
            offset.0.checked_add(source_end.0).map(Tick).ok_or_else(|| EngineError::Other(format!("audio clip {} source end overflows", clip.id.0)))?;
        let target = TimelineAudioTarget {
            item: clip.item,
            source_item,
            source_start,
            source_end: source_end_absolute,
            clip_start: clip.source_in,
            stream: clip.audio_stream,
        };
        if !targets.contains(&target) {
            targets.push(target);
        }
    }
    Ok(targets)
}

fn timeline_audio(s: &Session, target: TimelineAudioTarget) -> Result<Vec<f32>> {
    let src = s.source(target.source_item).ok_or_else(|| EngineError::Other(format!("media source for item {} is unavailable", target.item.0)))?;
    if src.info().audio_streams.get(target.stream).is_none() {
        return Err(EngineError::Other(format!("audio stream {} for item {} is unavailable", target.stream, target.item.0)));
    }
    let sr = filmcraft_speech::SAMPLE_RATE;
    let start = target.source_start.to_units_floor(sr as i64);
    let end = target.source_end.to_units_floor(sr as i64);
    let frames = end.checked_sub(start).ok_or_else(|| EngineError::Other(format!("audio range for item {} overflows", target.item.0)))?;
    let max_frames = MAX_TIMELINE_TRANSCRIPT_SECONDS.checked_mul(sr as i64).ok_or_else(|| EngineError::Other("transcript audio limit overflows".into()))?;
    if start < 0 || frames <= 0 || frames > max_frames {
        return Err(EngineError::Other(format!("audio range for item {} is invalid or longer than 30 minutes", target.item.0)));
    }
    let frames = usize::try_from(frames).map_err(|_| EngineError::Other(format!("audio range for item {} is too large", target.item.0)))?;
    let buf = src.audio_stream(target.stream, start, frames, sr).map_err(|e| EngineError::Other(format!("audio for item {}: {e}", target.item.0)))?;
    Ok(filmcraft_speech::downmix(&buf.channels))
}

fn timeline_transcript(
    s: &Session,
    targets: &[TimelineAudioTarget],
    t: &Arc<dyn Transcriber>,
    opts: &Options,
) -> Result<(Vec<(ItemId, Transcript)>, Vec<u64>)> {
    let mut transcripts: Vec<(ItemId, Transcript, Vec<TimeRange>)> = Vec::new();
    let mut skipped = Vec::new();
    for target in targets {
        let audio = timeline_audio(s, *target)?;
        let mut generated = t.transcribe(&audio, opts, &mut |_, _| true).map_err(speech_err)?;
        if generated.source.is_empty() {
            generated.source = t.id();
        }
        let range = TimeRange::from_bounds(target.clip_start, target.source_end - (target.source_start - target.clip_start));
        let shift = target.clip_start;
        let mut words = Vec::with_capacity(generated.words.len());
        for mut word in generated.words.drain(..) {
            word.start = word
                .start
                .0
                .checked_add(shift.0)
                .map(Tick)
                .ok_or_else(|| EngineError::Other(format!("transcript word time overflows for item {}", target.item.0)))?;
            word.end = word
                .end
                .0
                .checked_add(shift.0)
                .map(Tick)
                .ok_or_else(|| EngineError::Other(format!("transcript word time overflows for item {}", target.item.0)))?;
            let midpoint = Tick(word.start.0.saturating_add(word.end.0.saturating_sub(word.start.0) / 2));
            if range.contains(midpoint) {
                words.push(word);
            }
        }
        generated.words = words;
        generated.normalize();
        if generated.words.is_empty() {
            skipped.push(target.item.0);
        }
        if let Some((_, transcript, ranges)) = transcripts.iter_mut().find(|(item, _, _)| *item == target.item) {
            ranges.push(range);
            transcript.words.extend(generated.words);
            for speaker in generated.speakers {
                if !transcript.speakers.iter().any(|existing| existing.name == speaker.name) {
                    transcript.speakers.push(speaker);
                }
            }
            transcript.source = generated.source;
            if !generated.language.is_empty() {
                transcript.language = generated.language;
            }
        } else {
            transcripts.push((target.item, generated, vec![range]));
        }
    }
    let mut done = Vec::with_capacity(transcripts.len());
    for (item, mut generated, ranges) in transcripts {
        let mut transcript = s.project.transcripts.get(&item).map(|existing| existing.as_ref().clone()).unwrap_or_default();
        transcript.words.retain(|word| {
            let midpoint = Tick(word.start.0.saturating_add(word.end.0.saturating_sub(word.start.0) / 2));
            !ranges.iter().any(|range| range.contains(midpoint))
        });
        if !generated.language.is_empty() {
            transcript.language = generated.language;
        }
        if !generated.source.is_empty() {
            transcript.source = generated.source;
        }
        let mut speaker_map = Vec::with_capacity(generated.speakers.len());
        for speaker in generated.speakers {
            let index = match transcript.speakers.iter().position(|existing| existing.name == speaker.name) {
                Some(index) => index,
                None => {
                    transcript.speakers.push(speaker);
                    transcript.speakers.len() - 1
                }
            };
            speaker_map.push(u32::try_from(index).map_err(|_| EngineError::Other(format!("too many speakers in transcript for item {}", item.0)))?);
        }
        for mut word in generated.words.drain(..) {
            if let Some(speaker) = word.speaker {
                word.speaker = speaker_map.get(speaker as usize).copied();
            }
            transcript.words.push(word);
        }
        transcript.normalize();
        done.push((item, transcript));
    }
    Ok((done, skipped))
}

fn speech_err(e: SpeechError) -> EngineError {
    EngineError::Other(e.to_string())
}

/// The transcriber to use: the installed one, else the named catalogue model.
fn transcriber(s: &Session, p: &Value) -> Result<Arc<dyn Transcriber>> {
    if let Some(t) = &s.transcriber {
        return Ok(t.clone());
    }
    // Settings ▸ Media Analysis & Transcription ▸ Speech model
    let model = str_p(p, "model").unwrap_or(&s.prefs.media_analysis.whisper_model);
    if filmcraft_speech::models::find(model).is_none() {
        return Err(speech_err(SpeechError::UnknownModel(model.into())));
    }
    if !filmcraft_speech::available() {
        return Err(EngineError::Other(NO_SPEECH.into()));
    }
    let dir = models_dir().ok_or_else(|| EngineError::Other("no data directory for speech models".into()))?;
    filmcraft_speech::load(&dir, model).map_err(speech_err)
}

fn generate(s: &mut Session, p: &Value) -> Result<Value> {
    if bool_p(p, "timeline").unwrap_or(false) {
        return generate_timeline(s, p);
    }
    let items = targets(s, p);
    if items.is_empty() {
        return Err(bad("transcript.generate", "nothing to transcribe (pass `items`, select clips, or open a sequence with audio)"));
    }
    let t = transcriber(s, p)?;
    // Settings ▸ Media Analysis & Transcription: language (or auto-detect) and speaker labelling
    let ma = &s.prefs.media_analysis;
    let default_language = if ma.language_auto_detect { None } else { Some(ma.default_language.clone()) };
    let opts = Options {
        language: match str_p(p, "language") {
            Some(l) => Some(l).filter(|l| !l.is_empty() && *l != "auto").map(str::to_string),
            None => default_language,
        },
        diarize: bool_p(p, "diarize").unwrap_or(ma.speaker_labeling != "off"),
        max_speakers: u64_p(p, "maxSpeakers").map(|n| n.clamp(1, 32) as usize).unwrap_or(Options::default().max_speakers),
    };
    let mut done: Vec<(ItemId, Transcript)> = Vec::new();
    let mut skipped = Vec::new();
    for item in items {
        let Some(audio) = item_audio(s, item) else {
            skipped.push(item.0);
            continue;
        };
        let mut tr = t.transcribe(&audio, &opts, &mut |_, _| true).map_err(speech_err)?;
        tr.normalize();
        done.push((item, tr));
    }
    if done.is_empty() {
        return Err(EngineError::Other("none of the clips has audio to transcribe".into()));
    }
    let report: Vec<Value> = done
        .iter()
        .map(|(i, t)| json!({"item": i.0, "words": t.words.len(), "speakers": t.speakers.len(), "language": t.language, "source": t.source}))
        .collect();
    s.edit("Transcribe", move |pr, _| {
        for (i, t) in done {
            pr.transcripts.insert(i, Arc::new(t));
        }
        Ok(())
    })?;
    Ok(json!({"items": report, "skipped": skipped}))
}

fn generate_timeline(s: &mut Session, p: &Value) -> Result<Value> {
    let targets = timeline_targets(s, p)?;
    if targets.is_empty() {
        return Err(bad("transcript.generate", "the active sequence has no enabled audio clips to transcribe"));
    }
    let t = transcriber(s, p)?;
    let ma = &s.prefs.media_analysis;
    let default_language = if ma.language_auto_detect { None } else { Some(ma.default_language.clone()) };
    let opts = Options {
        language: match str_p(p, "language") {
            Some(l) => Some(l).filter(|l| !l.is_empty() && *l != "auto").map(str::to_string),
            None => default_language,
        },
        diarize: bool_p(p, "diarize").unwrap_or(ma.speaker_labeling != "off"),
        max_speakers: u64_p(p, "maxSpeakers").map(|n| n.clamp(1, 32) as usize).unwrap_or(Options::default().max_speakers),
    };
    let (done, skipped) = timeline_transcript(s, &targets, &t, &opts)?;
    let report: Vec<Value> = done
        .iter()
        .map(|(item, transcript)| {
            let ranges: Vec<Value> = targets
                .iter()
                .filter(|target| target.item == *item)
                .map(|target| json!({"start": target.clip_start.0, "end": target.source_end.0 - (target.source_start.0 - target.clip_start.0)}))
                .collect();
            json!({"item": item.0, "words": transcript.words.len(), "speakers": transcript.speakers.len(), "language": transcript.language, "source": transcript.source, "ranges": ranges})
        })
        .collect();
    s.edit("Transcribe Timeline", move |pr, _| {
        for (item, transcript) in done {
            pr.transcripts.insert(item, Arc::new(transcript));
        }
        Ok(())
    })?;
    Ok(json!({"items": report, "skipped": skipped}))
}

fn set(s: &mut Session, p: &Value) -> Result<Value> {
    let item = u64_p(p, "item").map(ItemId).ok_or_else(|| bad("transcript.set", "`item` is required"))?;
    let item = media_item(s, item).ok_or_else(|| bad("transcript.set", "no such media item"))?;
    let v = p.get("transcript").cloned().ok_or_else(|| bad("transcript.set", "`transcript` is required"))?;
    let mut t: Transcript = serde_json::from_value(v).map_err(|e| bad("transcript.set", e.to_string()))?;
    if t.source.is_empty() {
        t.source = "imported".into();
    }
    t.normalize();
    t.check().map_err(|e| bad("transcript.set", e))?;
    let n = t.words.len();
    s.edit("Set Transcript", move |pr, _| {
        pr.transcripts.insert(item, Arc::new(t));
        Ok(())
    })?;
    Ok(json!({"item": item.0, "words": n}))
}

fn delete(s: &mut Session, p: &Value) -> Result<Value> {
    let items: Vec<ItemId> = match ids_p(p, "items").or_else(|| u64_p(p, "item").map(|i| vec![ItemId(i)])) {
        Some(v) => v.into_iter().filter_map(|i| media_item(s, i)).collect(),
        None => s.project.transcripts.keys().copied().collect(),
    };
    let n = items.iter().filter(|i| s.project.transcripts.contains_key(i)).count();
    if n == 0 {
        return Err(EngineError::Other("no transcript to delete".into()));
    }
    s.edit("Delete Transcript", move |pr, _| {
        for i in items {
            pr.transcripts.remove(&i);
        }
        Ok(())
    })?;
    Ok(json!({"deleted": n}))
}

fn word_json(i: usize, w: &SeqWord) -> Value {
    json!({"i": i, "text": w.text, "start": w.start.0, "end": w.end.0, "speaker": w.speaker, "clip": w.clip.0, "item": w.item.0, "confidence": w.confidence})
}

fn inspect(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let gap = Tick::from_seconds_f64(f64_p(p, "paragraphGapSeconds").unwrap_or(1.5));
    let paras: Vec<Value> = tx::paragraphs(&words, gap)
        .into_iter()
        .map(|r| {
            let text = words[r.clone()].iter().map(|w| w.text.as_str()).collect::<Vec<_>>().join(" ");
            json!({"from": r.start, "to": r.end - 1, "speaker": words[r.start].speaker, "start": words[r.start].start.0, "end": words[r.end - 1].end.0, "text": text})
        })
        .collect();
    let speakers: Vec<String> = {
        let mut v: Vec<String> = Vec::new();
        for w in &words {
            if let Some(n) = &w.speaker
                && !v.contains(n)
            {
                v.push(n.clone());
            }
        }
        v
    };
    let current = tx::word_at(&words, s.playhead());
    Ok(json!({
        "words": words.iter().enumerate().map(|(i, w)| word_json(i, w)).collect::<Vec<_>>(),
        "paragraphs": paras,
        "speakers": speakers,
        "current": current,
        "items": s.project.transcripts.iter().map(|(i, t)| json!({"item": i.0, "words": t.words.len(), "language": t.language, "source": t.source, "speakers": t.speakers.iter().map(|k| &k.name).collect::<Vec<_>>()})).collect::<Vec<_>>(),
    }))
}

fn search(s: &mut Session, p: &Value) -> Result<Value> {
    let q = str_p(p, "query").ok_or_else(|| bad("transcript.search", "`query` is required"))?;
    let words = sequence_words(s);
    let hits: Vec<Value> = tx::search(&words, q)
        .into_iter()
        .map(|r| json!({"from": r.start, "to": r.end - 1, "start": words[r.start].start.0, "end": words[r.end - 1].end.0}))
        .collect();
    Ok(json!({"matches": hits}))
}

/// Timeline range of the words `from..=to` (frame-snapped outward).
fn range_p(s: &Session, p: &Value, cmd: &str) -> Result<TimeRange> {
    let words = sequence_words(s);
    let from = u64_p(p, "from").ok_or_else(|| bad(cmd, "`from` (word index) is required"))? as usize;
    let to = u64_p(p, "to").map(|n| n as usize).unwrap_or(from);
    tx::word_range(&words, from, to, s.sequence_rate()).ok_or_else(|| bad(cmd, format!("word index out of range (the transcript has {} words)", words.len())))
}

fn range_json(r: TimeRange) -> Value {
    json!({"start": r.start.0, "end": r.end().0})
}

fn select(s: &mut Session, p: &Value) -> Result<Value> {
    let r = range_p(s, p, "transcript.select")?;
    let fd = s.sequence_rate().frame_duration();
    s.edit_sequence("Mark Transcript Selection", |q, _, _| {
        q.mark_in = Some(r.start);
        q.mark_out = Some(r.end() - fd);
        Ok(())
    })?;
    s.set_playhead(r.start);
    Ok(range_json(r))
}

fn extract_or_lift(s: &mut Session, p: &Value, extract: bool) -> Result<Value> {
    let cmd = if extract { "transcript.extract" } else { "transcript.lift" };
    let r = range_p(s, p, cmd)?;
    let tg = s.targeting().targeted;
    s.edit_sequence(if extract { "Extract Text" } else { "Lift Text" }, |q, ctx, _| {
        if extract {
            edit::extract(q, &tg, r, ctx);
        } else {
            edit::lift(q, &tg, r, ctx);
        }
        q.mark_in = None;
        q.mark_out = None;
        Ok(())
    })?;
    s.set_playhead(r.start);
    Ok(range_json(r))
}

fn rename_speaker(s: &mut Session, p: &Value) -> Result<Value> {
    let name = str_p(p, "name").map(str::trim).filter(|n| !n.is_empty()).ok_or_else(|| bad("transcript.renameSpeaker", "`name` is required"))?.to_string();
    let item = u64_p(p, "item").map(ItemId).and_then(|i| media_item(s, i));
    // `speaker`: the current name (every transcript), or an index (needs `item`)
    let (old_name, index) = match p.get("speaker") {
        Some(Value::String(n)) => (Some(n.clone()), None),
        Some(v) if v.is_u64() => (None, v.as_u64().map(|n| n as usize)),
        _ => return Err(bad("transcript.renameSpeaker", "`speaker` (name, or index with `item`) is required")),
    };
    if index.is_some() && item.is_none() {
        return Err(bad("transcript.renameSpeaker", "a speaker index needs `item`"));
    }
    let mut n = 0;
    let mut next = s.project.transcripts.clone();
    for (i, t) in next.iter_mut() {
        if item.is_some_and(|x| x != *i) {
            continue;
        }
        let tt = Arc::make_mut(t);
        for (k, sp) in tt.speakers.iter_mut().enumerate() {
            if old_name.as_ref().is_some_and(|o| *o == sp.name) || index == Some(k) {
                sp.name = name.clone();
                n += 1;
            }
        }
    }
    if n == 0 {
        return Err(EngineError::Other("no such speaker".into()));
    }
    s.edit("Rename Speaker", move |pr, _| {
        pr.transcripts = next;
        Ok(())
    })?;
    Ok(json!({"renamed": n}))
}

fn remove_ranges(s: &mut Session, label: &str, ranges: Vec<TimeRange>) -> Result<Value> {
    let n = ranges.len();
    if n == 0 {
        return Ok(json!({"removed": 0, "ticks": 0}));
    }
    let total = s.edit_sequence(label, |q, ctx, _| Ok(tx::ripple_delete_ranges(q, ranges, ctx)))?;
    Ok(json!({"removed": n, "ticks": total.0, "seconds": total.0 as f64 / TICKS_PER_SECOND as f64}))
}

fn remove_pauses(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let min = Tick::from_seconds_f64(f64_p(p, "minSeconds").unwrap_or(1.0));
    let keep = Tick::from_seconds_f64(f64_p(p, "keepSeconds").unwrap_or(0.15));
    let ranges = tx::find_pauses(&words, min, keep, s.sequence_rate());
    remove_ranges(s, "Remove Pauses", ranges)
}

fn remove_fillers(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let fillers: Vec<String> = match p.get("fillers").and_then(Value::as_array) {
        Some(a) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        None => tx::DEFAULT_FILLERS.iter().map(|f| f.to_string()).collect(),
    };
    let hits = tx::find_fillers(&words, &fillers);
    let ranges = tx::filler_ranges(&words, &hits, s.sequence_rate());
    remove_ranges(s, "Remove Filler Words", ranges)
}

fn create_captions(s: &mut Session, p: &Value) -> Result<Value> {
    let words = sequence_words(s);
    let d = CaptionRules::default();
    let rules = CaptionRules {
        max_chars: u64_p(p, "maxChars").map(|n| n as usize).unwrap_or(d.max_chars),
        lines: u64_p(p, "lines").map(|n| n as usize).unwrap_or(d.lines),
        min_duration: f64_p(p, "minSeconds").map(Tick::from_seconds_f64).unwrap_or(d.min_duration),
        max_duration: f64_p(p, "maxSeconds").map(Tick::from_seconds_f64).unwrap_or(d.max_duration),
        gap_frames: p.get("gapFrames").and_then(Value::as_i64).unwrap_or(d.gap_frames),
        break_pause: d.break_pause,
    };
    let blocks = tx::caption_blocks(&words, &rules, s.sequence_rate());
    let format = str_p(p, "format").and_then(CaptionFormat::from_name).unwrap_or_default();
    let name = str_p(p, "name").unwrap_or("Transcript").to_string();
    let n = blocks.len();
    let tid = s.edit_sequence("Create Captions", |q, ctx, st| {
        let tid = TrackId(ctx.alloc());
        let mut t = CaptionTrack::new(tid, name, format);
        t.captions = tx::blocks_to_captions(&blocks, ctx);
        q.caption_tracks.insert(0, t);
        st.caption_selection.clear();
        Ok(tid)
    })?;
    Ok(json!({"track": tid.0, "captions": n}))
}

fn models(_: &mut Session, _: &Value) -> Result<Value> {
    let dir = models_dir();
    Ok(json!({
        "available": filmcraft_speech::available(),
        "default": filmcraft_speech::models::DEFAULT_MODEL,
        "dir": dir.as_ref().map(|d| d.to_string_lossy().to_string()),
        "models": filmcraft_speech::models::catalogue().iter().map(|m| json!({
            "id": m.id, "name": m.name, "multilingual": m.multilingual, "description": m.description,
            "license": m.license, "source": m.source, "size": m.size(),
            "installed": dir.as_ref().is_some_and(|d| filmcraft_speech::models::installed(d, m)),
        })).collect::<Vec<_>>(),
    }))
}

/// Download a catalogue model into `<data dir>/models` (feature `speech-download`). Hosts show
/// the size, source and licence (`transcript.models`) and ask before running this.
fn download_model(_: &mut Session, p: &Value) -> Result<Value> {
    let id = str_p(p, "model").unwrap_or(filmcraft_speech::models::DEFAULT_MODEL);
    let m = filmcraft_speech::models::find(id).ok_or_else(|| speech_err(SpeechError::UnknownModel(id.into())))?;
    let dir = models_dir().ok_or_else(|| EngineError::Other("no data directory for speech models".into()))?;
    #[cfg(feature = "speech-download")]
    {
        filmcraft_speech::models::download(&dir, m, &mut |_, _, _| true).map_err(speech_err)?;
        Ok(json!({"model": m.id, "dir": filmcraft_speech::models::model_dir(&dir, m).to_string_lossy()}))
    }
    #[cfg(not(feature = "speech-download"))]
    {
        let _ = (m, dir);
        Err(EngineError::Other("model downloads are not available in this build (built without the `speech-download` feature)".into()))
    }
}

pub fn commands() -> Vec<CommandSpec> {
    vec![
        spec(
            "transcript.generate",
            "Transcribe…",
            &["Sequence", "Transcript"],
            r#"{"items":[id]?,"timeline":bool?,"track":"mix"|id?,"model":"whisper-base"?,"language":"en|auto"?,"diarize":bool?,"maxSpeakers":n?}"#,
            can_transcribe,
            generate,
            true,
        ),
        spec(
            "transcript.set",
            "Import Transcript",
            &[],
            r#"{"item":id,"transcript":{"language":str,"speakers":[{"name":str}],"words":[{"text":str,"start":tick,"end":tick,"speaker":n?}]}}"#,
            always,
            set,
            true,
        ),
        spec("transcript.delete", "Delete Transcript", &["Sequence", "Transcript"], r#"{"items":[id]?}"#, has_transcripts, delete, true),
        spec("transcript.inspect", "Inspect Transcript", &[], r#"{"paragraphGapSeconds":f?}"#, always, inspect, false),
        spec("transcript.search", "Search Transcript", &[], r#"{"query":str}"#, always, search, false),
        spec("transcript.models", "List Speech Models", &[], "{}", always, models, false),
        spec("transcript.downloadModel", "Download Speech Model", &[], r#"{"model":"whisper-base"?}"#, can_download, download_model, true),
        spec("transcript.select", "Mark Selected Text", &[], r#"{"from":word,"to":word?}"#, has_transcript, select, true),
        spec("transcript.extract", "Extract Selected Text", &[], r#"{"from":word,"to":word?}"#, has_transcript, |s, p| extract_or_lift(s, p, true), true),
        spec("transcript.lift", "Lift Selected Text", &[], r#"{"from":word,"to":word?}"#, has_transcript, |s, p| extract_or_lift(s, p, false), true),
        spec(
            "transcript.renameSpeaker",
            "Rename Speaker…",
            &[],
            r#"{"speaker":"Speaker 1"|index,"name":str,"item":id?}"#,
            has_transcripts,
            rename_speaker,
            true,
        ),
        spec(
            "transcript.removePauses",
            "Remove Pauses",
            &["Sequence", "Transcript"],
            r#"{"minSeconds":f?,"keepSeconds":f?}"#,
            has_transcript,
            remove_pauses,
            true,
        ),
        spec("transcript.removeFillers", "Remove Filler Words", &["Sequence", "Transcript"], r#"{"fillers":[str]?}"#, has_transcript, remove_fillers, true),
        spec(
            "transcript.createCaptions",
            "Create Captions from Transcript…",
            &["Sequence", "Transcript"],
            r#"{"maxChars":n?,"lines":1|2?,"minSeconds":f?,"maxSeconds":f?,"gapFrames":n?,"format":str?,"name":str?}"#,
            has_transcript,
            create_captions,
            true,
        ),
    ]
}
