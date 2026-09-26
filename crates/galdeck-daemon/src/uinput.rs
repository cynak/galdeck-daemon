//! A virtual keyboard and mouse, made through `/dev/uinput`.
//!
//! Keystroke and scroll actions are sent by a device the kernel treats as real
//! hardware, so they reach whatever the compositor would give a real keyboard
//! and need nothing on the desktop side. The only requirement is read and
//! write access to `/dev/uinput`, which a `uaccess` udev rule grants to the
//! logged-in user. Writing sends the events. Reading brings back the NumLock
//! state, see [`VirtualInput::numlock`].
//!
//! Most of this module follows kernel rules that fail silently when broken:
//!
//! - Every key must be registered before the device is created. The kernel
//!   refuses registration afterwards, and it drops events for unregistered
//!   codes without an error, so [`VirtualInput::open`] takes the whole table.
//! - The compositor needs a moment to pick up a new device, and events sent
//!   before it has done so are lost. The first write waits until
//!   [`VirtualInput::ready_at`].
//! - One device is both the keyboard and the pointer. Two devices would have
//!   no ordering between them, and ctrl + wheel could arrive as a bare wheel.
//! - The device is on the virtual bus. libinput counts a USB pointer as an
//!   external mouse, which can switch the laptop's touchpad off.
//! - The kernel ignores a press of a key that is already down, so chords are
//!   de-duplicated, and every press is paired with a release, even after an
//!   error.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::offset_of;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, Instant};

/// Key codes the device never carries: SysRq, power, sleep, wakeup, suspend,
/// rfkill and screen lock.
///
/// Each of them acts on the machine rather than on the focused window. SysRq
/// is the dangerous one: the kernel's SysRq handler attaches to any keyboard,
/// this one included, and alt + SysRq + a letter is a kernel command. With a
/// common `kernel.sysrq` setting, that command can reboot the machine. Because
/// the device itself refuses these codes, no config can send them, whatever it
/// says.
pub const FORBIDDEN: &[u16] = &[99, 116, 142, 143, 205, 247, 152];

/// How long the compositor is given to pick up a new device.
///
/// The kernel accepts events as soon as the device exists, but it delivers
/// them only to readers that already have the device open. udev and the
/// compositor take somewhere between a few hundred milliseconds and two
/// seconds to open it. The kernel's own uinput example waits one second.
const SETTLE: Duration = Duration::from_secs(1);

/// How long a chord's keys stay down.
///
/// Pressing and releasing in the same instant works for most clients, but a
/// compositor that coalesces input arriving within one frame could see the
/// press and the release together. Holding for a few milliseconds avoids
/// that, and it costs nothing on the worker thread that calls this.
const HOLD: Duration = Duration::from_millis(8);

/// Chords per second, sustained.
///
/// A knob spun hard produces dozens of detents a second, and sending a chord
/// per detent is why one gets bound to a knob. The limit is there to stop a
/// runaway config or a stuck event source from typing into the focused window
/// faster than anyone could stop it.
const RATE: f64 = 50.0;
/// Chords that may be sent at once, before [`RATE`] applies.
const BURST: f64 = 16.0;

/// The most wheel notches one call sends.
///
/// libinput turns each notch into a full scroll step, often three lines. More
/// than this in one event is a knob spun harder than any page usefully
/// scrolls.
const MAX_NOTCHES: i32 = 10;

/// Where the kernel's uinput device lives.
const PATH: &str = "/dev/uinput";

// Event types and codes, from linux/input-event-codes.h. libc has none of
// these.
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const EV_LED: u16 = 0x11;
const SYN_REPORT: u16 = 0;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const LED_NUML: u16 = 0x00;
const KEY_RESERVED: u16 = 0;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;
/// The first code that is not a key or a mouse button.
///
/// Codes from here up are joystick, gamepad, tablet and touch buttons. Any one
/// of them makes udev and libinput classify the device as a joystick or a
/// tablet instead of a keyboard and mouse.
const BTN_JOYSTICK: u16 = 0x120;
/// From linux/input.h.
const BUS_VIRTUAL: u16 = 0x06;

/// The buttons that make udev tag the device as a mouse.
///
/// The tag requires a button in the mouse range in addition to the X and Y
/// axes. Without it, libinput would not treat the wheel as a pointer's wheel.
const BUTTONS: [u16; 3] = [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE];

const PRESS: i32 = 1;
const RELEASE: i32 = 0;

/// `_IOC` from asm-generic/ioctl.h, for the uinput ioctl type `'U'`.
///
/// x86_64 and aarch64 both use the generic encoding. The encoding is written
/// out here because libc's own helpers need a newer libc than the workspace
/// requires.
const fn uinput_ioctl(write: bool, nr: u32, size: usize) -> libc::Ioctl {
    const IOC_WRITE: u32 = 1;
    let direction = if write { IOC_WRITE } else { 0 };
    ((direction << 30) | ((size as u32) << 16) | ((b'U' as u32) << 8) | nr) as libc::Ioctl
}

const UI_DEV_CREATE: libc::Ioctl = uinput_ioctl(false, 1, 0);
const UI_DEV_DESTROY: libc::Ioctl = uinput_ioctl(false, 2, 0);
const UI_DEV_SETUP: libc::Ioctl = uinput_ioctl(true, 3, size_of::<libc::uinput_setup>());
const UI_SET_EVBIT: libc::Ioctl = uinput_ioctl(true, 100, size_of::<libc::c_int>());
const UI_SET_KEYBIT: libc::Ioctl = uinput_ioctl(true, 101, size_of::<libc::c_int>());
const UI_SET_RELBIT: libc::Ioctl = uinput_ioctl(true, 102, size_of::<libc::c_int>());
const UI_SET_LEDBIT: libc::Ioctl = uinput_ioctl(true, 105, size_of::<libc::c_int>());

