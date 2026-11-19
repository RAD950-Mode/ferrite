/* The stock bootloader owns 0x08000000-0x08002FFF and jumps here. The app runs to the end of the chip's 1024 KB
 * (0x080FFFFF), the bootloader's own limit (image offsets below 0xFD000): 1,012 KB. The top 64 KB (0x080F0000) was
 * stock's marker sector, one word (UV85/BJ90) only stock firmware writes; the backup records it and the way back
 * to stock puts it back or leaves it erased (decision 338, BACKUP-12). */
MEMORY
{
  FLASH : ORIGIN = 0x08003000, LENGTH = 1012K
  RAM   : ORIGIN = 0x20000000, LENGTH = 96K
}

/* Code starts after the model tags, which must sit at 0x080033E0 (image offset 0x3E0, read by the stock updater)
 * and 0x080037E0 (0x7E0: the bootloader compares it at every power-on and shows "Firmware Error!" instead of
 * starting the app when it differs; 0x08000654, found on the radio Oct 11, 2026). */
_stext = ORIGIN(FLASH) + 0x800;

SECTIONS
{
  /* The room around the tags (0x1C0-0x3E0 after the vector table, 0x3F0-0x7E0 between the tags) holds a few
   * fast functions instead of padding: ones whose size moves only with the pinned toolchain, so a change of ours
   * never pushes them past a tag. If one ever outgrows its gap, the ASSERTs below stop the link: drop its line
   * (it then goes back into .text). Each is as fast here as in .text (all zero-wait flash). */
  .fast_gap1 : ALIGN(4)
  {
    *(.text.*17compiler_builtins3int19specialized_div_rem11u64_div_rem);
  } > FLASH
  __efast_gap1 = .;
  .model_tag ORIGIN(FLASH) + 0x3E0 :
  {
    KEEP(*(.model_tag));
  } > FLASH
  .fast_gap2 : ALIGN(4)
  {
    *(.text.*17compiler_builtins3mem7memmove);
    *(.text.*17compiler_builtins3mem6memcpy);
  } > FLASH
  __efast_gap2 = .;
  .model_tag_boot ORIGIN(FLASH) + 0x7E0 :
  {
    KEEP(*(.model_tag_boot));
  } > FLASH
} INSERT AFTER .vector_table;

ASSERT(__efast_gap1 <= ORIGIN(FLASH) + 0x3E0, ".fast_gap1 runs into the model tag at 0x3E0: drop a line from it");
ASSERT(__efast_gap2 <= ORIGIN(FLASH) + 0x7E0, ".fast_gap2 runs into the model tag at 0x7E0: drop a line from it");

/* The slow part of flash. The AT32F403A runs code at full speed only from the first 256 KB (0x08000000-0x0803FFFF,
 * zero-wait; docs/firmware-design.md, "MCU flash"). Everything that has to be fast stays in .text and .rodata at
 * the front: the vector table, interrupt handlers, the executors and time driver, the radio, DSP, input, storage
 * and link tasks, the drivers (the display bus too), core and compiler_builtins, the fault paths, and the
 * named `static` tables they read.
 *
 * After them, in this order:
 *  - .slow_rodata: every unnamed constant (all literal strings, `const` tables and match tables: the rule since
 *    Oct 13, 2026), the UI's jump tables, font and logo data (tools/mkassets.py) and the other explicit
 *    `.slow_rodata` statics. Fast code may read from here: a wait state or two per read, nothing more (the firmware
 *    never writes the MCU flash, and no DMA reads it). Text read once per message is fine; a table read per
 *    sample, per interrupt or per pass of the Radio task must be a named `static` instead, which stays fast.
 *    tools/slowcheck.py fails on a table read from interrupt context unless its allow-list says why.
 *  - .slow_text: the UI's code, run only by the thread-mode UI task (drawing is bound by the bit-banged display
 *    bus): every function of ferrite-app and ferrite-ui, including generic instances made for them (their v0
 *    mangled names carry the crate as `11ferrite_app` / `10ferrite_ui`), and the UI task itself. Nothing on an
 *    interrupt path calls into it: tools/slowcheck.py checks that on the ELF.
 * Sections named by an INSERTed SECTIONS command are matched before the main script's `*(.text .text.*)`, so these
 * patterns win over cortex-m-rt's .text; the ASSERTs below catch it if that ever changes.
 *
 * The image stays one contiguous binary for the bootloader: no gap, no padding. */
