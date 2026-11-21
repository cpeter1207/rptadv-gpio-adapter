//! Linux dedicated tty modem lines, exclusively owned by the non-real-time service.
use super::{ABI_VERSION, GPIO_INVALID_ARGUMENT, GPIO_IO_ERROR, GPIO_OK};
use std::ffi::{CStr, c_char, c_int};
use std::fs::{File, OpenOptions};
use std::mem::{MaybeUninit, size_of};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::ptr;
use std::sync::Mutex;

#[repr(C)]
struct SerialConfig {
    size: u32,
    version: u32,
    path: *const c_char,
    baud: u32,
    data_bits: u32,
    parity: u32,
    stop_half_bits: u32,
    outputs: u32,
    idle: u32,
}

#[repr(C)]
pub struct SerialDevice {
    ready: bool,
    id: u64,
    file: File,
    outputs: u32,
    idle: u32,
}

const RTS: u32 = 1;
const DTR: u32 = 2;

fn lines(file: &File) -> Result<c_int, c_int> {
    let mut value: c_int = 0;
    // SAFETY: the kernel writes a single integer into this live local.
    let status = unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCMGET, &mut value) };
    (status == 0).then_some(value).ok_or(GPIO_IO_ERROR)
}

fn output_bits(value: c_int) -> u32 {
    (u32::from(value & libc::TIOCM_RTS != 0) * RTS)
        | (u32::from(value & libc::TIOCM_DTR != 0) * DTR)
}

fn apply(file: &File, desired: u32) -> Result<c_int, c_int> {
    let raw = lines(file)?;
    if output_bits(raw) == desired {
        return Ok(raw);
    }
    drive(file, desired)
}

// Both outputs belong to this dedicated tty. Deassertion must be attempted even
// when reading the modem register fails. A write alone never proves the idle level.
fn drive(file: &File, desired: u32) -> Result<c_int, c_int> {
    let mut value = 0;
    if desired & RTS != 0 {
        value |= libc::TIOCM_RTS;
    }
    if desired & DTR != 0 {
        value |= libc::TIOCM_DTR;
    }
    // SAFETY: the exclusive service passes one readable Linux modem-line mask.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCMSET, &value) } != 0 {
        return Err(GPIO_IO_ERROR);
    }
    let observed = lines(file)?;
    (output_bits(observed) == desired)
        .then_some(observed)
        .ok_or(GPIO_IO_ERROR)
}

impl SerialConfig {
    fn flags(&self) -> Option<libc::tcflag_t> {
        let bits = match self.data_bits {
            5 => libc::CS5,
            6 => libc::CS6,
            7 => libc::CS7,
            8 => libc::CS8,
            _ => return None,
        };
        let parity = match self.parity {
            0 => 0,
            1 => libc::PARENB | libc::PARODD,
            2 => libc::PARENB,
            3 => libc::PARENB | libc::PARODD | libc::CMSPAR,
            4 => libc::PARENB | libc::CMSPAR,
            _ => return None,
        };
        let stops = match (self.data_bits, self.stop_half_bits) {
            (_, 2) => 0,
            (5, 3) | (6..=8, 4) => libc::CSTOPB,
            _ => return None,
        };
        (self.baud > 0 && self.outputs & !(RTS | DTR) == 0 && self.idle & !self.outputs == 0)
            .then_some(bits | parity | stops | libc::BOTHER | libc::CREAD | libc::CLOCAL)
    }
}

fn configure(file: &File, config: &SerialConfig, flags: libc::tcflag_t) -> Result<(), c_int> {
    let mut settings = MaybeUninit::<libc::termios2>::uninit();
    // SAFETY: Linux fills this architecture-specific termios2 record on success.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::TCGETS2, settings.as_mut_ptr()) } != 0 {
        return Err(GPIO_IO_ERROR);
    }
    // SAFETY: TCGETS2 succeeded on the complete record.
    let mut settings = unsafe { settings.assume_init() };
    settings.c_iflag = 0;
    settings.c_oflag = 0;
    settings.c_lflag = 0;
    settings.c_cflag = flags;
    settings.c_ispeed = config.baud;
    settings.c_ospeed = config.baud;
    // SAFETY: TCSETS2 consumes the initialized record on this exclusive tty.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::TCSETS2, &settings) } != 0 {
        return Err(GPIO_IO_ERROR);
    }
    // SAFETY: verify what the driver accepted, including parity, speeds and flow control.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::TCGETS2, &mut settings) } != 0
        || settings.c_cflag != flags
        || settings.c_ispeed != config.baud
        || settings.c_ospeed != config.baud
        || settings.c_iflag & (libc::IXON | libc::IXOFF) != 0
    {
        return Err(GPIO_IO_ERROR);
    }
    Ok(())
}

