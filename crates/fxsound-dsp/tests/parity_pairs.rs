//! Every test that pins a change the port made on purpose has a twin at «Like FxSound for
//! Windows» = Interface and sound (roadmap 0.5.0 §1, A5).
//!
//! The 0.4.0 audit changed behaviour the port had copied from the Windows build, and each test
//! that used to hold the Windows behaviour and now holds the fix says so where it was changed: a
//! comment reading "changed on purpose", with the audit's item (`#13`, `R3` and so on). At
//! Interface and sound the output lane plays the Windows build's DSP again, so each of those
//! tests has a twin that holds the Windows behaviour there, while the test itself goes on holding
//! Off's — 0.4.0's. When 0.5.0 began there were 36 such comments, all in `fxsound-dsp`.
//!
//! [`PAIRS`] is the list: for each file and item, the twins. The tests below read the sources and
//! hold the list to them — every such comment names an item, every item a comment names in a
//! file has its twins listed, and every twin listed is a test that exists. A new change on purpose
//! does not build a green run without its twin, and a twin renamed or deleted is missed at once.
//!
//! One item has a twin of another kind. The voice chain's frozen fixture (`frozen_voice_chain.rs`)
//! was re-measured for the limiter's fixes (#8, R1, R2), which Dynamic Boost's limiter shares; the
//! Windows build has no voice chain, so at every level the voice chain keeps them, and the twin is
//! the application's test that Interface and sound leaves every voice route's snapshot alone.

use std::path::{Path, PathBuf};

/// The comment that marks a change made on purpose, matched without regard to case.
const MARKER: &str = "changed on purpose";

/// One file's item and its twins at Interface and sound.
struct Pair {
    /// The file the comments are in, from the workspace root.
    file: &'static str,
    /// The audit's item the comments name: `#13`, `R3`.
    item: &'static str,
    /// The tests that hold the Windows behaviour at Interface and sound, anywhere in the
    /// workspace.
    twins: &'static [&'static str],
}

const PAIRS: &[Pair] = &[
    // Ambience's wet/dry pair for every stored value, and the slider's straight line.
    Pair {
        file: "crates/fxsound-dsp/src/effects/ambience.rs",
        item: "#39",
        twins: &[
            "at_interface_and_sound_every_stored_value_gets_the_windows_builds_parameters",
            "at_interface_and_sound_ambiences_positions_are_the_windows_builds_straight_line",
        ],
    },
    // Dynamic Boost's limiter: no hold, the attack's overshoot, no link.
    Pair {
        file: "crates/fxsound-dsp/src/effects/dynamic_boost.rs",
        item: "R1",
        twins: &[
            "at_interface_and_sound_dynamic_boost_is_maxi32s_to_the_bit",
            "with_no_link_no_hold_and_the_overshoot_the_limiter_is_the_originals_to_the_bit",
        ],
    },
    // The back-off's flat 1.06 floor.
    Pair {
        file: "crates/fxsound-dsp/src/effects/dynamic_boost.rs",
        item: "#6",
        twins: &[
            "at_interface_and_sound_loud_material_at_slider_zero_is_lifted_half_a_decibel_as_on_windows",
        ],
    },
    // The level estimator on the left channel alone.
    Pair {
        file: "crates/fxsound-dsp/src/effects/dynamic_boost.rs",
        item: "#7",
        twins: &["at_interface_and_sound_the_level_is_the_left_channels_as_on_windows"],
    },
    // A curve of another band count, by position.
    Pair {
        file: "crates/fxsound-dsp/src/eq.rs",
        item: "#13",
        twins: &[
            "at_interface_and_sound_a_curve_of_another_count_is_read_by_position_as_on_windows",
            "the_windows_remap_by_position_is_0f05ba5s",
            "at_interface_and_sound_a_curve_of_another_count_is_fitted_by_position",
        ],
    },
    // The gain stage inside the GraphicEq block, and the Windows bypass.
    Pair {
        file: "crates/fxsound-dsp/src/engine.rs",
        item: "R3",
        twins: &[
            "at_interface_and_sound_the_gain_stage_sits_between_the_equalizer_and_the_leveller",
            "at_interface_and_sound_the_equalizer_switch_takes_the_master_gain_and_the_balance_with_it",
            "at_interface_and_sound_the_bypass_is_the_master_gain_alone_while_the_equalizer_is_on",
        ],
    },
    // The balance on stereo only, as the Windows C code has it.
    Pair {
        file: "crates/fxsound-dsp/src/engine.rs",
        item: "#44",
        twins: &[
            "at_interface_and_sound_the_balance_plays_on_stereo_only_as_the_windows_c_code_has_it",
        ],
    },
    // The subwoofer neither analysed nor levelled.
    Pair {
        file: "crates/fxsound-dsp/src/engine.rs",
        item: "#3",
        twins: &["at_interface_and_sound_the_subwoofer_is_left_at_unity_as_on_windows"],
    },
    Pair {
        file: "crates/fxsound-dsp/src/leveller.rs",
        item: "#3",
        twins: &["at_interface_and_sound_the_subwoofer_is_left_at_unity_as_on_windows"],
    },
    // A step per call; an empty call still changes nothing.
    Pair {
        file: "crates/fxsound-dsp/src/leveller.rs",
        item: "#2",
        twins: &[
            "at_interface_and_sound_the_state_machine_steps_once_a_call_as_on_windows",
            "at_interface_and_sound_an_empty_call_leaves_the_gain_where_it_was",
        ],
    },
    // The voice chain keeps the limiter's fixes at every level (module documentation).
    Pair {
        file: "crates/fxsound-dsp/tests/frozen_voice_chain.rs",
        item: "#8",
        twins: &[
            "interface_and_sound_resends_the_playback_routes_on_the_windows_dsp_and_leaves_the_voices_alone",
        ],
    },
    Pair {
        file: "crates/fxsound-dsp/tests/frozen_voice_chain.rs",
        item: "R1",
        twins: &[
            "interface_and_sound_resends_the_playback_routes_on_the_windows_dsp_and_leaves_the_voices_alone",
        ],
    },
    Pair {
        file: "crates/fxsound-dsp/tests/frozen_voice_chain.rs",
        item: "R2",
        twins: &[
            "interface_and_sound_resends_the_playback_routes_on_the_windows_dsp_and_leaves_the_voices_alone",
        ],
    },
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root")
        .to_path_buf()
}

