//! Versioned CM119 HID GPIO adapter with a narrow C-compatible descriptor ABI.
//!
//! The sole HID service owner performs libusb I/O outside real-time audio. Native
//! ticks publish output actions and read snapshots through atomics only.

#![deny(warnings)]
#![cfg_attr(coverage, feature(coverage_attribute))]

mod ffi;
mod parallel;

#[cfg(test)]
mod tests;

use std::cell::UnsafeCell;
use std::ffi::{CStr, c_char, c_int};
use std::mem::{offset_of, size_of};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// ABI version implemented by this descriptor.
const ABI_VERSION: u32 = 1;
/// C-Media USB vendor selected by an omitted explicit vendor ID.
const CMEDIA_VENDOR_ID: u16 = 0x0d8c;
/// CM108/CM119 products that use the established four-byte HID report protocol.
const CM119_PRODUCT_FAMILY: [u16; 6] = [0x0008, 0x000c, 0x0012, 0x0013, 0x013a, 0x013c];
/// CM108AH product whose HOOK input is exposed as logical GPIO2.
const CM108AH_PRODUCT_ID: u16 = 0x013c;
/// N1KDO's compatible vendor-defined CM119 product range.
const N1KDO_PRODUCT_FAMILY: u16 = 0x6a00;
/// CM119 HID interface number.
const HID_INTERFACE: c_int = 3;
/// HID report length in bytes.
const HID_REPORT_BYTES: usize = 4;
/// CM119 control-transfer timeout used by the established implementation.
const HID_TIMEOUT_MILLISECONDS: u32 = 20;
/// Conservative spacing retained for CM119/CM109 HID mode transitions.
const HID_SETTLE_DELAY: Duration = Duration::from_millis(3);
/// Mask layout for a packed desired or applied output state.
const OUTPUT_PTT_BIT: u32 = 1;
/// GPIO bits begin after the logical PTT state in an atomic output word.
const OUTPUT_GPIO_SHIFT: u32 = 8;
/// Mask layout for a packed input snapshot state.
const INPUT_COR_BIT: u32 = 1;
/// External CTCSS status bit in a packed input snapshot.
const INPUT_CTCSS_BIT: u32 = 2;
/// GPIO bits begin after logical input states in a packed input word.
const INPUT_GPIO_SHIFT: u32 = 8;
/// Device-info port-chain capacity fixed by the C ABI.
const PORT_CHAIN_CAPACITY: usize = 7;
/// Maximum bytes returned for one USB serial number, including the terminator.
const DEVICE_SERIAL_CAPACITY: usize = 128;
/// Maximum CM119 candidates returned by one discovery snapshot.
const DEVICE_LIST_CAPACITY: usize = 16;
/// Physical CM119 EEPROM address space in 16-bit words.
const EEPROM_WORD_COUNT: usize = 64;
/// First physical word in ASL3's established user tuning region.
const EEPROM_START_WORD: usize = 51;
/// EEPROM word containing the established tuning-image magic.
const EEPROM_MAGIC_WORD: usize = EEPROM_START_WORD;
/// Established CM119 tuning-image magic.
const EEPROM_MAGIC: u16 = 34329;
/// Physical EEPROM word containing the established tuning checksum.
const EEPROM_CHECKSUM_WORD: usize = 63;
/// A flushed-output sentinel that cannot be formed by `pack_output`.
const UNFLUSHED_OUTPUT: u32 = u32::MAX;
/// Reserved low-word bit marking a timed pulse that XORs its baseline.
///
/// CM119 and parallel output masks never use this bit, so one shared packed
/// request can preserve the established active-high parallel pulse behavior
/// and provide the legacy XOR pulse behavior used by CM119 GPIO.
const TIMED_PULSE_INVERT_BASELINE_BIT: u32 = 1 << 31;
/// Stable capability identifier exposed by the descriptor.
const CAPABILITY_NAME: &[u8] = b"rptadv.cm119-hid-gpio\0";

/// Result codes mirrored by the public C header.
const GPIO_OK: c_int = 0;
const GPIO_INVALID_ARGUMENT: c_int = -1;
const GPIO_USB_ERROR: c_int = -3;
const GPIO_UNSUPPORTED: c_int = -4;
/// Result returned when an optional parallel-port transport fails after opening.
const GPIO_IO_ERROR: c_int = -5;

/// Service-owner state for one lock-free timed output pulse.
///
/// Publishers store a packed request and advance its generation without I/O.
/// Only the configured non-real-time service owner mutates this state and
/// converts the request into an output value at a monotonic deadline.
struct TimedPulseState {
    /// Last pulse request observed by the service owner.
    observed_generation: u64,
    /// Output bits active for the current pulse.
    active_pulse_mask: u32,
    /// Whether the active mask XORs rather than ORs its persistent baseline.
    pulse_inverts_baseline: bool,
    /// Monotonic expiry for the active pulse, if any.
    pulse_deadline: Option<Instant>,
}

/// Service-owner state for independently timed baseline-XOR output bits.
///
/// One published request can replace or cancel selected per-bit deadlines.
/// The bounded fixed array keeps the service path allocation-free and lets
/// independent legacy GPIO/parallel pulses overlap without sharing one timer.
struct ScheduledPulseState {
    /// Last observed publication generation for each independently scheduled bit.
    observed_generations: [u64; u32::BITS as usize],
    /// Per-bit monotonic deadlines; `None` means no scheduled inversion.
    deadlines: [Option<Instant>; u32::BITS as usize],
}

impl ScheduledPulseState {
    /// Construct an idle per-bit schedule.
    fn new() -> Self {
        Self {
            observed_generations: [0; u32::BITS as usize],
            deadlines: [None; u32::BITS as usize],
        }
    }

    /// Apply each independently published schedule or cancellation once.
    ///
    /// Per-bit generations retain separate pin requests that arrive before one
    /// service cycle. A later publication for the same bit deliberately wins.
    fn observe_requests(
        &mut self,
        now: Instant,
        durations: &[AtomicU32; u32::BITS as usize],
        generations: &[AtomicU64; u32::BITS as usize],
    ) {
        for bit in 0..u32::BITS as usize {
            let generation = generations[bit].load(Ordering::Acquire);
            if generation != self.observed_generations[bit] {
                self.observed_generations[bit] = generation;
                let duration_milliseconds = durations[bit].load(Ordering::Acquire);
                self.deadlines[bit] = (duration_milliseconds != 0)
                    .then(|| now + Duration::from_millis(u64::from(duration_milliseconds)));
            }
        }
    }

    /// XOR all independently scheduled live bits into one current baseline.
    fn apply_to(&mut self, baseline: u32, now: Instant) -> u32 {
        let mut active_mask = 0_u32;
        for bit in 0..u32::BITS as usize {
            let bit_mask = 1_u32 << bit;
            if self.deadlines[bit].is_some_and(|deadline| now >= deadline) {
                self.deadlines[bit] = None;
            }
            if self.deadlines[bit].is_some() {
                active_mask |= bit_mask;
            }
        }
        baseline ^ active_mask
    }
}

impl TimedPulseState {
    /// Construct an idle service-owner pulse state.
    const fn new() -> Self {
        Self {
            observed_generation: 0,
            active_pulse_mask: 0,
            pulse_inverts_baseline: false,
            pulse_deadline: None,
        }
    }

    /// Observe the newest packed request at the service boundary.
    fn observe_request(&mut self, now: Instant, generation: u64, request: u64) {
        if generation == self.observed_generation {
            return;
        }
        self.observed_generation = generation;
        let request_bits = request as u32;
        let duration_milliseconds = (request >> 32) as u32;
        if duration_milliseconds == 0 {
            self.active_pulse_mask = 0;
            self.pulse_inverts_baseline = false;
            self.pulse_deadline = None;
            return;
        }
        self.active_pulse_mask = request_bits & !TIMED_PULSE_INVERT_BASELINE_BIT;
        self.pulse_inverts_baseline = request_bits & TIMED_PULSE_INVERT_BASELINE_BIT != 0;
        self.pulse_deadline = Some(now + Duration::from_millis(u64::from(duration_milliseconds)));
    }