impl Drop for SerialDevice {
    fn drop(&mut self) {
        // SAFETY: release only the exclusivity acquired for this live tty.
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), libc::TIOCNXCL);
        }
    }
}

// Prefix consumers receive a null handle on failed open. Keep any unverified
// exclusive owner until a later open of the same physical tty confirms idle.
static PENDING: Mutex<Vec<(u64, SerialDevice)>> = Mutex::new(Vec::new());

unsafe extern "C" fn open(config: *const SerialConfig, out: *mut *mut SerialDevice) -> c_int {
    // SAFETY: the common acquisition routine validates both ABI pointers.
    unsafe { acquire(config, out, false) }
}

unsafe extern "C" fn open_retiring(
    config: *const SerialConfig,
    out: *mut *mut SerialDevice,
) -> c_int {
    // SAFETY: the extended ABI explicitly transfers a failed owner for retirement.
    unsafe { acquire(config, out, true) }
}

unsafe fn acquire(
    config: *const SerialConfig,
    out: *mut *mut SerialDevice,
    return_failed: bool,
) -> c_int {
    if out.is_null() || !out.is_aligned() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: caller provides writable aligned output storage.
    unsafe {
        *out = ptr::null_mut();
    }
    if config.is_null() || !config.is_aligned() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: the public record starts with two readable u32 words. Check before borrowing its tail.
    let header = unsafe { &*config.cast::<[u32; 2]>() };
    if header[0] < size_of::<SerialConfig>() as u32 || header[1] != ABI_VERSION {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: the validated header covers the full readable input record.
    let config = unsafe { &*config };
    let Some(flags) = config.flags() else {
        return GPIO_INVALID_ARGUMENT;
    };
    if config.path.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: the ABI requires a terminated input path that remains live throughout open.
    let path = Path::new(std::ffi::OsStr::from_bytes(
        unsafe { CStr::from_ptr(config.path) }.to_bytes(),
    ));
    if path.as_os_str().is_empty() {
        return GPIO_INVALID_ARGUMENT;
    }
    let Ok(metadata) = path.metadata() else {
        return GPIO_IO_ERROR;
    };
    let id = metadata.rdev();
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(i) = pending.iter().position(|(dev, _)| *dev == id) {
        let retired = &pending[i].1;
        if drive(&retired.file, retired.idle).is_err() {
            return GPIO_IO_ERROR;
        }
        pending.swap_remove(i);
    }
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK)
        .open(path)
    else {
        return GPIO_IO_ERROR;
    };
    // SAFETY: the fd remains live through both nonblocking exclusive acquisition calls.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0
        || unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCEXCL) } != 0
    {
        return GPIO_IO_ERROR;
    }
    let mut device = SerialDevice {
        ready: false,
        id: file.metadata().map_or(id, |m| m.rdev()),
        file,
        outputs: config.outputs,
        idle: config.idle,
    };
    // Release both modem outputs before line setup, then confirm again after the UART is configured.
    let setup = apply(&device.file, device.idle)
        .and_then(|_| configure(&device.file, config, flags))
        .and_then(|_| apply(&device.file, device.idle));
    if setup.is_err() {
        if drive(&device.file, device.idle).is_err() {
            if return_failed {
                // SAFETY: only open_retiring consumers accept this sole retirement handle.
                unsafe { *out = Box::into_raw(Box::new(device)) };
            } else {
                // Use the acquired fd identity, independent of future pathname aliases.
                pending.push((device.id, device));
            }
        }
        return GPIO_IO_ERROR;
    }
    device.ready = true;
    // SAFETY: exclusive ownership transfers only after the complete startup baseline was confirmed.
    unsafe {
        *out = Box::into_raw(Box::new(device));
    }
    GPIO_OK
}

