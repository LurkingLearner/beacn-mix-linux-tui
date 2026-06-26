# beacn-mix-linux

Use a **Beacn Mix** as a PipeWire channel mixer on Linux: bind a running app's
audio to one of the four channels and ride its level with the matching hardware
encoder. Encoder **press** toggles mute.

## How it works

The Beacn Mix is a *vendor-specific USB control surface* (`33ae:0004`) — **not**
a sound card. It only emits encoder deltas + button presses (and drives its
screen). So the "four channels" live on the host:

- Each channel is a PipeWire **null-sink** (`BeacnCh1..4`), looped back to your
  default output so it's audible.
- Apps are routed onto a channel's sink; the encoder rides that **sink volume**,
  giving a true per-channel fader.

All USB work is handled by [`beacn-lib`](https://github.com/beacn-on-linux/beacn-lib),
which already implements the Mix control device. This tool is the host-side glue
(event → PipeWire), driving PipeWire through `pactl`.

## Usage

```sh
cargo build --release

# Sanity check: print raw encoder/button events
./target/release/beacn-mix events

# Run the mixer (creates the 4 channel sinks, then listens to the encoders)
./target/release/beacn-mix run

# In another terminal: bind a playing app to a channel (1-4)
./target/release/beacn-mix assign

# Remove the channel sinks when done
./target/release/beacn-mix teardown
```

Bindings (app → channel) persist to `~/.config/beacn-mix-linux/bindings.json`;
the `run` daemon re-routes a bound app onto its channel whenever it reappears.
Channel volumes/mutes persist to `~/.local/state/beacn-mix-linux/levels.json`
and are restored on the next `run`.

The panel shows live channel tiles (name / level / mute). It will enter the
device's low-power screen-off when idle; the next encoder turn or button press
wakes it and redraws. Preview the layout without hardware via `beacn-mix preview`.

## Permissions

Accessing the device needs no root — a logged-in session already gets an ACL on
the USB node. To make that robust across reconnects, install the udev rule:

```sh
sudo cp 50-beacn.rules /etc/udev/rules.d/
sudo udevadm control --reload-rules && sudo udevadm trigger
```

## Status / next steps

MVP: per-channel volume + mute from the encoders, runtime assignment. Not yet
done (see `beacn-lib`'s `set_image` / `set_button_colour`): rendering channel
labels/levels to the 800×480 screen, and lighting the encoder rings.