    /// Return the output value after expiring and applying a current pulse.
    fn apply_to(&mut self, baseline: u32, now: Instant) -> u32 {
        if self.pulse_deadline.is_some_and(|deadline| now >= deadline) {
            self.active_pulse_mask = 0;
            self.pulse_inverts_baseline = false;
            self.pulse_deadline = None;
        }
        if self.pulse_inverts_baseline {
            baseline ^ self.active_pulse_mask
        } else {
            baseline | self.active_pulse_mask
        }
    }
}

/// Publish one packed timed pulse without performing transport I/O.
///
/// The release stores form a generation/request pair for the single service
/// owner.  Callers validate their mask before using this helper.
fn publish_timed_pulse(
    pulse_request: &AtomicU64,
    pulse_generation: &AtomicU64,
    mask: u32,
    duration_milliseconds: u32,
    invert_baseline: bool,
) {
    debug_assert_eq!(mask & TIMED_PULSE_INVERT_BASELINE_BIT, 0);
    let request = (u64::from(duration_milliseconds) << 32)
        | u64::from(mask)
        | (u64::from(invert_baseline) * u64::from(TIMED_PULSE_INVERT_BASELINE_BIT));
    pulse_request.store(request, Ordering::Release);
    pulse_generation.fetch_add(1, Ordering::Release);
}

/// Publish selected independent deadlines without performing transport I/O.
///
/// Each selected pin receives its own publication generation, preserving
/// disjoint requests made before the next service cycle. Callers validate
/// output masks before this helper.
fn publish_scheduled_pulse(
    durations: &[AtomicU32; u32::BITS as usize],
    generations: &[AtomicU64; u32::BITS as usize],
    schedule: u32,
    cancel: u32,
    duration: u32,
) {
    for bit in 0..u32::BITS as usize {
        let bit_mask = 1_u32 << bit;
        if schedule & bit_mask != 0 || cancel & bit_mask != 0 {
            durations[bit].store(
                if schedule & bit_mask != 0 {
                    duration
                } else {
                    0
                },
                Ordering::Release,
            );
            generations[bit].fetch_add(1, Ordering::Release);
        }
    }
}

/// Opaque device handle exposed through the C ABI.
#[repr(C)]
pub struct GpioDevice {
    /// The service owner exclusively mutates this transport.
    transport: UnsafeCell<Box<dyn HidTransport>>,
    /// Immutable CM119 wiring map selected before the device opens.
    profile: Cm119Profile,
    /// Whether the CM108AH HOOK input must retain its legacy GPIO2 mapping.
    cm108ah_hook_gpio2: bool,
    /// Ordinary GPIO bits the selected configuration permits to change.
    gpio_output_enable_mask: u8,
    /// Logical PTT polarity selected before the device opens.
    ptt_inverted: bool,
    /// Latest lock-free desired logical PTT and GPIO output state.
    desired_outputs: AtomicU32,
    /// Latest state successfully sent by the service owner.
    flushed_outputs: AtomicU32,
    /// Latest timed XOR pulse request published without HID I/O.
    pulse_request: AtomicU64,
    /// Generation paired with @ref pulse_request.
    pulse_generation: AtomicU64,
    /// Service-owner-only pulse timing and effective-output state.
    service_state: UnsafeCell<TimedPulseState>,
    /// Per-bit durations for independently expiring baseline-XOR pulses.
    scheduled_pulse_durations: [AtomicU32; u32::BITS as usize],
    /// Per-bit publication generations for independently scheduled pulse bits.
    scheduled_pulse_generations: [AtomicU64; u32::BITS as usize],
    /// Service-owner-only state for independent pulse deadlines.
    scheduled_service_state: UnsafeCell<ScheduledPulseState>,
    /// Latest lock-free decoded COR, CTCSS, and GPIO input state.
    input_state: AtomicU32,
    /// Complete latest raw HID input report represented as little-endian bytes.
    input_report: AtomicU32,
    /// Number of HID input operations attempted by the service owner.
    input_read_count: AtomicU64,
    /// Number of HID output operations attempted by the service owner.
    output_apply_count: AtomicU64,
    /// Number of failed HID operations observed after opening.
    usb_error_count: AtomicU64,
    /// Latest successfully applied logical PTT state.
    ptt_applied: AtomicBool,
    /// True only while the exclusive HID interface is owned by this handle.
    online: AtomicBool,
    /// Last transfer error, reset to zero after a successful transfer.
    last_usb_error: AtomicI32,
    /// EEPROM words successfully read through the service-owner transport.
    eeprom_read_count: AtomicU64,
    /// EEPROM words successfully programmed through the service-owner transport.
    eeprom_write_count: AtomicU64,
}

/// C-compatible stable device-selection structure.
#[repr(C)]
struct DeviceConfig {
    struct_size: u32,
    abi_version: u32,
    usb_port_path: *const c_char,
    vendor_id: u16,
    product_id: u16,
    profile: u32,
    ptt_inverted: u32,
    gpio_output_enable_mask: u32,
    gpio_output_initial_mask: u32,
}

/// C-compatible non-owning device-probe result.
#[repr(C)]
struct DeviceInfo {
    struct_size: u32,
    abi_version: u32,
    present: u32,
    vendor_id: u16,
    product_id: u16,
    usb_bus: u32,
    usb_port_number_count: u32,
    usb_port_numbers: [u8; PORT_CHAIN_CAPACITY],
    serial: [c_char; DEVICE_SERIAL_CAPACITY],
}

/// C-compatible bounded device-discovery snapshot.
#[repr(C)]
struct DeviceList {
    struct_size: u32,
    abi_version: u32,
    matching_device_count: u32,
    returned_device_count: u32,
    devices: [DeviceInfo; DEVICE_LIST_CAPACITY],
}

/// C-compatible physical-addressed CM119 user tuning EEPROM image.
#[repr(C)]
struct EepromImage {
    struct_size: u32,
    abi_version: u32,
    checksum_valid: u32,
    magic_valid: u32,
    words: [u16; EEPROM_WORD_COUNT],
}

/// C-compatible atomic input snapshot.
#[repr(C)]
struct InputSnapshot {
    struct_size: u32,
    abi_version: u32,
    online: u32,
    cor_active: u32,
    ctcss_active: u32,
    gpio_input_mask: u32,
    hid_report: [u8; HID_REPORT_BYTES],
}

/// C-compatible lock-free output publication.
#[repr(C)]
struct OutputAction {
    struct_size: u32,
    abi_version: u32,
    ptt_asserted: u32,
    gpio_output_mask: u32,
}

/// C-compatible timed logical CM119 output inversion request.
#[repr(C)]
struct InvertingPulseAction {
    struct_size: u32,
    abi_version: u32,
    ptt_invert: u32,
    gpio_invert_mask: u32,
    pulse_duration_milliseconds: u32,
    cancel_pulse: u32,
}

/// C-compatible independently scheduled CM119 logical output inversion request.
#[repr(C)]
struct ScheduledInvertingPulseAction {
    struct_size: u32,
    abi_version: u32,
    ptt_invert: u32,
    gpio_invert_mask: u32,
    pulse_duration_milliseconds: u32,
    ptt_cancel: u32,
    gpio_cancel_mask: u32,
}

/// C-compatible lock-free statistics snapshot.
#[repr(C)]
struct DeviceStats {
    struct_size: u32,
    abi_version: u32,
    input_read_count: u64,
    output_apply_count: u64,
    usb_error_count: u64,
    ptt_applied: u32,
    online: u32,
    last_usb_error: i32,
    eeprom_read_count: u64,
    eeprom_write_count: u64,
}