// The ABI, checked against linux/uinput.h and linux/input.h on x86_64. On a
// target with a different layout the build fails here, rather than writing
// bytes the kernel would misread.
const _: () = assert!(UI_DEV_CREATE == 0x5501 as libc::Ioctl);
const _: () = assert!(UI_DEV_DESTROY == 0x5502 as libc::Ioctl);
const _: () = assert!(UI_DEV_SETUP == 0x405c_5503 as libc::Ioctl);
const _: () = assert!(UI_SET_EVBIT == 0x4004_5564 as libc::Ioctl);
const _: () = assert!(UI_SET_KEYBIT == 0x4004_5565 as libc::Ioctl);
const _: () = assert!(UI_SET_RELBIT == 0x4004_5566 as libc::Ioctl);
const _: () = assert!(UI_SET_LEDBIT == 0x4004_5569 as libc::Ioctl);
const _: () = assert!(size_of::<libc::uinput_setup>() == 92);
const _: () = assert!(offset_of!(libc::uinput_setup, name) == 8);
const _: () = assert!(offset_of!(libc::uinput_setup, ff_effects_max) == 88);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<libc::input_event>() == 24);
// On every target, type, code and value follow the timestamp with no padding.
// Viewing a slice of events as bytes depends on that, because padding bytes
// would be uninitialised memory.
const _: () = assert!(offset_of!(libc::input_event, type_) == 2 * size_of::<libc::c_long>());
const _: () =
    assert!(offset_of!(libc::input_event, code) == offset_of!(libc::input_event, type_) + 2);
const _: () =
    assert!(offset_of!(libc::input_event, value) == offset_of!(libc::input_event, type_) + 4);
const _: () = assert!(size_of::<libc::input_event>() == offset_of!(libc::input_event, value) + 4);

/// A keyboard and wheel mouse that exists for as long as this value does.
///
/// Calls block the caller for a few milliseconds at most, except calls made
/// before [`ready_at`](VirtualInput::ready_at), which first wait for the
/// compositor to pick the device up. Callers should own it on a worker thread,
/// not on the engine loop.
pub struct VirtualInput {
    device: Box<dyn Device>,
    /// Every key code the device was created with, sorted. No other code can
    /// be sent.
    keys: Vec<u16>,
    created_at: Instant,
    bucket: Bucket,
    /// Keys pressed and not yet known to be released, in the order they were
    /// pressed.
    held: Vec<u16>,
    /// The device's NumLock LED as the kernel last reported it, if it has.
    numlock: Option<bool>,
}

// The device is made where a config asks for it and used on a worker thread,
// so it has to be able to move between threads.
const _: () = {
    const fn sendable<T: Send>() {}
    sendable::<VirtualInput>();
};

impl VirtualInput {
    /// Create the device, with every key it will ever send.
    ///
    /// `keys` is the whole table because the kernel accepts no additions once
    /// the device exists. The three mouse buttons are always added. The key
    /// codes are checked before `/dev/uinput` is opened. A [`FORBIDDEN`] code,
    /// 0 (which is no key), or a code from 0x120 up (which would turn the
    /// device into a joystick or a tablet) is refused with
    /// [`io::ErrorKind::InvalidInput`], and no device is created.
    ///
    /// `name` is what `libinput list-devices` and the compositor's settings
    /// show. It is cut to the 79 bytes the kernel keeps, and an empty name is
    /// refused like a bad key.
    ///
    /// Without read and write access to `/dev/uinput` this fails with
    /// [`io::ErrorKind::PermissionDenied`]. Write access alone is not enough.
    pub fn open(name: &str, keys: &[u16]) -> io::Result<VirtualInput> {
        Self::open_with(name, keys, Uinput::open)
    }

