//! The manual's code-review rules, checked on the sources, assets and build structure (`docs/test-plan.md`
//! counts a rule as automated when its ID is in a test's doc comment). Each check is about structure, not
//! behaviour: what may touch what, what is compiled in, what ships. Behaviour is tested in the crates and the
//! simulator; the radio and the bench confirm the hardware.

use ferrite_checks::{code, files_using, hex_literals, root, rs_files, walk};
use std::collections::BTreeSet;

fn set(v: Vec<String>) -> BTreeSet<String> {
    v.into_iter().collect()
}

/// The Radio task's module directory: an `allowed` entry ending in `/` covers every file under it.
const RADIO: &str = "firmware/src/tasks/radio/";

fn only(found: Vec<String>, allowed: &[&str], what: &str) {
    let ok = |f: &str| allowed.iter().any(|a| if a.ends_with('/') { f.starts_with(a) } else { f == *a });
    let extra: Vec<_> = set(found).into_iter().filter(|f| !ok(f)).collect();
    assert!(extra.is_empty(), "{what}: only {allowed:?} may, but also {extra:?}");
}

/// GOAL-3, SAFE-10, TX-17, KISS-3, APRS-7: one owner per chip. Only the Radio task (`tasks/radio/`) uses the
/// BK4829 driver and the PA drive (`hal::pa`), so every transmission, whether PTT, VOX, DTMF, APRS, KISS or a
/// remote key, has to ask it; only the UI task and its display driver use the ST7789.
#[test]
fn one_owner_per_chip() {
    only(files_using("firmware/src", "bk4829"), &[RADIO], "the BK4829 driver");
    only(files_using("firmware/src", "pa::"), &[RADIO], "the PA drive (hal::pa)");
    only(
        files_using("firmware/src", "st7789"),
        &["firmware/src/display.rs", "firmware/src/bus.rs", "firmware/src/tasks/ui.rs"],
        "the ST7789 display driver",
    );
    only(files_using("firmware/src", "display::"), &["firmware/src/tasks/ui.rs"], "the display");
    // The tasks that ask for transmissions (APRS, KISS over the link, the UI) never reach the radio's chips.
    for t in ["firmware/src/tasks/aprs.rs", "firmware/src/tasks/link.rs", "firmware/src/tasks/ui.rs"] {
        let c = code(&root().join(t));
        assert!(!c.contains("bk4829") && !c.contains("pa::set"), "{t} must ask the Radio task to transmit");
    }
}

/// GOAL-8, INST-1, UPD-10 (with SAFE-3): Ferrite never writes MCU flash or the option bytes and has no update
/// code: nothing in the firmware or the crates names the flash controller's unlock keys or its write registers
/// (unlock, option-byte unlock, control and address, banks 1 and 2). Reading the option bytes and the flash
/// status (the boot report, `:mem`) is fine. Updates go through the stock bootloader.
#[test]
fn no_mcu_flash_writes() {
    const BANNED: [(u64, &str); 9] = [
        (0x4567_0123, "flash unlock key 1"),
        (0xCDEF_89AB, "flash unlock key 2"),
        (0x4002_2004, "FLASH_UNLOCK"),
        (0x4002_2008, "FLASH_USD_UNLOCK"),
        (0x4002_2010, "FLASH_CTRL"),
        (0x4002_2014, "FLASH_ADDR"),
        (0x4002_2044, "FLASH_UNLOCK2"),
        (0x4002_2050, "FLASH_CTRL2"),
        (0x4002_2054, "FLASH_ADDR2"),
    ];
    let r = root();
    let mut files = rs_files("firmware/src");
    files.extend(rs_files("crates"));
    for p in files {
        if p.starts_with(r.join("crates/ferrite-checks")) {
            continue;
        }
        for v in hex_literals(&code(&p)) {
            if let Some((_, name)) = BANNED.iter().find(|(b, _)| *b == v) {
                panic!("{} names {name} (0x{v:08X}): Ferrite never writes MCU flash", p.strip_prefix(&r).unwrap().display());
            }
        }
    }
    // No update code: the firmware has no module or task for it.
    assert!(files_using("firmware/src", "fn write_flash").is_empty() && files_using("firmware/src", "fn program").is_empty());
}

