# RustyDeck

A small service that drives an Elgato Stream Deck from a YAML file. Icons,
labels, colours and actions live in `~/.config/rustydeck/config.yaml`, and edits
are picked up while the service runs.

- **7448 Material Design Icons built in** — `icon: mdi:volume-high`, no files to
  manage, drawn as vector glyphs.
- **Keys that follow the system** — a polled command decides how a key looks, so
  a muted sink turns the speaker key red on its own.
- **Templates** — the output of that command can land in the label:
  `Vol {{ stdout }}`.
- **REST mode** — `rustydeck api` hands the deck to another program instead.
- **No C dependencies** — talks to `/dev/hidraw` directly, so `cargo build` is
  all it takes.

**Contents:** [Install](#install) · [Quick start](#quick-start) ·
[Configuration](#configuration) · [API mode](#api-mode) ·
[Commands](#commands) · [Behaviour](#behaviour) · [Development](#development) ·
[Hardware](#hardware)

## Install

Every release ships static binaries and packages for x86_64 and aarch64 on the
[releases page](https://github.com/luebke-dev/RustyDeck/releases):

```bash
# Fedora and friends
sudo dnf install ./rustydeck-<version>-1.x86_64.rpm

# Debian and Ubuntu
sudo apt install ./rustydeck_<version>-1_amd64.deb

# Anywhere else — statically linked, no dependencies
tar xzf rustydeck-<version>-linux-x86_64.tar.gz
```

The packages put the binary in `/usr/bin`, a systemd user unit in
`/usr/lib/systemd/user/` and the udev rule in `/usr/lib/udev/rules.d/`.

From source:

```bash
cargo build --release      # needs Rust 1.88 or newer
```

### Device permissions

If `rustydeck devices` finds nothing while the deck is plugged in, the access
permissions on `/dev/hidraw*` are missing. `rustydeck udev-rule` prints the
matching rule:

```bash
rustydeck udev-rule | sudo tee /etc/udev/rules.d/70-streamdeck.rules
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Then unplug the deck once and plug it back in.

## Quick start

```bash
rustydeck init       # write an example configuration
rustydeck devices    # is the deck detected?
rustydeck run        # start the service
```

As a user service — with a package the unit is already there, otherwise
`rustydeck install` writes one pointing at your build:

```bash
systemctl --user enable --now rustydeck.service
```

So programs launched from a key find the graphical session, run
`systemctl --user import-environment WAYLAND_DISPLAY DISPLAY XDG_CURRENT_DESKTOP`
once, or set those variables in the unit.

## Configuration

`rustydeck init` writes a commented example to
`~/.config/rustydeck/config.yaml`. Its shape:

```yaml
device:
  serial: null        # only needed with several decks, see `rustydeck devices`
  brightness: 60

defaults:             # applies everywhere; overridable per page and per key
  background: "#14161c"
  color: "#e6edf3"
  font_size: 13

start_page: main

pages:
  main:
    buttons:
      0:
        label: Editor
        icon: mdi:code-braces
        action:
          run: code                       # a string goes through `sh -c`
      1:
        label: Note
        action:
          run: ["notify-send", "Hello"]   # a list starts without a shell
      14:
        label: "Media »"
        action:
          page: media
  media:
    background: "#241c2e"                 # page-wide default
    buttons:
      14:
        label: Back
        action: back
```

Keys are numbered from the top left, row by row. On the MK.2 (5×3):

```
 0  1  2  3  4
 5  6  7  8  9
10 11 12 13 14
```

`rustydeck check` validates the file and reports missing icons;
`rustydeck preview` renders the pages to PNG so the layout can be checked
without hardware.

### Actions

| Action | Example | Effect |
| --- | --- | --- |
| `run` | `action: {run: firefox}` | Start a command; a string through `sh -c`, a list directly |
| `page` | `action: {page: media}` | Switch to the named page |
| `back` | `action: back` | Return to the previous page |
| `brightness` | `action: {brightness: "+10"}` | Set the brightness (`60`) or change it (`"+10"`, `"-10"`) |
| `reload` | `action: reload` | Re-read the configuration |

### Icons

Two kinds of value work under `icon:`:

```yaml
icon: mdi:volume-high     # a named Material Design Icon, built in
icon: icons/code.png      # an image file, relative to the configuration file
```

Named icons come from the [Material Design Icons](https://pictogrammers.com/library/mdi/)
webfont embedded in the binary, so nothing has to be installed. Names are
forgiving — `mdi:volume-high`, `mdi:volume_high` and `mdi:mdi-volume-high` all
find the same icon. Being glyphs, they stay sharp at any key size and take their
colour from `icon_color`, falling back to `color`.

```bash
rustydeck icons volume     # every icon whose name contains "volume"
rustydeck icons            # all of them
```

A name that does not exist is reported by `rustydeck check`, with close matches:

```
! key 3: unknown icon `mdi:volume-hi` — did you mean volume-high, volume-low?
```

Image files may be PNG, JPEG, GIF, BMP or WebP; they are scaled to the key size
and their transparency is composited over the background colour. SVG is not
read — convert it first, e.g. `rsvg-convert -w 144 -h 144 in.svg -o out.png`.

### Keys that follow the system

A key with a `state` block polls a command and takes its look from the first
matching case — so it shows what the system is actually doing, whoever changed
it:

```yaml
7:
  label: Sound
  icon: mdi:volume-high             # the resting look
  action:
    run: wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle
  state:
    run: wpctl get-volume @DEFAULT_AUDIO_SINK@   # prints "Volume: 0.73 [MUTED]"
    interval: 2                                  # seconds between polls
    cases:
      - contains: "[MUTED]"
        label: Muted
        icon: mdi:volume-off
        background: "#3a2224"
      - {}                          # no condition: always matches, goes last
```

A case may override `label`, `icon`, `action` and every styling key — so a key
can also *do* different things depending on what it currently shows. Conditions,
which all have to hold when several are given:

| Condition | Matches when |
| --- | --- |
| `contains: "text"` | the output contains that text |
| `equals: "text"` | the trimmed output equals it |
| `exit: 0` | the command exited with that status |

A case without any condition always matches; put it last as the resting state.
Leave it out and the key simply keeps its last look while nothing matches —
`rustydeck check` points that out.

### Templates

Labels, icons and colours may contain [minijinja](https://docs.rs/minijinja)
expressions, with the state command's output as `stdout`:

```yaml
8:
  icon: mdi:speaker
  state:
    run: wpctl get-volume @DEFAULT_AUDIO_SINK@
    interval: 2
    cases:
      - contains: "[MUTED]"
        label: "—"
      - label: "{{ ((stdout | replace('Volume: ', '') | float) * 100) | round | int }}%"
```

Without `cases` you get a single catch-all case, which is all a readout needs:

```yaml
9:
  label: "{{ stdout }}"        # a clock on the deck
  font_size: 20
  state:
    run: date +%H:%M
    interval: 20
```

A template can reach `stdout` (trimmed output), `lines` (its lines), `exit` (the
exit status) and `case` (index of the matching case). Icons can be picked inline
too — `icon: "mdi:volume-{% if 'MUTED' in stdout %}off{% else %}high{% endif %}"`
— though separate cases usually read better.

A failing template renders as empty rather than printing its own source onto the
key, and says why in the log. Before the first poll answers the context is
empty; that is expected and not logged as a problem.

### Styling

`background`, `color`, `icon_color`, `font`, `font_size`, `padding` and
`press_feedback` can be given under `defaults`, per page, per key and per state
case — the most specific value wins. Colours as `#rgb`, `#rrggbb`, or a name
(`red`, `blue`, `grey`, …).

## API mode

`rustydeck api` skips the YAML file and serves the keys over HTTP, so another
program owns the deck: it sets key images and learns about presses from an event
stream. Nothing is run on the deck's behalf here — what a press means is up to
the client.

```bash
rustydeck api --listen 127.0.0.1:8790 --token secret
```

It listens on localhost by default. `--token` is optional but worth setting as
soon as the port is reachable from elsewhere; pass it as
`Authorization: Bearer <token>` or, for event streams, as `?token=`.

| Endpoint | Purpose |
| --- | --- |
| `GET /api/device` | Model, serial, firmware, key count and layout |
| `GET /api/keys` | What every key currently shows |
| `PUT /api/keys/{index}` | Set one key |
| `PUT /api/keys` | Set several at once: `{"0": {...}, "3": {...}}` |
| `DELETE /api/keys/{index}` | Clear one key |
| `DELETE /api/keys` | Clear every key |
| `GET`, `PUT /api/brightness` | Read or set it: `{"value": 60}` or `{"delta": -10}` |
| `GET /api/events` | Key presses as server-sent events |

A key is described by the same JSON in both directions:

```bash
curl -X PUT -H 'Authorization: Bearer secret' \
     -d '{"label":"Build","icon":"mdi:hammer","background":"#1f2a44"}' \
     http://127.0.0.1:8790/api/keys/0
```

`icon` takes a `mdi:` name or a path on the machine running the service, while
`image` takes a base64-encoded PNG, JPEG, GIF, BMP or WebP — which spares a
remote client from needing files there. `label`, `background`, `color`,
`icon_color`, `font_size` and `padding` work as in the YAML file.

Presses arrive as an event stream:

```bash
curl -N -H 'Authorization: Bearer secret' http://127.0.0.1:8790/api/events

event: key
data: {"key":3,"action":"down"}

event: key
data: {"key":3,"action":"up"}
```

In a browser that is `new EventSource('http://host:8790/api/events?token=secret')`.
Reading presses and writing images use separate device handles, so neither side
waits on the other, and every stream gets a thread of its own.

## Commands

| Command | Purpose |
| --- | --- |
| `rustydeck run` | Start the service (default) |
| `rustydeck api` | Serve the keys over HTTP instead of a YAML file |
| `rustydeck devices` | List attached decks and their firmware |
| `rustydeck init` | Write an example configuration |
| `rustydeck check` | Validate the configuration, report missing icons |
| `rustydeck preview` | Render pages as PNG (check the layout without hardware) |
| `rustydeck icons [TEXT]` | Search the built-in icons by name |
| `rustydeck install` | Write a systemd user unit |
| `rustydeck udev-rule` | Print the udev rule |

`-c/--config` selects a different file, `-v` raises the log level, `--help`
lists the options of the API mode.

## Behaviour

- The configuration file and the directory around it are watched: saving is
  enough, and a new icon shows up immediately. `SIGHUP` forces the same.
- Broken YAML leaves a running service untouched — the previous configuration
  stays active and the error goes to the log.
- State commands are polled per page, on a thread of their own, so a slow one
  never blocks the keys. Leaving a page stops its polling, and a key press
  re-polls at once so the picture catches up with what the press did.
- When the deck is unplugged the service waits and reconnects as soon as it is
  back.

## Development

Git hooks are managed with [prek](https://github.com/j178/prek), a faster
drop-in replacement for pre-commit that reads the same
`.pre-commit-config.yaml` (plain `pre-commit` works just as well):

```bash
cargo install --locked prek
prek install --hook-type pre-commit --hook-type commit-msg --hook-type pre-push
```

Committing then checks file hygiene, `cargo fmt` and `cargo clippy`; pushing
also runs the tests. `prek run --all-files` checks everything at once, which is
what CI does too.

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org),
enforced by the `commit-msg` hook:

```
feat: serve the keys over HTTP
fix: use the portable ioctl request type so musl builds
ci: track the current versions of the GitHub actions
```

Versions follow the calendar — `YEAR.MONTH.MINOR`, where `MINOR` counts the
releases within that month. Pushing a tag such as `v2026.8.1` makes CI build the
binaries and packages and publish them as a GitHub release.

## Hardware

Stream Deck Original V2, MK.2, MK.2 Scissor, XL, XL V2, Neo, and the Stream
Deck + (keys only — no dials, no touch strip). Gen 1 hardware (the 2017 Original
and the Mini) speaks a different protocol and is not supported.

RustyDeck talks to the kernel directly through `/dev/hidraw*` rather than using
`hidapi`. That avoids the C dependencies (`libudev-devel`, `libusb-devel`) which
would otherwise have to be layered onto an rpm-ostree system, and it is why the
release binaries can be statically linked against musl.

## Bundled assets

`assets/materialdesignicons-webfont.ttf` and `assets/mdi-codepoints.txt` come
from [@mdi/font 7.4.47](https://www.npmjs.com/package/@mdi/font); the codepoint
table is generated from that release's CSS. The icons are licensed under
Apache 2.0 by the [Pictogrammers](https://pictogrammers.com/) — see
`assets/MDI-LICENSE`. To move to a newer release, replace the font, regenerate
the table from the matching `materialdesignicons.css`, and update
`icons::VERSION`.

RustyDeck itself is MIT licensed; see `LICENSE`.
