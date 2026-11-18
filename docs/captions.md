# Captions

FilmCraft keeps captions on **caption tracks** in the sequence, shown in their own area above the
video tracks (as Premiere does). Caption tracks are saved in the `.fcproj` (project format
version 2; version 1 projects open with no caption tracks).

## Model

- **Caption track** (`CaptionTrack`, `crates/project/src/caption.rs`): name, format (`Subtitle`,
  `CEA-608`, `CEA-708`, `Teletext`), language, output eye (shown in the Program monitor and
  burned in on export), lock, sync lock, and a **track style**:
  font size (pixels at 1080 lines, scaled to the frame), text colour, background box and its colour,
  outline width and colour, alignment (left/centre/right), position (top/middle/bottom) and
  margin (a fraction of the frame height, default 0.08; `captions.setStyle` clamps it to 0–0.45),
  independent background-box alignment, line spacing, a 1–5 rendered-line limit (text shrinks to
  preserve every word), optional drop shadow (colour, blur, and horizontal/vertical offsets), a
  reusable built-in template, and its burn-in animation, animation speed, words per group (1–5),
  and active-word highlight colour.
- **Caption**: in/out in exact `Tick`s, text (lines separated by `\n`; `<i>`, `<b>`, `<u>` kept
  as written), optional speaker, optional transcript word timings relative to the cue start, and the
  WebVTT cue id and cue settings, kept for round trips. Captions on a track never overlap.

## Files

| Format | Import | Export | Notes |
|---|---|---|---|
| SubRip `.srt` | ✓ | ✓ | Forgiving reader (BOM, UTF-16, Windows-1252, CRLF, missing indexes, `,`/`.` ms, missing hours). |
| WebVTT `.vtt` | ✓ | ✓ | Keeps cue ids, cue settings, `<v Speaker>`, STYLE/REGION/NOTE blocks. `line:N%` and `align:` are honoured when drawing. |
| Scenarist SCC `.scc` | ✓ | ✓ | CEA-608 at 29.97 fps. Reads pop-on, roll-up and paint-on; writes pop-on with drop-frame (default) or non-drop timecode. |

`File ▸ Import` (or dropping a file) recognises caption files by extension and content and adds a
new caption track to the open sequence (a sequence is created when none is open). Cue times are
snapped to the sequence's frames. `File ▸ Export ▸ Captions…` writes the chosen caption track in
the format given by the file extension (`.srt`, `.vtt`, `.scc`).

Details of each format and the SCC encoder are in [`crates/captions/README.md`](../crates/captions/README.md).

## Editing

| Command | Menu / shortcut | Params |
|---|---|---|
| `captions.newTrack` | Sequence ▸ Captions ▸ Add New Caption Track… (⌥⌘A) | `format`, `name`, `language` |
| `captions.add` | Sequence ▸ Captions ▸ Add Caption at Playhead (⌥⌘C) | `track`, `text`, `time`, `durationSeconds` (3) |
| `captions.next` / `captions.previous` | Sequence ▸ Captions ▸ Go to Next/Previous Caption Segment (⌥⌘↓/↑) | |
| `captions.showAll` / `captions.hideAll` | Sequence ▸ Captions | |
| `captions.split` | Text panel ✂; Razor tool on a caption (⇧: every caption track) | `caption`, `time` (default: every caption under the playhead) |
| `captions.merge` | Text panel | `captions` (consecutive, one track) |
| `captions.setText` | Text panel text field | `caption`, `text`, `speaker` |
| `captions.setTimes` | Text panel in/out fields | `caption`, `start…`/`end…` (`Time` ticks, `Frame`, `Seconds`, `Timecode`) |
| `captions.trim` | drag a caption edge in the timeline | `caption`, `edge` (`in`/`out`), `delta` or `deltaFrames` |
| `captions.move` | drag a caption in the timeline | `captions`, `delta` or `deltaFrames` |
| `captions.duplicate` | ⌥-drag a caption in the timeline | `captions`, `delta` or `deltaFrames` — copies on the same tracks, overwriting what is there |
| `captions.delete` | Edit ▸ Clear / Ripple Delete with captions selected | `captions`, `ripple` |
| `edit.copy` / `edit.cut` / `edit.paste` / `edit.pasteInsert` | ⌘C / ⌘X / ⌘V / ⇧⌘V with only captions selected | captions paste at the playhead on the tracks they came from, overwriting (Paste Insert pushes later captions on); Paste places whichever of clips or captions was copied last |
| `captions.select`, `captions.goTo` | click / number button | `captions`, `add` / `caption` |
| `captions.setTrack` | caption track header (eye, lock) | `track`, `name`, `format`, `language`, `enabled`, `locked`, `syncLock` |
| `captions.applyTemplate` | Text panel ▸ Captions ▸ Template | `track`, `template` (`classic`, `color_pop`, `shine`, `type_on_two`, `word_reveal`, `karaoke`, `pop_word`, `word_marker_box`, `word_marker_underline`) |
| `captions.setStyle` | Text panel style strip | `track`, `size`, `color`, `background`, `backgroundColor`, `outline`, `outlineColor`, `align`, `anchor`, `margin`, `lineSpacing`, `animationSpeed`, `wordsPerGroup`, `highlightColor`, `wordMarkerOpacity`, `wordMarkerBorderColor`, `wordMarkerBorderWidth`, `wordMarkerRadius`, `reset` |
| `captions.deleteTrack` | | `track` |
| `captions.import` / `captions.export` | File ▸ Export ▸ Captions… | `path`, `format`, `track`, `dropFrame` |
| `captions.list` | (query) | `track` — tracks, styles and captions with timecodes (a `track` that names no caption track is an error) |