SECTIONS
{
  /* The drawing primitives stay fast, ahead of the slow part: text, glyphs, masks, fills and the panel's pixel
   * runs run once per pixel, and from slow flash they made large redraws crawl on the radio (Oct 12, 2026). Matched
   * here first, so .slow_text's ferrite_ui pattern doesn't take them. */
  .fast_ui : ALIGN(4)
  {
    __sfast_ui = .;
    *(.text.*10ferrite_ui3gfx*);
    *(.text.*10ferrite_ui5font4*);
    *(.text.*10ferrite_ui5panel*);
    /* BOOT-18: the boot animation's pixels (the logo and the wordmark, about 30 frames a second for 1.3 s) */
    *(.text.*10ferrite_ui8bootanim*);
    /* the off-screen canvas's band (DISP-25): drawing into it is per-pixel work too */
    *(.text.*10ferrite_ui6canvas4Band*3gfx3Gfx*);
    *(.text.*10ferrite_ui6canvasNtB*4Band*);
    /* DISP-29: a recorded region's replay into each band, the band loop and a new screen's background */
    *(.text.*10ferrite_ui6canvas6replay*);
    *(.text.*10ferrite_ui6canvas5bands*);
    *(.text.*10ferrite_ui6canvas5cover*);
    *(.text.*10ferrite_ui6canvasNtB*2Rd*);
    /* SAT-6: satellite pass prediction's inner loop (SGP4, the look angles, the scan; about 21,000 propagations
     * for 19 satellites, Nov 3, 2026), CPU-bound in the UI task's time slices: a propagation, its look angles, the
     * scan and the math under them (about 4.6 KB); the rest of `sat` (a satellite's set-up included) stays slow */
    *(.text.*12ferrite_core3sat4sgp4*9propagate*);
    *(.text.*12ferrite_core3sat4look*7Tracker4look*);
    *(.text.*12ferrite_core3sat4look4gmst*);
    *(.text.*12ferrite_core3sat5fmath*);
    *(.text.*12ferrite_core3sat4pass*);
    . = ALIGN(4);
    __efast_ui = .;
  } > FLASH

  .slow_rodata : ALIGN(4)
  {
    __sslow_rodata = .;
    *(.slow_rodata .slow_rodata.*);
    /* Every unnamed constant: with LTO and one codegen unit, each `const`, literal string and table the compiler
     * doesn't name is `.rodata..Lanon.<hash>.<n>`, with no crate in the name to sort by, so they all come here.
     * Named `static`s keep their own `.rodata.<symbol>` sections and stay fast: a table read per sample, per
     * interrupt or per pass of the Radio task must be a `static` (tools/slowcheck.py fails on a table read from
     * interrupt context in this section). */
    *(.rodata..Lanon.*);
    /* Not the merged pools of 4, 8, 16 and 32-byte constants (`.rodata.cst*`, one input section each): hot code reads
     * them (the encoder, the safe state, the DSC framer), and after LTO small named statics land in them too. They
     * stay fast, short UI strings of those sizes with them (about 4 KB in the tx build, Oct 13, 2026). */
    /* The UI's match jump tables (the code that reads them is slow anyway). */
    *(.rodata..Lswitch.table.*11ferrite_app*);
    *(.rodata..Lswitch.table.*10ferrite_ui*);
    *(.rodata..Lswitch.table.*7ferrite5tasks2ui*);
    . = ALIGN(4);
    __eslow_rodata = .;
  } > FLASH

  .slow_text : ALIGN(4)
  {
    __sslow_text = .;
    /* `?*` after each crate, as for uaprs below: a core generic made in a crate ends in its name (the crate suffix),
     * and LLVM's function merging can hand that one copy to fast callers. Without it the Linux build put
     * `RangeInclusive::contains` (made in ferrite_app) here, under the Radio and DSP tasks (CI, Nov 6). */
    *(.text.*11ferrite_app?*);
    *(.text.*10ferrite_ui?*);
    *(.text.*13ferrite_store?*);
    *(.text.*7ferrite5tasks2ui*);
    /* The APRS task (run by the UI task) and the APRS library it parses and builds frames with; the modem
     * (uafsk) stays fast: the DSP task runs it. `5uaprs?*`, not `5uaprs*`: a core generic made for uaprs ends in
     * `5uaprs` (its crate suffix) and LLVM's function merging can hand that one copy to fast callers (the Radio and
     * DSP tasks' `RangeInclusive::contains`), so those stay fast. */
    *(.text.*7ferrite5tasks4aprs*);
    *(.text.*5uaprs?*);
    /* RAD950 conversation, distress & extended-frame codecs (UI task only; DSP receiver/LDPC stays fast). */
    *(.text.*7usol9504chat*);
    *(.text.*7usol9508distress*);
    *(.text.*7usol9503ext*);
    /* The tone hint (TSRCH-12, -13): the Radio task's step for it and the heard-tone decode, at most once a
     * receive tick, never from an interrupt. */
    *(.text.*8tonehint*);
    /* The GPS task and its NMEA parser (thread mode, below every interrupt): the UART interrupt (fast) fills a ring,
     * and NMEA at 9600 bd is under 1 KB/s, far from what parsing from the slow flash can do. With it the power
     * manager (GPS-26 to GPS-33) and the GGA copy for the terminal's `gps` (TERM-30). */
    *(.text.*7ferrite5tasks3gps*);
    *(.text.*12ferrite_core3gps*);
    /* satellite mode's prediction, geometry and tracks (chapter 28a): the UI task's, never per sample */
    *(.text.*12ferrite_core3sat*);
    *(.text.*7ferrite3sat*);
    *(.text.*8gpspower*);
    *(.text.*7ferrite5debug3gga*);
    /* Helpers only the UI task uses: the SSB patch check's SHA-256, `str` search (entry::commit) and UTF-8
     * checks, the frequency formatter. */
    *(.text.*12ferrite_core6sha256*);
    /* The SPI parts (decision 335): checked once at power-on, read when a screen or the terminal draws; and the SSB
     * patch streamed by the Radio task, once per switch to a sideband (HF-19). */
    *(.text.*12ferrite_core4part*);
    *(.text.*7ferrite5store9part_read*);
    *(.text.*4core3str7pattern*);
    *(.text.*4core3str8converts9from_utf8*);
    *(.text.*12ferrite_core4freq6format*);
    *(.text.*12ferrite_core4freq8put_uint*);
    /* Live View's compact form and deltas (LINK-47): the link sessions', in the UI task. */
    *(.text.*12ferrite_core8liveview*);
    *(.text.*4chip10heard_tone*);
    /* FM broadcast's report (at most ten times a second) and its watch mask (BCR-2, BCR-23): the Radio task's,
     * never from an interrupt. */
    *(.text.*5Radio7fm_poll*);
    *(.text.*5Radio9bcast_off*);
    /* The band scope's and home strip's step (SCOPE-4, MODE-5): once a point, at least 2 ms apart, the Radio
     * task's; the BK4829 writes it calls stay fast. */
    *(.text.*5Radio10scope_tick*);
    /* The backlight's PWM (DISP-23): set by the UI task only, once a pass. */
    *(.text.*3hal9backlight*);
    . = ALIGN(4);
    __eslow_text = .;
  } > FLASH
} INSERT AFTER .rodata;

