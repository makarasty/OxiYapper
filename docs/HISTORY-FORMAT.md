# History format

The layout of the saved recordings. It is a stable contract (owner decision R3.3): tools outside
the app, such as a Claude Code helper, may read it. Fields and files are only ever added; a reader
ignores what it does not know. The app never makes a network call for any of it.

## Where

The history folder is `[history] folder`. Set to an absolute path, the app keeps its own subfolder
`OxiYapper history` in it; left empty, it is `history/` in the data directory. The Copy path
button on the History page gives the path of one entry.

The folder holds a marker file `.oxiyapper-history`. The app lists, recovers and deletes only
inside a folder that has it.

## One recording

```
<history folder>/
  .oxiyapper-history
  2026-11-08/                 local day, YYYY-MM-DD
    134501-123.wav            local start time HHMMSS-mmm
    134501-123.txt
    134501-123.json
```

The id of a recording is `2026-11-08/134501-123` (day, slash, start time). The local time has no
zone. The three files of an id share the name; a recording is complete when all three exist.
An entry with no `.json` is read with no model or settings, as `status` done when its `.txt`
holds text (an older entry from before this layout) and as `failed` otherwise (a save that
stopped after the audio, for example on a full disk).

- `.wav`: mono, 16-bit PCM. A recording the app finished is at 16 kHz and holds what whisper got
  (gain, denoise, trim and normalise as set at the time). A recording recovered after a crash
  (status `interrupted`, or `failed` when it was finished but no text came) is the raw microphone
  audio at the device's own rate (often 44.1 or 48 kHz): read the rate from the header.
- `.txt`: the transcript, UTF-8, no trailing newline added. Empty for `cancelled`, `interrupted`
  and `failed`.
- `.json`: the meta file below, UTF-8.

## Meta file

```json
{
  "version": 1,
  "started": "2026-11-08T13:45:01.123",
  "duration_ms": 4500,
  "status": "done",
  "model": "small",
  "language": "ru",
  "settings": { "normalize": true, "denoise": false, "trim": true, "model": "small" }
}
```

- `version`: layout version of this file; 1 now.
- `started`: local start of the recording.
- `duration_ms`: length of the audio.
- `status`: how it ended.
  - `done`: transcribed and delivered, or transcribed again from the History page.
  - `cancelled`: cancelled after speech was heard; the audio was kept, there is no text.
  - `interrupted`: the app died while recording; the audio was recovered at the next start.
  - `failed`: recorded, but no text came out of it.
- `model`: registry id of the model that made the text, or `custom`; empty when unknown.
- `language`: `auto` or a language code; empty when unknown.
- `settings`: the audio settings and model of the run that made the text; `null` when unknown.
  Transcribing again from the History page replaces `model`, `language`, `settings` and sets
  `status` to `done`.

Optional fields (added 2026-11-11; `version` stays 1, and an entry without them is complete):

- `app`: file name of the program whose window got the text (`Code.exe`); empty or absent when unknown.
- `delivered`: how the text reached the user: `pasted`, `typed`, `clipboard` (left on the clipboard), `file` (to-file mode), `filtered` (the confidence filter held all of it back, nothing was inserted; the `.txt` still has it), `failed`, `unknown`.
- `held`: the parts of the dictation the confidence filter held back, in order, each `{ "text": "...", "t0": 5200, "t1": 6900, "why": "no_speech" }`. `t0` and `t1` are milliseconds from the start of the `.wav` (the audio whisper got is what the `.wav` holds, so no offset applies); `why` is `low_confidence` or `no_speech`. When only some parts were held, the `.txt` has the inserted text and `held` the rest; when all were held, the `.txt` has them too and `delivered` is `filtered`. Absent when nothing was held.
- `words`: the words of the recording in order, each `{ "w": "hello", "p": 0.93, "t0": 120, "t1": 480 }`. `p` is the confidence of the model in the word, 0 to 1. `t0` and `t1` are its start and end in milliseconds from the start of the `.wav`; they are absent until the times were computed (on demand, when the recording is opened in the History page). The words come from a pass of the model over the audio and can differ from the `.txt`, which also holds replacements and edits.
- `timings`: present when `words` carry times: `{ "by": "dtw", "preset": "large-v3", "model": "podlodka" }`; `by` is `dtw` or `tokens`.

## While recording

The audio is written to disk as it is recorded, so a crash or a power cut keeps what was said:

1. At key-down the app creates `<id>.wav.part` (raw microphone audio, device rate) and appends to
   it from a thread of its own, flushing and syncing it to the disk about once a second. A disk
   too slow to keep up stops the file growing; the dictation itself goes on.
2. At key-up the file is closed and waits for the text. The save renames it to `.wav` under the
   same name (the recording's start time), replaces it with the processed 16 kHz audio, then
   writes the `.txt` and the `.json`. A recording that is cancelled early or in which nothing was
   heard is deleted. One whose transcription fails, comes out empty, or is still waiting when the
   app quits becomes an entry with status `failed` at once.
3. At the next start, a `.wav.part` that no running recording owns is made whole: its header is
   set from the bytes on disk, it is renamed to `.wav`, and an empty `.txt` and a `.json` with
   status `interrupted` are written. A recording cut off while it waited for its text also reads
   `interrupted`. It is never deleted by the pass that recovers it. A part with no audio in it is
   removed.

A reader must ignore `*.part`, `*.tmp` and any file whose name is not `HHMMSS-mmm.<ext>`.

## Limits

Saving is on by default. The oldest entries go first when the folder is over `max_bytes` (default
2 GB) or older than `max_days` (default 30 days, by date; 0 is off for each). An entry is deleted
as a whole: wav, txt and json together. Limits act on the folder in use while saving is on or off.
