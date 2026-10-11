// SPDX-License-Identifier: MPL-2.0
//! Scan code set 1 → key events.
//!
//! **Set 1, not set 2**, and that was measured rather than assumed:
//! `scripts/m15-spike/` reads port 0x60 from a 512-byte boot sector
//! and prints what arrives. Under this project's QEMU flags, `a`
//! comes back as `1e 9e` — set 1, because SeaBIOS leaves the i8042's
//! translation bit on. See ADR 053.
//!
//! This module is **pure**: bytes in, events out, no capabilities, no
//! I/O. That is deliberate. A decoder tangled into the serve loop
//! could only be tested by pressing a key, and a test that needs a
//! key press cannot enumerate the cases that matter (every modifier,
//! every prefix, malformed input). Here the same code is exercised
//! two ways — by `self_test` below with a table of known sequences,
//! and by the real device at run time.

/// A key, as this driver names it.
///
/// Not a character: a character depends on the layout and on which
/// modifiers are held, and those are [`Layout`]'s business.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// A key with a position on the main block.
    Code(u8),
    /// A key reached through the 0xE0 prefix.
    Extended(u8),
}

/// What the decoder produces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    /// The key went down.
    Make(Key),
    /// The key came up.
    Break(Key),
}

/// Bit 7 of a set 1 scan code: the key was released.
///
/// The whole of set 1's make/break encoding. Set 2 uses a separate
/// 0xF0 byte instead, which is why a decoder written for one does
/// not work for the other.
const BREAK_BIT: u8 = 0x80;

/// The prefix that marks an extended key.
///
/// **Survives translation.** Measured: `up` arrives as
/// `e0 48 e0 c8`, so the prefix is repeated on release and the byte
/// after it carries the break bit as usual. A decoder that cleared
/// the prefix flag only on make would mis-decode every release.
const EXTENDED_PREFIX: u8 = 0xE0;

/// The prefix that begins the Pause/Break sequence.
///
/// Not decoded into a key; counted and skipped. Pause is six bytes
/// (`e1 1d 45 e1 9d c5`) and nothing in this milestone needs it,
/// but it has to be *consumed* rather than mistaken for five
/// ordinary keys.
const PAUSE_PREFIX: u8 = 0xE1;

/// Bytes the controller sends that are not keys.
///
/// 0x00 and 0xFF are the two error/overrun codes; 0xFA and 0xFE are
/// ACK and resend, which only appear in reply to a command this
/// driver never sends.
fn is_status_byte(byte: u8) -> bool {
    matches!(byte, 0x00 | 0xFA | 0xFE | 0xFF)
}

/// Turns a byte stream into events.
///
/// One bit of state and one counter. The counter exists because a
/// Pause sequence must be swallowed whole and the only way to know
/// how much is left is to count.
#[derive(Default)]
pub struct Decoder {
    /// A 0xE0 was seen and the next byte belongs to it.
    pending_extended: bool,
    /// How many bytes of a Pause sequence are still to come.
    pause_remaining: u8,
    /// Bytes that produced no event and were not prefixes.
    unrecognised: u32,
}

impl Decoder {
    /// A fresh decoder.
    pub const fn new() -> Decoder {
        Decoder {
            pending_extended: false,
            pause_remaining: 0,
            unrecognised: 0,
        }
    }

    /// Feed one byte. Returns an event when the byte completes one.
    ///
    /// Most bytes complete an event; prefixes do not.
    pub fn feed(&mut self, byte: u8) -> Option<Event> {
        // Pause first: its bytes overlap with ordinary codes (0x1d is
        // left control) and anything that looked at them in isolation
        // would report a phantom key.
        if self.pause_remaining > 0 {
            self.pause_remaining -= 1;
            return None;
        }

        if byte == PAUSE_PREFIX {
            // Five more after this one.
            self.pause_remaining = 5;
            return None;
        }

        if byte == EXTENDED_PREFIX {
            self.pending_extended = true;
            return None;
        }

        if is_status_byte(byte) {
            // **Counted, not panicked on.** A keyboard that reports
            // an overrun has not broken the system, and a driver
            // that died on one would turn a device fault into a
            // component fault.
            self.unrecognised += 1;
            self.pending_extended = false;
            return None;
        }

        // `take()` rather than reading and clearing separately: the
        // flag must be cleared on *every* path out of here, including
        // the break path, and a separate clear is a line somebody can
        // forget to add to a new branch.
        let extended = core::mem::take(&mut self.pending_extended);
        let released = byte & BREAK_BIT != 0;
        let code = byte & !BREAK_BIT;

        let key = if extended {
            Key::Extended(code)
        } else {
            Key::Code(code)
        };

        Some(if released {
            Event::Break(key)
        } else {
            Event::Make(key)
        })
    }

    /// Bytes that produced nothing and were not prefixes.
    ///
    /// **Read by the acceptance test**, and the reason it is read is
    /// `hardware-todo.md` item 11: if this driver ever runs on a
    /// machine whose firmware leaves translation *off*, every byte
    /// will be a set 2 code and most will land here. A non-zero
    /// count is how that is noticed rather than guessed at.
    pub fn unrecognised(&self) -> u32 {
        self.unrecognised
    }
}

