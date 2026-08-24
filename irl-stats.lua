-- Updates a text source with IRL Source stats.
--
-- The stats come from the plugin's "get_stats" proc handler, the same entry
-- point the obs-websocket vendor extension calls, so both report the same
-- numbers. Field names and types are documented in README.md.

obs = obslua

local last_text = nil
-- Empty means "find the source by its plugin id", which works whatever the
-- user renamed it to. Set it only when more than one IRL source exists.
local irl_source_name = ""
local text_source_name = "IRL Stats"

local IRL_SOURCE_ID = "irl_source"

function script_description()
    return "Updates a text source with IRL Source stats.\n\n" ..
        "Leave the source name empty to use the first IRL Source found."
end

function script_defaults(settings)
    obs.obs_data_set_default_string(settings, "irl_source_name", "")
    obs.obs_data_set_default_string(settings, "text_source_name", "IRL Stats")
end

function script_properties()
    local props = obs.obs_properties_create()
    obs.obs_properties_add_text(props, "irl_source_name",
        "IRL source name (optional)", obs.OBS_TEXT_DEFAULT)
    obs.obs_properties_add_text(props, "text_source_name",
        "Text source to update", obs.OBS_TEXT_DEFAULT)
    return props
end

function script_update(settings)
    irl_source_name = obs.obs_data_get_string(settings, "irl_source_name")
    text_source_name = obs.obs_data_get_string(settings, "text_source_name")
    last_text = nil
end

-- Returns a source reference the caller must release, or nil.
local function find_irl_source()
    if irl_source_name ~= "" then
        return obs.obs_get_source_by_name(irl_source_name)
    end

    -- By id rather than by display name: the source can be renamed, and the
    -- default name is localised.
    local found = nil
    local sources = obs.obs_enum_sources()
    if sources ~= nil then
        for _, source in ipairs(sources) do
            if obs.obs_source_get_unversioned_id(source) == IRL_SOURCE_ID then
                found = obs.obs_source_get_ref(source)
                break
            end
        end
        obs.source_list_release(sources)
    end
    return found
end

function update_stats()
    local source = find_irl_source()
    if not source then return end

    local ph = obs.obs_source_get_proc_handler(source)
    local cd = obs.calldata_create()
    obs.proc_handler_call(ph, "get_stats", cd)

    local buf_ms = obs.calldata_int(cd, "buffer_fill_ms")
    local speed = obs.calldata_float(cd, "current_speed")
    local ctrl = obs.calldata_bool(cd, "adaptive_latency_control")
    local reconnecting = obs.calldata_bool(cd, "reconnecting")
    local video = obs.calldata_int(cd, "total_video_frames")
    local audio = obs.calldata_int(cd, "total_audio_frames")
    local repairs = obs.calldata_int(cd, "pts_repairs")
    local silence = obs.calldata_int(cd, "silence_insertions")
    local underruns = obs.calldata_int(cd, "audio_underruns")
    local resync_skips = obs.calldata_int(cd, "audio_resync_skipped_chunks")
    local hidden_trims = obs.calldata_int(cd, "audio_hidden_trimmed_chunks")
    local quality_events = obs.calldata_int(cd, "audio_quality_events")
    local audio_flushes = obs.calldata_int(cd, "audio_decoder_flushes")
    local delay = obs.calldata_int(cd, "stream_delay_ms")

    obs.calldata_destroy(cd)
    obs.obs_source_release(source)

    local status = reconnecting and "RECONNECTING" or "LIVE"
    local text = string.format(
        "Status: %s\nDelay: %dms\nBuffer: %dms\nControl: %s\nCorrection: %.3fx\nFrames: %d/%d (v/a)\nPTS Repairs: %d\nAudio Quality: %d events\nSilence/Underruns: %d/%d\nHidden Trims: %d\nResync Skips: %d\nAudio Decoder Flushes: %d",
        status, delay, buf_ms, ctrl and "on" or "off", speed, video, audio,
        repairs, quality_events, silence, underruns, hidden_trims,
        resync_skips, audio_flushes
    )

    -- Re-rendering the text texture is the expensive part; skip it
    -- when nothing changed (e.g. while reconnecting or idle).
    if text == last_text then return end

    local text_source = obs.obs_get_source_by_name(text_source_name)
    if text_source then
        local settings = obs.obs_data_create()
        obs.obs_data_set_string(settings, "text", text)
        obs.obs_source_update(text_source, settings)
        obs.obs_data_release(settings)
        obs.obs_source_release(text_source)
        last_text = text
    end
end

function script_load(settings)
    obs.timer_add(update_stats, 1000)
end

function script_unload()
    obs.timer_remove(update_stats)
end