    /// [`open`](Self::open), on whatever device `open` returns.
    ///
    /// `open` is called only after `name` and `keys` have passed their
    /// checks, so a refused table never creates anything. Tests pass a fake
    /// device here.
    pub(crate) fn open_with<D: Device + 'static>(
        name: &str,
        keys: &[u16],
        open: impl FnOnce() -> io::Result<D>,
    ) -> io::Result<VirtualInput> {
        let keys = key_table(keys)?;
        let setup = device_setup(name)?;
        let mut device = open()?;
        register(&mut device, &keys, &setup)?;
        let created_at = device.now();
        Ok(VirtualInput {
            device: Box::new(device),
            keys,
            created_at,
            bucket: Bucket::full(created_at),
            held: Vec::new(),
            numlock: None,
        })
    }

    /// When the compositor can be expected to have picked the device up.
    ///
    /// Writes before this are delayed until then, not dropped.
    pub fn ready_at(&self) -> Instant {
        self.created_at + SETTLE
    }

    /// Press `codes` in order, then release them in reverse.
    ///
    /// Each press and each release is a separate input frame. That way the
    /// modifiers reach the client before the key they modify. Repeated codes
    /// are pressed once, at their first position, and an empty chord does
    /// nothing.
    ///
    /// Fails with [`io::ErrorKind::InvalidInput`] if a code was not registered
    /// at [`open`](Self::open), and with [`io::ErrorKind::WouldBlock`] if the
    /// rate limit is used up. Nothing is pressed in either case. If a write
    /// fails part-way, every key already pressed is still released before the
    /// error is returned.
    ///
    /// A key whose release failed stays down. Before pressing anything, this
    /// and [`wheel`](Self::wheel) release such keys, and if one still cannot
    /// be released, they fail without pressing anything. A modifier left down
    /// would otherwise change what they send: `t` would become ctrl + `t`.
    pub fn chord(&mut self, codes: &[u16]) -> io::Result<()> {
        let codes = self.registered(codes)?;
        if codes.is_empty() {
            return Ok(());
        }
        self.release_all()?;
        self.admit()?;
        let pressed = codes.iter().try_for_each(|&code| self.press(code));
        if pressed.is_ok() {
            self.device.sleep(HOLD);
        }
        let released = self.release_all();
        pressed.and(released)
    }

    /// Turn the wheel by `notches`, with `modifiers` held around it.
    ///
    /// Positive `notches` scroll up, or right if `horizontal`. That is the
    /// kernel's convention for real mice, so the desktop's natural-scrolling
    /// setting applies as it would to them. At most ten notches are sent per
    /// call. Zero sends nothing.
    ///
    /// The modifiers are for ctrl + wheel zoom. They share the wheel's device,
    /// so the compositor sees ctrl go down before the wheel turns. Failures
    /// are the same as for [`chord`](Self::chord), and the rate limit is
    /// shared with it. So is releasing keys left down first, since a stray
    /// ctrl would turn a scroll into a zoom.
    pub fn wheel(&mut self, horizontal: bool, notches: i32, modifiers: &[u16]) -> io::Result<()> {
        let modifiers = self.registered(modifiers)?;
        if notches == 0 {
            return Ok(());
        }
        self.release_all()?;
        self.admit()?;
        let axis = if horizontal { REL_HWHEEL } else { REL_WHEEL };
        let turn = [
            event(EV_REL, axis, notches.clamp(-MAX_NOTCHES, MAX_NOTCHES)),
            event(EV_SYN, SYN_REPORT, 0),
        ];
        let turned = modifiers
            .iter()
            .try_for_each(|&code| self.press(code))
            .and_then(|()| self.device.write_events(&turn));
        if turned.is_ok() && !modifiers.is_empty() {
            self.device.sleep(HOLD);
        }
        let released = self.release_all();
        turned.and(released)
    }

    /// Whether NumLock is on, as far as the device can tell.
    ///
    /// The numpad digits type digits only while NumLock is on, and a deck
    /// page of them may be the only numpad the user has, so the caller may
    /// have to tap NumLock before one. The answer is the NumLock LED the
    /// kernel keeps for this device, which the compositor sets from its own
    /// lock state.
    ///
    /// The kernel passes that LED back through the descriptor only when it
    /// changes, plus once when the device is first opened. The console's
    /// keyboard handler opens it while it is being created, and otherwise the
    /// compositor does. A new device's LED is off, so the first report is off,
    /// and a compositor that sets it off again causes no report. Off therefore
    /// means that the compositor has not turned the LED on. That equals its
    /// lock state only if it sets a keyboard's LEDs when it adds one. A
    /// compositor that waits for the next change instead leaves NumLock
    /// reading off while it is on, and one tap made on that reading turns it
    /// off. The next tap turns it on again, and that change is reported.
    ///
    /// The compositor sets the LED when it picks the device up, so a call
    /// before [`ready_at`](Self::ready_at) first waits until then, like the
    /// first write. After that it never waits. `None` means that nothing has
    /// been reported, or that the device could not be read, in which case a
    /// change may be waiting unread.
    pub fn numlock(&mut self) -> Option<bool> {
        self.settle();
        let events = match self.device.read_events() {
            Ok(events) => events,
            Err(e) => {
                log::debug!("reading LED state from the virtual input device: {e}");
                return None;
            }
        };
        // Only the newest report matters. The kernel keeps no more than 16
        // reports and overwrites the oldest, so older ones may be gone anyway.
        let last = events
            .iter()
            .rev()
            .find(|event| event.type_ == EV_LED && event.code == LED_NUML);
        if let Some(last) = last {
            self.numlock = Some(last.value != 0);
        }
        self.numlock
    }

    /// `codes` without repeats, or an error naming the first code the device
    /// cannot send.
    ///
    /// Checked here because the kernel would drop an unregistered code
    /// without an error, and the chord would lose a key without anyone
    /// noticing.
    fn registered(&self, codes: &[u16]) -> io::Result<Vec<u16>> {
        let mut unique: Vec<u16> = Vec::with_capacity(codes.len());
        for &code in codes {
            if self.keys.binary_search(&code).is_err() {
                return Err(invalid(format!(
                    "key code {code} is not registered on the virtual input device"
                )));
            }
            if !unique.contains(&code) {
                unique.push(code);
            }
        }
        Ok(unique)
    }

    /// Wait until [`ready_at`](Self::ready_at), if that is still to come.
    fn settle(&mut self) {
        let now = self.device.now();
        let ready = self.ready_at();
        if now < ready {
            self.device.sleep(ready - now);
        }
    }

    /// Wait out the settle time if the device is new, then take a token from
    /// the rate limit.
    fn admit(&mut self) -> io::Result<()> {
        self.settle();
        if self.bucket.take(self.device.now()) {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "virtual input rate limit reached",
            ))
        }
    }

    /// Press one key.
    ///
    /// The key is recorded as held before the write, so a write that fails
    /// part-way still gets a release. The kernel ignores a release for a key
    /// that never went down, but a key left down stays down.
    fn press(&mut self, code: u16) -> io::Result<()> {
        self.held.push(code);
        self.edge(code, PRESS)
    }

    /// Release every held key, newest first.
    ///
    /// Every key is attempted even after a failure, and the first error is
    /// returned. Keys whose release failed stay held. The next chord or turn
    /// of the wheel tries them again before pressing anything, and so does
    /// dropping the device.
    fn release_all(&mut self) -> io::Result<()> {
        let mut result = Ok(());
        let mut stuck = Vec::new();
        while let Some(code) = self.held.pop() {
            if let Err(e) = self.edge(code, RELEASE) {
                stuck.push(code);
                if result.is_ok() {
                    result = Err(e);
                }
            }
        }
        stuck.reverse();
        self.held = stuck;
        result
    }

    /// One key edge in a frame of its own.
    fn edge(&mut self, code: u16, value: i32) -> io::Result<()> {
        self.device
            .write_events(&[event(EV_KEY, code, value), event(EV_SYN, SYN_REPORT, 0)])
    }
}

impl Drop for VirtualInput {
    /// Release anything still held, then remove the device.
    ///
    /// Closing the descriptor would remove the device anyway, and the kernel
    /// releases held keys when a device goes away. Doing both explicitly means
    /// the compositor sees an ordinary release first.
    fn drop(&mut self) {
        if let Err(e) = self.release_all() {
            log::debug!("releasing keys on the virtual input device: {e}");
        }
        if let Err(e) = self.device.ioctl(UI_DEV_DESTROY, 0) {
            log::debug!("destroying the virtual input device: {e}");
        }
    }
}

impl fmt::Debug for VirtualInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualInput")
            .field("keys", &self.keys.len())
            .field("ready_at", &self.ready_at())
            .field("held", &self.held)
            .field("numlock", &self.numlock)
            .finish_non_exhaustive()
    }
}