/// Public function-table descriptor layout.
#[repr(C)]
pub struct AdapterDescriptor {
    struct_size: u32,
    abi_version: u32,
    capability_name: *const c_char,
    device_probe: extern "C" fn(*const DeviceConfig, *mut DeviceInfo) -> c_int,
    device_open: extern "C" fn(*const DeviceConfig, *mut *mut GpioDevice) -> c_int,
    device_publish_outputs: extern "C" fn(*mut GpioDevice, *const OutputAction) -> c_int,
    device_service: extern "C" fn(*mut GpioDevice) -> c_int,
    device_get_inputs: extern "C" fn(*const GpioDevice, *mut InputSnapshot) -> c_int,
    device_get_stats: extern "C" fn(*const GpioDevice, *mut DeviceStats) -> c_int,
    device_close: extern "C" fn(*mut GpioDevice),
    device_discover: extern "C" fn(*mut DeviceList) -> c_int,
    device_read_eeprom: extern "C" fn(*mut GpioDevice, *mut EepromImage) -> c_int,
    device_write_eeprom: extern "C" fn(*mut GpioDevice, *mut EepromImage) -> c_int,
    parallel_open:
        extern "C" fn(*const parallel::ParallelConfig, *mut *mut parallel::ParallelDevice) -> c_int,
    parallel_publish_outputs: extern "C" fn(
        *mut parallel::ParallelDevice,
        *const parallel::ParallelOutputAction,
    ) -> c_int,
    parallel_service: extern "C" fn(*mut parallel::ParallelDevice) -> c_int,
    parallel_control_write_data: extern "C" fn(*mut parallel::ParallelDevice, u32) -> c_int,
    parallel_get_inputs: extern "C" fn(
        *const parallel::ParallelDevice,
        *mut parallel::ParallelInputSnapshot,
    ) -> c_int,
    parallel_get_stats:
        extern "C" fn(*const parallel::ParallelDevice, *mut parallel::ParallelStats) -> c_int,
    parallel_close: extern "C" fn(*mut parallel::ParallelDevice),
    device_publish_inverting_pulse:
        extern "C" fn(*mut GpioDevice, *const InvertingPulseAction) -> c_int,
    parallel_publish_inverting_pulse: extern "C" fn(
        *mut parallel::ParallelDevice,
        *const parallel::ParallelInvertingPulseAction,
    ) -> c_int,
    device_schedule_inverting_pulse:
        extern "C" fn(*mut GpioDevice, *const ScheduledInvertingPulseAction) -> c_int,
    parallel_schedule_inverting_pulse: extern "C" fn(
        *mut parallel::ParallelDevice,
        *const parallel::ParallelScheduledInvertingPulseAction,
    ) -> c_int,
    parallel_set_binary_channel: extern "C" fn(*mut parallel::ParallelDevice, u8) -> c_int,
    parallel_program_rtx: extern "C" fn(*mut parallel::ParallelDevice, u32, u32, u32, u32) -> c_int,
    parallel_clear_rtx_transmit: extern "C" fn(*mut parallel::ParallelDevice) -> c_int,
}

/// The descriptor contains immutable function and static-data pointers only.
///
/// Its address is published for the shared object's entire lifetime and no
/// descriptor field is mutated after initialization.
unsafe impl Sync for AdapterDescriptor {}

/// Validated physical USB topology.
#[derive(Clone, Debug, Eq, PartialEq)]
struct UsbPortPath {
    /// libusb bus number.
    bus: u8,
    /// Hub-port chain below the bus.
    ports: Vec<u8>,
}

/// Static wiring details for one established CM119 profile.
#[derive(Clone, Copy)]
struct Cm119Profile {
    /// Initial output-control byte that enables the profile's PTT pin.
    gpio_control: u8,
    /// HID report byte holding the profile's COR input.
    cor_location: usize,
    /// Active-low COR bit in `cor_location`.
    cor_mask: u8,
    /// HID report byte holding the profile's external CTCSS input.
    ctcss_location: usize,
    /// Active-low external-CTCSS bit in `ctcss_location`.
    ctcss_mask: u8,
    /// Logical GPIO bit used for PTT.
    ptt_mask: u8,
    /// GPIO bits safely exposed as ordinary user outputs.
    valid_gpio_mask: u8,
}

/// Setup state constructed after C ABI validation.
struct ValidatedConfig {
    /// Required physical USB path.
    usb_port_path: UsbPortPath,
    /// Resolved USB vendor identifier.
    vendor_id: u16,
    /// Optional exact USB product identifier; absent selects the known family.
    product_id: Option<u16>,
    /// Immutable selected wiring map.
    profile: Cm119Profile,
    /// Logical PTT inversion.
    ptt_inverted: bool,
    /// Ordinary GPIO output permission mask.
    gpio_output_enable_mask: u8,
    /// Initial ordinary GPIO output values.
    gpio_output_initial_mask: u8,
}

/// Minimal transport hidden behind the public CM119 HID contract.
///
/// Only the hardware service owner mutates this object.  Tests use the same
/// narrow interface to prove report conversion without a physical USB device.
trait HidTransport: Send {
    /// Submit a complete CM119 HID output report.
    fn write_report(&mut self, report: [u8; HID_REPORT_BYTES]) -> Result<(), c_int>;
    /// Read a complete CM119 HID input report.
    fn read_report(&mut self) -> Result<[u8; HID_REPORT_BYTES], c_int>;
}

/// Open libusb CM119 HID transport.
struct LibusbHid {
    /// Process-owned libusb context.
    context: *mut ffi::LibusbContext,
    /// Claimed CM119 HID interface.
    handle: *mut ffi::LibusbDeviceHandle,
    /// True after successful claim and before Drop releases it.
    claimed: bool,
}

/// The transport is exclusively accessed through the declared service owner.
unsafe impl Send for LibusbHid {}

/// One opened HID transport paired with its immutable USB product identity.
struct OpenedHid {
    /// Exclusively claimed CM119 HID transport.
    transport: Box<dyn HidTransport>,
    /// Product identity used for product-specific input normalization.
    product_id: u16,
}

/// Error kind used while an exclusive libusb device is opened.
enum OpenError {
    /// The explicitly selected topology did not enumerate.
    Unsupported,
    /// libusb returned a concrete error.
    Usb(c_int),
}

impl Cm119Profile {
    /// Resolve a public hardware-interface profile number.
    fn from_public(value: u32) -> Option<Self> {
        match value {
            0 => Some(Self {
                gpio_control: 0x04,
                cor_location: 0,
                cor_mask: 0x02,
                ctcss_location: 0,
                ctcss_mask: 0x01,
                ptt_mask: 0x04,
                valid_gpio_mask: 0xfb,
            }),
            1 => Some(Self {
                gpio_control: 0x08,
                cor_location: 1,
                cor_mask: 0x04,
                ctcss_location: 1,
                ctcss_mask: 0x02,
                ptt_mask: 0x08,
                valid_gpio_mask: 0x01,
            }),
            2 => Some(Self {
                gpio_control: 0x04,
                cor_location: 0,
                cor_mask: 0x02,
                ctcss_location: 0,
                ctcss_mask: 0x01,
                ptt_mask: 0x04,
                valid_gpio_mask: 0x00,
            }),
            3 => Some(Self {
                gpio_control: 0x0c,
                cor_location: 0,
                cor_mask: 0x02,
                ctcss_location: 1,
                ctcss_mask: 0x02,
                ptt_mask: 0x04,
                valid_gpio_mask: 0x01,
            }),
            _ => None,
        }
    }

    /// Decode one active-low input report into public logical states.
    fn decode_inputs(self, report: [u8; HID_REPORT_BYTES], cm108ah_hook_gpio2: bool) -> u32 {
        let cor = u32::from((report[self.cor_location] & self.cor_mask) == 0);
        let ctcss = u32::from((report[self.ctcss_location] & self.ctcss_mask) == 0);
        let mut gpio = report[1];
        if cm108ah_hook_gpio2 {
            /* The CM108AH repurposes GPIO2 as HOOK.  Preserve the established
             * logical GPIO2 polarity so existing radio wiring keeps working. */
            gpio |= 0x02;
            if report[self.cor_location] & 0x10 != 0 {
                gpio &= !0x02;
            }
        }
        (cor * INPUT_COR_BIT) | (ctcss * INPUT_CTCSS_BIT) | (u32::from(gpio) << INPUT_GPIO_SHIFT)
    }

    /// Form one output report from the latest logical action.
    fn encode_outputs(
        self,
        ptt_inverted: bool,
        gpio_output_enable_mask: u8,
        packed: u32,
    ) -> [u8; HID_REPORT_BYTES] {
        let logical_ptt = packed & OUTPUT_PTT_BIT != 0;
        let physical_ptt = logical_ptt ^ ptt_inverted;
        let ordinary_gpio = (packed >> OUTPUT_GPIO_SHIFT) as u8;
        let mut output = ordinary_gpio;
        if physical_ptt {
            output |= self.ptt_mask;
        }
        [0, output, self.gpio_control | gpio_output_enable_mask, 0]
    }
}

