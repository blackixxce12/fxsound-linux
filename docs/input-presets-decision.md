# Input presets: what to ship, and with what numbers

Decided 2026-09-20 from two surveys — what comparable products ship as named voice profiles,
and which microphone situations the planned set serves badly. Tracker item `s3-05`.
The source material for the first seven is in `voice-presets-research.md` beside this file.

## Verdict

Eleven, and the four additions are two tone-pair repairs, one acoustically forced case and one reference — not four new ideas.

The set, on the five axes it actually navigates: **Reference** — Flat, Clean Voice. **Destination** — Streaming, Podcast, Voice Chat, Broadcaster. **Tone** — Warm Voice, Bright Voice. **Source/repair** — Laptop Mic, Headset. **Capture** — Studio.

Why not seven. Two of the seven are not really settled: Laptop Mic has no row in the draft table and no reviewed numbers, and Warm Voice was never reviewed at all — so the claim that the seven cover the common cases rests partly on presets nobody has written down. Beyond that, exactly two gaps survive scepticism. Shipping Warm without Bright is an asymmetry five independent vendors contradict, and it is the cheapest possible fix. And the entire set is voiced for a microphone at desk distance while the largest acoustic variable in the input problem — proximity effect, up to +16 dB — is eight times bigger than any EQ move in the table; a headset user gets a preset built for a condition they are nowhere near. Broadcaster is the one market-driven addition (four vendors, three occurrences inside Blue VO!CE alone) and it is the weakest of the four, shipped with a kill condition: if the mic-side distinctness test collapses it into Streaming, drop Broadcaster.

Why not fourteen or more. Five of the nine cases the researchers surfaced are not preset-shaped at all, and shipping presets for them would encode a bug as a product feature: Noisy Room would ship a high gate threshold as the fix for noise during speech; Narrowband would ship two numbers where the honest fix is a Nyquist guard that makes all eleven presets degrade honestly; Room Echo and Game Voice Chat name effects this chain structurally cannot produce; Conference addresses the leveling third of a problem that is mostly reverberation. Conference was the closest reject — real, uncovered, expressible — and it loses because no vendor ships it, the chain fixes the least important part of it, and the near/far talker problem is one of the cases the brief correctly guessed may be outside what one gate and one compressor can do.

Eleven is defensible against the market: Blue VO!CE ships about eight named presets plus a community browser, Shure MOTIV shipped five, RØDECaster Pro II ships three. Eleven is at the top of that range, which is why Flat and Broadcaster both carry explicit conditions and why nothing ships before the mic-side analogue of shipped_presets.rs exists.

Three things must land before any preset numbers are frozen, and all three are cheap now and expensive later, because the input chain is still almost unbuilt (only mod.rs and limiter.rs exist; InputDspParams is not written): gate range_db, a named compressor detector with its window, and RNNoise as a per-preset field moved to the FRONT of the chain — which forces re-voicing every gate threshold in the draft table, since all of them were chosen against an un-denoised floor. None of this goes near crates/fxsound-preset/src/lib.rs; the .fac byte-exact contract and the shipped 34 output presets are untouched.

## Ship

### Bright Voice  
*confidence: high*

**Why.** The clearest asymmetry in the planned seven. Warm/bright ship as a PAIR in every product that ships either: Blue VO!CE 'Warm and Vintage' + 'Crisp and Modern'; Shure MV7+ Dark/Natural/Bright; RØDE Tone Deep/Medium/High plus the Pro II 'Sparkle' macro; SteelSeries 'Deep Voice' vs 'Balanced'; Elgato 'High Clarity'. Five independent vendors, and the set currently ships only the warm end, so a user whose voice is already chesty (or whose mic is dull) has nowhere to go but Clean Voice. It is also the cheapest addition in the set: it is Warm Voice mirrored about zero, no new stage, no new field.