Tracks are addressed by id or `"C1"`, `"C2"`… (top first). Edits are limited by neighbouring
captions and a one-frame minimum. Sync-locked caption tracks follow insert and extract edits on
the media tracks (a caption across an insert point is split). `Add Edit to All Tracks` (⇧⌘K)
also splits captions.

## Timeline and Text panel

- Caption rows: header with `C1…` badge, lock, output eye, name and format; caption blocks in
  violet showing their text (Arabic and Hebrew in reading order from the block's right edge). Click selects (⌘/⇧ adds), drag moves (the whole selection; ⌥-drag
  copies), edge drag trims, the Razor tool cuts, double-click
  opens the Text panel. Automation ids: `timeline.caption.<id>`,
  `timeline.captionTrack.C1[.locked|.enabled]`.
- **Text panel ▸ Captions**: search, track picker (also creates tracks of each format), add /
  split / merge / delete / export buttons, the list of segments with editable in/out timecodes
  and text (committed when the field loses focus; Arabic and Hebrew show in reading order and are
  edited in logical order while the field has focus). The list keeps the caption under the playhead in view (centred as the
  playhead reaches it, not while a field is being edited). Automation ids: `text.tab.Captions`, `text.captions.add|split|merge|delete|export|search|track`,
  `text.captions.<id>.in|out|text|goto|row`, `text.captions.style.*`, and
  `text.captions.template[.option.<id>]`.
- **Text panel ▸ Template**: track style, template/animation, text formatting, line limit, box
  alignment, outline, and shadow settings. The line limit automatically reduces font size when
  necessary rather than hiding caption text. Selecting **Word marker (box)** adds controls for its
  fill opacity, independent border colour and width, and corner radius. Border width and radius are
  pixels relative to a 1080-line frame and scale with sequence resolution. Automation ids:
  `text.tab.Template`, `text.captions.style.wordMarkerOpacity`,
  `text.captions.style.wordMarkerBorderColor`,
  `text.captions.style.wordMarkerBorderWidth`, and
  `text.captions.style.wordMarkerRadius`.
- **Text panel ▸ Transcript**: the **Create captions** action opens settings for words per line,
  words per caption, and lines per caption. The command accepts `maxWordsPerLine` and
  `maxWordsPerCaption` for the same controls.

The Template picker applies a built-in look: **Classic** (static subtitle), **Color Pop** (warm
text that grows in), **Shine** (cool text with a moving highlight), **Type on (2 characters)**,
**Word by word** (reveals words in groups), **Karaoke highlight** (keeps the phrase visible and
highlights the current word group), **Pop word** (pops and highlights the current word group),
**Word marker (box)**, and **Word marker (underline)** (a highlight-coloured shape tracks the active
timed word). The box marker has independently adjustable fill opacity, border colour and width,
and corner radius.
The word-based looks use stored transcript word timings when available; imported captions without
those timings are evenly distributed across the cue. Editing caption text or timing clears stale
word timing, so animation falls back to even cue timing. The Template tab exposes group size, animation
speed, and highlight colour. Speed affects fallback timing and motion, but does not retime transcript
word boundaries. Templates are saved with the caption track and affect the Program monitor and
burned-in video exports. SRT/WebVTT/CEA sidecar exports remain text and timing only. Manual style
edits mark the track Custom; applying a template replaces the track style in one undoable action.

## Burn-in

The Program monitor shows the visible caption tracks. On export, `file.exportMedia`
`{"burnCaptions": true}` (Export mode: **Burn Captions Into Video**) draws them into every frame.
Captions are set in Inter SemiBold (OFL, bundled) by the text engine (`crates/text`: shaping,
kerning, bidi); italics/bold markup is not yet drawn differently. The captions of a nested sequence
are part of the nest's picture: they are drawn wherever the nest is shown, in the Program monitor
and in every export, whether or not the outer sequence shows or burns in its own.