impl UsbPortPath {
    /// Parse one strict stable Linux USB topology string.
    fn parse(value: &CStr) -> Option<Self> {
        let text = value.to_str().ok()?;
        let topology = text.split_once(':').map_or(text, |(path, _)| path);
        let (bus_text, ports_text) = topology.split_once('-')?;
        let bus = bus_text.parse::<u8>().ok()?;
        if bus == 0 || ports_text.is_empty() {
            return None;
        }
        let ports = ports_text
            .split('.')
            .map(str::parse::<u8>)
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        if ports.len() > PORT_CHAIN_CAPACITY || ports.iter().any(|port| *port == 0) {
            return None;
        }
        Some(Self { bus, ports })
    }

    /// Construct a stable topology from one live libusb device.
    unsafe fn from_device(device: *mut ffi::LibusbDevice) -> Option<Self> {
        // SAFETY: caller obtained `device` from the live libusb device list.
        let bus = unsafe { ffi::libusb_get_bus_number(device) };
        if bus == 0 {
            return None;
        }
        let mut ports = [0_u8; PORT_CHAIN_CAPACITY];
        // SAFETY: `ports` has the exact declared capacity and `device` is live.
        let count = unsafe {
            ffi::libusb_get_port_numbers(device, ports.as_mut_ptr(), PORT_CHAIN_CAPACITY as c_int)
        };
        if count <= 0 || count as usize > PORT_CHAIN_CAPACITY {
            return None;
        }
        let ports = ports[..count as usize].to_vec();
        if ports.iter().any(|port| *port == 0) {
            return None;
        }
        Some(Self { bus, ports })
    }

    /// Compare this stable identity with one libusb device.
    unsafe fn matches_device(&self, device: *mut ffi::LibusbDevice) -> bool {
        // SAFETY: caller obtained `device` from the live libusb device list.
        unsafe { Self::from_device(device) }.as_ref() == Some(self)
    }
}

impl ValidatedConfig {
    /// Validate one public C configuration without opening a USB device.
    unsafe fn from_ffi(config: *const DeviceConfig) -> Option<Self> {
        // SAFETY: the caller checks `config` before passing it here.
        let config = unsafe { config.as_ref()? };
        if config.struct_size < size_of::<DeviceConfig>() as u32
            || config.abi_version != ABI_VERSION
            || config.usb_port_path.is_null()
            || config.ptt_inverted > 1
            || config.gpio_output_enable_mask > u32::from(u8::MAX)
            || config.gpio_output_initial_mask > u32::from(u8::MAX)
        {
            return None;
        }
        // SAFETY: nonnull `usb_port_path` is a C string required by the ABI.
        let usb_port_path = UsbPortPath::parse(unsafe { CStr::from_ptr(config.usb_port_path) })?;
        let profile = Cm119Profile::from_public(config.profile)?;
        let enable_mask = config.gpio_output_enable_mask as u8;
        let initial_mask = config.gpio_output_initial_mask as u8;
        if enable_mask & !profile.valid_gpio_mask != 0 || initial_mask & !enable_mask != 0 {
            return None;
        }
        Some(Self {
            usb_port_path,
            vendor_id: if config.vendor_id == 0 {
                CMEDIA_VENDOR_ID
            } else {
                config.vendor_id
            },
            product_id: (config.product_id != 0).then_some(config.product_id),
            profile,
            ptt_inverted: config.ptt_inverted != 0,
            gpio_output_enable_mask: enable_mask,
            gpio_output_initial_mask: initial_mask,
        })
    }
}

impl GpioDevice {
    /// Construct a device around an already acquired transport.
    fn with_transport(
        config: ValidatedConfig,
        transport: Box<dyn HidTransport>,
        product_id: u16,
    ) -> Self {
        Self {
            transport: UnsafeCell::new(transport),
            profile: config.profile,
            cm108ah_hook_gpio2: product_id == CM108AH_PRODUCT_ID,
            gpio_output_enable_mask: config.gpio_output_enable_mask,
            ptt_inverted: config.ptt_inverted,
            desired_outputs: AtomicU32::new(pack_output(false, config.gpio_output_initial_mask)),
            flushed_outputs: AtomicU32::new(UNFLUSHED_OUTPUT),
            pulse_request: AtomicU64::new(0),
            pulse_generation: AtomicU64::new(0),
            service_state: UnsafeCell::new(TimedPulseState::new()),
            scheduled_pulse_durations: std::array::from_fn(|_| AtomicU32::new(0)),
            scheduled_pulse_generations: std::array::from_fn(|_| AtomicU64::new(0)),
            scheduled_service_state: UnsafeCell::new(ScheduledPulseState::new()),
            input_state: AtomicU32::new(0),
            input_report: AtomicU32::new(0),
            input_read_count: AtomicU64::new(0),
            output_apply_count: AtomicU64::new(0),
            usb_error_count: AtomicU64::new(0),
            ptt_applied: AtomicBool::new(false),
            online: AtomicBool::new(true),
            last_usb_error: AtomicI32::new(0),
            eeprom_read_count: AtomicU64::new(0),
            eeprom_write_count: AtomicU64::new(0),
        }
    }

    /// Remember one failed HID operation for lock-free observers.
    fn note_usb_error(&self, error: c_int) {
        self.usb_error_count.fetch_add(1, Ordering::Relaxed);
        self.last_usb_error.store(error, Ordering::Release);
    }

    /// Record a successful HID operation.
    fn note_usb_success(&self) {
        self.last_usb_error.store(0, Ordering::Release);
    }

    /// Publish a prepared logical action without touching libusb.
    fn publish_outputs(&self, action: &OutputAction) -> c_int {
        if action.struct_size < size_of::<OutputAction>() as u32
            || action.abi_version != ABI_VERSION
            || action.ptt_asserted > 1
            || action.gpio_output_mask > u32::from(u8::MAX)
        {
            return GPIO_INVALID_ARGUMENT;
        }
        let gpio = action.gpio_output_mask as u8;
        if gpio & !self.gpio_output_enable_mask != 0 {
            return GPIO_INVALID_ARGUMENT;
        }
        self.desired_outputs.store(
            pack_output(action.ptt_asserted != 0, gpio),
            Ordering::Release,
        );
        GPIO_OK
    }

    /// Publish one timed logical output inversion without touching libusb.
    ///
    /// The service owner XORs the requested PTT and GPIO bits with the latest
    /// persistent output baseline until the deadline.  This matches legacy
    /// CM119 pulse semantics for PTT and the clip LED regardless of configured
    /// logical or physical PTT polarity.
    fn publish_inverting_pulse(&self, action: &InvertingPulseAction) -> c_int {
        if action.struct_size < size_of::<InvertingPulseAction>() as u32
            || action.abi_version != ABI_VERSION
            || action.ptt_invert > 1
            || action.gpio_invert_mask > u32::from(u8::MAX)
            || action.cancel_pulse > 1
            || (action.cancel_pulse != 0
                && (action.pulse_duration_milliseconds != 0
                    || action.ptt_invert != 0
                    || action.gpio_invert_mask != 0))
            || (action.cancel_pulse == 0
                && ((action.pulse_duration_milliseconds == 0)
                    != (action.ptt_invert == 0 && action.gpio_invert_mask == 0)))
        {
            return GPIO_INVALID_ARGUMENT;
        }
        let gpio = action.gpio_invert_mask as u8;
        if gpio & !self.gpio_output_enable_mask != 0 {
            return GPIO_INVALID_ARGUMENT;
        }
        if action.pulse_duration_milliseconds != 0 || action.cancel_pulse != 0 {
            publish_timed_pulse(
                &self.pulse_request,
                &self.pulse_generation,
                pack_output(action.ptt_invert != 0, gpio),
                action.pulse_duration_milliseconds,
                true,
            );
        }
        GPIO_OK
    }

