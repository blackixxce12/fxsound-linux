-- FxSound for Linux: smooth moves in WirePlumber.
--
-- Copyright © 2026 the FxSound for Linux contributors
--
-- SPDX-License-Identifier: AGPL-3.0-or-later
--
-- Installed by FxSound's Settings ▸ Experimental ▸ "Smooth moves in WirePlumber", and taken away
-- again by unticking it. Taken away by hand, it is this file and
-- ~/.config/wireplumber/wireplumber.conf.d/90-fxsound-fade-on-move.conf, and a restart of
-- WirePlumber (systemctl --user restart wireplumber).
--
-- WirePlumber moves a stream from one node to another by unlinking it and linking it again
-- (linking/prepare-link, linking/link-target), in the middle of the wave: the click of a device
-- picked in the desktop's sound settings, with or without FxSound. This hook fades a stream that
-- plays to silence before WirePlumber unlinks it, and gives it its volume back once its new link
-- is up. It moves the stream's master volume, with the ramp PipeWire's audio converter puts on it
-- (volumeRampTime, volumeRampStepSamples); channelVolumes, the level a desktop's mixer shows,
-- stays. WirePlumber's own ducking of media roles moves the same volume
-- (linking/rescan-media-role-links.lua).
--
-- The component is optional (90-fxsound-fade-on-move.conf): should this file be missing, or fail
-- on a WirePlumber that changed, WirePlumber says so in its log and runs without it.

if not AsyncEventHook or not SimpleEventHook then
  return
end

local lutils = require ("linking-utils")
local cutils = require ("common-utils")
local log = Log.open_topic ("s-fxsound")

-- The fade to silence, and the fade back: as FxSound's own handover of a stream
-- (crates/fxsound-audio/src/stream_handover.rs, RAMP_MS and RAMP_IN_MS).
local FADE_OUT_MS = 20
local FADE_IN_MS = 50
-- Samples a step of the ramp: not one, whose hundreds of steps leave PipeWire's converter time to
-- play a cycle at the new volume before the ramp to it (stream_handover.rs, RAMP_STEP_SAMPLES).
local STEP_SAMPLES = 8
-- From the fade's write to the move: the fade, what the stream has queued, and a quantum. 30 ms
-- left a click in 3 moves of 18; 70 ms none in 24 (roadmap 0.5.0 §7, D-power §3.3).
local SETTLE_MS = 70
-- From the new link to the fade back.
local LINKED_MS = 20
-- The same for a stream moved off FxSound's own nodes: FxSound, on, takes the default back a
-- moment later and has the stream moved back. Held silent until then, the stream's move back is
-- made in silence too; given back meanwhile, FxSound's own fade of it would meet this one's ramp,
-- which PipeWire's converter refuses and jumps instead: a click (measured at D5, −19 to −31 dBFS).
local CLAIM_WAIT_MS = 250
-- For a recorder, LINKED_MS and this much more: a recorder is handed what its source played in
-- the cycle before, so the first cycles after its new link bring it nothing yet, and a ramp given
-- back at once is spent on them; the sound then starts at full volume in the middle of a wave. As
-- FxSound's own handover waits (stream_handover.rs, RECORDER_LINKED). Given back 20 ms after its
-- link, a recorder FxSound took back after the desktop's pick clicked at −18 to −21 dBFS, two to
-- four switches in 24.
local RECORDER_LINKED_MS = 50
-- The volume comes back after this whatever happened to the move.
local GIVE_BACK_MS = 1500
-- How often a stream whose new link failed is looked at again, until it has one: WirePlumber gives
-- up on a link to a port that is replaced during the move (a recorder moved from a mono microphone
-- onto FxSound's stereo source), and FxSound moves the stream again a quarter of a second later
-- (crates/fxsound-audio/src/stranded.rs). Given back while it had no link, a recorder started at
-- full volume in the middle of a wave once it had one: −17.7 to −38.3 dBFS, 3 of 120 recorders
-- FxSound took back after the desktop's pick in the click test.
local UNLINKED_POLL_MS = 20
-- From the volume given back to the end of its ramp, with a margin for a quantum of up to 2048
-- frames: the volume is then said once more, at once, and again every CONFIRM_WAIT_MS until the
-- server reports it, CONFIRM_TRIES times at most (stream_handover.rs, RAMP_IN_DONE, CONFIRM_WAIT,
-- CONFIRM_TRIES). The converter reports a point of a ramp when its Props are sent again in the
-- middle of one, and may never report its end: seen on PipeWire 1.6.9, a stream whose last report
-- was 0.10 of the way up kept showing 0.10 — what WirePlumber keeps for the application's next
-- stream, and what the next move reads as the stream's volume.
local RAMP_IN_DONE_MS = 100
local CONFIRM_WAIT_MS = 50
local CONFIRM_TRIES = 4
-- A master volume at or below this is silent already: FxSound's own handover faded the stream
-- before it had it moved, or someone muted it. Nothing to fade, and nothing to give back. Within
-- it of each other, two volumes are the same, as WirePlumber compares them (state-stream.lua).
local SILENT = 0.0001
-- The first PipeWire whose audio converter ramps a volume (stream_handover.rs, RAMPS_SINCE).
local RAMPS_SINCE = { 0, 3, 68 }
-- The key in the default metadata that says, under a stream's id, that this hook holds it silent
-- for its move: FxSound's own handover leaves such a stream alone (stream_handover.rs,
-- Watched::hook_held), rather than take the point of this hook's fade it may read for the
-- stream's own volume.
local HELD_KEY = "fxsound.held"