/// GOAL-2: receive never depends on transmit code. Transmit is the firmware's `tx` cargo feature, which is what
/// turns on the BK4829 driver's own `tx` feature (directly and through `ferrite-radio`'s, which only forwards it
/// and is no default); nothing else enables the driver's transmit code, so a build without it has no transmit
/// code at all. `ferrite-radio`'s host tests enable its `tx` as a dev-dependency, which never reaches `cargo fw`.
#[test]
fn transmit_is_a_feature() {
    let section = |manifest: &str| {
        let m = std::fs::read_to_string(root().join(manifest)).unwrap();
        m.split("[features]").nth(1).and_then(|s| s.split("\n[").next()).unwrap_or("").to_string()
    };
    let fw = section("firmware/Cargo.toml");
    assert!(fw.contains("tx = [\"bk4829/tx\", \"ferrite-radio/tx\"]"), "the firmware's tx feature enables bk4829/tx");
    let radio = section("crates/ferrite-radio/Cargo.toml");
    assert!(radio.contains("tx = [\"bk4829/tx\"]"), "ferrite-radio's tx only forwards the driver's");
    for f in [&fw, &radio] {
        assert!(!f.lines().any(|l| l.trim_start().starts_with("default") && l.contains("tx")), "tx is not a default");
    }
    // No other manifest switches the driver's transmit code on.
    let mut others = Vec::new();
    walk(&root(), &mut |p| {
        if p.file_name().is_some_and(|n| n == "Cargo.toml")
            && !p.ends_with("firmware/Cargo.toml")
            && !p.ends_with("crates/ferrite-radio/Cargo.toml")
            && std::fs::read_to_string(p).unwrap_or_default().contains("bk4829/tx")
        {
            others.push(p.display().to_string());
        }
    });
    assert!(others.is_empty(), "only the firmware's tx feature may enable bk4829/tx: {others:?}");
}

/// TERM-38 (decision 151): the bench commands are the `bench` cargo feature, never a default. The firmware's
/// `bringup` and `tx-bench` turn it on and its plain `tx` doesn't, so the receive and tx images a user installs have
/// none of it; no other manifest turns it on but by forwarding a `bench` feature of its own (the sims' is off), and
/// the transmit test generators, the readable memory map and the TMR5 bit clock are compiled out without it.
#[test]
fn bench_is_a_feature() {
    let section = |manifest: &str| {
        let m = std::fs::read_to_string(root().join(manifest)).unwrap();
        m.split("[features]").nth(1).and_then(|s| s.split("\n[").next()).unwrap_or("").to_string()
    };
    let fw = section("firmware/Cargo.toml");
    let line = |name: &str| fw.lines().find(|l| l.starts_with(&format!("{name} = "))).unwrap_or_default().to_string();
    assert!(line("tx-bench").contains("\"bench\"") && line("bringup").contains("\"bench\""), "tx-bench and bringup are bench builds");
    assert!(!line("tx").contains("bench"), "the tx build a user installs has no bench commands");
    let mut on = Vec::new();
    walk(&root(), &mut |p| {
        if p.file_name().is_some_and(|n| n == "Cargo.toml") {
            let m = std::fs::read_to_string(p).unwrap_or_default();
            // Feature lists and dependency lines (`bench = false` on a [[bin]] is cargo's own benchmark switch).
            for l in m.lines().filter(|l| l.contains("bench") && !l.trim_start().starts_with('#') && l.trim() != "bench = false") {
                let forwarding = l.starts_with("bench = [") || l.starts_with("tx-bench = [") || l.starts_with("bringup = [");
                if !forwarding || l.trim_start().starts_with("default") {
                    on.push(format!("{}: {l}", p.display()));
                }
            }
        }
    });
    assert!(on.is_empty(), "only a crate's own bench (or tx-bench, bringup) feature may turn bench on: {on:?}");
    let core = code(&root().join("crates/ferrite-core/src/lib.rs"));
    for m in ["pub mod txtest;", "pub mod txsuite;", "pub mod memmap;"] {
        let before = core.split(m).next().unwrap_or("");
        assert!(core.contains(m) && before.trim_end().ends_with("#[cfg(feature = \"bench\")]"), "{m} only in a bench build");
    }
    let hal = code(&root().join("firmware/src/hal/mod.rs"));
    assert!(hal.contains("#[cfg(feature = \"tx\")]\npub mod edge;"), "TMR5's symbol timer only in a tx build");
}

