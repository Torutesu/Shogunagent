# ShogunAgent videos

The launch film and seven scene-by-scene explainers, in English and Japanese. They are HTML compositions rendered frame by frame, so the copy, the timing and the pictures live in source and every export is reproducible.

| File | What it covers | Length |
|---|---|---|
| `00-launch` | The launch film: the repeated-backstory problem, save, recall, typed state, many agents, approvals, local-first, call to action | 1:24 |
| `01-save` | How a decision gets from a chat into the store: checkpoints, `actor`, the idempotency key, masking on the way in | 0:41 |
| `02-recall` | How a fresh session gets context back: the budget, facts / `possibly:` / evidence, and what comes back before review is configured | 0:43 |
| `03-state` | From saved notes to people, projects, commitments and open loops: the event log, provenance, confidence bands, state over time | 0:53 |
| `04-faces` | MCP, REST and CLI as one behavior; many clients on one store; handoff; the 21 frozen tool names | 0:50 |
| `05-permissions` | L1 / L2 / L3, what an L3 call returns, why approved is not delivered | 0:52 |
| `06-local-first` | One principal per encrypted file, who holds what, export, the open engine | 0:51 |
| `07-quickstart` | The README quickstart in five steps: build, keys and start, save, recall, bridge check | 1:02 |

Each export is 1920×1080, 30 fps, H.264 with a generated music bed, plus `.srt` and `.vtt` subtitles and a poster frame. The written script for all eight is in [SCRIPT.md](SCRIPT.md).

## Render

Needs Node.js 22, Playwright with Chromium, ffmpeg (libx264 and aac) and Python 3 with numpy. From the repository root:

```sh
node apps/video/scripts/render.mjs
```

Output goes to `apps/video/out/<lang>/` (ignored by Git). All sixteen files take about fifteen minutes on four cores.

```sh
node apps/video/scripts/render.mjs --video launch --lang ja
```

```sh
node apps/video/scripts/render.mjs --no-audio --jobs 2 --out /tmp/videos
```

## Preview and edit

```sh
node apps/video/scripts/serve.mjs
```

Open the printed URL for a player with a video picker, a language picker and a scrubber. Add `&t=12.5` to hold one moment.

- `src/launch.js` and `src/explainers.js`: the scenes. Each has a length, markup with timing attributes, and its subtitle lines in both languages.
- `src/ui.js`: the shared parts (chat window, store, state card, terminal).
- `src/engine.js`: the timeline. Nothing reads the clock; a frame is a pure function of its timestamp. The attribute vocabulary is documented at the top of the file.
- `src/styles.css`: the 1920×1080 stage, in the website's palette and type.
- `scripts/audio.py`: the music bed, synthesized, so there is nothing to license.

After changing copy or layout:

```sh
node apps/video/scripts/check.mjs
```

```sh
node apps/video/scripts/captions.mjs --write
```

```sh
node apps/video/scripts/fonts.mjs --check
```

`check.mjs` samples every scene of every video in both languages and fails when something leaves the frame, runs into the caption band, or is clipped. `fonts.mjs --check` fails when the copy uses a glyph the subset fonts in `fonts/` lack; rebuild them with `fonts.mjs --from <directory holding the @fontsource packages>`.

## What the videos may claim

The videos follow the same boundaries as the website ([apps/web/README.md](../web/README.md)) and the README's "what is available" table.

- Every example is fictional and each frame that shows one says so. There are no customers, benchmarks or testimonials.
- No frame shows a send succeeding. The approval card stays at "Waiting for you"; `05-permissions` says that approved is not delivered.
- Typed state is shown as the output of a review step the operator configures, and `02-recall` and `07-quickstart` say that `facts` and `items` are empty before that.
- Hosted access is described as not publicly available; packages on npm and PyPI as not published; platforms as Linux and macOS built from source.
- Client names appear as MCP clients you can configure. `04-faces` points to the repository for which ones are verified.
- Terminal scenes show the README's commands. Expected results appear beside the terminal as labelled notes, not as invented log output.
- Tool names, levels and status values come from `docs/api/core-tools.md`.

When the product changes, change the scene and re-render; do not edit an export.

## Fonts

`fonts/` holds subsets limited to the characters the videos draw: Manrope (as on the website), Noto Sans JP, Noto Serif JP, Gelasio and JetBrains Mono. All are under the SIL Open Font License 1.1; the licence texts for the four families added here sit next to them.