-- The streams held, by session item: the node, its object's id in WirePlumber and its id in
-- PipeWire, its name, the volume to give back, which fade holds it, so that a timer of an earlier
-- move does not end a later one, how long after its new link the volume comes back, whether it
-- went away meanwhile, and where it is: "out", faded and held silent — the volume last reported in
-- `last`, and whether the fade has been played out in `played` — or "back", its volume given back
-- and said once more until the server reports it. `theirs`: someone else wrote a volume meanwhile,
-- which is theirs to keep. `polling`: it had no link when its volume was to come back, and is
-- looked at again until it has one (`await_link`).
local held = {}
local fades = 0

local function master_volume (node)
  for p in node:iterate_params ("Props") do
    local props = cutils.parseParam (p, "Props")
    if props and props.volume then
      return props.volume
    end
  end
  return nil
end

local function same (a, b)
  return math.abs (a - b) <= SILENT
end

local function node_name (om, item_id)
  local item = om:lookup { Constraint { "id", "=", item_id, type = "gobject" } }
  return item and item.properties ["node.name"] or ""
end

local function ramp (node, volume, ms)
  node:set_param ("Props", Pod.Object {
    "Spa:Pod:Object:Param:Props", "Props",
    volumeRampTime = ms, volumeRampStepSamples = STEP_SAMPLES, volume = volume,
  })
end

-- The volume, at once: to say where a ramp back has ended.
local function say (node, volume)
  node:set_param ("Props", Pod.Object {
    "Spa:Pod:Object:Param:Props", "Props", volume = volume,
  })
end

-- Whether `text`, a version as libpipewire says it, is `since` or later; nil when it does not read
-- as three numbers.
local function version_at_least (text, since)
  if type (text) ~= "string" then
    return nil
  end
  local major, minor, micro = text:match ("^%s*(%d+)%.(%d+)%.(%d+)")
  if not major then
    return nil
  end
  local version = { tonumber (major), tonumber (minor), tonumber (micro) }
  for i = 1, 3 do
    if version [i] ~= since [i] then
      return version [i] > since [i]
    end
  end
  return true
end

