# Assistant access over MCP

An MCP client can inspect the open session and edit MIDI through transit's command channel.
The client runs `soundcraft-cli mcp` as a stdio MCP server. To control the desktop session,
that server connects to transit's authenticated TCP socket on `127.0.0.1`.

## Connect to the desktop session

Start transit with a chosen port, for example `transit --control 7801`, then configure a stdio
MCP server in your client:

```json
{
  "mcpServers": {
    "transit": {
      "command": "soundcraft-cli",
      "args": ["mcp", "--connect", "7801"]
    }
  }
}
```

Use absolute executable paths if they are not on PATH. `TRANSIT_CONTROL_PORT` also selects
the app's port; `--control 0` chooses an available port and logs it. Pass that actual port to
`--connect`. This is a loopback TCP socket, not an HTTP MCP endpoint or a Unix domain socket.
The CLI bridges newline-delimited MCP JSON-RPC on stdio to the app's JSON-lines protocol.

At each launch the app generates a fresh token. As defined in
[`crates/automation/src/token.rs`](../crates/automation/src/token.rs), its location is:

| Platform | Default token file |
|---|---|
| Windows | `%APPDATA%\transit\control-token` |
| macOS | `~/Library/Application Support/transit/control-token` |
| Linux | `$XDG_CONFIG_HOME/transit/control-token`, or `~/.config/transit/control-token` |

