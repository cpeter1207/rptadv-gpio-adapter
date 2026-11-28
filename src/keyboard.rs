//! Non-grabbing Linux evdev held-key state for one explicitly selected input device.
use super::{ABI_VERSION, GPIO_INVALID_ARGUMENT, GPIO_IO_ERROR, GPIO_OK, GPIO_UNSUPPORTED};
use std::ffi::{CStr, c_char, c_int};
use std::fs::{File, OpenOptions};
use std::mem::size_of;
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::ptr;

// Linux input-event-codes.h KEY_MAX is 0x2ff; both deployed architectures use asm-generic ioctl encoding.
const BYTES: usize = 96;
const KEY_QUERY: libc::c_ulong =
    ((2u32 << 30) | ((BYTES as u32) << 16) | ((b'E' as u32) << 8)) as libc::c_ulong;
const SUPPORTED: libc::c_ulong = KEY_QUERY | 0x21; // EVIOCGBIT(EV_KEY, BYTES)
const PRESSED: libc::c_ulong = KEY_QUERY | 0x18; // EVIOCGKEY(BYTES)

pub struct Keyboard {
    file: File,
    keys: [u32; 2],
    count: usize,
    minimum_bytes: usize,
}

fn read(file: &File, request: libc::c_ulong, minimum_bytes: usize) -> Result<[u8; BYTES], c_int> {
    let mut bits = [0; BYTES];
    // SAFETY: both requests return at most the fixed size encoded in the Linux ioctl.
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request, bits.as_mut_ptr()) };
    if result < minimum_bytes as c_int || result as usize > BYTES {
        Err(GPIO_IO_ERROR)
    } else {
        Ok(bits)
    }
}
fn bit(bits: &[u8; BYTES], key: u32) -> bool {
    bits[key as usize / 8] & (1 << (key % 8)) != 0
}

unsafe extern "C" fn open(
    path: *const c_char,
    keys: *const u32,
    count: u32,
    out: *mut *mut Keyboard,
) -> c_int {
    if out.is_null() || !out.is_aligned() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: caller provides writable output storage for exactly one handle.
    unsafe { out.write(ptr::null_mut()) };
    if path.is_null() || keys.is_null() || !keys.is_aligned() || !(1..=2).contains(&count) {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: the caller lends a terminated pathname and count readable key codes.
    let (path, requested) = unsafe {
        (
            CStr::from_ptr(path),
            std::slice::from_raw_parts(keys, count as usize),
        )
    };
    if path.to_bytes().is_empty()
        || requested
            .iter()
            .any(|key| *key == 0 || *key >= (BYTES * 8) as u32)
    {
        return GPIO_INVALID_ARGUMENT;
    }
    let Ok(file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)
        .open(std::ffi::OsStr::from_bytes(path.to_bytes()))
    else {
        return GPIO_IO_ERROR;
    };
    let minimum_bytes = requested.iter().copied().max().unwrap_or(0) as usize / 8 + 1;
    let supported = match read(&file, SUPPORTED, minimum_bytes) {
        Ok(bits) => bits,
        Err(code) => return code,
    };
    if requested.iter().any(|key| !bit(&supported, *key)) {
        return GPIO_UNSUPPORTED;
    }
    let mut selected = [0; 2];
    selected[..requested.len()].copy_from_slice(requested);
    let owner = Keyboard {
        file,
        keys: selected,
        count: requested.len(),
        minimum_bytes,
    };
    // SAFETY: the validated output receives the sole owner, after capability admission.
    unsafe { out.write(Box::into_raw(Box::new(owner))) };
    GPIO_OK
}
unsafe extern "C" fn sample(device: *mut Keyboard, down: *mut u32) -> c_int {
    if down.is_null() || !down.is_aligned() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: a failed acquisition clears the output word.
    unsafe { down.write(0) };
    if device.is_null() || !device.is_aligned() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: sample borrows the exclusively retained opaque handle for this call.
    let owner = unsafe { &*device };
    let mut valid = 0;
    // SAFETY: this caller supplied a checked output word; valid is a distinct local.
    let result = unsafe { sample_valid(device, down, &mut valid) };
    if result == GPIO_OK && valid != (1 << owner.count) - 1 {
        // SAFETY: the legacy operation has no way to represent partial validity.
        unsafe { down.write(0) };
        return GPIO_UNSUPPORTED;
    }
    result
}

unsafe extern "C" fn sample_valid(device: *mut Keyboard, down: *mut u32, valid: *mut u32) -> c_int {
    if down.is_null()
        || !down.is_aligned()
        || valid.is_null()
        || !valid.is_aligned()
        || down == valid
    {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: failure clears both distinct caller-owned result words.
    unsafe {
        down.write(0);
        valid.write(0);
    }
    if device.is_null() || !device.is_aligned() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: the observation worker retains the opaque owner throughout the call.
    let owner = unsafe { &*device };
    let samples = (|| {
        let supported = read(&owner.file, SUPPORTED, owner.minimum_bytes)?;
        let pressed = read(&owner.file, PRESSED, owner.minimum_bytes)?;
        let mut valid = 0;
        let mut down = 0;
        for (i, key) in owner.keys[..owner.count].iter().enumerate() {
            if bit(&supported, *key) {
                valid |= 1 << i;
                down |= u32::from(bit(&pressed, *key)) << i;
            }
        }
        Ok((down, valid))
    })();
    match samples {
        Ok((levels, available)) => {
            // SAFETY: both checked words remain valid until this call returns.
            unsafe {
                down.write(levels);
                valid.write(available);
            }
            GPIO_OK
        }
        Err(code) => code,
    }
}
unsafe extern "C" fn close(device: *mut Keyboard) {
    if !device.is_null() && device.is_aligned() {
        // SAFETY: close consumes the sole evdev observation handle returned by open.
        drop(unsafe { Box::from_raw(device) });
    }
}
#[repr(C)]
pub struct KeyboardDescriptor {
    size: u32,
    version: u32,
    name: *const c_char,
    open: unsafe extern "C" fn(*const c_char, *const u32, u32, *mut *mut Keyboard) -> c_int,
    sample: unsafe extern "C" fn(*mut Keyboard, *mut u32) -> c_int,
    close: unsafe extern "C" fn(*mut Keyboard),
    sample_valid: unsafe extern "C" fn(*mut Keyboard, *mut u32, *mut u32) -> c_int,
}
// SAFETY: only static immutable strings and functions are published.
unsafe impl Sync for KeyboardDescriptor {}
static API: KeyboardDescriptor = KeyboardDescriptor {
    size: size_of::<KeyboardDescriptor>() as u32,
    version: ABI_VERSION,
    name: c"rptadv.evdev-held-key1".as_ptr(),
    open,
    sample,
    close,
    sample_valid,
};

/// Return the process-lifetime Linux evdev held-key observation descriptor.
#[unsafe(no_mangle)]
pub extern "C" fn rptadv_gpio_keyboard_descriptor() -> *const KeyboardDescriptor {
    &API
}