/// GOAL-2: the receive-only firmware builds (and the transmit one too). Slow (two firmware builds, a minute or
/// more), so ignored by default: `cargo test -p ferrite-checks -- --ignored`, or CI. Builds into
/// `target/checks` so the image you flash is left alone.
#[test]
#[ignore = "builds the firmware twice; run with -- --ignored"]
fn receive_only_firmware_builds() {
    for features in [&[][..], &["--features", "tx"][..]] {
        let st = std::process::Command::new(env!("CARGO"))
            .current_dir(root())
            .args(["fw", "--target-dir", "target/checks"])
            .args(features)
            .status()
            .expect("cargo");
        assert!(st.success(), "cargo fw {features:?}");
    }
}

/// GOAL-11, DISP-7: English only. Every font is generated from the TTFs in `fonts/` (no stock font data) and
/// holds only Latin code points: printable ASCII, Latin-1 and a few UI symbols, nothing from CJK or other
/// scripts; no pinyin input or translation tables anywhere.
#[test]
fn fonts_are_english_only() {
    let assets = std::fs::read_to_string(root().join("crates/ferrite-ui/src/assets.rs")).unwrap();
    assert!(assets.starts_with("// Generated by tools/mkassets.py"), "fonts come from mkassets.py");
    let mut fonts = 0;
    for block in assets.split("_CODES: [u16;").skip(1) {
        let body = block.split("];").next().unwrap();
        let body = body.split_once('=').map(|(_, b)| b).unwrap_or(body);
        for v in hex_literals(body) {
            assert!(v < 0x2E80, "a font has code point U+{v:04X}, outside Latin and the UI's symbols");
        }
        fonts += 1;
    }
    assert!(fonts >= 5, "found {fonts} font code lists in assets.rs");
    // Each generated font names a TTF that is in fonts/.
    for line in assets.lines().filter(|l| l.starts_with("// ") && l.contains(".ttf,")) {
        let ttf = line.split(": ").nth(1).and_then(|s| s.split(',').next()).unwrap();
        assert!(root().join("fonts").join(ttf).exists(), "{ttf} is not in fonts/");
    }
    let mut hits = Vec::new();
    for dir in ["firmware/src", "crates", "sim/src"] {
        hits.extend(files_using(dir, "pinyin"));
    }
    assert!(hits.is_empty(), "pinyin found in {hits:?}");
}

/// GOAL-11: no voice prompts: no recorded audio ships with the firmware (no WAV, MP3, raw PCM or ADPCM files
/// among the sources and assets).
#[test]
fn no_voice_prompts() {
    let mut audio = Vec::new();
    for dir in ["assets", "firmware", "crates", "fonts"] {
        walk(&root().join(dir), &mut |p| {
            let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
            if ["wav", "mp3", "pcm", "adpcm", "ogg", "flac", "raw", "voc"].contains(&ext.as_str()) {
                audio.push(p.display().to_string());
            }
        });
    }
    assert!(audio.is_empty(), "audio files: {audio:?}");
}

/// GOAL-10: no digital voice yet: no codec or digital-voice crate is linked (Codec2 and M17 wait for the
/// baseband checks).
#[test]
fn no_digital_voice_yet() {
    let lock = std::fs::read_to_string(root().join("Cargo.lock")).unwrap();
    for name in lock.lines().filter_map(|l| l.strip_prefix("name = \"")).map(|l| l.trim_end_matches('"')) {
        let n = name.to_lowercase();
        assert!(
            !["codec2", "m17", "mbelib", "ambe", "dmr", "dstar"].iter().any(|d| n.starts_with(d)),
            "digital voice crate linked: {name}"
        );
    }
}

/// TEXT-8: the dictionary is small on purpose, about 570 words, and costs a few KB of flash.
#[test]
fn dictionary_is_small() {
    let words = std::fs::read_to_string(root().join("assets/words/dict.txt")).unwrap();
    let n = words.lines().filter(|l| !l.trim().is_empty()).count();
    assert!((500..=650).contains(&n), "{n} words");
    let bin = std::fs::metadata(root().join("assets/words/dict.bin")).unwrap().len();
    assert!(bin < 4096, "dict.bin is {bin} bytes");
}