`TRANSIT_CONFIG_DIR` overrides the settings directory. `TRANSIT_CONTROL_TOKEN_FILE` overrides
the entire token path and takes precedence. An empty override disables that path. Give the
app and CLI the same override environment when using a custom location. The CLI reads the
token and sends `auth <token>` as the first socket line automatically. Do not paste the token
into chat or source control. A custom socket client must perform the same authentication;
see [the control protocol](control-protocol.md#authentication-transit).

For a separate headless session, omit `--connect`: `soundcraft-cli mcp` opens neither a window
nor an audio device. That session is separate from the desktop. File access confinement for
headless clients is described in [MCP server](mcp.md#confining-file-access).

## Musical addressing

Discover command parameter docs with `list_commands`, then call them with `execute` or `batch`.
MIDI `start` and `length` strings accept `bar|beat` or `bar|beat|tick`, such as `2|3|120`.
Bars and beats are **1-based**; the optional tick is a **0-based** offset within a beat.
The session uses 960 ticks per quarter note; a beat follows the meter denominator, so in 6/8
a beat is 480 ticks. Tempo changes affect conversion to samples, not ticks per quarter.

| Parameter | Meaning |
|---|---|
| `midi.new_clip.start` / `.end` | Session positions: `1\|1` is the session beginning. |
| Note `start` | Clip-relative position: `1\|1` is the clip beginning, `2\|3` is local bar 2, beat 3. |
| `length` | An offset using the same 1-based notation: `1\|2` = one beat, `1\|3` = two beats, `2\|1` = one bar, `5\|1` = four bars. `1\|1` is zero and is rejected for lengths. |
| Retrospective recording note `start` | A session position; the generated clip is placed around the captured notes. |

Local bars begin at the clip start (or the note start for lengths), using the meter there.
Later meter changes begin new local bars; a partial local bar ends at the change. An address
past that partial bar must use the next bar number. Clip durations integrate the session's
tempo map from the clip start through its end.

The aliases apply to `midi.note_add`, `midi.note_edit`, `midi.chord_add`, `event.midi_note`, and
each note in `event.retrospective_record`. Existing numeric `start_ticks` and `length_ticks`
remain available; do not supply an alias and its tick field together. Invalid musical
components, negative values, and beats beyond the applicable meter produce errors.
Retrospective recording retains its existing length cap of 1024 quarter notes.

`midi.new_clip` and retrospective note `start` keep their existing sample and time formats.
Colon strings remain clock time: `0:12` means 12 seconds, `1:30` means 90 seconds, and
`00:00:10:00` is timecode for 10 seconds. `1:2` means 62 seconds; `1|2` is musical beat 2.
The existing explicit position form `bars_beats:5|1|000` remains available too. Other commands
keep their existing addressing syntax, including `midi.notes_move`'s signed `ticks` delta.

For clip-relative note commands, `start` and `length` must be musical strings. Numeric or
other non-string aliases (including null) are rejected with an error directing you to
`start_ticks` or `length_ticks`. Retrospective `length` follows the same rule; retrospective
`start` and `midi.new_clip` retain their existing numeric sample and `{seconds}` positions.

## Chords

`midi.chord_add` takes a clip ID, `root`, exactly one of `quality` or `notes`, optional
`inversion`, musical `start`/`length` (or their tick fields), and optional `velocity` (1–127,
default 100). Start defaults to the clip beginning; length defaults to 480 ticks.

Roots may be MIDI numbers 0–127 or note names with an optional sharp/flat and octave:
`G3`, `Bb2`, `F#3`. Names use transit's display convention: **C3 = MIDI 60**. A bare name
uses octave 3 (`G` means `G3`). Qualities are `maj`, `min`, `dim`, `aug`, `maj7`, `min7`,
`dom7`, `sus2`, and `sus4`. Alternatively, `notes: [0, 3, 7]` supplies semitone intervals
from the root (1–32 entries, each between -127 and 127).

Inversion 0 is root position; inversion N raises the lowest N distinct tones by one octave.
The maximum inversion is the number of distinct tones minus one. Intervals and resulting
pitches are sorted and deduplicated; pitches outside MIDI range are rejected. Custom
intervals can express open voicings, for example `[0, 7, 15]`. All notes are inserted on MIDI
channel 0 in one undo step, and the result contains the added notes. A rejected chord adds
nothing. Use `edit.undo` to undo a chord and `edit.redo` to restore it.

## Worked example: four bars in G minor

Each JSON block below is the **arguments** to the named MCP tool. Inspect first:

```json
{}
```

Call that with `inspect_session`. Its `tempo` and `meter` arrays give changes at tick positions;
`ticks_per_quarter` gives the resolution. `key` is the key at session start, or null when none
is recorded; `key_signatures` lists all key changes with `at` positions in samples. Both
summary and full detail include these fields. Find an existing instrument track in `tracks`;
this example uses one named `Keys`, with an instrument already assigned for playback.
It assumes the inspected session is 4/4 throughout these four bars.

Use `list_commands` with `{"filter":"midi"}` to discover the helpers. Set the session key
with `execute` (this labels the key; it does not constrain pitches automatically):

```json
{"command":"event.add_key_change","params":{"key":"G minor","at":0}}
```

Create the clip with `execute`:

```json
{"command":"midi.new_clip","params":{"track":"Keys","start":"1|1","length":"5|1"}}
```

Read the returned `clip` ID. The following example uses **42**; replace every 42 with the
actual ID. Call `batch` for Gm–Eb–Bb–F, using inversions to keep the voices close:

```json
{"calls":[
  {"command":"midi.chord_add","params":{"clip":42,"root":"G3","quality":"min","start":"1|1","length":"2|1","velocity":90}},
  {"command":"midi.chord_add","params":{"clip":42,"root":"Eb3","quality":"maj","inversion":1,"start":"2|1","length":"2|1","velocity":86}},
  {"command":"midi.chord_add","params":{"clip":42,"root":"Bb2","quality":"maj","inversion":2,"start":"3|1","length":"2|1","velocity":88}},
  {"command":"midi.chord_add","params":{"clip":42,"root":"F3","quality":"maj","start":"4|1","length":"2|1","velocity":86}}
]}
```

Add movement inside bar 1: let the top voice move from D4 to Eb4 on beat 3. First call
`execute` with `{"command":"midi.notes","params":{"clip":42}}`. In this freshly created
clip, index 2 is D4 (MIDI 74). Indices can change after any edit, so inspect again if you
have made other edits. Shorten that note, then add Eb4 for the remaining two beats:

```json
{"calls":[
  {"command":"midi.note_edit","params":{"clip":42,"index":2,"length":"1|3"}},
  {"command":"midi.chord_add","params":{"clip":42,"root":"Eb4","notes":[0],"start":"1|3","length":"1|3","velocity":85}}
]}
```

Call that with `batch`, then verify with `inspect_session` and `execute` / `midi.notes`.
The clip spans four bars and contains 13 notes, all in G natural minor. No tick arithmetic
is needed. Each command in a batch is its own undo step; a batch stops on the first error
by default and does not roll back earlier successful commands.

The MCP acceptance test is
[`crates/automation/tests/assistant_access.rs`](../crates/automation/tests/assistant_access.rs).
