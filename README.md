# RustyDeck

A small service that drives an Elgato Stream Deck from a YAML file. Key images
(icon, label, colour) and actions live in `~/.config/rustydeck/config.yaml`;
edits to that file are picked up while the service runs.

Icons can be named rather than drawn: 7448 Material Design Icons are built into
the binary, so `icon: mdi:volume-high` is all a key needs. Keys can also follow
the state of the system — a muted sink turns the speaker key red on its own.

## Why no `hidapi`?

RustyDeck talks to the kernel directly through `/dev/hidraw*`. That removes the
C dependencies (`libudev-devel`, `libusb-devel`) which would otherwise have to
be layered onto an rpm-ostree system — `cargo build` is enough.

## Supported devices

Stream Deck Original V2, MK.2, MK.2 Scissor, XL, XL V2, Neo, and the Stream
Deck + (keys only — no dials, no touch strip). Gen 1 hardware (the 2017
Original, and the Mini) uses a different image format and is not supported.

## Installing

Every release ships a static binary and packages on the
[releases page](https://github.com/luebke-dev/RustyDeck/releases), for x86_64
and aarch64:

```bash
# Fedora and friends
sudo dnf install ./rustydeck-2026.8.1-1.x86_64.rpm

# Debian and Ubuntu
sudo apt install ./rustydeck_2026.8.1-1_amd64.deb

# Anywhere else: a statically linked binary, no dependencies
tar xzf rustydeck-2026.8.1-linux-x86_64.tar.gz
```

The packages place the binary in `/usr/bin`, a systemd user unit in
`/usr/lib/systemd/user/rustydeck.service` and the udev rule in
`/usr/lib/udev/rules.d`, so after installing it is:

```bash
rustydeck init
systemctl --user enable --now rustydeck.service
```

## Building it yourself

```bash
cargo build --release
./target/release/rustydeck init      # write an example configuration
./target/release/rustydeck devices   # is the deck detected?
./target/release/rustydeck run       # start the service
```

Run it as a user service:

```bash
./target/release/rustydeck install
systemctl --user daemon-reload
systemctl --user enable --now rustydeck.service
```

So launched programs find the graphical session, run
`systemctl --user import-environment WAYLAND_DISPLAY DISPLAY XDG_CURRENT_DESKTOP`
once, or set those variables in the unit.

### Permissions on `/dev/hidraw*`

If `rustydeck devices` reports nothing while the deck is plugged in, the access
permissions are missing. `rustydeck udev-rule` prints the matching rule:

```bash
rustydeck udev-rule | sudo tee /etc/udev/rules.d/70-streamdeck.rules
sudo udevadm control --reload-rules && sudo udevadm trigger
```

Then unplug the deck once and plug it back in.

## Configuration

```yaml
device:
  serial: null        # only needed with several decks, see `rustydeck devices`
  brightness: 60

defaults:             # applies everywhere; overridable per page and per key
  background: "#14161c"
  color: "#e6edf3"
  font_size: 13
  padding: 5
  press_feedback: true
  # font: /usr/share/fonts/liberation-sans-fonts/LiberationSans-Bold.ttf

start_page: main

pages:
  main:
    buttons:
      0:
        label: Editor
        icon: mdi:code-braces     # or a file path, relative to this file
        background: "#1f2a44"
        action:
          run: code               # a string goes through `sh -c`
      1:
        label: Note
        action:
          run: ["notify-send", "Hello"]   # a list starts without a shell
      14:
        label: "Media »"
        action:
          page: media
  media:
    background: "#241c2e"          # page-wide default
    buttons:
      0:
        label: "▶ ⏸"
        action:
          run: playerctl play-pause
      14:
        label: "« Back"
        action: back
```

### Key numbers

From the top left, row by row. On the MK.2 (5×3):

```
 0  1  2  3  4
 5  6  7  8  9
10 11 12 13 14
```

### Actions

| Action | Example | Effect |
| --- | --- | --- |
| `run` | `action: {run: firefox}` | Start a command; a string through `sh -c`, a list directly |
| `page` | `action: {page: media}` | Switch to the named page |
| `back` | `action: back` | Return to the previous page |
| `brightness` | `action: {brightness: "+10"}` | Set the brightness (`60`) or change it (`"+10"`, `"-10"`) |
| `reload` | `action: reload` | Re-read the configuration |

A state case may carry its own `action`, so a key can do different things
depending on what it currently shows.

### Icons

Two kinds of value work under `icon:`:

```yaml
icon: mdi:volume-high     # a named Material Design Icon, built in
icon: icons/code.png      # an image file, relative to the configuration file
```

**Named icons** come from the [Material Design Icons](https://pictogrammers.com/library/mdi/)
webfont (release 7.4.47, 7448 icons), which is embedded in the binary — nothing
has to be installed. Names are forgiving: `mdi:volume-high`, `mdi:volume_high`
and `mdi:mdi-volume-high` all work. They are drawn as vector glyphs, so they
stay sharp at any key size and take their colour from `icon_color`, falling
back to `color`.

Find a name with:

```bash
rustydeck icons volume     # every icon whose name contains "volume"
rustydeck icons            # all of them
```

`rustydeck check` reports names that do not exist and suggests close matches:

```
! key 3: unknown icon `mdi:volume-hi` — did you mean volume-high, volume-low?
```

**Image files** may be PNG, JPEG, GIF, BMP or WebP; they are scaled to the key
size and transparency is composited over the background colour. SVG is not read
— convert it to PNG first, for example with
`rsvg-convert -w 144 -h 144 icon.svg -o icons/icon.png`.

### Keys that follow the system

A key with a `state` block polls a command and picks its look from the first
matching case. The classic example is audio: the key shows what the sink is
actually doing, whoever changed it.

```yaml
7:
  label: Sound
  icon: mdi:volume-high             # the resting look
  action:
    run: wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle
  state:
    run: wpctl get-volume @DEFAULT_AUDIO_SINK@   # printed: "Volume: 0.73 [MUTED]"
    interval: 2                                  # seconds between polls
    cases:
      - contains: "[MUTED]"
        label: Muted
        icon: mdi:volume-off
        background: "#3a2224"
      - {}                          # no condition: always matches, goes last
```

A case may override `label`, `icon`, `action` and every styling key. Three
conditions exist, and giving several means all of them must hold:

| Condition | Matches when |
| --- | --- |
| `contains: "text"` | the output contains that text |
| `equals: "text"` | the trimmed output equals it |
| `exit: 0` | the command exited with that status |

A case with no condition at all always matches — put it last as the resting
state. Without one, a key simply keeps its last look while nothing matches, and
`rustydeck check` points that out.

The state commands run on a thread of their own, so a slow one never blocks the
keys. After a key press its state is polled again immediately, so the picture
catches up with what the press just did.

### Templates

Labels, icons and colours may contain [minijinja](https://docs.rs/minijinja)
expressions, with the state command's output available as `stdout`:

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

Leaving `cases` out gives a single catch-all case, which is all a readout needs:

```yaml
9:
  label: "{{ stdout }}"        # a clock on the deck
  font_size: 20
  state:
    run: date +%H:%M
    interval: 20
```

Available in a template: `stdout` (trimmed output), `lines` (its lines),
`exit` (the exit status) and `case` (index of the matching case). An icon can
be chosen inline — `icon: "mdi:volume-{% if 'MUTED' in stdout %}off{% else %}high{% endif %}"`
— though separate cases usually read better.

A template that fails renders as empty rather than printing its own source onto
the key, and the reason goes to the log. Before the first poll answers, the
context is empty; that is expected and not logged as a problem.

`rustydeck preview` has no live state to show, so it renders the catch-all case
and leaves templated text empty.

### Styling

`background`, `color`, `icon_color`, `font`, `font_size`, `padding` and
`press_feedback` can be given under `defaults`, per page, and per key — the most
specific value wins. Colours as `#rgb`, `#rrggbb`, or a name (`red`, `blue`,
`grey`, …).

## API mode

`rustydeck api` skips the YAML file entirely and serves the keys over HTTP, so
another program owns the deck: it sets key images and learns about presses from
an event stream. Nothing is run on the deck's behalf in this mode — what a press
means is up to the client.

```bash
rustydeck api --listen 127.0.0.1:8790 --token secret
```

It listens on localhost by default. `--token` is optional but recommended as
soon as the port is reachable from elsewhere; pass it as
`Authorization: Bearer <token>` or, for event streams, as a `?token=` parameter.

| Endpoint | Purpose |
| --- | --- |
| `GET /api/device` | Model, serial, firmware, key count and layout |
| `GET /api/keys` | What every key currently shows |
| `PUT /api/keys/{index}` | Set one key |
| `PUT /api/keys` | Set several at once: `{"0": {...}, "3": {...}}` |
| `DELETE /api/keys/{index}` | Clear one key |
| `DELETE /api/keys` | Clear every key |
| `GET /api/brightness`, `PUT /api/brightness` | Read or set it: `{"value": 60}` or `{"delta": -10}` |
| `GET /api/events` | Key presses as server-sent events |

A key is described by the same JSON in every direction:

```bash
curl -X PUT -H 'Authorization: Bearer secret' \
     -d '{"label":"Build","icon":"mdi:hammer","background":"#1f2a44"}' \
     http://127.0.0.1:8790/api/keys/0
```

`icon` takes a `mdi:` name or a path on the machine running the service;
`image` takes a base64-encoded PNG, JPEG, GIF, BMP or WebP, which spares a
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

In the browser that is `new EventSource('http://host:8790/api/events?token=secret')`.
Reading presses and writing images use separate device handles, so a slow
client never delays the other, and every stream gets a thread of its own.

## Commands

| Command | Purpose |
| --- | --- |
| `rustydeck run` | Start the service (default) |
| `rustydeck devices` | List attached decks and their firmware |
| `rustydeck init` | Write an example configuration |
| `rustydeck check` | Validate the configuration, report missing icons |
| `rustydeck preview` | Render pages as PNG (check the layout without hardware) |
| `rustydeck icons [TEXT]` | Search the built-in icons by name |
| `rustydeck install` | Write a systemd user unit |
| `rustydeck udev-rule` | Print the udev rule |
| `rustydeck api` | Serve the keys over HTTP instead of a YAML file |

`-c/--config` selects a different file, `-v` raises the log level.

## Operation

* The configuration file and the directory around it are watched: saving is
  enough, and a new icon shows up immediately. `SIGHUP` forces the same.
* State commands are polled per page — leaving a page stops its polling.
* When the deck is unplugged the service waits and reconnects as soon as it
  comes back.
* Broken YAML leaves a running service untouched — the previous configuration
  stays active and the error goes to the log.

## Contributing

Git hooks are managed with [prek](https://github.com/j178/prek), a faster
drop-in replacement for pre-commit that reads the same
`.pre-commit-config.yaml`:

```bash
cargo install --locked prek
prek install --hook-type pre-commit --hook-type pre-push
```

Committing then checks file hygiene, `cargo fmt` and `cargo clippy`; pushing
also runs the tests. `prek run --all-files` checks everything at once, which is
what CI does as well. Plain `pre-commit` works with the same file if you prefer
it.

Commit messages follow [Conventional Commits](https://www.conventionalcommits.org)
and a `commit-msg` hook enforces it:

```
feat: serve the keys over HTTP
fix: use the portable ioctl request type so musl builds
ci: track the current versions of the GitHub actions
```

Install that hook along with the others:

```bash
prek install --hook-type pre-commit --hook-type commit-msg --hook-type pre-push
```

## Releases

Versions follow the calendar: `YEAR.MONTH.MINOR`, for example `2026.8.1`, where
`MINOR` counts the releases within that month. Pushing a tag such as `v2026.8.1`
makes the CI build the binaries and packages and publish them as a GitHub
release.

## Bundled assets

`assets/materialdesignicons-webfont.ttf` and `assets/mdi-codepoints.txt` come
from [@mdi/font 7.4.47](https://www.npmjs.com/package/@mdi/font); the codepoint
table is generated from that release's CSS. The icons are licensed under
Apache 2.0 by the [Pictogrammers](https://pictogrammers.com/) — see
`assets/MDI-LICENSE`.

To move to a newer release, replace the font, regenerate the table from the
matching `materialdesignicons.css`, and update `icons::VERSION`.