/// TONE-12, SND-11: tones that go on air come from the BK4829's generator; only local beeps use DAC1, as stock's
/// key beeps do (decision 100). The DAC tone (`hal::tone`) is used by the board's reset, the bring-up build and
/// the Radio task's `beep`, `chime` and `boot_sound` only; the tone code (`ferrite_core::tonegen`) never touches the DAC,
/// and the Radio task never uses the audio DAC's DMA path (`audio::`). BOOT-18.2 (decision 321): the boot sound is a
/// local sound like the key beeps, played once at power-on before anything can key, so it joins them; it is the Radio
/// task's (not the UI's boot path's) because that task owns the speaker amp, which its tick switches every pass.
#[test]
fn tones_never_use_the_dac() {
    only(files_using("firmware/src", "tone::"), &["firmware/src/board.rs", "firmware/src/bringup.rs", "firmware/src/hal/clock.rs", "firmware/src/hal/audio.rs", RADIO], "the DAC tone");
    let tonegen = code(&root().join("crates/ferrite-core/src/tonegen.rs"));
    assert!(!tonegen.contains("hal::tone") && !tonegen.contains("audio::"), "tonegen must not use the DAC");
    let radio: String = rs_files(RADIO).iter().map(|p| code(p)).collect();
    assert!(!radio.is_empty(), "the Radio task's sources");
    assert!(!radio.contains("audio::"), "the Radio task must not use the audio DAC path");
    let uses: Vec<_> = radio.match_indices("hal::tone::").map(|(i, _)| i).collect();
    let span = |name: &str| {
        let at = radio.find(name).expect(name);
        (at, at + radio[at..].find("\n    }\n").expect("its end"))
    };
    let (beep, chime) = (span("async fn beep"), span("async fn chime")); // APRS-6.1: the message sound
    let boot = span("async fn boot_sound"); // BOOT-18.2: the boot sound
    let inside = |i: usize| [beep, chime, boot].iter().any(|s| i > s.0 && i < s.1);
    assert!(uses.iter().all(|&i| inside(i)), "only the beep, the chime and the boot sound use the DAC tone");
}

/// BCR-14: PA7/PC5, the SI4732's input switch, are set only by the Radio task's path state.
#[test]
fn si_input_pins_belong_to_the_radio_task() {
    only(files_using("firmware/src", "SI_IN"), &["firmware/src/board.rs", RADIO], "PA7/PC5 (SI_IN)");
}

/// GPS-1.1, GPS-22: the GPS task owns the module's power (PA8) and its TX pin PB10 (driven low while keyed); only
/// the board's safe state and the UART driver's pin table name them too.
#[test]
fn gps_pins_belong_to_the_gps_task() {
    only(files_using("firmware/src", "GPS_PWR_N"), &["firmware/src/board.rs", "firmware/src/tasks/gps.rs"], "PA8 (GPS power)");
    only(
        files_using("firmware/src", "pin(GPIOB, 10)"),
        &["firmware/src/board.rs", "firmware/src/tasks/gps.rs", "firmware/src/hal/uart.rs"],
        "PB10 (GPS TX)",
    );
}

/// KEY-2: one table maps the matrix codes, direct keys, PTTs and side keys to logical keys (`ferrite_core::keys`);
/// the firmware's input task scans with it and defines no key map of its own.
#[test]
fn one_key_table() {
    let input = code(&root().join("firmware/src/tasks/input.rs"));
    assert!(input.contains("ferrite_core::keys::"), "the input task uses ferrite_core::keys");
    for def in ["const ROWS: [[u8; 4]; 4]", "const GROUND_ROW: [u8; 4]", "const DIRECT: [u8; 2]"] {
        assert!(files_using("firmware/src", def).is_empty(), "the firmware defines its own `{def}`");
        assert_eq!(files_using("crates", def), ["crates/ferrite-core/src/keys.rs"], "{def}");
    }
}

