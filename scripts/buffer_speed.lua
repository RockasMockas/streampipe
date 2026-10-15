-- buffer_speed.lua — catch up on the live edge of buffer on one smooth curve.
--
-- Streampipe stages this file to the temp dir and forwards it as
-- `--script=<path>` on every mpv launch.
--
-- Set STREAMPIPE_SPEED_DEBUG=1 in the environment of the player process to get
-- a one-line trace of every evaluation (t, reason, paused, raw reading,
-- smoothed reading, interval, speed).

local mp = require("mp")
local msg = require("mp.msg")

---------------------------------------------------------------------

local LOW, HIGH = 0.2, 1.0 -- the law's operating range, in seconds
local FLOOR, CEIL = 1.0, 1.10 -- the speed is clamped to this, always
local MAX = CEIL - FLOOR -- the most speed-up there is
local ALPHA = 0.35 -- low-pass weight of a new reading
local SLEW_UP, SLEW_DOWN = 0.02, 0.20 -- speed units per second, asymmetric
local SLEW_DT_MAX = 1.0 -- the slew rate is per wall-clock second
local WRITE_EPS = 0.02 -- only write a speed the player can notice; keep speed
local BASE, SPREAD, CAP = 1.0, 120.0, 60.0 -- watchdog interval policy
local PAUSE_DT = 60.0 -- the interval while paused
local MIN_DT = 0.25 -- never arm shorter than this
local B_EPS = 0.05 -- a bhat move bigger than this is a change
local ABSURD = 3600.0 -- past an hour of buffer it is not a reading

local DEBUG = (os.getenv("STREAMPIPE_SPEED_DEBUG") == "1")

---------------------------------------------------------------------

local bhat = nil -- smoothed reading; nil = nothing to go on
local s = FLOOR -- the speed `evaluate` last computed
local s_written = FLOOR -- the speed last written to the player
local paused = false
local cache_state = nil -- idle | buffering | buffering-full | empty | full
local quiet_since = mp.get_time()
local t_prev = quiet_since -- feeds the slew limiter

---------------------------------------------------------------------

local function clamp(v, lo, hi)
	if type(v) ~= "number" or v ~= v then
		return lo
	end
	if v < lo then
		return lo
	end
	if v > hi then
		return hi
	end
	return v
end

-- Rejects nil, NaN, +-inf, negative (a negative cache makes no sense) and anything absurd.
local function usable(b)
	return type(b) == "number" and b == b and b > 0.0 and b <= ABSURD
end

local function trace(fmt, ...)
	if DEBUG then
		msg.info(fmt:format(...))
	end
end

---------------------------------------------------------------------