/// Every `.rs` file under `crates/`, as its path from the workspace root and its text. This file
/// is left out: it quotes the marker.
fn sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let entries =
            std::fs::read_dir(dir).unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|name| name == "target") {
                    continue;
                }
                walk(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }
    let root = workspace_root();
    let mut files = Vec::new();
    walk(&root.join("crates"), &mut files);
    files.sort();
    let this = Path::new(file!()).file_name().expect("this file's name");
    files
        .into_iter()
        .filter(|path| {
            !(path.file_name() == Some(this) && path.parent().is_some_and(|p| p.ends_with("tests")))
        })
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("{}: {err}", path.display()));
            let relative = path
                .strip_prefix(&root)
                .expect("under the root")
                .to_string_lossy()
                .into_owned();
            (relative, text)
        })
        .collect()
}

/// The audit's items a line names: `#` or `R` and a number.
fn items(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes = text.as_bytes();
    for (at, &byte) in bytes.iter().enumerate() {
        let starts_word = at == 0 || !bytes[at - 1].is_ascii_alphanumeric();
        if (byte == b'#' || (byte == b'R' && starts_word))
            && bytes.get(at + 1).is_some_and(u8::is_ascii_digit)
        {
            let digits: String = text[at + 1..]
                .chars()
                .take_while(char::is_ascii_digit)
                .collect();
            let item = format!("{}{digits}", byte as char);
            if !found.contains(&item) {
                found.push(item);
            }
        }
    }
    found
}

/// Every marker: its file, its line (from 1), and the items it names — on its line, from 40
/// characters before the marker on, and on the line after, where a comment wraps.
fn markers() -> Vec<(String, usize, Vec<String>)> {
    let mut out = Vec::new();
    for (file, text) in sources() {
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            let lower = line.to_lowercase();
            let Some(at) = lower.find(MARKER) else {
                continue;
            };
            let from = line
                .char_indices()
                .map(|(byte, _)| byte)
                .filter(|byte| *byte <= at)
                .rev()
                .nth(40)
                .unwrap_or(0);
            let next = lines.get(index + 1).copied().unwrap_or("");
            out.push((
                file.clone(),
                index + 1,
                items(&format!("{} {next}", &line[from..])),
            ));
        }
    }
    out
}

#[test]
fn the_list_finds_the_changes_made_on_purpose() {
    // The list has something to hold: the 36 comments 0.5.0 began with are still where they were
    // found, give or take any a later phase removed with its test.
    let found = markers();
    assert!(found.len() >= 30, "only {} markers found", found.len());
    assert!(found.iter().all(|(file, _, _)| file.starts_with("crates/")));
}

#[test]
fn every_change_made_on_purpose_has_its_twin_at_interface_and_sound() {
    let mut missing = Vec::new();
    for (file, line, named) in markers() {
        if named.is_empty() {
            missing.push(format!("{file}:{line}: names no audit item"));
        }
        for item in named {
            if !PAIRS
                .iter()
                .any(|pair| pair.file == file && pair.item == item)
            {
                missing.push(format!("{file}:{line}: {item} has no twin in PAIRS"));
            }
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

#[test]
fn every_twin_listed_is_a_test_that_exists() {
    let sources = sources();
    let mut missing = Vec::new();
    for pair in PAIRS {
        for twin in pair.twins {
            let declared = format!("fn {twin}(");
            let is_a_test = sources.iter().any(|(_, text)| {
                let lines: Vec<&str> = text.lines().collect();
                lines.iter().enumerate().any(|(index, line)| {
                    line.contains(&declared)
                        && lines[index.saturating_sub(3)..index]
                            .iter()
                            .any(|above| above.trim() == "#[test]")
                })
            });
            if !is_a_test {
                missing.push(format!("{} {}: no test {twin}", pair.file, pair.item));
            }
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

#[test]
fn every_pair_listed_is_still_named_by_a_comment() {
    // A pair whose comments are gone is a stale line of the list.
    let found = markers();
    let stale: Vec<String> = PAIRS
        .iter()
        .filter(|pair| {
            !found.iter().any(|(file, _, named)| {
                file == pair.file && named.iter().any(|item| item == pair.item)
            })
        })
        .map(|pair| format!("{} {}", pair.file, pair.item))
        .collect();
    assert!(stale.is_empty(), "no comment names {}", stale.join(", "));
}

#[test]
fn an_item_is_read_as_the_comments_write_it() {
    assert_eq!(items("audit report #13 (held ends)"), ["#13"]);
    assert_eq!(items("audit reports R3 and #44 — the"), ["R3", "#44"]);
    assert_eq!(items("(#8, R1, R2) changed"), ["#8", "R1", "R2"]);
    assert!(items("ROUTE R-1 FR2 #x").is_empty());
}