/// What [`VirtualInput`] needs from outside: the uinput descriptor, and a
/// clock.
///
/// Behind a trait so tests can see exactly what would reach the kernel, and
/// how long it would have waited, without creating a device on the machine
/// running them. The clock is part of it because the settle wait and the rate
/// limit are among the behaviour most worth testing, and a real clock makes
/// neither testable.
pub(crate) trait Device: Send {
    /// An ioctl that takes an int, or nothing: `UI_SET_*BIT`,
    /// `UI_DEV_CREATE` and `UI_DEV_DESTROY`.
    fn ioctl(&mut self, request: libc::Ioctl, arg: libc::c_int) -> io::Result<()>;
    /// `UI_DEV_SETUP`, the one request that takes a struct.
    fn setup(&mut self, setup: &libc::uinput_setup) -> io::Result<()>;
    /// Hand events to the kernel, in order, in one write.
    fn write_events(&mut self, events: &[libc::input_event]) -> io::Result<()>;
    /// Every event the kernel has sent back to the device, without waiting.
    fn read_events(&mut self) -> io::Result<Vec<libc::input_event>>;

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// The real device: an open `/dev/uinput`.
///
/// Only [`Uinput::open`] knows the path. The rest works on whatever the file
/// is, which is how tests check the byte handling without a device.
struct Uinput {
    file: File,
}

impl Uinput {
    /// Open read-write and non-blocking.
    ///
    /// Read access is for the NumLock LED that the kernel reports back, and
    /// non-blocking means reading it never waits. There is no write-only
    /// fallback: the `uaccess` rule grants both, and so do the usual group
    /// rules (mode 0660). std adds `O_CLOEXEC` itself, which matters: shell
    /// actions are forked from this process, and they can outlive it. An
    /// inherited descriptor would keep a stale device alive after the daemon
    /// had gone.
    fn open() -> io::Result<Uinput> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(PATH)?;
        Ok(Uinput { file })
    }
}

impl Device for Uinput {
    fn ioctl(&mut self, request: libc::Ioctl, arg: libc::c_int) -> io::Result<()> {
        // SAFETY: every request passed here takes an int by value or ignores
        // its argument, so the kernel reads and writes no memory through it.
        let result = unsafe { libc::ioctl(self.file.as_raw_fd(), request, arg) };
        check(result)
    }

    fn setup(&mut self, setup: &libc::uinput_setup) -> io::Result<()> {
        // SAFETY: UI_DEV_SETUP reads one uinput_setup through the pointer. It
        // points to a struct of the size the kernel expects (asserted above),
        // which outlives the call.
        let result = unsafe {
            libc::ioctl(
                self.file.as_raw_fd(),
                UI_DEV_SETUP,
                std::ptr::from_ref(setup),
            )
        };
        check(result)
    }

    fn write_events(&mut self, events: &[libc::input_event]) -> io::Result<()> {
        // SAFETY: input_event is plain integers with no padding (asserted
        // above), so each of its bytes is initialised. The view covers
        // exactly the slice and lives no longer than it.
        let bytes = unsafe {
            std::slice::from_raw_parts(events.as_ptr().cast::<u8>(), size_of_val(events))
        };
        self.file.write_all(bytes)
    }