**Settings.** HPF 90 Hz, 2nd order. Gate -45 dB, 2.0:1, range -14 dB. Compressor -18 dB, 3.0:1, 15/150 ms, RMS. Makeup +5.0 dB. De-esser 5500 Hz, -26 dB. Ceiling -3.0 dBFS. RNNoise off. EQ (62.5 / 115.734 / 214.311 / 396.85 / 734.867 / 1360.79 / 2519.84 / 4666.12 / 8640.48 / 16000): 0, -1.5, -1.0, -0.5, 0, 0, +1.5, +1.5, +1.0, 0. Two deliberate asymmetries against a literal mirror of Warm Voice: band 1 is -1.5 not -2.0 (cutting a male fundamental thins more than boosting one fattens), and band 9 (16 kHz) stays at 0.0 rather than mirroring Warm's -2.0, because on a microphone 16 kHz is hiss and on most capture rates that band is at or above Nyquist. The de-esser threshold MUST be 4 dB hotter than Clean Voice's -22 dB: bands 7 and 8 feed energy straight into the detection band, so for this one preset the EQ gains and the de-esser threshold are not independently tunable.

### Headset (close boom mic, 2-5 cm)  
*confidence: high*

**Why.** Not a market finding — forced by acoustics, which is the other bar the brief sets. The existing research already concedes the number that decides it: proximity effect is worth up to +16 dB, and every one of the seven was voiced around desk distance. A 16 dB low-frequency swing is eight times larger than the ±2 dB any EQ move in the draft table makes, so no re-tuning inside one preset can straddle both distances, and the user population (gamers, callers, anyone on a headset) is the largest single group the input feature will have. Laptop Mic does not cover it: that is the far-field, thin, noisy case; this is the near-field, boomy, plosive case. They are opposite ends of the same axis.

**Settings.** HPF 120 Hz, 4th ORDER (the general case chose 2nd order to protect an 80 Hz male fundamental; at a mouth-adjacent capsule that concern inverts and plosive energy 10-20 dB above speech sits below 150 Hz). Gate -38 dB, 2.0:1, range -12 dB (the wanted signal is far hotter relative to the room, so the threshold can safely rise). Compressor -18 dB, 3.0:1, 10 ms attack / 150 ms release, RMS — the fast attack is for close-in transients, not for level. Makeup +3.0 dB (hot source; +6 would be wrong here). De-esser 5500 Hz, -26 dB, i.e. 4 dB hotter than Clean Voice. Ceiling -3.0 dBFS. RNNoise off by default (close-mic SNR is already good). EQ: 0, -2.5, -1.5, -0.5, 0, 0, +1.5, 0, 0, 0. Band 1 (115.734 Hz, span 85-157 Hz) is the only genuinely effective LF control in the chain and carries the proximity cut. Document Clean Voice, Podcast, Warm Voice and Bright Voice as desk-distance presets once this ships.

### Flat  
*confidence: medium*

**Why.** The reference slot: protection with no voicing, so the user can hear what the other ten are actually doing. Recurs as a named entry in three products plus the community consensus — Shure MOTIV 'Flat', RØDECaster Pro II 'Neutral', Discord's 'None', and every OBS guide's advice to hear the raw mic first. Distinct from the app's own input on/off toggle, which is the obvious objection: with the chain disabled the app is out of the path entirely, so there is no high-pass and no limiter. Flat is 'nothing above the rumble and nothing below the ceiling', which is a real and common want for someone with a good microphone. It is one row and ten zeros.

**Settings.** HPF 75 Hz, 2nd order. Gate OFF. All ten EQ bands 0.0 dB. De-esser OFF. Compressor OFF (not 'gentle' — off). Makeup 0.0 dB. Limiter -3.0 dBFS. RNNoise off. Ships only if Studio is revoiced as below; as currently drafted Flat and Studio differ by one gentle compressor, which is exactly the duplicate-voicing defect the output-side shipped_presets.rs was written to catch.

### Broadcaster  
*confidence: medium*

**Why.** The most-repeated single name in the market: Blue VO!CE ships 'Broadcaster 1', 'Broadcaster 2' AND 'Classic Radio Voice'; RØDECaster Pro II ships 'Broadcast' as one of only three presets; GoXLR profiles are sold as broadcast profiles; the whole hardware lineage (dbx 286s, Aphex 230, Symetrix) is marketed as broadcast voice processing. Four independent vendors, three occurrences inside one vendor's set. It is a processing-DEPTH case, which is why Podcast does not cover it, and its EQ shape — chest lift plus scooped low-mid plus presence — is unoccupied by anything else in the set (Warm lifts the chest but cuts presence; Streaming lifts presence but with a bright tilt and no chest). Lowest-confidence addition, and it carries an explicit kill condition: its dynamics sit within half a ratio point and 2 dB of Streaming's, so if the mic-side distinctness test collapses the two, drop Broadcaster, not Streaming.