--- One evaluation: read, filter, target, slew, write if visible.
--- Idempotent — same inputs in, same speed out, no extra writes.
--- `reason` is only there for the trace.
local function evaluate(reason)
	local now = mp.get_time()
	if type(now) ~= "number" then
		return
	end
	local elapsed = now - t_prev
	-- A clock jump (suspend, NTP) is not elapsed time for the slew limiter:
	-- treat a missing, negative, zero, NaN or huge dt as one short tick, and
	-- cap the rest, so the slew rate is a rate per wall-clock second.
	local dt = elapsed
	if not (dt > 0.0) then
		dt = MIN_DT
	end
	if dt > SLEW_DT_MAX then
		dt = SLEW_DT_MAX
	end

	-- 1. read and condition the state we act on. `evaluate` pulls, so it is
	-- self-contained and idempotent whatever triggered it. A reading of nil,
	-- NaN, +-inf, negative or absurd means "no reading at all".
	local b = mp.get_property_number("demuxer-cache-duration")
	local is_paused = (mp.get_property_bool("pause") == true)
	if is_paused ~= paused then
		-- Readings taken across a pause are stale: the cache keeps filling (up to
		-- cache-secs / readahead) while nothing is consumed.
		bhat = nil
		paused = is_paused
	end
	if usable(b) then
		if bhat == nil then
			bhat = b
		else
			bhat = bhat + ALPHA * (b - bhat)
		end
	else
		bhat = nil
	end

	-- 2. The target. Paused, starved ("empty"), no cache ("idle") or no
	-- reading => exactly 1.00.
	local target
	if paused or bhat == nil or cache_state == "empty" or cache_state == "idle" then
		target = FLOOR
	else
		target = FLOOR + MAX * clamp((bhat - LOW) / (HIGH - LOW), 0.0, 1.0)
	end

	-- 3. slew limit: rise slow, fall fast. Every safety case asks for FLOOR,
	-- so the fast fall covers them all: the speed is back at 1.00 within half
	-- a second of the estimate going optimistic.
	local step
	if target >= s then
		step = math.min(target - s, SLEW_UP * dt)
	else
		step = -math.min(s - target, SLEW_DOWN * dt)
	end
	local new_s = clamp(s + step, FLOOR, CEIL)

	-- 4. act: write the speed only on a change big enough to be visible
	-- (churning scaletempo2 makes the audio wobble).
	local visible = math.abs(new_s - s_written) >= WRITE_EPS
	local safe = new_s <= FLOOR -- clamped, so this means == FLOOR
	if visible or (safe and s_written > FLOOR) then
		mp.set_property_number("speed", new_s)
		s_written = new_s
		-- A change is what makes the next interval short. This is the invariant:
		-- long intervals only happen while everything is flat.
		quiet_since = now
	end

	-- One line per evaluation, so the trace shows the whole
	-- curve, not just the writes. `gap` is the real time since the last one.
	-- Silent unless STREAMPIPE_SPEED_DEBUG=1.
	trace(
		"t=%.1f %-7s paused=%-5s b=%-8s bhat=%-8s gap=%.1f s=%.3f",
		now,
		reason,
		tostring(paused),
		tostring(b),
		tostring(bhat),
		elapsed,
		new_s
	)

	s = new_s
	t_prev = now
end

---------------------------------------------------------------------

mp.observe_property("demuxer-cache-duration", "number", function(_, _)
	evaluate("cache")
end)

mp.observe_property("cache-state", "string", function(_, state)
	cache_state = state
	evaluate("state")
end)

mp.observe_property("pause", "bool", function(_, is_paused)
	evaluate(is_paused and "pause" or "unpause")
end)

-- The watchdog: the one timer. Evaluate, then arm the next one-shot, longer
-- the longer everything has been quiet, bounded by CAP (PAUSE_DT when paused)
-- and never shorter than MIN_DT. It re-reads the property itself, so a missed
-- notification is picked up again at the next tick.
local function watchdog()
	evaluate("watch")
	local now = mp.get_time()
	local quiet = 0.0
	if type(now) == "number" then
		quiet = now - quiet_since
		if not (quiet > 0.0) then
			quiet = 0.0
		end
	end
	local dt
	if paused then
		dt = PAUSE_DT
	else
		dt = math.min(CAP, BASE + quiet / SPREAD)
	end
	if not (dt >= MIN_DT) or dt > CAP then
		dt = BASE
	end
	mp.add_timeout(dt, watchdog)
end

---------------------------------------------------------------------
-- Buffer jumps: hop to the live edge, or back to the oldest frame still
-- in the cache, so the user can scrub the buffered window by hand.
--
-- Every jump is a relative `seek` by the distance from the play position.
-- What breaks buffering is a seek whose demuxer target lands outside the
-- cache's seekable range: mpv then does a low-level seek on the RTP stream
-- and throws the whole cache away. Two traps:
--  * the seekable range ends at the newest *completed* GOP. Packets after
--    the newest keyframe are cached but are not seek points, so the real tip
--    (`cache-end`) is up to a GOP past the end of the range;
--  * the start of the range moves forward as old packets are pruned.
-- The edge jump is an exact seek to tip - EDGE_MARGIN, with
-- hr-seek-demuxer-offset pulling the demuxer half of it back inside the
-- range: the demuxer lands on a cached keyframe and the decoder discards
-- frames up to the target. The start jump is a keyframe seek a little past
-- the start of the range, which snaps back to the first cached keyframe.
-- The edge keeps a margin back — landing right on the tip stalls the instant
-- the next packet is late.