unsafe extern "C" fn service(
    device: *mut SerialDevice,
    keyed: u32,
    inputs: *mut u32,
    outputs: *mut u32,
) -> c_int {
    if keyed > 1 || device.is_null() || !device.is_aligned() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: the input-only borrow of the owned handle ends before the service operation.
    let selected = if keyed == 0 {
        0
    } else {
        unsafe { (*device).outputs }
    };
    // SAFETY: the caller lends the same unique handle and disjoint output words.
    unsafe { service_outputs(device, selected, inputs, outputs) }
}

unsafe extern "C" fn service_outputs(
    device: *mut SerialDevice,
    active: u32,
    inputs: *mut u32,
    outputs: *mut u32,
) -> c_int {
    if inputs.is_null()
        || !inputs.is_aligned()
        || outputs.is_null()
        || !outputs.is_aligned()
        || inputs == outputs
    {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: the caller owns distinct writable output words.
    unsafe {
        *inputs = 0;
        *outputs = 0;
    }
    if !device.is_aligned() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: the opaque handle is null or the unique owner transferred at open.
    let Some(device) = (unsafe { device.as_mut() }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    if !device.ready {
        return GPIO_IO_ERROR;
    }
    if active & !device.outputs != 0 {
        return GPIO_INVALID_ARGUMENT;
    }
    let requested = device.idle ^ active;
    match apply(&device.file, requested) {
        Ok(raw) => {
            let pins = [
                libc::TIOCM_CTS,
                libc::TIOCM_DSR,
                libc::TIOCM_CAR,
                libc::TIOCM_RNG,
            ];
            let bits = pins
                .iter()
                .enumerate()
                .fold(0, |mask, (i, pin)| mask | (u32::from(raw & pin != 0) << i));
            // SAFETY: both distinct output words remain writable through this call.
            unsafe {
                *inputs = bits;
                *outputs = output_bits(raw);
            }
            GPIO_OK
        }
        Err(code) => {
            let _ = drive(&device.file, device.idle);
            code
        }
    }
}

unsafe extern "C" fn close(device: *mut SerialDevice) -> c_int {
    if !device.is_aligned() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: close requires the unique handle acquired from open.
    let Some(owner) = (unsafe { device.as_mut() }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    if drive(&owner.file, owner.idle).is_err() {
        return GPIO_IO_ERROR;
    }
    // SAFETY: a confirmed idle consumes the sole handle. Failure above retains it for retry.
    drop(unsafe { Box::from_raw(device) });
    GPIO_OK
}

// Preserve electrical ownership when the high-level owner is destroyed during an
// unrecoverable I/O failure. The next open of this tty retries its idle baseline.
unsafe extern "C" fn quarantine(device: *mut SerialDevice) {
    if !device.is_null() && device.is_aligned() {
        // SAFETY: the caller transfers the sole non-null owner.
        let owner = *unsafe { Box::from_raw(device) };
        PENDING
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((owner.id, owner));
    }
}

#[repr(C)]
pub struct SerialDescriptor {
    size: u32,
    version: u32,
    name: *const c_char,
    open: unsafe extern "C" fn(*const SerialConfig, *mut *mut SerialDevice) -> c_int,
    service: unsafe extern "C" fn(*mut SerialDevice, u32, *mut u32, *mut u32) -> c_int,
    close: unsafe extern "C" fn(*mut SerialDevice) -> c_int,
    service_outputs: unsafe extern "C" fn(*mut SerialDevice, u32, *mut u32, *mut u32) -> c_int,
    open_retiring: unsafe extern "C" fn(*const SerialConfig, *mut *mut SerialDevice) -> c_int,
    quarantine: unsafe extern "C" fn(*mut SerialDevice),
}
// SAFETY: every function and the identity string have static lifetime.
unsafe impl Sync for SerialDescriptor {}
static SERIAL: SerialDescriptor = SerialDescriptor {
    size: size_of::<SerialDescriptor>() as u32,
    version: ABI_VERSION,
    name: c"rptadv.serial-modem1".as_ptr(),
    open,
    service,
    close,
    service_outputs,
    open_retiring,
    quarantine,
};

/// Return the immutable dedicated serial modem-line operations on GPIO SONAME 1.
#[unsafe(no_mangle)]
pub extern "C" fn rptadv_gpio_serial_descriptor() -> *const SerialDescriptor {
    &SERIAL
}
