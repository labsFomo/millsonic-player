# Millsonic Linux Player v0.7.0 — QA Handoff

> Hand this to a tester with **zero prior context**. It explains what the product is,
> how to install it, and exactly what to test and verify.

---

## 1. What is this product? (context)

**Millsonic** is a **background-music system for retail venues** (restaurants, fashion
stores, breweries, big chains). Each venue runs a small **player app** that streams the
brand's curated music on a schedule, mixed with **audio ads ("spots")**.

Key concepts you need to know:

- **Zone**: a venue/area that has an assigned music programming. The player is bound to a
  zone when it's "paired".
- **Grid / schedule**: a weekly timetable. Each time slot (e.g. "Morning 06:00–12:00") has
  a **playlist** of tracks. The player figures out *which song should be playing right now*
  based on the current time, and plays it.
- **Spots (anuncios)**: audio advertisements inserted between songs at a configured
  frequency (e.g. one spot every 4 songs).
- **SonicBox**: an interactive feature (popular in bars/breweries). Customers **vote with
  credits** for a song from a phone app. The player must play the most-voted song **next**
  (when the current one ends), then resume the normal grid. Multiple votes form a queue
  ordered by credits.

**This package is the Linux desktop player** (built with Tauri = Rust + a small web UI).
Version **0.7.0** adds full SonicBox support and a batch of reliability hardening.

### The product's non-negotiable rules (what "correct" means)
1. **The music must NEVER stop.** Silence/dead-air is the worst possible failure.
2. **Errors must be imperceptible** to staff and customers — the player should self-recover.
3. **A song must NEVER repeat back-to-back or loop.** People hate hearing the same song or
   the same ad on repeat. (A song may play again later, but not within a few songs.)
4. **SonicBox is secondary to the grid.** If SonicBox breaks, normal music keeps playing.

Keep these four rules in mind — most test cases verify one of them.

---

## 2. Install

**Target OS:** Ubuntu 22.04 / 24.04 (x86-64).

**Download (link valid 7 days):**
`millsonic-player_0.7.0_amd64.deb`
SHA-256: `106e21f7996763fa47bfe875921ce7b7068d15fc3f13e174f2487ca7703bb408`

```bash
# Verify the download
sha256sum millsonic-player_0.7.0_amd64.deb

# Install (apt resolves dependencies automatically)
sudo apt install -y ./millsonic-player_0.7.0_amd64.deb
# If it complains about deps:
sudo apt install -f -y
```

Runtime dependencies (apt pulls them): `libwebkit2gtk-4.1-0`, `libgtk-3-0`,
`libappindicator3-1` (the last lives in the `universe` repo — enable it with
`sudo add-apt-repository universe` if needed).

**Audio:** the machine needs a working audio output session (PipeWire or PulseAudio).
Verify with `aplay -l` (should list a card) before testing playback.

**Launch:** open "Millsonic Player" from the app menu, or run `millsonic-player` in a terminal.

### Pairing (required before anything plays)
On first launch the player shows a **pairing screen**. You need a **6-character pairing
code** for a test zone — **ask the Millsonic team (Pablo) for one**, plus tell them which
zone so they can confirm content/schedule exist. Enter the code → the player binds to the
zone, downloads the schedule + tracks, and starts playing. After pairing, the player is set
to **auto-start on boot**.

### Where the logs are (your main tool)
```
~/.config/Millsonic/millsonic.log      # everything the player does
~/.config/Millsonic/config.json        # pairing/config (+ .bak backup)
~/.config/Millsonic/cache/tracks/      # downloaded songs (<trackId>.mp3)
~/.config/Millsonic/millsonic.db        # local SQLite cache
```
Keep `tail -f ~/.config/Millsonic/millsonic.log` open while testing.

---

## 3. Version sanity check
On launch the log must show:
```
=== Millsonic Player started ===
Version: 0.7.0
```
The UI "About"/footer should also read **v0.7.0**. If it doesn't, you're running the wrong
build — stop and re-install.

---

## 4. Test plan

For each test: **do the action**, check the **expected result**, and confirm via the
**log**. Report PASS/FAIL with the relevant log lines + a short note.