local EDGE_MARGIN = 0.5 -- stay this far inside the live edge
local START_MARGIN = 0.5 -- aim this far past the cache start, in case it moves
local DEMUX_EPS = 0.05 -- keep the demuxer seek this far inside the range end

-- The seekable window [lo, hi] and the newest cached timestamp (tip), on the
-- same clock as playback-time. nil when nothing is seekable.
local function cache_bounds()
	local st = mp.get_property_native("demuxer-cache-state")
	local ranges = (type(st) == "table") and st["seekable-ranges"]
	if type(ranges) ~= "table" or #ranges == 0 then
		return nil
	end
	local lo, hi
	for _, r in ipairs(ranges) do
		local s, e = r["start"], r["end"]
		if type(s) == "number" and s == s and type(e) == "number" and e == e then
			if lo == nil or s < lo then
				lo = s
			end
			if hi == nil or e > hi then
				hi = e
			end
		end
	end
	if lo == nil then
		return nil
	end
	local tip = st["cache-end"]
	if type(tip) ~= "number" or tip ~= tip or tip < hi then
		tip = hi
	end
	return lo, hi, tip
end

local function play_pos()
	local pos = mp.get_property_number("playback-time")
	if type(pos) ~= "number" or pos ~= pos then
		return nil
	end
	return pos
end

-- hr-seek-demuxer-offset is only read when a queued seek executes; the
-- `seek` event comes after that, so that is where the old value goes back.
local saved_hr_offset = nil

local function restore_hr_offset()
	if saved_hr_offset ~= nil then
		mp.set_property_number("hr-seek-demuxer-offset", saved_hr_offset)
		saved_hr_offset = nil
	end
end

mp.register_event("seek", restore_hr_offset)

local function jump_to_edge()
	local pos = play_pos()
	local lo, hi, tip = cache_bounds()
	if pos == nil or lo == nil then
		msg.warn("buffer position unavailable")
		return
	end
	local target = tip - EDGE_MARGIN
	if target - pos < DEMUX_EPS then
		return -- already at the edge; never seek backwards
	end
	if saved_hr_offset == nil then
		saved_hr_offset = mp.get_property_number("hr-seek-demuxer-offset", 0)
	end
	-- The demuxer seeks to target - offset = min(target, hi) - DEMUX_EPS.
	mp.set_property_number("hr-seek-demuxer-offset", math.max(0, target - hi) + DEMUX_EPS)
	local _, err = mp.command_native({ name = "seek", target = target - pos, flags = "relative+exact" })
	if err ~= nil then
		msg.warn("edge jump failed: " .. tostring(err))
		restore_hr_offset()
	end
end

local function jump_to_start()
	local pos = play_pos()
	local lo = cache_bounds()
	if pos == nil or lo == nil then
		msg.warn("buffer position unavailable")
		return
	end
	local target = lo + START_MARGIN
	if pos <= target then
		return -- already at the start
	end
	local _, err = mp.command_native({ name = "seek", target = target - pos, flags = "relative+keyframes" })
	if err ~= nil then
		msg.warn("start jump failed: " .. tostring(err))
	end
end

-- The user's input.conf is loaded (see `Player::command`). forced: the two
-- jump keys win over it. END/HOME are plain bindings: they replace mpv's
-- built-in END/HOME (seek to end/start of file, useless on a live buffer),
-- but the user's own END/HOME binding, if any, wins.
mp.add_key_binding("ctrl+shift+RIGHT", "buffer-edge", jump_to_edge, { forced = true })
mp.add_key_binding("END", "buffer-edge-end", jump_to_edge)
mp.add_key_binding("ctrl+shift+LEFT", "buffer-start", jump_to_start, { forced = true })
mp.add_key_binding("HOME", "buffer-start-home", jump_to_start)

mp.add_timeout(BASE, watchdog)