-- Whether the audio converter of the stream on `node` ramps a volume. The converter is in the
-- stream's own process, and of its own libpipewire: an application from an older runtime, or with
-- a libpipewire of its own older than 0.3.68, ignores the ramp and jumps to the volume written, a
-- click where the move alone makes one. Its client says which libpipewire it is (core.version); a
-- stream with no client, or one that says nothing that can be read, is taken at the word of the
-- server under this WirePlumber, which ramps: WirePlumber 0.5 needs PipeWire 1.0.
local function ramps (node)
  local client_id = node.properties ["client.id"]
  if not client_id then
    return true
  end
  local client = cutils.get_object_manager ("client"):lookup {
    Constraint { "bound-id", "=", client_id, type = "gobject" },
  }
  local at_least = version_at_least (client and client.properties ["core.version"], RAMPS_SINCE)
  if at_least == nil then
    return true
  end
  return at_least
end

-- Say in the default metadata whether this hook holds `h` silent (HELD_KEY).
local function mark (h, on)
  local ok, err = pcall (function ()
    local metadata = cutils.get_default_metadata_object ()
    if not metadata then
      return
    end
    if on then
      metadata:set (h.bound_id, HELD_KEY, "Spa:String", tostring (h.volume))
    else
      metadata:set (h.bound_id, HELD_KEY, nil, nil)
    end
  end)
  if not ok then
    log:info ("fxsound: could not say whether " .. h.name .. " is held: " .. tostring (err))
  end
end

-- The stream this hook holds whose node is `node`, by its object's id in WirePlumber, which a
-- removed node keeps where its id in PipeWire and its properties are gone; nil when it holds none.
local function held_node (node)
  for _, h in pairs (held) do
    if h.node_id == node.id then
      return h
    end
  end
  return nil
end

-- Let go of the stream held under `id`.
local function release (id)
  local h = held [id]
  held [id] = nil
  if h and h.phase == "out" then
    mark (h, false)
  end
end

-- The volume given back, said once more where its ramp has ended, and again until the server
-- reports it (RAMP_IN_DONE_MS): `tries` is how many times it has been said.
local function confirm (id, fade, tries)
  local h = held [id]
  if not h or h.fade ~= fade or h.phase ~= "back" then
    return
  end
  if h.gone or h.theirs then
    release (id)
    return
  end
  local ok, err = pcall (function ()
    local now = master_volume (h.node)
    if tries > 0 and now ~= nil and same (now, h.volume) then
      release (id)
      return
    end
    if tries >= CONFIRM_TRIES then
      log:info (h.node, "fxsound: " .. h.name
          .. " never reported its volume back; letting it go at " .. tostring (now))
      release (id)
      return
    end
    say (h.node, h.volume)
    Core.timeout_add (CONFIRM_WAIT_MS, function ()
      confirm (id, fade, tries + 1)
      return false
    end)
  end)
  if not ok then
    release (id)
    log:info ("fxsound: the stream went away before its volume was back: " .. tostring (err))
  end
end

-- Whether the stream `h` holds has a link: any, into its node or out of it.
local function linked (h)
  for l in cutils.get_object_manager ("link"):iterate () do
    local p = l.properties
    if tonumber (p ["link.input.node"]) == h.bound_id
        or tonumber (p ["link.output.node"]) == h.bound_id then
      return true
    end
  end
  return false
end

local give_back

-- The stream held under `id` had no link when its volume was to come back: look again every
-- UNLINKED_POLL_MS, and give the volume back `h.linked_ms` after a link is there. GIVE_BACK_MS
-- still gives it back whatever happens.
local function await_link (id, fade)
  Core.timeout_add (UNLINKED_POLL_MS, function ()
    local h = held [id]
    if not h or h.fade ~= fade or h.phase ~= "out" or h.gone then
      return false
    end
    local ok, has_link = pcall (linked, h)
    if ok and not has_link then
      await_link (id, fade)
      return false
    end
    h.polling = false
    Core.timeout_add (h.linked_ms, function ()
      give_back (id, fade)
      return false
    end)
    return false
  end)
end