// ---- Scan codes this driver knows -------------------------------------
//
// Set 1 positions, from the IBM PC/XT keyboard. Only the keys the
// acceptance criteria need are named; everything else decodes to a
// `Key::Code` the layout does not map, which is a key press that
// produces no character rather than an error.

/// Left shift.
pub const SC_LSHIFT: u8 = 0x2A;
/// Right shift.
pub const SC_RSHIFT: u8 = 0x36;
/// Left control.
pub const SC_LCTRL: u8 = 0x1D;
/// Left alt.
pub const SC_LALT: u8 = 0x38;

/// Up arrow, behind the 0xE0 prefix.
///
/// The only extended code this component names, and it is named
/// because [`self_test`] checks the measured `e0 48 e0 c8` sequence.
///
/// **Backspace, Enter and the other arrows are deliberately absent.**
/// This driver does not interpret them: it forwards a character and
/// a scan code, and what a cursor key *means* is the console's
/// business (ADR 053's split between decoder and consumer). An
/// earlier draft defined all of them here and the compiler rejected
/// it — `warnings = "deny"` caught constants that belonged to the
/// other side of the protocol, which is the lint being more right
/// than the author.
pub const SC_EXT_UP: u8 = 0x48;

// ---- Layout ------------------------------------------------------------

/// Which modifiers are held.
///
/// State, not events: "shift is down" persists between key presses
/// and the decoder deliberately knows nothing about it.
#[derive(Default, Clone, Copy)]
pub struct Modifiers {
    /// Either shift key is down.
    pub shift: bool,
    /// Left control is down.
    pub ctrl: bool,
    /// Left alt is down.
    pub alt: bool,
}

impl Modifiers {
    /// Update from an event. Returns whether this event *was* a
    /// modifier, so the caller knows not to treat it as a character.
    pub fn apply(&mut self, event: Event) -> bool {
        let (key, down) = match event {
            Event::Make(k) => (k, true),
            Event::Break(k) => (k, false),
        };

        match key {
            Key::Code(SC_LSHIFT | SC_RSHIFT) => self.shift = down,
            Key::Code(SC_LCTRL) => self.ctrl = down,
            Key::Code(SC_LALT) => self.alt = down,
            _ => return false,
        }
        true
    }
}

/// Unshifted characters, indexed by set 1 scan code.
///
/// A US layout, and only as far as the criteria need. Zero means
/// "no character at this position" — a key that exists but produces
/// nothing printable.
const UNSHIFTED: [u8; 0x40] = [
    0, 0, b'1', b'2', b'3', b'4', b'5', b'6', // 0x00
    b'7', b'8', b'9', b'0', b'-', b'=', 0, 0, // 0x08
    b'q', b'w', b'e', b'r', b't', b'y', b'u', b'i', // 0x10
    b'o', b'p', b'[', b']', 0, 0, b'a', b's', // 0x18
    b'd', b'f', b'g', b'h', b'j', b'k', b'l', b';', // 0x20
    b'\'', b'`', 0, b'\\', b'z', b'x', b'c', b'v', // 0x28
    b'b', b'n', b'm', b',', b'.', b'/', 0, b'*', // 0x30
    0, b' ', 0, 0, 0, 0, 0, 0, // 0x38
];

/// Shifted characters, same indexing.
const SHIFTED: [u8; 0x40] = [
    0, 0, b'!', b'@', b'#', b'$', b'%', b'^', // 0x00
    b'&', b'*', b'(', b')', b'_', b'+', 0, 0, // 0x08
    b'Q', b'W', b'E', b'R', b'T', b'Y', b'U', b'I', // 0x10
    b'O', b'P', b'{', b'}', 0, 0, b'A', b'S', // 0x18
    b'D', b'F', b'G', b'H', b'J', b'K', b'L', b':', // 0x20
    b'"', b'~', 0, b'|', b'Z', b'X', b'C', b'V', // 0x28
    b'B', b'N', b'M', b'<', b'>', b'?', 0, b'*', // 0x30
    0, b' ', 0, 0, 0, 0, 0, 0, // 0x38
];

/// What a key produces, given the modifiers.
///
/// Returns zero for keys with no printable character. Extended keys
/// never produce one — they are handled by the consumer as cursor
/// movement (ADR 054).
pub fn to_char(key: Key, modifiers: Modifiers) -> u8 {
    let Key::Code(code) = key else {
        return 0;
    };

    if (code as usize) >= UNSHIFTED.len() {
        return 0;
    }

    let ch = if modifiers.shift {
        SHIFTED[code as usize]
    } else {
        UNSHIFTED[code as usize]
    };

    // Control turns a letter into its control code: ctrl+C is 0x03.
    // Checked after the table lookup so that ctrl+shift+c is also
    // 0x03, which is what every terminal does.
    if modifiers.ctrl && ch.is_ascii_alphabetic() {
        return ch.to_ascii_uppercase() - b'A' + 1;
    }

    ch
}