    /// Schedule selected logical PTT/GPIO inversions without touching libusb.
    ///
    /// Each selected bit keeps its own deadline. This preserves independent
    /// legacy GPIO timers while the sole service owner retains all timing and
    /// transport access outside the native tick.
    fn schedule_inverting_pulse(&self, action: &ScheduledInvertingPulseAction) -> c_int {
        if action.struct_size < size_of::<ScheduledInvertingPulseAction>() as u32
            || action.abi_version != ABI_VERSION
            || action.ptt_invert > 1
            || action.ptt_cancel > 1
            || action.gpio_invert_mask > u32::from(u8::MAX)
            || action.gpio_cancel_mask > u32::from(u8::MAX)
        {
            return GPIO_INVALID_ARGUMENT;
        }
        let schedule = pack_output(action.ptt_invert != 0, action.gpio_invert_mask as u8);
        let cancel = pack_output(action.ptt_cancel != 0, action.gpio_cancel_mask as u8);
        if schedule & cancel != 0
            || (action.pulse_duration_milliseconds == 0 && schedule != 0)
            || (action.pulse_duration_milliseconds != 0 && schedule == 0)
            || (unpack_gpio(schedule) | unpack_gpio(cancel)) & !self.gpio_output_enable_mask != 0
        {
            return GPIO_INVALID_ARGUMENT;
        }
        if schedule != 0 || cancel != 0 {
            publish_scheduled_pulse(
                &self.scheduled_pulse_durations,
                &self.scheduled_pulse_generations,
                schedule,
                cancel,
                action.pulse_duration_milliseconds,
            );
        }
        GPIO_OK
    }

    /// Submit one HID output report through the sole service-owner transport.
    fn write_report(&self, report: [u8; HID_REPORT_BYTES]) -> Result<(), c_int> {
        self.output_apply_count.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the descriptor contract reserves this call to one service
        // owner, so this mutable transport access cannot alias another service.
        match unsafe { (*self.transport.get()).write_report(report) } {
            Ok(()) => {
                self.note_usb_success();
                Ok(())
            }
            Err(error) => {
                self.note_usb_error(error);
                Err(error)
            }
        }
    }

    /// Read one HID input report through the sole service-owner transport.
    fn read_report(&self) -> Result<[u8; HID_REPORT_BYTES], c_int> {
        self.input_read_count.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the descriptor contract reserves this call to one service
        // owner, so this mutable transport access cannot alias another service.
        match unsafe { (*self.transport.get()).read_report() } {
            Ok(report) => {
                self.note_usb_success();
                Ok(report)
            }
            Err(error) => {
                self.note_usb_error(error);
                Err(error)
            }
        }
    }

    /// Flush the effective logical PTT and GPIO output when it changed.
    fn flush_outputs_at(&self, now: Instant) -> Result<(), c_int> {
        let requested_generation = self.pulse_generation.load(Ordering::Acquire);
        let requested_pulse = self.pulse_request.load(Ordering::Acquire);
        // SAFETY: the descriptor contract reserves timed state to one service owner.
        let state = unsafe { &mut *self.service_state.get() };
        state.observe_request(now, requested_generation, requested_pulse);
        // SAFETY: the same descriptor contract reserves scheduled state to that owner.
        let scheduled_state = unsafe { &mut *self.scheduled_service_state.get() };
        scheduled_state.observe_requests(
            now,
            &self.scheduled_pulse_durations,
            &self.scheduled_pulse_generations,
        );
        let desired = scheduled_state.apply_to(
            state.apply_to(self.desired_outputs.load(Ordering::Acquire), now),
            now,
        );
        if desired == self.flushed_outputs.load(Ordering::Acquire) {
            return Ok(());
        }
        let report =
            self.profile
                .encode_outputs(self.ptt_inverted, self.gpio_output_enable_mask, desired);
        self.write_report(report)?;
        self.flushed_outputs.store(desired, Ordering::Release);
        self.ptt_applied
            .store(desired & OUTPUT_PTT_BIT != 0, Ordering::Release);
        Ok(())
    }

    /// Flush the newest effective output and poll one HID input report.
    fn service_at(&self, now: Instant) -> c_int {
        if self.flush_outputs_at(now).is_err() {
            return GPIO_USB_ERROR;
        }
        match self.read_report() {
            Ok(report) => {
                self.input_report
                    .store(u32::from_le_bytes(report), Ordering::Release);
                self.input_state.store(
                    self.profile.decode_inputs(report, self.cm108ah_hook_gpio2),
                    Ordering::Release,
                );
                GPIO_OK
            }
            Err(_) => GPIO_USB_ERROR,
        }
    }

    /// Flush and poll using the current monotonic time.
    fn service(&self) -> c_int {
        self.service_at(Instant::now())
    }

    /// Copy the latest service-owned input report through lock-free atomics.
    fn inputs(&self, snapshot: &mut InputSnapshot) -> c_int {
        if snapshot.struct_size < size_of::<InputSnapshot>() as u32 {
            return GPIO_INVALID_ARGUMENT;
        }
        let state = self.input_state.load(Ordering::Acquire);
        snapshot.abi_version = ABI_VERSION;
        snapshot.online = u32::from(self.online.load(Ordering::Acquire));
        snapshot.cor_active = u32::from(state & INPUT_COR_BIT != 0);
        snapshot.ctcss_active = u32::from(state & INPUT_CTCSS_BIT != 0);
        snapshot.gpio_input_mask = state >> INPUT_GPIO_SHIFT;
        snapshot.hid_report = self.input_report.load(Ordering::Acquire).to_le_bytes();
        GPIO_OK
    }

    /// Copy lock-free HID service statistics.
    fn stats(&self, stats: &mut DeviceStats) -> c_int {
        if stats.struct_size < offset_of!(DeviceStats, eeprom_read_count) as u32 {
            return GPIO_INVALID_ARGUMENT;
        }
        stats.abi_version = ABI_VERSION;
        stats.input_read_count = self.input_read_count.load(Ordering::Acquire);
        stats.output_apply_count = self.output_apply_count.load(Ordering::Acquire);
        stats.usb_error_count = self.usb_error_count.load(Ordering::Acquire);
        stats.ptt_applied = u32::from(self.ptt_applied.load(Ordering::Acquire));
        stats.online = u32::from(self.online.load(Ordering::Acquire));
        stats.last_usb_error = self.last_usb_error.load(Ordering::Acquire);
        if stats.struct_size >= size_of::<DeviceStats>() as u32 {
            stats.eeprom_read_count = self.eeprom_read_count.load(Ordering::Acquire);
            stats.eeprom_write_count = self.eeprom_write_count.load(Ordering::Acquire);
        }
        GPIO_OK
    }

    /// Read one established 16-bit EEPROM word through CM119 HID control reports.
    fn read_eeprom_word(&self, address: u8) -> Result<u16, c_int> {
        let command = [0x80, 0, 0, 0x80 | (address & 0x3f)];
        self.write_report(command)?;
        let response = self.read_report()?;
        self.eeprom_read_count.fetch_add(1, Ordering::Relaxed);
        Ok(u16::from(response[1]) | (u16::from(response[2]) << 8))
    }