    fn read_events(&mut self) -> io::Result<Vec<libc::input_event>> {
        let mut events = Vec::new();
        let mut buffer = [event(0, 0, 0); 16];
        loop {
            let read = {
                // SAFETY: as for writing, and in this direction too, any bytes
                // the kernel writes form a valid input_event, because every
                // field is a plain integer.
                let bytes = unsafe {
                    std::slice::from_raw_parts_mut(
                        buffer.as_mut_ptr().cast::<u8>(),
                        size_of_val(&buffer),
                    )
                };
                self.file.read(bytes)
            };
            match read {
                Ok(n) => {
                    // The kernel only ever hands back whole events.
                    events.extend_from_slice(&buffer[..n / size_of::<libc::input_event>()]);
                    if n < size_of_val(&buffer) {
                        return Ok(events);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(events),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }
}

/// Everything the device is, told to the kernel before it exists.
///
/// No key repeat (`EV_REP`): Wayland clients repeat keys themselves, and
/// libinput ignores the kernel's repeats. No high-resolution wheel: if a
/// device advertises it and then sends only whole notches, libinput logs a
/// bug warning.
fn register(device: &mut dyn Device, keys: &[u16], setup: &libc::uinput_setup) -> io::Result<()> {
    device.ioctl(UI_SET_EVBIT, EV_KEY.into())?;
    for &key in keys {
        device.ioctl(UI_SET_KEYBIT, key.into())?;
    }
    device.ioctl(UI_SET_EVBIT, EV_REL.into())?;
    for axis in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL] {
        device.ioctl(UI_SET_RELBIT, axis.into())?;
    }
    device.ioctl(UI_SET_EVBIT, EV_LED.into())?;
    device.ioctl(UI_SET_LEDBIT, LED_NUML.into())?;
    device.setup(setup)?;
    device.ioctl(UI_DEV_CREATE, 0)
}

/// The codes to register: `keys` and the mouse buttons, sorted, without
/// repeats. Refuses codes the device must never have.
fn key_table(keys: &[u16]) -> io::Result<Vec<u16>> {
    for &code in keys {
        if FORBIDDEN.contains(&code) {
            return Err(invalid(format!(
                "key code {code} acts on the machine rather than the focused window, \
                 and is never sent"
            )));
        }
        if code == KEY_RESERVED || code >= BTN_JOYSTICK {
            return Err(invalid(format!(
                "key code {code} is not a keyboard key or mouse button"
            )));
        }
    }
    let mut table: Vec<u16> = keys.iter().copied().chain(BUTTONS).collect();
    table.sort_unstable();
    table.dedup();
    Ok(table)
}

/// The device's identity: its name, and the virtual bus.
fn device_setup(name: &str) -> io::Result<libc::uinput_setup> {
    let name = device_name(name);
    if name.is_empty() {
        // The kernel refuses an empty name too, but with a bare EINVAL.
        return Err(invalid("the virtual input device needs a name".into()));
    }
    // SAFETY: uinput_setup is integers and a char array, for which all zeroes
    // is a valid value. Zero is also what ff_effects_max should be, and the
    // terminator the name needs.
    let mut setup: libc::uinput_setup = unsafe { std::mem::zeroed() };
    setup.id = libc::input_id {
        bustype: BUS_VIRTUAL,
        vendor: 0,
        product: 0,
        version: 1,
    };
    for (to, &from) in setup.name.iter_mut().zip(name.as_bytes()) {
        // c_char is i8 on x86_64 and u8 on aarch64.
        *to = from as libc::c_char;
    }
    Ok(setup)
}

/// The part of `name` the kernel keeps.
///
/// That is everything before the first NUL, and at most 79 bytes. The name is
/// cut on a character boundary, so sysfs and the desktop's settings show valid
/// UTF-8.
fn device_name(name: &str) -> &str {
    let name = name.split('\0').next().unwrap_or_default();
    let mut end = name.len().min(libc::UINPUT_MAX_NAME_SIZE - 1);
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// A token bucket: [`BURST`] tokens at once, refilled at [`RATE`] a second.
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn full(at: Instant) -> Bucket {
        Bucket { tokens: BURST, at }
    }

    /// Take a token if there is one.
    fn take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + elapsed * RATE).min(BURST);
        self.at = self.at.max(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// One event. The kernel stamps events itself, so the time is left zero.
fn event(type_: u16, code: u16, value: i32) -> libc::input_event {
    // SAFETY: input_event is plain integers, for which all zeroes is a valid
    // value. Zeroing it rather than naming the time field also keeps this
    // independent of how each target spells that field.
    let mut event: libc::input_event = unsafe { std::mem::zeroed() };
    event.type_ = type_;
    event.code = code;
    event.value = value;
    event
}

fn check(result: libc::c_int) -> io::Result<()> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn invalid(message: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

/// A [`Device`] that records what it is asked to do, for tests.
#[cfg(test)]
pub(crate) mod fake {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// What a [`FakeDevice`] was asked to do, and what it should do next.
    ///
    /// Shared with the test that made the device, which keeps it after the
    /// device is dropped.
    #[derive(Default)]
    pub(crate) struct Record {
        /// Every `UI_SET_*BIT`, `UI_DEV_CREATE` and `UI_DEV_DESTROY`, in order.
        pub(crate) ioctls: Vec<(libc::Ioctl, libc::c_int)>,
        /// The name and bus given to `UI_DEV_SETUP`.
        pub(crate) setup: Option<(String, u16)>,
        /// Each successful write, as (type, code, value) triples.
        pub(crate) writes: Vec<Vec<(u16, u16, i32)>>,
        /// Writes attempted so far, failed ones included.
        pub(crate) attempts: usize,
        /// Attempts (counting from 1) that fail.
        pub(crate) fail_writes: Vec<usize>,
        /// An ioctl request that fails.
        pub(crate) fail_ioctl: Option<libc::Ioctl>,
        /// Events waiting to be read back, as the kernel would queue them.
        pub(crate) unread: Vec<(u16, u16, i32)>,
        /// Whether reads fail.
        pub(crate) fail_reads: bool,
        /// For each read, how long had been slept before it.
        pub(crate) reads: Vec<Duration>,
        /// Every sleep asked for. Sleeping does not move the clock.
        pub(crate) slept: Vec<Duration>,
        /// How far the clock has been moved since the device was made.
        pub(crate) elapsed: Duration,
    }

    impl Record {
        /// Every event written, in order, however the writes were split.
        pub(crate) fn events(&self) -> Vec<(u16, u16, i32)> {
            self.writes.iter().flatten().copied().collect()
        }
    }

    /// A device on a clock that moves only when the test moves it.
    ///
    /// Sleeps are recorded rather than slept, so a test sees what a real
    /// caller would have waited, without waiting, and a burst of chords
    /// really does arrive at a single instant.
    pub(crate) struct FakeDevice {
        start: Instant,
        record: Arc<Mutex<Record>>,
    }

    impl FakeDevice {
        pub(crate) fn new() -> (FakeDevice, Arc<Mutex<Record>>) {
            let record = Arc::new(Mutex::new(Record::default()));
            let device = FakeDevice {
                start: Instant::now(),
                record: Arc::clone(&record),
            };
            (device, record)
        }

        /// The record, even after a failed assertion poisoned it.
        ///
        /// A test that fails while holding the record still drops its
        /// device, which writes the releases. Panicking again in that drop
        /// would abort the whole test run instead of reporting the failure.
        fn record(&self) -> std::sync::MutexGuard<'_, Record> {
            self.record
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    impl Device for FakeDevice {
        fn ioctl(&mut self, request: libc::Ioctl, arg: libc::c_int) -> io::Result<()> {
            let mut record = self.record();
            if record.fail_ioctl == Some(request) {
                return Err(io::Error::from_raw_os_error(libc::EINVAL));
            }
            record.ioctls.push((request, arg));
            Ok(())
        }

        fn setup(&mut self, setup: &libc::uinput_setup) -> io::Result<()> {
            let name: Vec<u8> = setup
                .name
                .iter()
                .take_while(|&&c| c != 0)
                .map(|&c| c as u8)
                .collect();
            let name = String::from_utf8(name).expect("the name is cut on a character boundary");
            self.record().setup = Some((name, setup.id.bustype));
            Ok(())
        }

        fn write_events(&mut self, events: &[libc::input_event]) -> io::Result<()> {
            let mut record = self.record();
            record.attempts += 1;
            if record.fail_writes.contains(&record.attempts) {
                return Err(io::Error::from_raw_os_error(libc::ENODEV));
            }
            let events = events.iter().map(|e| (e.type_, e.code, e.value)).collect();
            record.writes.push(events);
            Ok(())
        }

        fn read_events(&mut self) -> io::Result<Vec<libc::input_event>> {
            let mut record = self.record();
            let slept = record.slept.iter().sum();
            record.reads.push(slept);
            if record.fail_reads {
                return Err(io::Error::from_raw_os_error(libc::ENODEV));
            }
            let unread = std::mem::take(&mut record.unread);
            Ok(unread.into_iter().map(|(t, c, v)| event(t, c, v)).collect())
        }

        fn now(&self) -> Instant {
            self.start + self.record().elapsed
        }

        fn sleep(&mut self, duration: Duration) {
            self.record().slept.push(duration);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};

    use super::fake::{FakeDevice, Record};
    use super::*;

    const CTRL: u16 = 29;
    const SHIFT: u16 = 42;
    const T: u16 = 20;
    const A: u16 = 30;
    const NUMLOCK: u16 = 69;
    const LED_CAPSL: u16 = 0x01;

    fn syn() -> (u16, u16, i32) {
        (EV_SYN, SYN_REPORT, 0)
    }

    fn down(code: u16) -> Vec<(u16, u16, i32)> {
        vec![(EV_KEY, code, PRESS), syn()]
    }

    fn up(code: u16) -> Vec<(u16, u16, i32)> {
        vec![(EV_KEY, code, RELEASE), syn()]
    }

    /// A device made just now, whose first write will wait to settle.
    fn fresh(keys: &[u16]) -> (VirtualInput, Arc<Mutex<Record>>) {
        let (device, record) = FakeDevice::new();
        let input = VirtualInput::open_with("galdeck test", keys, || Ok(device)).unwrap();
        (input, record)
    }

    /// A device the compositor has had time to pick up.
    fn settled(keys: &[u16]) -> (VirtualInput, Arc<Mutex<Record>>) {
        let (input, record) = fresh(keys);
        record.lock().unwrap().elapsed = SETTLE;
        (input, record)
    }

    fn requests(record: &Record, request: libc::Ioctl) -> Vec<libc::c_int> {
        record
            .ioctls
            .iter()
            .filter(|(r, _)| *r == request)
            .map(|&(_, arg)| arg)
            .collect()
    }

    #[test]
    fn open_registers_everything_before_creating_the_device() {
        let (input, record) = fresh(&[T, CTRL, T, SHIFT, BTN_LEFT]);
        let record = record.lock().unwrap();

        // Keys sorted and without repeats, and the mouse buttons always.
        let keys = requests(&record, UI_SET_KEYBIT);
        assert_eq!(keys, [20, 29, 42, 0x110, 0x111, 0x112]);
        let types = requests(&record, UI_SET_EVBIT);
        assert_eq!(types, [EV_KEY, EV_REL, EV_LED].map(libc::c_int::from));
        // Whole notches only: no high-resolution wheel.
        let axes = requests(&record, UI_SET_RELBIT);
        let expected = [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL].map(libc::c_int::from);
        assert_eq!(axes, expected);
        assert_eq!(
            requests(&record, UI_SET_LEDBIT),
            [libc::c_int::from(LED_NUML)]
        );
        assert_eq!(record.ioctls.last(), Some(&(UI_DEV_CREATE, 0)));
        assert_eq!(
            record.setup,
            Some(("galdeck test".to_string(), BUS_VIRTUAL))
        );
        assert!(record.writes.is_empty());
        assert_eq!(input.ready_at(), input.created_at + Duration::from_secs(1));
    }

    #[test]
    fn forbidden_and_non_keyboard_codes_are_refused_before_opening() {
        let never = || -> io::Result<FakeDevice> { panic!("opened a device for a refused table") };
        let refused = FORBIDDEN
            .iter()
            .chain(&[KEY_RESERVED, BTN_JOYSTICK, 0x14a, 0x2ff]);
        for &code in refused {
            let error = VirtualInput::open_with("galdeck test", &[CTRL, code], never).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "code {code}");
        }
    }

    #[test]
    fn a_device_that_fails_to_register_is_never_destroyed() {
        let (device, record) = FakeDevice::new();
        record.lock().unwrap().fail_ioctl = Some(UI_DEV_CREATE);
        let error = VirtualInput::open_with("galdeck test", &[CTRL], || Ok(device)).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
        assert!(requests(&record.lock().unwrap(), UI_DEV_DESTROY).is_empty());
    }

    #[test]
    fn names_are_cut_to_what_the_kernel_keeps() {
        let long = "x".repeat(100);
        assert_eq!(device_name(&long).len(), 79);
        // Two bytes a character: 39 of them fit, the 40th would straddle 79.
        let wide = "é".repeat(50);
        assert_eq!(device_name(&wide), "é".repeat(39));
        assert_eq!(device_name("galdeck\0virtual"), "galdeck");

        let setup = device_setup(&long).unwrap();
        assert_eq!(setup.name[78], b'x' as libc::c_char);
        assert_eq!(setup.name[79], 0);
        for empty in ["", "\0galdeck"] {
            let error = device_setup(empty).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn a_chord_presses_in_order_and_releases_in_reverse() {
        let (mut input, record) = settled(&[CTRL, SHIFT, T]);
        input.chord(&[CTRL, SHIFT, T]).unwrap();

        let record = record.lock().unwrap();
        // One frame per edge, so the modifiers land before the key.
        let expected = [down(CTRL), down(SHIFT), down(T), up(T), up(SHIFT), up(CTRL)];
        assert_eq!(record.writes, expected);
        assert_eq!(record.slept, [HOLD]);
    }

    #[test]
    fn repeated_codes_are_pressed_once() {
        let (mut input, record) = settled(&[CTRL, T]);
        input.chord(&[CTRL, CTRL, T, CTRL]).unwrap();
        let expected = [down(CTRL), down(T), up(T), up(CTRL)];
        assert_eq!(record.lock().unwrap().writes, expected);
    }

    #[test]
    fn unregistered_codes_are_refused_without_writing() {
        let (mut input, record) = settled(&[CTRL]);
        let error = input.chord(&[CTRL, A]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        let error = input.wheel(false, 1, &[A]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(record.lock().unwrap().writes.is_empty());
    }

    #[test]
    fn an_empty_chord_does_nothing() {
        let (mut input, record) = fresh(&[CTRL]);
        input.chord(&[]).unwrap();
        let record = record.lock().unwrap();
        assert!(record.writes.is_empty());
        // Not even the settle wait.
        assert!(record.slept.is_empty());
    }

    #[test]
    fn an_early_chord_waits_until_the_device_is_ready() {
        let (mut input, record) = fresh(&[CTRL, T]);
        record.lock().unwrap().elapsed = Duration::from_millis(300);
        input.chord(&[CTRL, T]).unwrap();
        assert_eq!(
            record.lock().unwrap().slept,
            [Duration::from_millis(700), HOLD]
        );

        // Once ready, only the hold remains.
        {
            let mut record = record.lock().unwrap();
            record.elapsed = SETTLE;
            record.slept.clear();
        }
        input.chord(&[CTRL, T]).unwrap();
        assert_eq!(record.lock().unwrap().slept, [HOLD]);
    }

    #[test]
    fn a_failed_press_still_releases_what_went_down() {
        let (mut input, record) = settled(&[CTRL, SHIFT, T]);
        // The shift press fails, after ctrl went down.
        record.lock().unwrap().fail_writes = vec![2];
        let error = input.chord(&[CTRL, SHIFT, T]).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENODEV));

        let record = record.lock().unwrap();
        // Shift is released too: the failed write may have got through.
        assert_eq!(record.writes, [down(CTRL), up(SHIFT), up(CTRL)]);
        // No point holding a chord that never formed.
        assert!(record.slept.is_empty());
        drop(record);
        assert!(input.held.is_empty());
    }

    #[test]
    fn a_failed_release_is_retried_on_drop() {
        let (mut input, record) = settled(&[CTRL, T]);
        // Writes: ctrl down, t down, t up, then ctrl up fails.
        record.lock().unwrap().fail_writes = vec![4];
        assert!(input.chord(&[CTRL, T]).is_err());
        assert_eq!(input.held, [CTRL]);

        drop(input);
        let record = record.lock().unwrap();
        assert_eq!(record.writes, [down(CTRL), down(T), up(T), up(CTRL)]);
        assert_eq!(record.ioctls.last(), Some(&(UI_DEV_DESTROY, 0)));
    }

    #[test]
    fn dropping_destroys_the_device() {
        let (input, record) = settled(&[CTRL]);
        drop(input);
        let record = record.lock().unwrap();
        assert!(record.writes.is_empty());
        assert_eq!(requests(&record, UI_DEV_DESTROY), [0]);
        assert_eq!(record.ioctls.last(), Some(&(UI_DEV_DESTROY, 0)));
    }

    #[test]
    fn a_burst_is_allowed_and_then_refused_until_it_refills() {
        let (mut input, record) = settled(&[NUMLOCK]);
        for _ in 0..16 {
            input.chord(&[NUMLOCK]).unwrap();
        }
        let written = record.lock().unwrap().attempts;
        let error = input.chord(&[NUMLOCK]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        // The wheel draws on the same tokens.
        let error = input.wheel(false, 1, &[]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(record.lock().unwrap().attempts, written);

        // A token comes back every 20 ms.
        record.lock().unwrap().elapsed += Duration::from_millis(25);
        input.chord(&[NUMLOCK]).unwrap();
        let error = input.chord(&[NUMLOCK]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn the_wheel_turns_inside_its_modifiers() {
        let (mut input, record) = settled(&[CTRL]);
        input.wheel(false, 1, &[CTRL]).unwrap();
        let turn = vec![(EV_REL, REL_WHEEL, 1), syn()];
        let record = record.lock().unwrap();
        assert_eq!(record.writes, [down(CTRL), turn, up(CTRL)]);
        assert_eq!(record.slept, [HOLD]);
    }

    #[test]
    fn the_wheel_alone_is_one_frame() {
        let (mut input, record) = settled(&[]);
        input.wheel(true, -2, &[]).unwrap();
        let record = record.lock().unwrap();
        assert_eq!(record.writes, [vec![(EV_REL, REL_HWHEEL, -2), syn()]]);
        assert!(record.slept.is_empty());
    }

    #[test]
    fn the_wheel_is_clamped_and_zero_sends_nothing() {
        let (mut input, record) = settled(&[CTRL]);
        input.wheel(false, 0, &[CTRL]).unwrap();
        assert!(record.lock().unwrap().writes.is_empty());

        input.wheel(false, 25, &[]).unwrap();
        input.wheel(false, -25, &[]).unwrap();
        let events = record.lock().unwrap().events();
        let values: Vec<i32> = events
            .iter()
            .filter(|e| e.0 == EV_REL)
            .map(|e| e.2)
            .collect();
        assert_eq!(values, [10, -10]);
    }

    #[test]
    fn a_key_left_down_is_released_before_the_next_chord() {
        let (mut input, record) = settled(&[CTRL, T]);
        // Writes: ctrl down, t down, t up, then ctrl up fails.
        record.lock().unwrap().fail_writes = vec![4];
        assert!(input.chord(&[CTRL, T]).is_err());
        record.lock().unwrap().writes.clear();

        // Otherwise this t would be ctrl + t.
        input.chord(&[T]).unwrap();
        let expected = [up(CTRL), down(T), up(T)];
        assert_eq!(record.lock().unwrap().writes, expected);
        assert!(input.held.is_empty());
    }

    #[test]
    fn a_key_left_down_is_released_before_the_wheel_turns() {
        let (mut input, record) = settled(&[CTRL, T]);
        record.lock().unwrap().fail_writes = vec![4];
        assert!(input.chord(&[CTRL, T]).is_err());
        record.lock().unwrap().writes.clear();

        // Otherwise this scroll would be a ctrl + wheel zoom.
        input.wheel(false, 1, &[]).unwrap();
        let turn = vec![(EV_REL, REL_WHEEL, 1), syn()];
        assert_eq!(record.lock().unwrap().writes, [up(CTRL), turn]);
    }

    #[test]
    fn nothing_is_pressed_while_a_key_cannot_be_released() {
        let (mut input, record) = settled(&[CTRL, T]);
        // The ctrl release fails, and so do the next two tries.
        record.lock().unwrap().fail_writes = vec![4, 5, 6];
        assert!(input.chord(&[CTRL, T]).is_err());
        record.lock().unwrap().writes.clear();

        let error = input.chord(&[T]).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENODEV));
        let error = input.wheel(false, 1, &[]).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ENODEV));
        assert!(record.lock().unwrap().writes.is_empty());
        assert_eq!(input.held, [CTRL]);

        // Once ctrl comes up, chords go out again.
        input.chord(&[T]).unwrap();
        assert_eq!(record.lock().unwrap().writes, [up(CTRL), down(T), up(T)]);
    }

    #[test]
    fn numlock_is_the_led_the_kernel_reports() {
        let (mut input, record) = settled(&[NUMLOCK]);
        // Nothing has opened the device yet.
        assert_eq!(input.numlock(), None);

        // What the kernel reports when the device is first opened: a new
        // device's LED is off.
        record.lock().unwrap().unread = vec![(EV_LED, LED_NUML, 0)];
        assert_eq!(input.numlock(), Some(false));
        // With nothing new, the last state stands.
        assert_eq!(input.numlock(), Some(false));

        // Caps Lock says nothing about NumLock.
        record.lock().unwrap().unread = vec![(EV_LED, LED_CAPSL, 1), syn()];
        assert_eq!(input.numlock(), Some(false));

        record.lock().unwrap().unread = vec![(EV_LED, LED_NUML, 1)];
        assert_eq!(input.numlock(), Some(true));

        // The newest report wins.
        record.lock().unwrap().unread = vec![(EV_LED, LED_NUML, 0), (EV_LED, LED_NUML, 1)];
        assert_eq!(input.numlock(), Some(true));
        record.lock().unwrap().unread = vec![(EV_LED, LED_NUML, 1), (EV_LED, LED_NUML, 0)];
        assert_eq!(input.numlock(), Some(false));
    }

    #[test]
    fn numlock_waits_for_the_compositor_before_reading() {
        let (mut input, record) = fresh(&[NUMLOCK]);
        {
            let mut record = record.lock().unwrap();
            record.elapsed = Duration::from_millis(400);
            record.unread = vec![(EV_LED, LED_NUML, 0), (EV_LED, LED_NUML, 1)];
        }
        assert_eq!(input.numlock(), Some(true));
        // The read came after the wait, when the compositor has had its say.
        let record = record.lock().unwrap();
        assert_eq!(record.slept, [Duration::from_millis(600)]);
        assert_eq!(record.reads, [Duration::from_millis(600)]);
    }

    #[test]
    fn numlock_is_unknown_while_the_device_cannot_be_read() {
        let (mut input, record) = settled(&[NUMLOCK]);
        record.lock().unwrap().unread = vec![(EV_LED, LED_NUML, 0)];
        assert_eq!(input.numlock(), Some(false));

        // A report may be waiting, so the old state is not good enough.
        record.lock().unwrap().fail_reads = true;
        assert_eq!(input.numlock(), None);

        record.lock().unwrap().fail_reads = false;
        assert_eq!(input.numlock(), Some(false));
    }

    /// A [`Uinput`] on one end of a socket pair, and the other end.
    ///
    /// The byte handling does not depend on what the descriptor is, so a
    /// socket stands in for /dev/uinput: what one end writes, the other reads.
    fn socket_device() -> (Uinput, UnixStream) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        ours.set_nonblocking(true).unwrap();
        theirs.set_nonblocking(true).unwrap();
        let file = File::from(OwnedFd::from(ours));
        (Uinput { file }, theirs)
    }

    /// One event as the kernel lays it out on 64-bit targets, put together
    /// byte by byte rather than through `libc::input_event`. `time` goes in
    /// both the seconds and the microseconds.
    #[cfg(target_pointer_width = "64")]
    fn raw(time: i64, type_: u16, code: u16, value: i32) -> [u8; 24] {
        let mut bytes = [0; 24];
        bytes[..8].copy_from_slice(&time.to_ne_bytes());
        bytes[8..16].copy_from_slice(&time.to_ne_bytes());
        bytes[16..18].copy_from_slice(&type_.to_ne_bytes());
        bytes[18..20].copy_from_slice(&code.to_ne_bytes());
        bytes[20..].copy_from_slice(&value.to_ne_bytes());
        bytes
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn events_are_written_as_the_kernel_reads_them() {
        let (mut device, mut theirs) = socket_device();
        let events = [event(EV_KEY, CTRL, PRESS), event(EV_SYN, SYN_REPORT, 0)];
        device.write_events(&events).unwrap();

        let mut bytes = [0; 48];
        theirs.read_exact(&mut bytes).unwrap();
        // No time: the kernel stamps events itself.
        assert_eq!(bytes[..24], raw(0, EV_KEY, CTRL, PRESS));
        assert_eq!(bytes[24..], raw(0, EV_SYN, SYN_REPORT, 0));
        let more = theirs.read(&mut [0; 1]).unwrap_err();
        assert_eq!(more.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn every_waiting_event_is_read_back() {
        let (mut device, mut theirs) = socket_device();
        // Nothing waiting is no error, and no wait.
        assert!(device.read_events().unwrap().is_empty());

        // Reads take 16 events at a time, so these cover one partial read,
        // exactly one full read, one more than that, and several full reads.
        // A time of -1 sets every timestamp bit, so a field read from the
        // wrong place would show.
        for count in [1, 16, 17, 40] {
            let sent: Vec<(u16, u16, i32)> = (0..count).map(|i| (EV_LED, LED_NUML, i)).collect();
            for &(type_, code, value) in &sent {
                theirs.write_all(&raw(-1, type_, code, value)).unwrap();
            }
            let read: Vec<(u16, u16, i32)> = device
                .read_events()
                .unwrap()
                .iter()
                .map(|e| (e.type_, e.code, e.value))
                .collect();
            assert_eq!(read, sent, "{count} events");
            assert!(device.read_events().unwrap().is_empty(), "{count} events");
        }
    }

    /// Creates a real device for about a second and taps shift on it.
    ///
    /// Needs read and write access to /dev/uinput. Run by hand with
    /// `cargo test -p galdeck-daemon --lib uinput -- --ignored`, and watch
    /// `libinput debug-events` or the compositor's device list.
    #[test]
    #[ignore = "creates a real input device"]
    fn a_real_device_can_be_created_and_used() {
        let mut input = VirtualInput::open("galdeck uinput test", &[SHIFT]).unwrap();
        input.chord(&[SHIFT]).unwrap();
        eprintln!("numlock: {:?}", input.numlock());
    }
}
