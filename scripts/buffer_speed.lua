-- buffer_speed.lua — nudge playback toward the live edge, and show the speed.
--
-- Streampipe forwards this with `--script=<path>` on every mpv launch (see
-- `launch()` in src/webrtc.rs). mpv loads it alongside `--no-config`: files
-- explicitly requested on the command line are still loaded.
--
-- The control law is three bands, driven by `demuxer-cache-duration`:
--
--   cache > 1.0s            -> speed 1.05  (drain the buffer)
--   0.5s < cache <= 1.0s   -> speed 1.01  (ease off near the live edge)
--   cache <= 0.5s, or nil  -> speed 1.0   (at the edge / nothing to do)
--
-- So playback runs at most 5% fast, eases to 1% as it approaches the live
-- edge, and never slows down. mpv's default pitch correction keeps audio
-- pitch the same at these rates.
--
-- Polling starts at 1.0s and self-scales: every 1000 polls the interval is
-- multiplied by 1.1, unbounded (1.0, 1.1, 1.21, ...), so a stream of any
-- length costs ever fewer wakeups. Exactly one poll timer is pending at a
-- time — a one-shot chain, re-armed at the end of each tick.
--
-- An OSD pill showing the current speed is kept on screen the whole time,
-- top-left (mpv's default `osd-align-x/-y`). OSD messages expire, so a
-- separate 0.9s periodic timer re-raises the text with a 1.0s lifetime: the
-- refresh always lands before the old message dies, so it never blanks. The
-- refresh rate is fixed (it must not slow down with the polls, or gaps
-- appear); it is the one extra wakeup per second the display costs.
--
-- `demuxer-cache-duration` is documented as very unreliable, and can be nil,
-- so this loop is best-effort: nil maps to 1.0 and nothing crashes. Measured on
-- mpv v0.41.0: with `--cache=yes` it tracks the cache, so the bands above play
-- out as written (1.05 while the cache builds, easing to 1.01 then 1.0 as the
-- buffer drains toward the live edge). On the lowest-latency path
-- (`--no-cache --profile=low-latency`, the default) a live stream keeps only a
-- fraction of a second in the pipeline, so it reads ~0 and speed stays 1.0.

local mp = require 'mp'

local BASE = 1.0      -- base poll interval, in seconds
local FACTOR = 1.1    -- interval multiplier ...
local EVERY = 1000    -- ... applied every N polls, unbounded

local FAST = 1.05     -- over the high band
local MID = 1.01      -- between the bands
local NORM = 1.0      -- at the live edge, or no reading

local HIGH = 1.0      -- upper band edge, seconds of cache
local LOW = 0.5       -- lower band edge, seconds of cache

local OSD_REFRESH = 0.9  -- re-raise the OSD faster than its lifetime ...
local OSD_LIFE = 1.0     -- ... so a new message lands before the old dies

local polls = 0
local speed = NORM  -- what the OSD shows; the poll tick is the only writer

local function tick()
  polls = polls + 1
  -- Nil when mpv cannot estimate it (nothing playing yet, an unmeasurable
  -- pipeline); a number is the seconds of media buffered up ahead of us.
  local buf = mp.get_property_number("demuxer-cache-duration")
  if buf == nil then
    speed = NORM
  elseif buf > HIGH then
    speed = FAST
  elseif buf > LOW then
    speed = MID
  else
    speed = NORM
  end
  mp.set_property_number("speed", speed)
  -- Re-arm the next one-shot poll, lengthening it every `EVERY` polls.
  mp.add_timeout(BASE * (FACTOR ^ math.floor(polls / EVERY)), tick)
end

mp.add_timeout(BASE, tick)

-- Keep "Speed: N.NNx" in the top-left corner for the whole session: show it
-- now, then re-raise it just before each message expires. `speed` is read at
-- fire time, so the OSD always shows the latest value set by `tick`.
mp.osd_message(("Speed: %.2fx"):format(speed), OSD_LIFE)
mp.add_periodic_timer(OSD_REFRESH, function()
  mp.osd_message(("Speed: %.2fx"):format(speed), OSD_LIFE)
end)