-- Give the stream held under `id` its volume back, if `fade` still holds it: not while it has no
-- link (`await_link`), unless `always`, when GIVE_BACK_MS is up.
give_back = function (id, fade, always)
  local h = held [id]
  if not h or h.fade ~= fade or h.phase ~= "out" then
    return
  end
  if h.gone then
    release (id)
    return
  end
  if not always then
    local ok, has_link = pcall (linked, h)
    if ok and not has_link then
      if not h.polling then
        h.polling = true
        log:info (h.node, "fxsound: " .. h.name .. " has no link yet; its volume waits for one")
        await_link (id, fade)
      end
      return
    end
  end
  local ok, err = pcall (function ()
    -- A volume someone else wrote meanwhile — a mixer, WirePlumber's ducking, FxSound — is
    -- theirs. A point on the way down is this hook's own fade, last reported before its end.
    local now = master_volume (h.node)
    if h.theirs or (now ~= nil and now > h.volume + SILENT) then
      log:info (h.node, "fxsound: the volume moved meanwhile; leaving it at " .. tostring (now))
      release (id)
      return
    end
    mark (h, false)
    h.phase = "back"
    log:info (h.node, "fxsound: giving " .. h.name .. " its volume back")
    ramp (h.node, h.volume, FADE_IN_MS)
    Core.timeout_add (RAMP_IN_DONE_MS, function ()
      confirm (id, fade, 0)
      return false
    end)
  end)
  if not ok then
    release (id)
    log:info ("fxsound: the stream went away before its volume came back: " .. tostring (err))
  end
end

AsyncEventHook {
  name = "fxsound/fade-before-move",
  after = "linking/get-filter-from-target",
  before = "linking/prepare-link",
  interests = {
    EventInterest {
      Constraint { "event.type", "=", "select-target" },
    },
  },
  steps = {
    start = {
      next = "none",
      execute = function (event, transition)
        local _, om, si, si_props, si_flags, target = lutils:unwrap_select_target_event (event)
        local moving = target ~= nil and si_flags.peer_id ~= nil and si_flags.peer_id ~= target.id
        local name = si_props ["node.name"] or ""
        if not moving or si_props ["item.node.type"] ~= "stream"
            or name:find ("^fxsound") then
          transition:advance ()
          return
        end
        local node = si:get_associated_proxy ("node")
        -- A stream standing still is left alone: its ramp would wait in its process() for the
        -- next sound, and a stream that plays nothing clicks at nothing.
        if not node or node.state ~= "running"
            or cutils.parseBool (node.properties ["channelmix.lock-volumes"]) then
          transition:advance ()
          return
        end
        -- So is one whose converter has no ramp: written a volume, it would jump to it, a click
        -- of its own, and another when its volume came back.
        local has_ramp, answer = pcall (ramps, node)
        if has_ramp and not answer then
          log:info (node, "fxsound: " .. name .. " has no volume ramp; moving it as it is")
          transition:advance ()
          return
        end
        local id = si.id
        local earlier = held [id]
        if earlier and (earlier.gone or earlier.theirs) then
          release (id)
          earlier = nil
        end
        local volume = earlier and earlier.volume or master_volume (node)
        if volume == nil or volume <= SILENT then
          transition:advance ()
          return
        end
        local off_fxsound = node_name (om, si_flags.peer_id):find ("^fxsound") ~= nil
        local onto_fxsound = (target.properties ["node.name"] or ""):find ("^fxsound") ~= nil
        local records = (si_props ["media.class"] or ""):find ("^Stream/Input") ~= nil
        -- Moved again while the last move holds it silent: it is silent already. Moved again
        -- while its volume comes back, it is faded anew.
        local silent_already = earlier ~= nil and earlier.phase == "out"
        fades = fades + 1
        local fade = fades
        local h = {
          node = node, node_id = node.id, bound_id = node ["bound-id"], name = name,
          volume = volume, fade = fade, phase = "out",
          last = silent_already and earlier.last or volume,
          played = silent_already and earlier.played or false,
          linked_ms = (off_fxsound and not onto_fxsound) and CLAIM_WAIT_MS
              or (records and LINKED_MS + RECORDER_LINKED_MS or LINKED_MS),
        }
        held [id] = h
        Core.timeout_add (GIVE_BACK_MS, function ()
          give_back (id, fade, true)
          return false
        end)
        if silent_already then
          transition:advance ()
          return
        end
        mark (h, true)
        log:info (node, "fxsound: fading " .. name .. " out for its move")
        ramp (node, 0.0, FADE_OUT_MS)
        Core.timeout_add (SETTLE_MS, function ()
          local e = held [id]
          if e and e.phase == "out" then
            e.played = true
          end
          transition:advance ()
          return false
        end)
      end,
    },
  },
}:register ()

