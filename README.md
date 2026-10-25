# pwaudio

Terminal audio control for PipeWire, written in Rust. Manage devices, application volume, mute, default devices, stream targets and audio card profiles.

Requires Linux, PipeWire, WirePlumber, `pw-dump`, `wpctl` and `pw-metadata`. Building requires Rust 1.88 or newer.

## Installation

```sh
cargo install pwaudio --locked
```

Manual installation from source:

```sh
git clone https://github.com/pr0sp3ro/pwaudio.git
cd pwaudio
cargo install --path . --locked
```

Add `~/.cargo/bin` to your `PATH`, then run `pwaudio`.

## Controls

| Key | Action |
| --- | --- |
| `j/k`, up/down | Select a row |
| `h/l`, left/right | Adjust volume by 5 percentage points, within 0-100% |
| `m` | Mute/unmute the selected device or application |
| `M` | Toggle persistent playback mute for an application |
| `d` | Set the default output or input device |
| `Tab`, `Shift+Tab` | Switch sections |
| `Enter` | Open details or confirm a choice |
| `t` | Choose a target device |
| `T` | In details, choose a target for all application streams |
| `p` | Choose an audio card profile, affecting the entire card |
| `Esc` | Go back or cancel |
| `r` | Reload audio state and configuration |
| `q` | Quit the panel |

Applications are grouped by process binary, falling back to application name. Recording streams appear separately. Group controls affect all current streams. Details allow individual stream control. `MIXED` indicates different volume or mute values. Volume adjustments start from the group's highest volume and set a common value. New streams keep WirePlumber's volume and routing policy.

## Persistent mute

`M` saves an exact, case-sensitive rule in `~/.config/pwaudio/config.toml`, or `$XDG_CONFIG_HOME/pwaudio/config.toml`:

```toml
[[applications]]
match = "application.name"
value = "Firefox"
muted = true

[[applications]]
match = "application.process.binary"
value = "discord"
muted = true
```

Rules apply to playback, including new streams and restarted applications. They do not mute microphone capture. `m` cannot override an active rule. Disable it with `M` first, then use `m` to unmute. Saved rules remain visible when an application is inactive.

To enforce rules after closing the panel and after logging in, install the user service from the source checkout:

```sh
mkdir -p ~/.config/systemd/user
cp contrib/pwaudio.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now pwaudio.service
```

The service expects `~/.cargo/bin/pwaudio`. If installed elsewhere, change `ExecStart` before enabling it. Alternatively, run `pwaudio daemon` under your session's process supervisor. Without the daemon, rules are enforced only while the panel is open. The panel shows whether the daemon is running.

Configuration changes are detected within 500 ms. Invalid updates keep the last valid rules and report an error. View daemon diagnostics with `journalctl --user -u pwaudio.service`.

Mute is applied after a stream is detected, so a short initial sound is possible. Stream routing requires WirePlumber to allow moving streams and may be restricted by an application. The panel shows actual connections. Changing the default device follows WirePlumber policy and does not explicitly reroute every existing stream.