    /// Program one established 16-bit EEPROM word through CM119 HID control reports.
    fn write_eeprom_word(&self, address: u8, value: u16) -> Result<(), c_int> {
        let command = [
            0x80,
            value as u8,
            (value >> 8) as u8,
            0xc0 | (address & 0x3f),
        ];
        self.write_report(command)?;
        self.eeprom_write_count.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    /// Restore the most recently published logical outputs after EEPROM control traffic.
    fn restore_outputs_after_eeprom(&self) -> Result<(), c_int> {
        self.flushed_outputs
            .store(UNFLUSHED_OUTPUT, Ordering::Release);
        self.flush_outputs_at(Instant::now())
    }

    /// Read the established ASL3 CM119 user tuning region through the service owner.
    fn read_eeprom(&self, image: &mut EepromImage) -> c_int {
        if image.struct_size < size_of::<EepromImage>() as u32 {
            return GPIO_INVALID_ARGUMENT;
        }
        image.abi_version = ABI_VERSION;
        image.checksum_valid = 0;
        image.magic_valid = 0;
        image.words = [0; EEPROM_WORD_COUNT];
        let result = (|| -> Result<(), c_int> {
            for address in EEPROM_START_WORD..=EEPROM_CHECKSUM_WORD {
                image.words[address] = self.read_eeprom_word(address as u8)?;
            }
            Ok(())
        })();
        let restore = self.restore_outputs_after_eeprom();
        if result.is_err() || restore.is_err() {
            return GPIO_USB_ERROR;
        }
        image.checksum_valid = u32::from(eeprom_checksum_is_valid(&image.words));
        image.magic_valid = u32::from(image.words[EEPROM_MAGIC_WORD] == EEPROM_MAGIC);
        GPIO_OK
    }

    /// Validate and program the established ASL3 CM119 user tuning region through the service owner.
    fn write_eeprom(&self, image: &mut EepromImage) -> c_int {
        if image.struct_size < size_of::<EepromImage>() as u32 || image.abi_version != ABI_VERSION {
            return GPIO_INVALID_ARGUMENT;
        }
        image.words[EEPROM_MAGIC_WORD] = EEPROM_MAGIC;
        image.words[EEPROM_CHECKSUM_WORD] = eeprom_checksum_word(&image.words);
        let result = (|| -> Result<(), c_int> {
            for address in EEPROM_START_WORD..EEPROM_CHECKSUM_WORD {
                self.write_eeprom_word(address as u8, image.words[address])?;
            }
            self.write_eeprom_word(
                EEPROM_CHECKSUM_WORD as u8,
                image.words[EEPROM_CHECKSUM_WORD],
            )
        })();
        let restore = self.restore_outputs_after_eeprom();
        if result.is_err() || restore.is_err() {
            return GPIO_USB_ERROR;
        }
        image.checksum_valid = 1;
        image.magic_valid = 1;
        GPIO_OK
    }

    /// Best-effort physical unkey before the exclusive transport is released.
    fn close(&mut self) {
        publish_timed_pulse(&self.pulse_request, &self.pulse_generation, 0, 0, false);
        publish_scheduled_pulse(
            &self.scheduled_pulse_durations,
            &self.scheduled_pulse_generations,
            0,
            u32::MAX,
            0,
        );
        self.desired_outputs.store(
            pack_output(
                false,
                unpack_gpio(self.desired_outputs.load(Ordering::Acquire)),
            ),
            Ordering::Release,
        );
        let _ = self.service();
        self.online.store(false, Ordering::Release);
    }
}

/// Pack a logical PTT state and ordinary GPIO byte into one atomic word.
fn pack_output(ptt: bool, gpio: u8) -> u32 {
    (u32::from(ptt) * OUTPUT_PTT_BIT) | (u32::from(gpio) << OUTPUT_GPIO_SHIFT)
}

/// Extract the ordinary GPIO byte from one packed output word.
fn unpack_gpio(packed: u32) -> u8 {
    (packed >> OUTPUT_GPIO_SHIFT) as u8
}

/// Calculate the established two's-complement checksum for one tuning image.
fn eeprom_checksum_word(words: &[u16; EEPROM_WORD_COUNT]) -> u16 {
    let sum = words[EEPROM_START_WORD..EEPROM_CHECKSUM_WORD]
        .iter()
        .fold(u16::MAX, |sum, value| sum.wrapping_add(*value));
    sum.wrapping_neg()
}

/// Verify the established checksum stored within one complete tuning image.
fn eeprom_checksum_is_valid(words: &[u16; EEPROM_WORD_COUNT]) -> bool {
    let sum = words[EEPROM_START_WORD..=EEPROM_CHECKSUM_WORD]
        .iter()
        .fold(u16::MAX, |sum, value| sum.wrapping_add(*value));
    sum == 0
}

impl LibusbHid {
    /// Open and exclusively claim the precise CM119 HID device selected by configuration.
    fn open(config: &ValidatedConfig) -> Result<OpenedHid, OpenError> {
        let mut context = ptr::null_mut();
        // SAFETY: `context` is valid storage for libusb's result pointer.
        let result = unsafe { ffi::libusb_init(&mut context) };
        if result != ffi::LIBUSB_SUCCESS || context.is_null() {
            return Err(OpenError::Usb(result));
        }
        let mut list = ptr::null_mut();
        // SAFETY: the initialized context remains live until list processing finishes.
        let count = unsafe { ffi::libusb_get_device_list(context, &mut list) };
        if count < 0 {
            // SAFETY: `context` was initialized above.
            unsafe { ffi::libusb_exit(context) };
            return Err(OpenError::Usb(count as c_int));
        }
        let mut outcome = Err(OpenError::Unsupported);
        for index in 0..count as usize {
            // SAFETY: index stays within the list count returned by libusb.
            let device = unsafe { *list.add(index) };
            if device.is_null() || !device_matches(device, config) {
                continue;
            }
            let Some(descriptor) = device_descriptor(device) else {
                continue;
            };
            // SAFETY: matching device belongs to the live libusb list.
            outcome = Self::open_device(context, device, descriptor.id_product);
            break;
        }
        // SAFETY: list belongs to this call and device handles retain their own references.
        unsafe { ffi::libusb_free_device_list(list, 1) };
        if outcome.is_err() {
            // SAFETY: on success ownership moved into `LibusbHid`.
            unsafe { ffi::libusb_exit(context) };
        }
        outcome
    }

    /// Open and claim one already-matched CM119 device.
    fn open_device(
        context: *mut ffi::LibusbContext,
        device: *mut ffi::LibusbDevice,
        product_id: u16,
    ) -> Result<OpenedHid, OpenError> {
        let mut handle = ptr::null_mut();
        // SAFETY: `device` was obtained from the active device list.
        let open_result = unsafe { ffi::libusb_open(device, &mut handle) };
        if open_result != ffi::LIBUSB_SUCCESS || handle.is_null() {
            return Err(OpenError::Usb(open_result));
        }
        // SAFETY: handle is live until it is closed below or transferred to the result.
        let mut claim_result = unsafe { ffi::libusb_claim_interface(handle, HID_INTERFACE) };
        if claim_result != ffi::LIBUSB_SUCCESS {
            // SAFETY: querying/detaching this live handle is a control-plane operation.
            let active = unsafe { ffi::libusb_kernel_driver_active(handle, HID_INTERFACE) };
            if active > 0 {
                // SAFETY: the interface is still owned by a kernel driver.
                let detach = unsafe { ffi::libusb_detach_kernel_driver(handle, HID_INTERFACE) };
                if detach == ffi::LIBUSB_SUCCESS {
                    // SAFETY: retry claim after the successful detach.
                    claim_result = unsafe { ffi::libusb_claim_interface(handle, HID_INTERFACE) };
                }
            }
        }
        if claim_result != ffi::LIBUSB_SUCCESS {
            // SAFETY: this path still owns the open handle and context.
            unsafe { ffi::libusb_close(handle) };
            return Err(OpenError::Usb(claim_result));
        }
        Ok(OpenedHid {
            transport: Box::new(Self {
                context,
                handle,
                claimed: true,
            }),
            product_id,
        })
    }
}

impl HidTransport for LibusbHid {
    /// Write exactly one CM119 HID output report.
    fn write_report(&mut self, mut report: [u8; HID_REPORT_BYTES]) -> Result<(), c_int> {
        thread::sleep(HID_SETTLE_DELAY);
        // SAFETY: handle is exclusively owned by this transport and report is writable storage.
        let result = unsafe {
            ffi::libusb_control_transfer(ffi::LibusbControlTransfer {
                handle: self.handle,
                request_type: ffi::LIBUSB_ENDPOINT_OUT
                    | ffi::LIBUSB_REQUEST_TYPE_CLASS
                    | ffi::LIBUSB_RECIPIENT_INTERFACE,
                request: ffi::HID_REPORT_SET,
                value: ffi::HID_REPORT_OUTPUT,
                index: HID_INTERFACE as u16,
                data: report.as_mut_ptr(),
                length: HID_REPORT_BYTES as u16,
                timeout_milliseconds: HID_TIMEOUT_MILLISECONDS,
            })
        };
        if result == HID_REPORT_BYTES as c_int {
            Ok(())
        } else {
            Err(if result < 0 { result } else { -1 })
        }
    }