/* The fast-flash budget: everything from the image start to the end of .fast_ui (vectors, the gaps by the tags,
 * .text, .rodata, .fast_ui). Set Oct 13, 2026 at the tx build's use (141.5 KB; the receive-only build 127.6 KB)
 * plus a small margin, of the 244 KB zero-wait room: growth past it is a decision (move something to the slow
 * part, or raise this), not an accident. tools/slowcheck.py prints the use against it. After the flash moves of the
 * same day (unnamed constants, the outliner, the GPS task) the tx build uses 81,604 B and the receive-only one
 * 68,200 B; the budget is left where it was until lowering it is decided. */
/* Nov 3, 2026 (Alex): raised to 180 KB. tx-bench reached 147.6 KB with satellite mode's fast-flash prediction loop
 * and the clock-aware LCD strobe; the zero-wait room is 244 KB, so 180 stays a tripwire with real room behind it. */
/* Nov 19, 2026: raised to 196 KB for RAD950 (usol950 DSP receiver/LDPC + non-inlined decoder init helpers);
 * zero-wait room is 244 KB, leaving 48 KB headroom. */
__fast_budget = 196K;
ASSERT(__efast_ui - ORIGIN(FLASH) <= __fast_budget, "fast flash over its budget (__fast_budget in memory.x): move code to the slow part or raise the budget");

/* The fast part (code and the other constants) must end inside the zero-wait region (0x08000000 + 256 KB). */
ASSERT(__erodata <= 0x08040000, "fast code and constants run past the 256 KB zero-wait flash");
ASSERT(__efast_ui <= 0x08040000, "the fast drawing code runs past the 256 KB zero-wait flash");
/* The UI code really went to the slow part (the name patterns above still match the symbols). */
ASSERT(__eslow_text - __sslow_text > 32K, ".slow_text is (nearly) empty: the UI code stayed in the zero-wait flash");