/// TERM-26: no CHIRP CSV in the terminal: the web app converts CHIRP and RepeaterBook files. The one CSV the
/// terminal reads is a satellite's OMM line (`sat add`, SAT-3), parsed in ferrite-core: lines naming OMM are left out.
#[test]
fn no_csv_in_the_terminal() {
    let mut hits = Vec::new();
    for p in rs_files("crates/ferrite-app/src") {
        let name = p.to_string_lossy();
        let src = code(&p).to_lowercase();
        if name.contains("/term") && src.lines().any(|l| l.contains("csv") && !l.contains("omm") && !l.contains("parse_csv")) {
            hits.push(name.into_owned());
        }
    }
    assert!(hits.is_empty(), "CSV handling in the terminal: {hits:?}");
}

/// HF-17: Ferrite never ships or commits the SSB patch: no binary file in the repository is the size of the
/// patch (15,832 bytes, or with its 64-byte header), and none is named after it.
#[test]
fn ssb_patch_never_committed() {
    let mut found = Vec::new();
    walk(&root(), &mut |p| {
        let n = p.file_name().unwrap().to_string_lossy().to_lowercase();
        let len = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
        // Python's bytecode cache (git-ignored) is code, not data: tools/ssbpatch.py's own .pyc is named after it.
        if p.components().any(|c| c.as_os_str() == "__pycache__") {
            return;
        }
        let binary = !matches!(p.extension().and_then(|e| e.to_str()), Some("rs" | "md" | "txt" | "toml" | "py" | "svg" | "html" | "json" | "lock" | "sh" | "yml"));
        if binary && ((15_800..=15_960).contains(&len) || n.contains("ssb") && !n.ends_with(".png")) {
            found.push(p.display().to_string());
        }
    });
    assert!(found.is_empty(), "possible SSB patch files: {found:?}");
}

/// BOOT-1 (as SAFE-7): the safe state comes first, before RAM is initialised: the reset path's `__pre_init` masks
/// interrupts and calls `before_ram`, whose first act is the board's safe state.
#[test]
fn safe_state_before_ram() {
    let main = std::fs::read_to_string(root().join("firmware/src/main.rs")).unwrap();
    assert!(main.contains("__pre_init:") && main.contains("cpsid i") && main.contains("b before_ram"));
    let board = code(&root().join("firmware/src/board.rs"));
    let body = board.split("pub fn before_ram()").nth(1).expect("board::before_ram");
    let first = body.lines().map(str::trim).find(|l| !l.is_empty() && *l != "{").unwrap();
    assert_eq!(first, "safe_state();", "before_ram starts with the safe state");
}

/// PWR-10, GOAL-9: the core sleeps (WFE) whenever every task waits: the thread-mode executor's loop ends each
/// pass with `wfe`.
#[test]
fn core_sleeps_when_idle() {
    let main = code(&root().join("firmware/src/main.rs"));
    assert!(main.contains("cortex_m::asm::wfe()"), "the thread executor's loop sleeps with WFE");
    let fw = std::fs::read_to_string(root().join("firmware/Cargo.toml")).unwrap();
    assert!(fw.contains("executor-thread") && fw.contains("executor-interrupt"));
}

/// CHAN-7, CHAN-9, CHAN-36 (decision 333): no channel list in RAM and no per-channel field at all: the store keeps
/// the member lists of the slots' zones (three, each at most a zone's 256 channels) and reads channel records from
/// flash when asked.
#[test]
fn channels_are_read_on_demand() {
    let store = code(&root().join("crates/ferrite-store/src/store.rs"));
    let body = store.split("pub struct FlashStore").nth(1).unwrap().split("\n}").next().unwrap();
    let per_channel: Vec<_> = body.lines().filter(|l| l.contains("MAX_CHANNELS")).collect();
    assert!(per_channel.is_empty(), "no per-channel field: {per_channel:?}");
    assert!(body.contains("lists: RefCell<[Members; 3]>"), "the slots' member lists");
    let members = code(&root().join("crates/ferrite-store/src/members.rs"));
    assert!(members.contains("pub nums: [u16; ZONE_CAP as usize],"), "a list holds a zone's channels at most");
    assert!(!body.contains("Channel;") && !body.contains("[Channel"), "no channel array in the store");
}