    /// Read exactly one CM119 HID input report.
    fn read_report(&mut self) -> Result<[u8; HID_REPORT_BYTES], c_int> {
        let mut report = [0_u8; HID_REPORT_BYTES];
        thread::sleep(HID_SETTLE_DELAY);
        // SAFETY: handle is exclusively owned by this transport and report is writable storage.
        let result = unsafe {
            ffi::libusb_control_transfer(ffi::LibusbControlTransfer {
                handle: self.handle,
                request_type: ffi::LIBUSB_ENDPOINT_IN
                    | ffi::LIBUSB_REQUEST_TYPE_CLASS
                    | ffi::LIBUSB_RECIPIENT_INTERFACE,
                request: ffi::HID_REPORT_GET,
                value: ffi::HID_REPORT_INPUT,
                index: HID_INTERFACE as u16,
                data: report.as_mut_ptr(),
                length: HID_REPORT_BYTES as u16,
                timeout_milliseconds: HID_TIMEOUT_MILLISECONDS,
            })
        };
        if result == HID_REPORT_BYTES as c_int {
            Ok(report)
        } else {
            Err(if result < 0 { result } else { -1 })
        }
    }
}

impl Drop for LibusbHid {
    /// Release the claimed HID interface and its process context.
    fn drop(&mut self) {
        if self.claimed {
            // SAFETY: Drop remains the sole owner of the claimed handle.
            unsafe { ffi::libusb_release_interface(self.handle, HID_INTERFACE) };
            self.claimed = false;
        }
        if !self.handle.is_null() {
            // SAFETY: the handle has not been closed elsewhere.
            unsafe { ffi::libusb_close(self.handle) };
            self.handle = ptr::null_mut();
        }
        if !self.context.is_null() {
            // SAFETY: this transport owns the context after a successful open.
            unsafe { ffi::libusb_exit(self.context) };
            self.context = ptr::null_mut();
        }
    }
}

/// Return whether a libusb device precisely matches the configured stable identity.
fn device_matches(device: *mut ffi::LibusbDevice, config: &ValidatedConfig) -> bool {
    let Some(descriptor) = device_descriptor(device) else {
        return false;
    };
    descriptor.id_vendor == config.vendor_id
        && product_matches(config.product_id, descriptor.id_product)
        // SAFETY: `device` remains live while the caller owns its enumeration list.
        && unsafe { config.usb_port_path.matches_device(device) }
}

/// Return one initialized libusb device descriptor when it can be read.
fn device_descriptor(device: *mut ffi::LibusbDevice) -> Option<ffi::LibusbDeviceDescriptor> {
    let mut descriptor = std::mem::MaybeUninit::<ffi::LibusbDeviceDescriptor>::uninit();
    // SAFETY: `device` is a live entry from the libusb enumeration list.
    if unsafe { ffi::libusb_get_device_descriptor(device, descriptor.as_mut_ptr()) }
        != ffi::LIBUSB_SUCCESS
    {
        return None;
    }
    // SAFETY: libusb initialized the descriptor after reporting success.
    Some(unsafe { descriptor.assume_init() })
}

/// Read an optional USB serial without claiming the CM119 HID interface.
fn device_serial(
    device: *mut ffi::LibusbDevice,
    descriptor: ffi::LibusbDeviceDescriptor,
) -> [c_char; DEVICE_SERIAL_CAPACITY] {
    let mut serial = [0; DEVICE_SERIAL_CAPACITY];
    if descriptor.i_serial_number == 0 {
        return serial;
    }
    let mut handle = ptr::null_mut();
    // SAFETY: `device` is live in the caller's libusb device list.
    if unsafe { ffi::libusb_open(device, &mut handle) } != ffi::LIBUSB_SUCCESS || handle.is_null() {
        return serial;
    }
    let mut bytes = [0_u8; DEVICE_SERIAL_CAPACITY - 1];
    // SAFETY: the temporary open handle and output buffer remain valid for this call.
    let count = unsafe {
        ffi::libusb_get_string_descriptor_ascii(
            handle,
            descriptor.i_serial_number,
            bytes.as_mut_ptr(),
            bytes.len() as c_int,
        )
    };
    // SAFETY: this function owns the temporary unclaimed handle.
    unsafe { ffi::libusb_close(handle) };
    if count > 0 {
        let length = usize::min(count as usize, bytes.len());
        for (destination, source) in serial.iter_mut().zip(bytes[..length].iter()) {
            *destination = *source as c_char;
        }
    }
    serial
}

/// Accept an explicit product or any known CM108/CM119 HID product.
fn product_matches(requested: Option<u16>, actual: u16) -> bool {
    requested.map_or_else(
        || CM119_PRODUCT_FAMILY.contains(&actual) || actual & 0xff00 == N1KDO_PRODUCT_FAMILY,
        |product| product == actual,
    )
}

/// Return whether a descriptor belongs to the supported C-Media HID family.
fn supported_cm119_descriptor(descriptor: ffi::LibusbDeviceDescriptor) -> bool {
    descriptor.id_vendor == CMEDIA_VENDOR_ID && product_matches(None, descriptor.id_product)
}

/// Construct one empty full-size device-info entry for discovery output.
fn empty_device_info() -> DeviceInfo {
    DeviceInfo {
        struct_size: size_of::<DeviceInfo>() as u32,
        abi_version: ABI_VERSION,
        present: 0,
        vendor_id: 0,
        product_id: 0,
        usb_bus: 0,
        usb_port_number_count: 0,
        usb_port_numbers: [0; PORT_CHAIN_CAPACITY],
        serial: [0; DEVICE_SERIAL_CAPACITY],
    }
}

/// Reset the prefix that every ABI-1 device-info consumer can receive safely.
fn clear_device_info(info: &mut DeviceInfo) {
    info.abi_version = ABI_VERSION;
    info.present = 0;
    info.vendor_id = 0;
    info.product_id = 0;
    info.usb_bus = 0;
    info.usb_port_number_count = 0;
    info.usb_port_numbers = [0; PORT_CHAIN_CAPACITY];
    if info.struct_size >= size_of::<DeviceInfo>() as u32 {
        info.serial = [0; DEVICE_SERIAL_CAPACITY];
    }
}

/// Populate non-owning probe information for one matching device.
fn fill_device_info(
    info: &mut DeviceInfo,
    topology: &UsbPortPath,
    descriptor: ffi::LibusbDeviceDescriptor,
    serial: [c_char; DEVICE_SERIAL_CAPACITY],
) {
    clear_device_info(info);
    info.present = 1;
    info.vendor_id = descriptor.id_vendor;
    info.product_id = descriptor.id_product;
    info.usb_bus = u32::from(topology.bus);
    info.usb_port_number_count = topology.ports.len() as u32;
    info.usb_port_numbers[..topology.ports.len()].copy_from_slice(&topology.ports);
    if info.struct_size >= size_of::<DeviceInfo>() as u32 {
        info.serial = serial;
    }
}

/// Probe a configured CM119 without claiming its HID interface.
extern "C" fn device_probe(config: *const DeviceConfig, info: *mut DeviceInfo) -> c_int {
    if info.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: nonnull pointer checked above; caller owns writable result storage.
    let info = unsafe { &mut *info };
    if info.struct_size < offset_of!(DeviceInfo, serial) as u32 {
        return GPIO_INVALID_ARGUMENT;
    }
    clear_device_info(info);
    // SAFETY: public configuration is read-only and fully validated before use.
    let Some(config) = (unsafe { ValidatedConfig::from_ffi(config) }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    let mut context = ptr::null_mut();
    // SAFETY: `context` is valid result storage.
    let initialize = unsafe { ffi::libusb_init(&mut context) };
    if initialize != ffi::LIBUSB_SUCCESS || context.is_null() {
        return GPIO_USB_ERROR;
    }
    let mut list = ptr::null_mut();
    // SAFETY: initialized context is live during the enumeration.
    let count = unsafe { ffi::libusb_get_device_list(context, &mut list) };
    if count < 0 {
        // SAFETY: context was initialized above.
        unsafe { ffi::libusb_exit(context) };
        return GPIO_USB_ERROR;
    }
    let mut found = false;
    for index in 0..count as usize {
        // SAFETY: index stays within the list count returned by libusb.
        let device = unsafe { *list.add(index) };
        if !device.is_null() && device_matches(device, &config) {
            if let (Some(descriptor), Some(topology)) = (
                device_descriptor(device),
                // SAFETY: the device remains live for this list iteration.
                unsafe { UsbPortPath::from_device(device) },
            ) {
                fill_device_info(
                    info,
                    &topology,
                    descriptor,
                    device_serial(device, descriptor),
                );
                found = true;
                break;
            }
        }
    }
    // SAFETY: this function owns the returned list and context.
    unsafe {
        ffi::libusb_free_device_list(list, 1);
        ffi::libusb_exit(context);
    }
    if found { GPIO_OK } else { GPIO_UNSUPPORTED }
}

/// Enumerate bounded supported CM119 candidates without claiming an HID interface.
extern "C" fn device_discover(list: *mut DeviceList) -> c_int {
    if list.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: nonnull pointer checked above; caller owns writable list storage.
    let list = unsafe { &mut *list };
    if list.struct_size < size_of::<DeviceList>() as u32 {
        return GPIO_INVALID_ARGUMENT;
    }
    list.abi_version = ABI_VERSION;
    list.matching_device_count = 0;
    list.returned_device_count = 0;
    for info in &mut list.devices {
        *info = empty_device_info();
    }
    let mut context = ptr::null_mut();
    // SAFETY: `context` is valid result storage.
    if unsafe { ffi::libusb_init(&mut context) } != ffi::LIBUSB_SUCCESS || context.is_null() {
        return GPIO_USB_ERROR;
    }
    let mut devices = ptr::null_mut();
    // SAFETY: initialized context stays live during enumeration.
    let count = unsafe { ffi::libusb_get_device_list(context, &mut devices) };
    if count < 0 {
        // SAFETY: this function owns the initialized context.
        unsafe { ffi::libusb_exit(context) };
        return GPIO_USB_ERROR;
    }
    for index in 0..count as usize {
        // SAFETY: index stays within the list count returned by libusb.
        let device = unsafe { *devices.add(index) };
        let Some(descriptor) = (!device.is_null())
            .then(|| device_descriptor(device))
            .flatten()
        else {
            continue;
        };
        if !supported_cm119_descriptor(descriptor) {
            continue;
        }
        // SAFETY: the device remains live for this list iteration.
        let Some(topology) = (unsafe { UsbPortPath::from_device(device) }) else {
            continue;
        };
        list.matching_device_count = list.matching_device_count.saturating_add(1);
        let output_index = list.returned_device_count as usize;
        if output_index >= DEVICE_LIST_CAPACITY {
            continue;
        }
        fill_device_info(
            &mut list.devices[output_index],
            &topology,
            descriptor,
            device_serial(device, descriptor),
        );
        list.returned_device_count += 1;
    }
    // SAFETY: this function owns the returned list and initialized context.
    unsafe {
        ffi::libusb_free_device_list(devices, 1);
        ffi::libusb_exit(context);
    }
    GPIO_OK
}

/// Open and exclusively claim one configured CM119 HID interface.
extern "C" fn device_open(config: *const DeviceConfig, device: *mut *mut GpioDevice) -> c_int {
    if device.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: caller supplied writable output storage and it is initialized before failure paths.
    unsafe { *device = ptr::null_mut() };
    // SAFETY: public configuration is read-only and fully validated before opening hardware.
    let Some(config) = (unsafe { ValidatedConfig::from_ffi(config) }) else {
        return GPIO_INVALID_ARGUMENT;
    };
    match LibusbHid::open(&config) {
        Ok(opened) => {
            let handle = Box::new(GpioDevice::with_transport(
                config,
                opened.transport,
                opened.product_id,
            ));
            // SAFETY: caller owns output storage and the Box pointer remains valid until close.
            unsafe { *device = Box::into_raw(handle) };
            GPIO_OK
        }
        Err(OpenError::Unsupported) => GPIO_UNSUPPORTED,
        Err(OpenError::Usb(_error)) => GPIO_USB_ERROR,
    }
}

/// Publish an atomic PTT and GPIO action without performing hardware I/O.
extern "C" fn device_publish_outputs(
    device: *mut GpioDevice,
    action: *const OutputAction,
) -> c_int {
    if device.is_null() || action.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: pointers were checked and the API retains both objects for this call.
    unsafe { (*device).publish_outputs(&*action) }
}

/// Publish a timed CM119 PTT/GPIO inversion without performing HID I/O.
extern "C" fn device_publish_inverting_pulse(
    device: *mut GpioDevice,
    action: *const InvertingPulseAction,
) -> c_int {
    if device.is_null() || action.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: pointers were checked and publication uses only lock-free atomics.
    unsafe { (*device).publish_inverting_pulse(&*action) }
}

/// Schedule independently expiring CM119 PTT/GPIO inversions without HID I/O.
extern "C" fn device_schedule_inverting_pulse(
    device: *mut GpioDevice,
    action: *const ScheduledInvertingPulseAction,
) -> c_int {
    if device.is_null() || action.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: pointers were checked and publication uses only lock-free atomics.
    unsafe { (*device).schedule_inverting_pulse(&*action) }
}

/// Service the exclusive CM119 HID interface from its one non-real-time owner.
extern "C" fn device_service(device: *mut GpioDevice) -> c_int {
    if device.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: the descriptor contract designates one serialized service caller.
    unsafe { (*device).service() }
}

/// Read the most recently serviced CM119 input snapshot through atomics.
extern "C" fn device_get_inputs(device: *const GpioDevice, snapshot: *mut InputSnapshot) -> c_int {
    if device.is_null() || snapshot.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: the output belongs to the caller and the device remains live for this call.
    unsafe { (*device).inputs(&mut *snapshot) }
}

/// Read lock-free HID service statistics.
extern "C" fn device_get_stats(device: *const GpioDevice, stats: *mut DeviceStats) -> c_int {
    if device.is_null() || stats.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: the output belongs to the caller and the device remains live for this call.
    unsafe { (*device).stats(&mut *stats) }
}

/// Read the established CM119 tuning EEPROM through the sole service owner.
extern "C" fn device_read_eeprom(device: *mut GpioDevice, image: *mut EepromImage) -> c_int {
    if device.is_null() || image.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: the descriptor contract reserves HID I/O to one service owner.
    unsafe { (*device).read_eeprom(&mut *image) }
}

/// Program the established CM119 tuning EEPROM through the sole service owner.
extern "C" fn device_write_eeprom(device: *mut GpioDevice, image: *mut EepromImage) -> c_int {
    if device.is_null() || image.is_null() {
        return GPIO_INVALID_ARGUMENT;
    }
    // SAFETY: the descriptor contract reserves HID I/O to one service owner.
    unsafe { (*device).write_eeprom(&mut *image) }
}

/// Unkey and release an exclusive CM119 HID interface.
extern "C" fn device_close(device: *mut GpioDevice) {
    if device.is_null() {
        return;
    }
    // SAFETY: ownership transfers exactly once from the C caller back into this Box.
    let mut device = unsafe { Box::from_raw(device) };
    device.close();
}

/// Immutable descriptor valid for the shared object's complete lifetime.
static DESCRIPTOR: AdapterDescriptor = AdapterDescriptor {
    struct_size: size_of::<AdapterDescriptor>() as u32,
    abi_version: ABI_VERSION,
    capability_name: CAPABILITY_NAME.as_ptr().cast::<c_char>(),
    device_probe,
    device_open,
    device_publish_outputs,
    device_service,
    device_get_inputs,
    device_get_stats,
    device_close,
    device_discover,
    device_read_eeprom,
    device_write_eeprom,
    parallel_open: parallel::parallel_open,
    parallel_publish_outputs: parallel::parallel_publish_outputs,
    parallel_service: parallel::parallel_service,
    parallel_control_write_data: parallel::parallel_control_write_data,
    parallel_get_inputs: parallel::parallel_get_inputs,
    parallel_get_stats: parallel::parallel_get_stats,
    parallel_close: parallel::parallel_close,
    device_publish_inverting_pulse,
    parallel_publish_inverting_pulse: parallel::parallel_publish_inverting_pulse,
    device_schedule_inverting_pulse,
    parallel_schedule_inverting_pulse: parallel::parallel_schedule_inverting_pulse,
    parallel_set_binary_channel: parallel::parallel_set_binary_channel,
    parallel_program_rtx: parallel::parallel_program_rtx,
    parallel_clear_rtx_transmit: parallel::parallel_clear_rtx_transmit,
};

/// Return the stable adapter descriptor.
#[unsafe(no_mangle)]
pub extern "C" fn rptadv_gpio_adapter_descriptor() -> *const AdapterDescriptor {
    &DESCRIPTOR
}