### A. Basic playback & schedule
| # | Do | Expect | Verify |
|---|----|--------|--------|
| A1 | Pair + wait | Music starts within a few seconds; UI shows now-playing (title/artist) | log: `Sync loop started`, `play_file: '<song>'` |
| A2 | Let it run 10+ min | Songs advance automatically with no gaps/silence | UI title changes; no `Error` lines |
| A3 | Check the song that starts | It starts at the song the **time-of-day schedule** dictates (not always track #1) | the player computes position from current time |
| A4 | Let several songs play | A **spot (ad)** plays after the configured number of songs, then music resumes | log: `Playing spot` then normal track |

### B. The "never repeat" rules (high priority)
| # | Do | Expect | Verify |
|---|----|--------|--------|
| B1 | Listen for ~30–40 min | No song plays twice **back-to-back**; no song repeats within a short window | note any repeat + timestamp |
| B2 | Listen across multiple spots | The **same ad does NOT play twice in a row** when more than one ad is eligible | log: `Spot eligible: <id>` should rotate ids |

### C. SonicBox (vote-to-play) — needs the team to cast a test vote
> Coordinate with the Millsonic team to cast a vote on your zone (they'll tell you the song).
| # | Do | Expect | Verify |
|---|----|--------|--------|
| C1 | Team casts a vote while a song plays | When the current song ends, the **voted song plays next**, then the normal grid resumes | log: `SonicBox: pre-downloading...`, `staged voted track`, then `playing voted track` |
| C2 | Watch the cached folder | The voted song's mp3 appears in `cache/tracks/` **before** it's due (pre-downloaded) | file `<trackId>.mp3` present ahead of time |
| C3 | Team casts 2+ votes with different credit amounts | The **higher-credit** song plays first (queue ordered by votes) | which song plays first |
| C4 | **Anti-loop:** after a voted song plays, see if it repeats immediately | It must **NOT** play again back-to-back; it can only come back after ~4 other songs | log: `SonicBox: skipping repeat of track ...` |
| C5 | Team casts a vote, then the venue goes offline (see D) | Votes simply stop entering; **normal music keeps playing** with no glitch | no `panic`, grid continues |

### D. No internet (pull the network cable / disable wifi)
| # | Do | Expect | Verify |
|---|----|--------|--------|
| D1 | Disconnect network **while a song is playing** | Music **keeps playing** uninterrupted; status indicator shows offline | log: `Offline`; audio never stops |
| D2 | Stay offline through several songs | Cached songs keep playing on the grid; cached spots still play | no silence |
| D3 | Reconnect | Player re-syncs; play reports get flushed; SonicBox resumes | log: `Flushing N pending play reports`, `Online` |

### E. Resilience / self-recovery (the hardening in 0.7.0)
| # | Do | Expect | Verify |
|---|----|--------|--------|
| E1 | **Corrupt a cached song**: while offline, overwrite one `cache/tracks/*.mp3` with junk (`echo x > file.mp3`) | When it's that song's turn, the player **skips it** without audible glitch | log shows decode error then next song |
| E2 | **Corrupt ALL cached songs** (junk into every mp3) | The player must **NOT stop**; it recovers (re-sync + emergency shuffle), never shows permanent "Error de reproducción" | log: `recovering ... NOT stopping` |
| E3 | **Corrupt the database**: `echo junk > ~/.config/Millsonic/millsonic.db`, restart player | Player **boots normally** (it rebuilds the DB), no crash | log: `SQLite cache DB is unusable — deleting and recreating it` |
| E4 | **Corrupt config**: `echo junk > ~/.config/Millsonic/config.json`, restart | Player **keeps its pairing** (restored from `.bak`), still plays | log: `restored from config.json.bak` |
| E5 | **Stuck check**: (hard to force) confirm a watchdog exists | If audio freezes >~30s, it auto-restarts the track | log: `WATCHDOG: ... force-restarting` |
| E6 | **No audio device**: stop PipeWire/PulseAudio, launch player | Player boots, shows a red banner "no audio device detected", does **not** crash | log: `No audio output device available`; UI banner |
| E7 | **Low disk**: fill the disk to <800 MB free, let it download | Player evicts old cached songs to make room; never fills the disk to 100% | log: `running LRU cleanup` |

### F. Restart & resume
| # | Do | Expect | Verify |
|---|----|--------|--------|
| F1 | Kill the player (`pkill millsonic-player`) and relaunch | It resumes the schedule at **the song that should be playing now** (by time), not always song #1 | compare to a second device or the time |
| F2 | Reboot the machine | Player **auto-starts** and resumes music without manual action | it launches on its own |

### G. Update flow
| # | Do | Expect | Verify |
|---|----|--------|--------|
| G1 | If the team pushes a newer version | Player shows an "update available" prompt; it does **NOT** auto-install | a modal appears |
| G2 | Accept the update | Before restarting it flushes pending data; then relaunches on the new version | log: `flushing reports + stopping audio before restart` |

---

## 5. What to report back

For each test: **ID, PASS/FAIL, the log lines, and a 1-line note.** Especially flag:
- Any moment of **silence / dead-air** (rule 1).
- Any **song or ad that repeats** back-to-back or loops (rule 3).
- Any **crash / app exit / "Error de reproducción"** that doesn't self-recover (rules 1–2).
- Anything the customer/staff **could perceive** as broken.

Attach `~/.config/Millsonic/millsonic.log` with your report.

### Known limitations in 0.7.0 (not bugs — out of scope for this round)
- **Multi-branch sync**: two venues in the same zone play roughly the same song at the same
  time, but **not synced to the exact second** (no continuous re-sync / NTP yet).
- The player relies on the machine's **system clock** for schedule position — make sure the
  test machine's clock/timezone is correct (ideally NTP-synced), or the wrong song may start.
- The voted-song hand-off at end-of-track + the server "mark played" confirmation are best
  verified on real hardware with audio (covered by C1–C4).
