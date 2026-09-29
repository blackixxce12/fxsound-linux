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
-- The volume comes back after this whatever happened to the move.
local GIVE_BACK_MS = 1500
-- A master volume at or below this is silent already: FxSound's own handover faded the stream
-- before it had it moved, or someone muted it. Nothing to fade, and nothing to give back.
local SILENT = 0.0001

-- The streams held at 0, by session item: the node, its object's id in WirePlumber, its name, the
-- volume to give back, which fade holds it, so that a timer of an earlier move does not end a
-- later one, how long after its new link the volume comes back, and whether it went away
-- meanwhile.
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

-- The stream this hook holds silent whose node is `node`, by its object's id in WirePlumber,
-- which a removed node keeps where its id in PipeWire and its properties are gone; nil when it
-- holds none.
local function held_node (node)
  for _, h in pairs (held) do
    if h.node_id == node.id then
      return h
    end
  end
  return nil
end

local function give_back (id, fade)
  local h = held [id]
  if not h or h.fade ~= fade then
    return
  end
  held [id] = nil
  if h.gone then
    return
  end
  local ok, err = pcall (function ()
    -- A volume someone else wrote meanwhile — a mixer, WirePlumber's ducking, FxSound — is
    -- theirs.
    local now = master_volume (h.node)
    if now ~= nil and now > SILENT then
      log:info (h.node, "fxsound: the volume moved meanwhile; leaving it at " .. tostring (now))
      return
    end
    ramp (h.node, h.volume, FADE_IN_MS)
  end)
  if not ok then
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
        local id = si.id
        local earlier = held [id]
        local volume = earlier and earlier.volume or master_volume (node)
        if volume == nil or volume <= SILENT then
          transition:advance ()
          return
        end
        local off_fxsound = node_name (om, si_flags.peer_id):find ("^fxsound") ~= nil
        local onto_fxsound = (target.properties ["node.name"] or ""):find ("^fxsound") ~= nil
        fades = fades + 1
        local fade = fades
        held [id] = {
          node = node, node_id = node.id, name = name, volume = volume, fade = fade,
          linked_ms = (off_fxsound and not onto_fxsound) and CLAIM_WAIT_MS or LINKED_MS,
        }
        Core.timeout_add (GIVE_BACK_MS, function ()
          give_back (id, fade)
          return false
        end)
        if earlier then
          -- Moved again before the last move gave its volume back: it is silent already.
          transition:advance ()
          return
        end
        log:info (node, "fxsound: fading " .. name .. " out for its move")
        ramp (node, 0.0, FADE_OUT_MS)
        Core.timeout_add (SETTLE_MS, function ()
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
-- at is not the stream's own. Kept, a stream closed while it is held — a player stopped, a tab
-- closed, a short sound ended — would have every later stream of the application play silent
-- while the desktop's mixer, which shows the channel volumes, says 100 %. So a held stream's
-- change to silence goes no further than this hook, and state-stream keeps the volume it had; the
-- volume given back is a change like any other, which it sees. Writing state-stream's state file
-- from here instead would not do: state-stream keeps what it saved in memory, gives that to the
-- next stream, and writes it over the file at its next save.
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
    if not held_node (node) then
      return
    end
    local ok, now = pcall (master_volume, node)
    if ok and now ~= nil and now <= SILENT then
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
      log:info ("fxsound: " .. h.name .. " went away while silent for its move")
    end
  end,
}:register ()