// ---- The decoder's own test --------------------------------------------

/// What [`self_test`] reports, packed for `Echo`.
///
/// A bitmask rather than a count: a count says how many failed, a
/// mask says *which*, and the which is what tells you where to look.
pub struct SelfTest {
    /// Bit N set means case N failed.
    pub failures: u32,
    /// How many cases ran.
    pub cases: u32,
}

/// Run the decoder against sequences measured from real hardware.
///
/// **Every byte sequence here was observed**, not invented: they are
/// the output of `scripts/m15-spike/` under this project's QEMU
/// flags. That matters, because a decoder tested against sequences
/// its author imagined would agree with its author rather than with
/// the device.
///
/// This runs inside the component at startup. It needs no device, no
/// interrupt and no capability, so it also runs in the scenarios
/// where the keyboard has no IOPort — which is how `kbd-no-cap`
/// distinguishes "the driver could not reach the hardware" from "the
/// driver is broken".
pub fn self_test() -> SelfTest {
    let mut failures = 0u32;
    let mut case = 0u32;

    // Case 0: 'a' pressed and released.
    let mut d = Decoder::new();
    if d.feed(0x1E) != Some(Event::Make(Key::Code(0x1E)))
        || d.feed(0x9E) != Some(Event::Break(Key::Code(0x1E)))
    {
        failures |= 1 << case;
    }
    case += 1;

    // Case 1: shift+a, the nesting the spike showed —
    // `2a 1e 9e aa`, modifier outermost.
    let mut d = Decoder::new();
    let mut m = Modifiers::default();
    let mut produced = 0u8;
    for byte in [0x2A, 0x1E, 0x9E, 0xAA] {
        if let Some(event) = d.feed(byte) {
            if m.apply(event) {
                continue;
            }
            if let Event::Make(key) = event {
                produced = to_char(key, m);
            }
        }
    }
    // 'A', not 'a': shift was still down when 0x1e arrived.
    if produced != b'A' || m.shift {
        failures |= 1 << case;
    }
    case += 1;

    // Case 2: the extended prefix, `e0 48 e0 c8`. The prefix comes
    // back on release and the byte after it carries the break bit.
    let mut d = Decoder::new();
    let a = d.feed(0xE0);
    let b = d.feed(0x48);
    let c = d.feed(0xE0);
    let e = d.feed(0xC8);
    if a.is_some()
        || b != Some(Event::Make(Key::Extended(SC_EXT_UP)))
        || c.is_some()
        || e != Some(Event::Break(Key::Extended(SC_EXT_UP)))
    {
        failures |= 1 << case;
    }
    case += 1;

    // Case 3: an extended key must not collide with the plain key of
    // the same number. 0x48 unprefixed is the numeric keypad's 8.
    //
    // **This is the case that catches a decoder which drops the
    // prefix.** Such a decoder passes case 2 — it still produces an
    // event — and fails here, because both sequences would decode to
    // the same key.
    let mut d = Decoder::new();
    let plain = d.feed(0x48);
    let mut d = Decoder::new();
    d.feed(0xE0);
    let ext = d.feed(0x48);
    if plain == ext {
        failures |= 1 << case;
    }
    case += 1;

    // Case 4: ctrl+c is 0x03, from `1d 2e ae 9d`.
    let mut d = Decoder::new();
    let mut m = Modifiers::default();
    let mut produced = 0u8;
    for byte in [0x1D, 0x2E, 0xAE, 0x9D] {
        if let Some(event) = d.feed(byte) {
            if m.apply(event) {
                continue;
            }
            if let Event::Make(key) = event {
                produced = to_char(key, m);
            }
        }
    }
    if produced != 0x03 || m.ctrl {
        failures |= 1 << case;
    }
    case += 1;

    // Case 5: a status byte produces no event and is counted.
    let mut d = Decoder::new();
    if d.feed(0xFF).is_some() || d.unrecognised() != 1 {
        failures |= 1 << case;
    }
    case += 1;

    // Case 6: a status byte between a prefix and its key must not
    // leave the prefix set — otherwise the *next* ordinary key would
    // silently become an extended one.
    let mut d = Decoder::new();
    d.feed(0xE0);
    d.feed(0xFF);
    if d.feed(0x1E) != Some(Event::Make(Key::Code(0x1E))) {
        failures |= 1 << case;
    }
    case += 1;

    // Case 7: Pause is swallowed whole and produces nothing. Its
    // second byte is 0x1d, which is left control — a decoder that
    // did not count would report control held down forever.
    let mut d = Decoder::new();
    let mut events = 0u32;
    for byte in [0xE1, 0x1D, 0x45, 0xE1, 0x9D, 0xC5] {
        if d.feed(byte).is_some() {
            events += 1;
        }
    }
    if events != 0 {
        failures |= 1 << case;
    }
    case += 1;

    SelfTest {
        failures,
        cases: case,
    }
}