**Settings.** HPF 70 Hz, 2nd order (keep the chest). Gate -42 dB, 2.0:1, range -16 dB. Compressor -22 dB, 4.5:1, 30 ms attack / 250 ms release, RMS — the slow attack is what makes it read as 'radio' rather than 'squashed'. Makeup +10.0 dB. De-esser 6000 Hz, -26 dB (tight, because the presence lift exposes sibilance). Ceiling -3.0 dBFS. RNNoise off. EQ: 0, +2.0, 0, -1.0, -0.5, -1.5, +2.0, 0, 0, 0. This is the one preset allowed past the set's ±2 dB restraint and the only one where the user picking it wants to hear the difference; holding it to Clean Voice's restraint makes it a Podcast duplicate. Note the 396.85/734.867 cuts are softened against the researcher's proposal: +2.0 at 115 Hz plus deep cuts at 396 Hz hollows the voice, so the scoop lives at 1360.79 alone.

### Studio, revoiced as the capture preset  
*confidence: high*

**Why.** Not an addition — a repair, and the precondition for Flat. As drafted, Studio is Flat with a 2:1 compressor, which is two presets for one voicing. Two independent findings give it a real identity instead. First, it is the preset a singer or anyone recording into Ardour/Reaper/Audacity will reach for, and its 75 Hz corner cuts a bass singer's fundamental: sung fundamentals reach E2 at 82.4 Hz and C2 at 65.4 Hz, well below the ~85 Hz speech floor that justifies 75-80 Hz everywhere else. Second, the -3.0 dBFS ceiling is justified by lossy-codec overshoot (Opus, AAC), and a recording path is not lossy-encoded, so there the limiter should be a safety catch, not a working ceiling — and an editor doing their own gain staging wants unity makeup.

