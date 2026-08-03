# 🎹 Keyboard Hero

**A novelty fork of [Neothesia](https://github.com/PolyMeilex/Neothesia) that turns MIDI practice into an arcade game.**

For fun, not profit. This exists because falling-note practice is more motivating when the
crowd cheers, the streak counter climbs, and the song hands you a letter grade at the end.
It's a hobby project — no releases, no support, no roadmap. Play with it, fork it, break it.

Upstream Neothesia is a genuinely excellent MIDI visualizer and this fork owes it everything.
If you want a serious, polished practice tool, **go use [Neothesia](https://github.com/PolyMeilex/Neothesia)** —
it's on Flathub and the AUR, and it's better maintained than this will ever be.

---

## What you need

**A MIDI keyboard, plugged in.** This is not optional. Keyboard Hero grades what *you* play,
so with no MIDI input device there's nothing to grade — you get a pretty visualizer and a
results screen that never appears. Any USB MIDI controller works; it just needs to show up as
a MIDI input on your system.

**MIDI files to play.** The app plays standard `.mid` files and none ship with it. Quality
matters more than you'd think: auto-generated or badly quantized MIDI feels awful to play
against, because the grading is timing-based and a sloppy file punishes you for its own
sloppiness.

For genuinely well-arranged piano MIDI, **[Betacustic's Patreon](https://www.patreon.com/betacustic)**
is the recommendation — hand-made arrangements that are actually pleasant to play along to,
which is exactly what this app needs. Well worth supporting if you get use out of this.

Other decent sources: [BitMidi](https://bitmidi.com/), [MuseScore](https://musescore.com/)
(export to MIDI), or your own DAW.

## Running it

Needs a [Rust toolchain](https://rustup.rs/).

```bash
cargo run --release --bin neothesia
# or, via the makefile:
make run-app
```

To install it properly:

```bash
make install-app
```

## The three modes

Switch between them any time with the segmented toggle in the **top right**, even mid-song.
Your tallies keep running across switches.

| Mode | What the song does | What you do |
|--|--|--|
| **HERO** | Rolls on, but the notes you're meant to play stay **silent** | Perform them. Guitar-Hero style — if you don't play it, nobody hears it |
| **AUTO** | Plays itself, fully audible | Jam over the top. Graded on whatever you actually play; never stalls |
| **HUMAN** | **Waits** for you at every note | Classic Synthesia-style play-along, at your own pace |

## Scoring

Every note you land is judged on timing — **PERFECT** (within 120ms), **GOOD** (within 280ms),
or a late catch-up the song had to sit and wait for. Chords count as one event, so rolling one
doesn't inflate your streak.

Landing notes builds a **combo**, which drives a x1–x4 score multiplier. Pass a 15 streak and
the keyboard catches fire. Wrong notes and missed notes break it. An audience gauge watches
your last 20 notes and reacts accordingly.

At the end you get a results screen with a letter grade from **A++** down to **F**, scored on
timing-weighted performance rather than raw accuracy — the idea being that an A should mean
"played in time", not "eventually hit the right keys". As of 0.5 the ladder is tuned so that
playing a song genuinely well lands you in A/B territory rather than perpetual C's.

## Optional extras

**Favourites.** Drop `.mid` files in `~/Music/MIDI/Favourites` and they appear as a
keyboard-navigable list right on the main menu — no file browsing.

**Crowd reactions.** Drop audio clips in `~/Music/FX` named `<anything>_<band>.<ext>`, where
`<band>` is the first letter of a grade (`a`, `b`, `c`, `d`, `f`). The matching clip plays over
the results screen, so the crowd goes wild for an A++ and lets you know about an F. Any format
[rodio](https://github.com/RustAudio/rodio) can decode works — ogg, mp3, wav, flac.

## Controls

| Key | Action |
|--|--|
| **Space** | Pause / resume |
| **Esc** | Back to the menu |
| **↑ / ↓** | Playback speed (hold **Shift** for bigger steps) |
| **PgUp / PgDn** | Note scroll speed (hold **Shift** for bigger steps) |
| **Enter** | Replay the song, on the results screen |
| **Backspace** | Back to the menu, on the results screen |

## What this fork adds

Everything upstream Neothesia does, plus: the HERO/AUTO/HUMAN toggle, Guitar-Hero hit effects
(light pillars, bloom sparks, on-fire keyboard), the combo counter and score multiplier, an
audience sentiment gauge, PERFECT/GOOD timing grades rising out of the struck keys, the
end-of-song results screen with letter grades, crowd SFX, rainbow note-guide squares with
letter labels on the keys, and the Favourites list.

## Licence

GPL-3.0, inherited from [Neothesia](https://github.com/PolyMeilex/Neothesia) by
[@PolyMeilex](https://github.com/PolyMeilex). See [LICENSE](LICENSE).

All credit for the engine — the renderer, the MIDI handling, the waterfall, the whole
foundation — belongs upstream. This fork just bolted an arcade cabinet onto it.