SimpleEventHook {
  name = "fxsound/fade-after-move",
  after = "linking/link-target",
  interests = {
    EventInterest {
      Constraint { "event.type", "=", "select-target" },
    },
  },
  execute = function (event)
    local id = event:get_subject ().id
    local h = held [id]
    if h then
      local fade = h.fade
      Core.timeout_add (h.linked_ms, function ()
        give_back (id, fade)
        return false
      end)
    end
  end,
}:register ()


-- WirePlumber keeps a stream's master volume for the application's next stream: its
-- node/state-stream.lua saves it each time the stream's Props change (store_stream_props_hook) and
-- gives it to the application's next stream (restore_stream_hook). The 0 this hook holds a stream
-- at is not the stream's own, nor is any point of its ramps down and back that the converter
-- reports on the way — and it may report one and never the end of the ramp. Kept, a stream closed
-- while it is held — a player stopped, a tab closed, a short sound ended — would have every later
-- stream of the application play silent, or 30 dB down, while the desktop's mixer, which shows the
-- channel volumes, says 100 %. So while a stream is held, a report of a volume below the one it
-- is to get back goes no further than this hook, and state-stream keeps the volume it had; the
-- volume given back, said once more where its ramp has ended, is a change like any other, which
-- it sees. Writing state-stream's state file from here instead would not do: state-stream keeps
-- what it saved in memory, gives that to the next stream, and writes it over the file at its
-- next save.
--
-- A report is someone else's — a mixer's, WirePlumber's ducking, FxSound's — when it is above the
-- volume to give back, or when it rises once the fade has been played out (SETTLE_MS). Not before:
-- the converter's first report of a ramp is where the ramp goes, and the points it reports on the
-- way come after it — measured on PipeWire 1.6.9, a ramp back to 0.5 reported 0.5, then 0.09,
-- 0.19, 0.28, 0.37. Theirs, it goes on to state-stream, and the hook gives nothing back over it.
SimpleEventHook {
  name = "fxsound/keep-the-silence-unsaved",
  before = "node/store-stream-props",
  interests = {
    EventInterest {
      Constraint { "event.type", "=", "node-params-changed" },
      Constraint { "event.subject.param-id", "=", "Props" },
      Constraint { "media.class", "matches", "Stream/*" },
    },
  },
  execute = function (event)
    local node = event:get_subject ()
    local h = held_node (node)
    if not h or h.theirs then
      return
    end
    local ok, now = pcall (master_volume, node)
    if not ok or now == nil then
      return
    end
    if now > h.volume + SILENT or (h.phase == "out" and h.played and now > h.last + SILENT) then
      h.theirs = true
      if h.phase == "out" then
        mark (h, false)
      end
      log:info (node, "fxsound: " .. h.name .. " was given a volume of " .. tostring (now)
          .. " while held; it is theirs")
      return
    end
    if h.phase == "out" then
      h.last = now
    end
    if now < h.volume - SILENT then
      event:stop_processing ()
    end
  end,
}:register ()

SimpleEventHook {
  name = "fxsound/gone-while-silent",
  interests = {
    EventInterest {
      Constraint { "event.type", "=", "node-removed" },
      Constraint { "media.class", "matches", "Stream/*" },
    },
  },
  execute = function (event)
    local h = held_node (event:get_subject ())
    if h and not h.gone then
      h.gone = true
      if h.phase == "out" then
        mark (h, false)
        log:info ("fxsound: " .. h.name .. " went away while silent for its move")
      else
        log:info ("fxsound: " .. h.name .. " went away while its volume came back")
      end
    end
  end,
}:register ()