**Settings.** HPF 40 Hz, 2nd order (or off). Gate OFF. Compressor -18 dB, 2.0:1, 25 ms / 400 ms (release lengthened from 200 ms: singing's measured dynamic range is 33.60 dB against 30.95 dB for speech and crest factor runs 16-20 dB against ~12 dB), RMS. Makeup 0.0 dB (was +1.5). De-esser OFF. Ceiling -1.0 dBFS sample-peak (was -3.0). All ten EQ bands 0.0 dB. RNNoise off. This costs the documentation's third headline finding a stated exception: 'every preset ships a -3.0 dBFS ceiling' becomes 'every preset except Studio, and here is why'. Dropping the corner to 40 Hz also lets desk rumble into the compressor detector — acceptable in the preset that already has gate and de-esser off and is explicitly the least-processing option, unacceptable as a general default.

### Rename Discord -> Voice Chat, and fix its high-pass corner  
*confidence: medium*

**Why.** No comparable product names a voice profile after a destination app — they name by role ('Podcast Studio', 'Broadcast'), by tone ('Crisp and Modern', 'Bright') or by source ('Speech', 'Acoustic Instrument'). Krisp integrates with Discord and Zoom and still ships no Discord profile. The name dates badly and implies a codec-specific tuning the preset does not have: the -3 dBFS ceiling is the Opus/AAC headroom argument and it applies to Teams, Zoom and in-game voice identically. Carry the app names in the description line, not the preset name, so discoverability does not suffer. Explicitly NOT extending this to Laptop Mic: 'Laptop Mic' names a source, which is the vendor-sanctioned axis, and it is more informative than 'Small Mic' now that the headset case has its own preset.

**Settings.** Keep the drafted Discord numbers, with one correction: HPF 80 Hz, not 90. At 90 Hz the preset cascades with the VoIP stack's own 2nd-order 100 Hz Butterworth (WebRTC measured -3.54 dB @100 Hz) for about -7.1 dB at 100 Hz on a deep male voice — already on the objection list and cheapest to fix now. Otherwise: gate -45 dB / 2.0:1 / range -14 dB; compressor -18 dB, 3.0:1, 20/150 ms, RMS; makeup +4.0 dB; de-esser 6000 Hz, -20 dB; ceiling -3.0 dBFS; EQ 0, -2.0, -1.0, -2.0, -1.0, 0, +2.0, 0, 0, 0.

### Close the parameter set before any preset is written down  
*confidence: high*

**Why.** Both new presets above are literally unspecified without this, and the input chain does not exist in code yet (crates/fxsound-dsp/src/input/ holds only mod.rs and limiter.rs; InputDspParams is not written), so these are design-in costs now and retrofit costs later. Three items, all already raised by the existing reviewers. The gate has no range cap, so it pumps the floor in and out; every reference implementation surveyed (LSP, Calf, OBS) caps it. The compressor's detector is unspecified, and peak vs RMS against the same threshold differ by 3-7 dB of gain reduction, which means Broadcaster's 4.5:1 at -22 dB means two different things. And RNNoise must be a per-preset field, not only a global switch, because a preset that ignores the denoiser is describing half its own sound.

**Settings.** On InputDspParams: gate `range_db: f32`, default -14.0, per preset (-12 Headset, -16 Broadcaster, gate off entirely on Flat and Studio). Compressor `detector: Detector` with variants Peak and Rms { window_ms }, default Rms { window_ms: 10.0 } — every threshold in the draft table and above is an RMS threshold, say so in the asset header. `rnnoise: bool` per preset, with a defined fallback when the model is absent: degrade to the gate and log once, never fail the preset load; and because a preset can now move a control the user set deliberately, the UI must show that the selection changed it. Move RNNoise FIRST in the chain, ahead of the gate (5 of 9 surveyed community presets put denoise first, none put it after the limiter) — this forces re-voicing every gate threshold in the draft table, since all of them were chosen against an un-denoised floor. Do all of it in crates/fxsound-dsp/src/input/mod.rs; none of it goes near crates/fxsound-preset/src/lib.rs, whose only job is byte-exact .fac round-tripping.

### Nyquist guard on the EQ bands and the de-esser detector  
*confidence: high*

**Why.** This is what replaces a Narrowband preset, and it is the most concretely broken thing either researcher found. HFP capture is 8 kHz with CVSD and 16 kHz with mSBC; even LC3-SWB is 32 kHz mono. At 16 kHz capture, bands 8 (8640.48) and 9 (16000) are at or above Nyquist and do nothing. At 8 kHz, bands 7, 8 and 9 are dead AND the de-esser detector at 5500-6000 Hz is above Nyquist, so the de-esser is silently inert in five of the seven planned presets. The users hitting this are the ones with the worst microphones and the least ability to diagnose it, and they are currently handed phantom controls. Once the guard exists, every preset degrades honestly on a narrowband link and a dedicated Narrowband preset earns almost nothing.

**Settings.** Read the capture node's rate at stream setup and on format change. Grey out (and neutralise) any EQ band whose centre is >= Nyquist, with a tooltip naming the capture rate. When the de-esser detector frequency is >= Nyquist, disable the de-esser rather than letting it run inert, and say so in the UI. Note for the record that band 9 at 16000 Hz sits exactly at Nyquist even on a 32 kHz link, which is a second reason Bright Voice leaves it at 0.0 dB.

### Mic-side analogue of shipped_presets.rs, before any of the above ships  
*confidence: high*

**Why.** The gate on Bright Voice and Broadcaster specifically. At the derived Q of about 1.6 (ten bands), adjacent bands sum past what either band asks for — that is the exact defect the output-side test already measures. Bright Voice lifts bands 7, 8 and 9 together and Broadcaster lifts band 1 while cutting band 5, so both are the case where summing matters most. The test is also what enforces the Flat/Studio and Broadcaster/Streaming separations rather than leaving them to assertion: it is the mechanism that would have caught 'two presets that are the same voicing under different names' on the output side, and the input set is now eleven presets deep.

**Settings.** Impulse through the input GraphicEq, realfft, the 29 third-octave points already tabulated at crates/fxsound-dsp/tests/shipped_presets.rs:167, pairwise worst-case dB difference across all eleven presets with a stated minimum margin. Add two checks the output set does not need: effective per-band gain versus stored per-band gain (catches the Q-summing overshoot), and a full-chain distinctness pass that includes HPF corner/order, compressor ratio+threshold+detector and de-esser frequency+threshold, so Broadcaster vs Streaming and Flat vs Studio are measured, not assumed. Keep it entirely out of the .fac path.

## Rejected, and why

Keeping this list is the point of having made it: each of these looks reasonable until the
reason it fails is written down, and without that they get proposed again.

- **Conference / several talkers at one microphone** — Real case, but it fails two of the three bars. No vendor ships a conference voice preset — conference systems solve it with auto-mixing and AGC, not with a named profile — and the chain can express only the leveling third of the problem. The dominant complaints in a meeting room are reverberation and HVAC, and one broadband compressor with a 600 ms release does nothing about either; it would ship as a preset that claims to fix the conference case and fixes the least important part of it. The finding worth keeping is the negative one: a gate thresholded for the near talker deletes the far talker entirely, which is why Flat and Studio being gate-off matters, and why range_db exists.

- **Narrowband / Bluetooth-headset preset** — The arithmetic behind it is correct and important, but it argues for the runtime guard, not for a preset. Once bands above Nyquist are neutralised and the de-esser auto-disables, every preset behaves honestly on an HFP link and the remaining Narrowband-specific deltas are two numbers (makeup +3, HPF 120). Two numbers is not a voicing. Shipping both the guard and the preset would also mean the preset's own presence lift duplicates what Clean Voice already does in the surviving bands.

- **Noisy Room** — It would ship the failure mode as the fix. A gate is a time-domain switch: it does nothing about noise DURING speech, which is the actual complaint. With the floor at -30 dB the threshold must climb to about -25 dB, roughly 10 dB below conversational peaks, so it chatters on every word onset and clips quiet sentence-final syllables. Every product that serves this case ships it as a SWITCH and never as a named voice — NVIDIA 'Noise Removal', Krisp, Discord's Krisp mode, SteelSeries ClearCast, Elgato Voice Focus, RNNoise first in every OBS chain. The gain lives in stage order and a missing field, both of which are in the ship list.

- **Room echo / de-reverb preset** — The chain cannot do it. No combination of one high-pass, one gate, ten EQ bands, one de-esser and one compressor removes reverberation, and a spectral or ML de-reverb stage needs an FFT with a frame buffer, which breaks the input chain's allocation-free-after-construction contract. Naming a preset 'Room Echo' when nothing in the path can touch reverb is a preset claiming an effect it does not get — the exact defect the output-side tests exist to catch. The case is real (NVIDIA, Krisp, Adobe all advertise it second after noise) and it is not a preset in any of them either.

- **Game voice chat with the game loud in the room** — Categorically not preset-shaped. The dominant problem is acoustic echo, and cancelling it requires an adaptive filter fed with the PLAYBACK signal as a reference, which no stage in this chain has access to. On Linux the answer is libpipewire-module-echo-cancel with the WebRTC AEC. Worth logging separately as an architectural opportunity — FxSound Linux owns both the playback and the capture path and is unusually well placed to wire that reference up — but not as a row in the combo box. For the majority case (headphones) there is no echo at all and it collapses into Voice Chat plus keyboard clatter, i.e. the RNNoise switch.

- **Singing / Acoustic Instrument / Loud (the MOTIV source-type modes)** — Fails the scepticism test outright: these appear in exactly one vendor's lineup (Shure MOTIV, MV5/MV51/MV88) and that vendor dropped them for the MV7/MV7+ in favour of the Dark/Natural/Bright tone axis. One vendor, who abandoned the taxonomy. A Singing preset with gate and de-esser off is also Flat under a different name — and the one genuinely distinct parameter, a low high-pass corner for sung fundamentals down to C2 at 65.4 Hz, is already absorbed by the Studio revoicing above.

- **Character presets — AM Radio, Megaphone, Telephone** — Different product category, and unreachable anyway. The chain has no low-pass stage, so the result would be a thin voice rather than a canned one, and adding a low-pass to serve a novelty is the wrong trade. Blue VO!CE does ship 'AM Radio' and Voicemod's whole 'Devices' category is megaphones and vintage radios, but in both products these live in a separate EFFECTS section next to pitch and formant shifting, explicitly not among the broadcast presets — Blue's own guide says the two sets do not always match up.

- **Mic-type presets — Dynamic / Condenser / Headset-by-gain** — Conflates gain staging with voicing. Three presets whose ten EQ gains are identical and whose only real difference is makeup_db is the duplicate-voicing defect by construction, and it multiplies the set by three for a value one control sets. Everywhere the case appears — RØDECaster's Dynamic/Condenser/RE-20, GoXLR's Dynamic/Condenser/Jack, Wave XLR — it is a HARDWARE input selection (phantom power, preamp gain, impedance), not a voice profile. On a Linux port with no control over the interface, the honest equivalent is an input-gain control and a level meter. Note this is distinct from the Headset preset shipped above, which is justified by capsule DISTANCE (a +16 dB proximity swing and a plosive problem), not by transducer type.

- **Language-specific sibilance (Russian, Polish) as a preset — and the 7000-7500 Hz default shift** — The preset is rejected because users do not self-identify by sibilant inventory and will never pick a 'Russian' preset. The underlying phonetics is sound — Polish has a three-way and Russian a four-way sibilant contrast against English's two, Polish adult dental /s/ is reported with CoG around 8000 Hz and above while the retroflex and alveolo-palatal sit distinctly lower, so a 5500 Hz detector sits in the valley between the two modes. But the proposed remedy, raising the detector default toward 7000-7500 Hz across the set, is also rejected FOR NOW: that figure is extrapolated from Polish, per-phoneme Hz values for Russian were never retrieved, and moving it would invalidate every de-esser threshold in the draft table (less energy reaches the detector, so -22 to -25 dB stops meaning what it meant). Keep the detector frequency as the per-preset field it already is, keep 5500/6000, and log the measurement as a task: retrieve the Kochetov table or measure a handful of Russian speakers, then decide. Do not defend an inferred number in a shipped preset.

- **A Dark / Neutral / Bright tone ladder applied over the loaded preset** — Right observation, wrong project phase. Three vendors converged on tone-over-destination (Shure, RØDE, RØDECaster's Depth/Sparkle/Punch macros), which is real evidence about how people navigate — but it breaks the 'a preset is a complete stored state' model the whole port rests on, makes the effective EQ curve a sum of two stored things (so no test can assert that a preset's audible curve equals its stored curve), and two summed curves on a fixed ladder at derived Q ~1.6 is precisely how bands add past what either asks for. It is a UI-contract change, not a preset addition. Revisit after the input set has shipped and been listened to.

- **A separate Recording preset** — Distinct in intent, already served in substance. The delta against Studio was two numbers — the ceiling and the makeup — and both are folded into the Studio revoicing above. A twelfth row that differs from the eleventh by 2 dB of makeup and 2 dB of ceiling is the duplicate case.

- **Voice-pitch presets (deep male ~85-100 Hz vs high ~200-250 Hz)** — A no-op. The measured numbers already settle it: male F0 runs 85-180 Hz and female 165-255 Hz, the 80 Hz corner sits deliberately below the male floor, and at Q=1.5976 band 1 spans 85-157 Hz (male fundamental) while band 2 spans 157-292 Hz (female), which is why band 0 is 0.0 dB everywhere. A fixed 80 Hz high-pass demonstrably serves both, and users cannot self-identify by fundamental frequency anyway. The one genuine pitch failure — the Discord/WebRTC high-pass cascade — is a per-preset corner and is fixed in the Voice Chat entry above.

- **Rename Laptop Mic -> Small Mic** — Half of the rename proposal, and the half that is wrong. The vendor argument is against DESTINATION names (Discord), not against source names — 'Speech' and 'Acoustic Instrument' are source names and they are exactly what vendors do ship. 'Laptop Mic' tells a user instantly whether it is for them; 'Small Mic' does not, and its stated coverage of headset booms is now served by a preset voiced for that distance rather than by a vaguer name. Keep Laptop Mic, and write its numbers down — it is currently the only one of the seven with no row in the draft table and no reviewed values at all.


## Revisited for 0.4.0: the denoiser has a level, and three cases become presets

Decided 2026-09-22 against `docs/0.4.0-design.md` §2–3 and §7. The set goes from ten to thirteen,
and the reason is not that the rejections above were wrong. They were right about the chain as it
was: a denoiser that is a switch, a gate that hears only level. Two things changed underneath them.

**The `[denoise]` table.** RNNoise is no longer on or off; it is a control surface — a floor on
the network's band gains, an attenuation of frames the network calls noise, a share of the gap to
unity handed back in proportion to how sure it is of a voice — with three rows, `light`, `medium`
and `strong`, and a channel mode, `mono`, `linked` or `independent`. A preset names the row and
the mode in a `[denoise]` table and may override any of the four numbers beside them. Everything
the format promised still holds: the table is optional, a stage that is off has no table, and
`rnnoise` stays in every file as the master switch a 0.3.0 binary reads — written in step with
the table, so both binaries hear the same stage. A 0.3.0 file with `rnnoise = true` and no table
is the `medium` row with one network per channel, which is what that version did; the
distinction is enforced by a test, because `Laptop Mic` must sound after the upgrade as it did
before. `Laptop Mic` now says `medium` explicitly. `Streaming` and `Podcast` gain the `light` row
— a 12 dB floor with most of the voice handed back, the row for a good microphone in an ordinary
room where the full network's smearing of a quiet consonant would cost more than the hiss it
removes. Their gate thresholds are unchanged: a floor lowered by twelve decibels is still under
them.

Three keys ride with it. `[deesser] mode = "adaptive"` lets the corner follow the source's
bandwidth, which is the Nyquist guard above made a preset choice. `[dereverb] level` names the
late-reverberation suppressor the design adds after the denoiser; no shipped preset turns it on,
because a reverb is a room's property and not a microphone's, and it belongs to the setting. And
`vad_gate = true` lets the network's voice probability hold the gate open, which is the field the
two rejections below turned on.

**Noisy Room** — *confidence: medium.* Rejected above because "a gate is a time-domain switch: it
does nothing about noise during speech", and a threshold high enough to hold a −30 dB floor
chatters on every word. Both objections stand, and neither is what the preset now does. The work
is in the table: `strong` — the network's whole opinion, nothing handed back — and `mono`, one
network on the downmix and the same signal to every channel, because a room's noise is not an
image worth keeping. The gate then sits at −40 dB against a *denoised* floor with `vad_gate`
holding it open while the network hears a voice, and a −20 dB range so what is left of the room
comes and goes gently. The vendors that ship this case as a switch still ship it as a switch; here
the switch has a row, and the row has a gate voiced for it, and that is a preset. HPF 120 Hz
fourth order for the fan's fundamental; −1.5 dB at 200 Hz, +2 dB at 2.5 kHz; makeup +7; the rest
as Clean Voice.

**Mechanical Keyboard** — *confidence: medium.* The opposite problem from Noisy Room: transients,
not a floor. A key click is a millisecond of energy an RMS detector barely sees and a 150 ms
release lets ring, so this is the one preset in the set with a `peak` gate — 1 ms attack, 60 ms
release, 40 ms hold, 4:1, −24 dB range, closed between words and closed fast, with the voice
probability keeping it open through a sentence so the fast release does not chop the ends of
words. `strong` and `independent`: the network knows keyboards, and a desk microphone's stereo
image is worth keeping. HPF 100 Hz second order; +2 dB at 2.5 kHz; makeup +5.

**Gaming Headset** — *confidence: medium-high.* Headset's near-field case with the room switched
on, and the reason it is not Headset with `rnnoise = true`: it is `linked`. A headset that captures
in stereo must not have its two sides disagree about what is voice, so one network analyses the
downmix and its mask is applied to each channel through that channel's own transform. `medium`,
`vad_gate`, a −18 dB range because what the gate closes on is keys rather than room tone, a
100 Hz fourth-order corner between Headset's 120 and Laptop Mic's 85, and the de-esser at
`adaptive` because a headset on a Bluetooth profile captures at 16 kHz, where a 6 kHz corner has
no sibilance band above it and the stage should stand aside rather than run inert. Presence +2 at
2.5 kHz and +1.5 at 5 kHz; makeup +6.

All thirteen pass the same contract the first ten did (`crates/fxsound-dsp/tests/voice_presets.rs`):
nothing the engine would clamp, every gate range in (−40, 0), every de-esser buildable at 48 kHz,
every band within half a decibel of what it stores, and no two presets within a decibel of each
other anywhere in the chain. The rule for the three additions was the rule for the first ten: a
preset ships only where the named source's acoustics force it, and a denoise row is set only there.
Room Echo and Game Voice Chat stay rejected — de-reverb and echo cancellation are session settings
in 0.4.0, not voicings.
