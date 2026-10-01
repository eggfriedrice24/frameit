# frameit

A temporary selection rectangle for Wayland. Hold the left mouse button, drag,
and a translucent rectangle follows the pointer. Release and it is gone.

The rectangle is a real compositor overlay (a layer-shell surface), so it shows
up in screen shares through PipeWire: Discord, Google Meet, OBS, and so on.
Nothing is captured, saved, or left on screen.

Built for Hyprland. Works on any compositor that offers layer-shell,
viewporter, and single-pixel-buffer, which covers wlroots-based compositors
such as sway and river.

## Install

Requires a Rust toolchain.

```sh
cargo install --path .
```

This puts `frameit` in `~/.cargo/bin`. Make sure that directory is on the `PATH`
Hyprland sees, or use the full path in the bind.

## Trigger

Hold Super+Shift, tap Z, then drag with the left mouse button while still
holding the modifiers. Release the button to dismiss. Letting go of both
modifiers before you start dragging dismisses it too, so a trigger you did
not mean leaves nothing behind. Escape or a right-click also cancels.

Bind it either way:

**In your Hyprland config.** Lua, for example in `~/.config/hypr/keybinds.lua`:

```lua
hl.bind("SUPER + SHIFT + Z", hl.dsp.exec_cmd("frameit"))
```

Classic `hyprland.conf`:

```ini
bind = SUPER SHIFT, Z, exec, frameit
```

**In frameit's config.** Set `trigger` in the config file (see below) and have
Hyprland run `frameit bind` at startup, which registers that chord through
`hyprctl`. Lua, inside your `hl.on("hyprland.start", ...)` block:

```lua
hl.exec_cmd("frameit bind")
```

Classic:

```ini
exec = frameit bind
```

A bind registered this way is a runtime bind, so if a config reload drops it,
run `frameit bind` again.

### Changing the trigger

Any chord works, with one constraint: the modifiers you are still holding when
you press the left button must not form a mouse bind of their own. Hyprland
evaluates mouse binds before passing a press to any surface, and
`SUPER + mouse:272` is the window-drag bind in most configs. That is why
Super+Z does not work as a trigger unless you move window dragging to another
chord. Super+Shift with the left button is normally free.

frameit does not know which keys launched it. It reads the modifier keys held at
the moment it gains focus and disarms when all of them are released without a
drag. With a trigger that has no modifier, such as a bare key or a mouse side
button like `mouse:276`, it stays armed until you drag, press Escape, or
right-click.

The one trigger that cannot work is the left mouse button itself. When a
focus-grabbing layer surface maps, Hyprland synthesises a release for every
button currently held and then drops the real release when it arrives, so a
tool launched on that press can never see the button go up.

## Configuration

Optional, at `~/.config/frameit/config.toml` (or `$XDG_CONFIG_HOME/frameit/`).
Every key has a default, and a bad value only prints a warning and keeps the
default, so a typo never leaves you without the tool. See
[`examples/config.toml`](examples/config.toml).

| Key | Default | Meaning |
|---|---|---|
| `fill` | `"#4287f540"` | Fill colour, `#rrggbb` or `#rrggbbaa` |
| `border` | `"#4287f5"` | Border colour, same format |
| `border_width` | `2` | Logical pixels, 0 to 64 |
| `border_radius` | `0` | Corner radius in logical pixels, 0 to 256 |
| `cursor` | `"crosshair"` | `crosshair`, `default`, or `hidden` |
| `trigger` | `"SUPER + SHIFT + Z"` | Chord registered by `frameit bind` |

Sizes are logical pixels, so the rectangle looks the same on a scale-2
monitor as on a scale-1 one. A different file can be given with
`frameit --config PATH`.

## Usage

```
frameit [--config PATH]        wait for a left-button press, then drag
frameit bind [--config PATH]   register the configured trigger with Hyprland
```

- Drag in any direction. The rectangle is always normalised.
- The rectangle stays on the monitor where the drag started and clamps to its
  edges, even if the pointer crosses to another monitor.
- A second `frameit` launched while one is running exits immediately.

## How it works

One fullscreen, fully transparent layer-shell surface per output receives the
pointer. The rectangle is eleven subsurfaces: three fill bands and four border
edges backed by 1x1 single-pixel buffers that `wp_viewporter` stretches, and
four corners backed by small textures drawn once at startup into shared memory
with the border arc and fill baked in. A redraw only updates positions and
sizes, so no pixels are written while dragging and scaling is handled by the
compositor. Redraws are paced by frame callbacks. When the button is released
the process exits and the compositor drops the surfaces.
