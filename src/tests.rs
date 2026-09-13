//! Deterministic CM119 HID transport tests.

use super::*;
use crate::parallel::{
    ParallelConfig, ParallelInputSnapshot, ParallelInvertingPulseAction, ParallelOutputAction,
    ParallelScheduledInvertingPulseAction, ParallelStats,
};
use std::collections::VecDeque;
use std::ffi::CStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared observations recorded by one fake HID transport.
#[derive(Default)]
struct FakeState {
    /// HID reports submitted by the service owner.
    writes: Vec<[u8; HID_REPORT_BYTES]>,
    /// Scripted input reports or transport errors.
    reads: VecDeque<Result<[u8; HID_REPORT_BYTES], c_int>>,
}

/// Deterministic in-memory CM119 HID transport.
struct FakeTransport {
    /// Test-owned observation and response state.
    state: Arc<Mutex<FakeState>>,
    /// Optional output error consumed by the next write.
    next_write_error: Option<c_int>,
}

impl HidTransport for FakeTransport {
    /// Record a report unless the scripted output error is pending.
    fn write_report(&mut self, report: [u8; HID_REPORT_BYTES]) -> Result<(), c_int> {
        if let Some(error) = self.next_write_error.take() {
            return Err(error);
        }
        self.state
            .lock()
            .expect("test state lock")
            .writes
            .push(report);
        Ok(())
    }

    /// Consume the next scripted report, defaulting to an idle all-high input.
    fn read_report(&mut self) -> Result<[u8; HID_REPORT_BYTES], c_int> {
        self.state
            .lock()
            .expect("test state lock")
            .reads
            .pop_front()
            .unwrap_or(Ok([u8::MAX; HID_REPORT_BYTES]))
    }
}

/// Deterministic HID transport that fails exactly one requested write operation.
struct FailOnNthWriteTransport {
    /// Test-owned observation and input-report state.
    state: Arc<Mutex<FakeState>>,
    /// One-based HID write operation that must fail.
    fail_on_write: usize,
    /// Writes attempted so far by the exclusive service owner.
    write_count: usize,
}

impl HidTransport for FailOnNthWriteTransport {
    /// Record every successful write and inject the configured transport failure once.
    fn write_report(&mut self, report: [u8; HID_REPORT_BYTES]) -> Result<(), c_int> {
        self.write_count += 1;
        if self.write_count == self.fail_on_write {
            return Err(-73);
        }
        self.state
            .lock()
            .expect("test state lock")
            .writes
            .push(report);
        Ok(())
    }

    /// Consume the next scripted report, defaulting to an idle all-high input.
    fn read_report(&mut self) -> Result<[u8; HID_REPORT_BYTES], c_int> {
        self.state
            .lock()
            .expect("test state lock")
            .reads
            .pop_front()
            .unwrap_or(Ok([u8::MAX; HID_REPORT_BYTES]))
    }
}

/// Build a public configuration matching the ordinary CM119 profile.
fn ffi_config() -> DeviceConfig {
    DeviceConfig {
        struct_size: size_of::<DeviceConfig>() as u32,
        abi_version: ABI_VERSION,
        usb_port_path: c"3-1:1.0".as_ptr(),
        vendor_id: 0,
        product_id: 0,
        profile: 0,
        ptt_inverted: 0,
        gpio_output_enable_mask: 1,
        gpio_output_initial_mask: 1,
    }
}

/// Produce validated configuration for an in-memory transport.
fn validated(config: &DeviceConfig) -> ValidatedConfig {
    // SAFETY: test configuration remains alive throughout conversion.
    unsafe { ValidatedConfig::from_ffi(config) }.expect("valid CM119 configuration")
}

/// Construct one fake device and return its observable transport state.
fn fake_device(config: &DeviceConfig) -> (GpioDevice, Arc<Mutex<FakeState>>) {
    fake_device_with_product(config, 0x0008)
}

/// Construct one fake device for a specific USB product and return its observable state.
fn fake_device_with_product(
    config: &DeviceConfig,
    product_id: u16,
) -> (GpioDevice, Arc<Mutex<FakeState>>) {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let transport = FakeTransport {
        state: Arc::clone(&state),
        next_write_error: None,
    };
    (
        GpioDevice::with_transport(validated(config), Box::new(transport), product_id),
        state,
    )
}

/// Construct a fake device whose final EEPROM-output restoration will fail once.
fn fake_device_failing_on_write(
    config: &DeviceConfig,
    fail_on_write: usize,
) -> (GpioDevice, Arc<Mutex<FakeState>>) {
    let state = Arc::new(Mutex::new(FakeState::default()));
    let transport = FailOnNthWriteTransport {
        state: Arc::clone(&state),
        fail_on_write,
        write_count: 0,
    };
    (
        GpioDevice::with_transport(validated(config), Box::new(transport), 0x0008),
        state,
    )
}

/// Mutable observations from one deterministic in-memory libusb backend.
#[derive(Default)]
struct FakeLibusbState {
    /// Process-context initialization calls.
    init_calls: usize,
    /// Process-context shutdown calls.
    exit_calls: usize,
    /// Device-list enumeration calls.
    list_calls: usize,
    /// Device-list release calls.
    free_list_calls: usize,
    /// Descriptor-read calls.
    descriptor_calls: usize,
    /// Hub-port-chain calls.
    port_calls: usize,
    /// Device-open calls, including temporary serial reads.
    open_calls: usize,
    /// Serial descriptor requests.
    serial_calls: usize,
    /// Device-handle close calls.
    close_calls: usize,
    /// HID interface claim calls.
    claim_calls: usize,
    /// HID interface release calls.
    release_calls: usize,
    /// Kernel-driver ownership queries.
    kernel_active_calls: usize,
    /// Kernel-driver detach calls.
    detach_calls: usize,
    /// HID control transfer calls.
    transfer_calls: usize,
    /// Complete output reports submitted through the HID transport.
    writes: Vec<[u8; HID_REPORT_BYTES]>,
}

/// Deterministic libusb implementation that never touches an actual USB device.
struct FakeLibusbBackend {
    /// Test-visible call observations.
    state: Arc<Mutex<FakeLibusbState>>,
    /// Result returned by context initialization.
    init_result: c_int,
    /// Whether a successful initialization deliberately returns a null context.
    null_context_on_init: bool,
    /// Result returned by device-list enumeration.
    device_list_result: isize,
    /// Result returned by descriptor reads.
    descriptor_result: c_int,
    /// Per-read descriptor results used to model a device that changes or fails during probing.
    descriptor_results: VecDeque<c_int>,
    /// Descriptor copied on successful descriptor reads.
    descriptor: ffi::LibusbDeviceDescriptor,
    /// Synthetic physical bus number.
    bus: u8,
    /// Synthetic hub-port chain.
    ports: Vec<u8>,
    /// Optional explicit port-number result used for malformed-device tests.
    port_result: Option<c_int>,
    /// Results consumed by device-open calls.
    open_results: VecDeque<c_int>,
    /// Whether successful open calls deliberately return a null handle.
    null_handle_on_open: bool,
    /// Bytes returned by a successful serial descriptor read.
    serial: Vec<u8>,
    /// Optional explicit serial descriptor result.
    serial_result: Option<c_int>,
    /// Results consumed by HID interface claim calls.
    claim_results: VecDeque<c_int>,
    /// Result returned by the kernel-driver ownership query.
    kernel_active_result: c_int,
    /// Result returned by kernel-driver detach.
    detach_result: c_int,
    /// Results consumed by HID output control transfers.
    output_results: VecDeque<c_int>,
    /// Results and reports consumed by HID input control transfers.
    input_results: VecDeque<Result<[u8; HID_REPORT_BYTES], c_int>>,
    /// Optional short successful result codes for HID input transfers.
    input_return_results: VecDeque<c_int>,
    /// Null-terminated synthetic libusb device list.
    device_list: Box<[*mut ffi::LibusbDevice; DEVICE_LIST_CAPACITY + 2]>,
}

// SAFETY: synthetic pointer values are never dereferenced and all backend calls
// run under the test backend's serialized installation guard.
unsafe impl Send for FakeLibusbBackend {}

impl FakeLibusbBackend {
    /// Construct a normally successful one-device CM119 backend.
    fn new(state: Arc<Mutex<FakeLibusbState>>) -> Self {
        Self {
            state,
            init_result: ffi::LIBUSB_SUCCESS,
            null_context_on_init: false,
            device_list_result: 1,
            descriptor_result: ffi::LIBUSB_SUCCESS,
            descriptor_results: VecDeque::new(),
            descriptor: ffi::LibusbDeviceDescriptor {
                b_length: 18,
                b_descriptor_type: 1,
                bcd_usb: 0x0200,
                b_device_class: 0,
                b_device_sub_class: 0,
                b_device_protocol: 0,
                b_max_packet_size_0: 64,
                id_vendor: CMEDIA_VENDOR_ID,
                id_product: 0x0008,
                bcd_device: 0x0100,
                i_manufacturer: 1,
                i_product: 2,
                i_serial_number: 3,
                b_num_configurations: 1,
            },
            bus: 3,
            ports: vec![1],
            port_result: None,
            open_results: VecDeque::new(),
            null_handle_on_open: false,
            serial: b"CM119-TEST".to_vec(),
            serial_result: None,
            claim_results: VecDeque::new(),
            kernel_active_result: 0,
            detach_result: ffi::LIBUSB_SUCCESS,
            output_results: VecDeque::new(),
            input_results: VecDeque::new(),
            input_return_results: VecDeque::new(),
            device_list: Box::new(std::array::from_fn(|index| {
                if index == 0 {
                    0x1000_usize as *mut ffi::LibusbDevice
                } else {
                    std::ptr::null_mut()
                }
            })),
        }
    }

    /// Record one backend call without allowing test mutex poisoning to mask evidence.
    fn observe(&self, update: impl FnOnce(&mut FakeLibusbState)) {
        update(
            &mut self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
    }

    /// Expose a bounded number of repeated synthetic devices through the C-style list.
    fn set_device_count(&mut self, count: usize) {
        assert!(count <= DEVICE_LIST_CAPACITY + 1);
        self.device_list_result = count as isize;
        for (index, device) in self.device_list.iter_mut().enumerate() {
            *device = if index < count {
                0x1000_usize as *mut ffi::LibusbDevice
            } else {
                std::ptr::null_mut()
            };
        }
    }
}

impl ffi::TestLibusbBackend for FakeLibusbBackend {
    /// Return the configured synthetic process context result.
    unsafe fn init(&mut self, context: *mut *mut ffi::LibusbContext) -> c_int {
        self.observe(|state| state.init_calls += 1);
        if self.init_result == ffi::LIBUSB_SUCCESS && !context.is_null() {
            // SAFETY: the libusb wrapper forwards writable caller-owned output storage.
            unsafe {
                *context = if self.null_context_on_init {
                    std::ptr::null_mut()
                } else {
                    0x2000_usize as *mut ffi::LibusbContext
                };
            }
        }
        self.init_result
    }

    /// Record process-context shutdown.
    unsafe fn exit(&mut self, _context: *mut ffi::LibusbContext) {
        self.observe(|state| state.exit_calls += 1);
    }

    /// Return the configured synthetic device list.
    unsafe fn get_device_list(
        &mut self,
        _context: *mut ffi::LibusbContext,
        devices: *mut *mut *mut ffi::LibusbDevice,
    ) -> isize {
        self.observe(|state| state.list_calls += 1);
        if self.device_list_result >= 0 && !devices.is_null() {
            // SAFETY: the libusb wrapper forwards writable caller-owned output storage.
            unsafe { *devices = self.device_list.as_mut_ptr() };
        }
        self.device_list_result
    }

    /// Record device-list release.
    unsafe fn free_device_list(
        &mut self,
        _devices: *mut *mut ffi::LibusbDevice,
        _unref_devices: c_int,
    ) {
        self.observe(|state| state.free_list_calls += 1);
    }

    /// Copy the configured device descriptor.
    unsafe fn get_device_descriptor(
        &mut self,
        _device: *mut ffi::LibusbDevice,
        descriptor: *mut ffi::LibusbDeviceDescriptor,
    ) -> c_int {
        self.observe(|state| state.descriptor_calls += 1);
        let result = self
            .descriptor_results
            .pop_front()
            .unwrap_or(self.descriptor_result);
        if result == ffi::LIBUSB_SUCCESS && !descriptor.is_null() {
            // SAFETY: the libusb wrapper forwards writable caller-owned output storage.
            unsafe { *descriptor = self.descriptor };
        }
        result
    }

    /// Return the configured bus number.
    unsafe fn get_bus_number(&mut self, _device: *mut ffi::LibusbDevice) -> u8 {
        self.bus
    }

    /// Copy the configured hub-port chain or return an injected malformed result.
    unsafe fn get_port_numbers(
        &mut self,
        _device: *mut ffi::LibusbDevice,
        ports: *mut u8,
        port_count: c_int,
    ) -> c_int {
        self.observe(|state| state.port_calls += 1);
        let result = self.port_result.unwrap_or(self.ports.len() as c_int);
        if result > 0 && !ports.is_null() && port_count >= result {
            let copied = usize::min(result as usize, self.ports.len());
            // SAFETY: the libusb wrapper supplied storage for at least `port_count` bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(self.ports.as_ptr(), ports, copied);
            }
        }
        result
    }

    /// Return the next configured open result.
    unsafe fn open(
        &mut self,
        _device: *mut ffi::LibusbDevice,
        handle: *mut *mut ffi::LibusbDeviceHandle,
    ) -> c_int {
        self.observe(|state| state.open_calls += 1);
        let result = self.open_results.pop_front().unwrap_or(ffi::LIBUSB_SUCCESS);
        if result == ffi::LIBUSB_SUCCESS && !handle.is_null() {
            // SAFETY: the libusb wrapper forwards writable caller-owned output storage.
            unsafe {
                *handle = if self.null_handle_on_open {
                    std::ptr::null_mut()
                } else {
                    0x3000_usize as *mut ffi::LibusbDeviceHandle
                };
            }
        }
        result
    }

    /// Copy the configured serial number or return an injected result.
    unsafe fn get_string_descriptor_ascii(
        &mut self,
        _handle: *mut ffi::LibusbDeviceHandle,
        _descriptor_index: u8,
        data: *mut u8,
        length: c_int,
    ) -> c_int {
        self.observe(|state| state.serial_calls += 1);
        let result = self.serial_result.unwrap_or(self.serial.len() as c_int);
        if result > 0 && !data.is_null() && length >= result {
            // SAFETY: the libusb wrapper supplied storage for `length` bytes.
            unsafe { std::ptr::copy_nonoverlapping(self.serial.as_ptr(), data, result as usize) };
        }
        result
    }

    /// Record device-handle close.
    unsafe fn close(&mut self, _handle: *mut ffi::LibusbDeviceHandle) {
        self.observe(|state| state.close_calls += 1);
    }

    /// Return the next configured claim result.
    unsafe fn claim_interface(
        &mut self,
        _handle: *mut ffi::LibusbDeviceHandle,
        _interface: c_int,
    ) -> c_int {
        self.observe(|state| state.claim_calls += 1);
        self.claim_results
            .pop_front()
            .unwrap_or(ffi::LIBUSB_SUCCESS)
    }

    /// Record HID interface release.
    unsafe fn release_interface(
        &mut self,
        _handle: *mut ffi::LibusbDeviceHandle,
        _interface: c_int,
    ) -> c_int {
        self.observe(|state| state.release_calls += 1);
        ffi::LIBUSB_SUCCESS
    }

    /// Return the configured kernel-driver ownership result.
    unsafe fn kernel_driver_active(
        &mut self,
        _handle: *mut ffi::LibusbDeviceHandle,
        _interface: c_int,
    ) -> c_int {
        self.observe(|state| state.kernel_active_calls += 1);
        self.kernel_active_result
    }

    /// Record a kernel-driver detach and return the configured result.
    unsafe fn detach_kernel_driver(
        &mut self,
        _handle: *mut ffi::LibusbDeviceHandle,
        _interface: c_int,
    ) -> c_int {
        self.observe(|state| state.detach_calls += 1);
        self.detach_result
    }

    /// Process deterministic complete HID input and output reports.
    unsafe fn control_transfer(&mut self, transfer: ffi::LibusbControlTransfer) -> c_int {
        let _ = (
            transfer.handle,
            transfer.request_type,
            transfer.value,
            transfer.index,
            transfer.timeout_milliseconds,
        );
        self.observe(|state| state.transfer_calls += 1);
        if transfer.request == ffi::HID_REPORT_SET {
            if transfer.length as usize == HID_REPORT_BYTES && !transfer.data.is_null() {
                // SAFETY: the caller supplied the stated complete HID report length.
                let report = unsafe { std::slice::from_raw_parts(transfer.data, HID_REPORT_BYTES) };
                self.observe(|state| state.writes.push(report.try_into().expect("report width")));
            }
            return self
                .output_results
                .pop_front()
                .unwrap_or(HID_REPORT_BYTES as c_int);
        }
        if transfer.request == ffi::HID_REPORT_GET {
            return match self
                .input_results
                .pop_front()
                .unwrap_or(Ok([u8::MAX; HID_REPORT_BYTES]))
            {
                Ok(report) => {
                    if transfer.length as usize == HID_REPORT_BYTES && !transfer.data.is_null() {
                        // SAFETY: the caller supplied the stated complete HID report length.
                        unsafe {
                            std::ptr::copy_nonoverlapping(
                                report.as_ptr(),
                                transfer.data,
                                HID_REPORT_BYTES,
                            )
                        };
                    }
                    self.input_return_results
                        .pop_front()
                        .unwrap_or(HID_REPORT_BYTES as c_int)
                }
                Err(error) => error,
            };
        }
        -99
    }
}

/// Create and install a normal deterministic libusb backend for one test scope.
fn install_fake_libusb() -> (ffi::TestBackendGuard, Arc<Mutex<FakeLibusbState>>) {
    let state = Arc::new(Mutex::new(FakeLibusbState::default()));
    let backend = FakeLibusbBackend::new(Arc::clone(&state));
    (ffi::install_test_backend(Box::new(backend)), state)
}

/// Return an EEPROM image initialized for a caller-owned read or write operation.
fn eeprom_image(abi_version: u32) -> EepromImage {
    EepromImage {
        struct_size: size_of::<EepromImage>() as u32,
        abi_version,
        checksum_valid: 0,
        magic_valid: 0,
        words: [0; EEPROM_WORD_COUNT],
    }
}

/// Queue CM119 HID input reports that encode the ASL3 user tuning region.
fn queue_eeprom_reads(state: &Arc<Mutex<FakeState>>, words: &[u16; EEPROM_WORD_COUNT]) {
    let mut state = state.lock().expect("test state lock");
    for value in &words[EEPROM_START_WORD..=EEPROM_CHECKSUM_WORD] {
        state
            .reads
            .push_back(Ok([0, *value as u8, (*value >> 8) as u8, 0]));
    }
}

/// Verify a static descriptor does not expose libusb internals.
#[test]
fn descriptor_exposes_the_cm119_hid_contract() {
    let descriptor = rptadv_gpio_adapter_descriptor();
    assert!(!descriptor.is_null());
    // SAFETY: descriptor is static for the shared object's lifetime.
    let descriptor = unsafe { &*descriptor };
    assert_eq!(descriptor.abi_version, ABI_VERSION);
    assert_eq!(
        descriptor.struct_size as usize,
        size_of::<AdapterDescriptor>()
    );
    // SAFETY: capability name is a static NUL-terminated byte string.
    assert_eq!(
        unsafe { CStr::from_ptr(descriptor.capability_name) }.to_bytes(),
        b"rptadv.cm119-hid-gpio"
    );
    assert_ne!(descriptor.device_probe as usize, 0);
    assert_ne!(descriptor.device_open as usize, 0);
    assert_ne!(descriptor.device_publish_outputs as usize, 0);
    assert_ne!(descriptor.device_service as usize, 0);
    assert_ne!(descriptor.device_get_inputs as usize, 0);
    assert_ne!(descriptor.device_get_stats as usize, 0);
    assert_ne!(descriptor.device_close as usize, 0);
    assert_ne!(descriptor.device_discover as usize, 0);
    assert_ne!(descriptor.device_read_eeprom as usize, 0);
    assert_ne!(descriptor.device_write_eeprom as usize, 0);
    assert_ne!(descriptor.parallel_open as usize, 0);
    assert_ne!(descriptor.parallel_publish_outputs as usize, 0);
    assert_ne!(descriptor.parallel_service as usize, 0);
    assert_ne!(descriptor.parallel_control_write_data as usize, 0);
    assert_ne!(descriptor.parallel_get_inputs as usize, 0);
    assert_ne!(descriptor.parallel_get_stats as usize, 0);
    assert_ne!(descriptor.parallel_close as usize, 0);
    assert_ne!(descriptor.device_publish_inverting_pulse as usize, 0);
    assert_ne!(descriptor.parallel_publish_inverting_pulse as usize, 0);
    assert_ne!(descriptor.device_schedule_inverting_pulse as usize, 0);
    assert_ne!(descriptor.parallel_schedule_inverting_pulse as usize, 0);
    assert_ne!(descriptor.parallel_set_binary_channel as usize, 0);
    assert_ne!(descriptor.parallel_program_rtx as usize, 0);
    assert_ne!(descriptor.parallel_clear_rtx_transmit as usize, 0);
}

/// Keep each public structure layout identical to the documented LP64 C ABI.
#[test]
fn public_ffi_layouts_are_stable() {
    assert_eq!(size_of::<DeviceConfig>(), 40);
    assert_eq!(size_of::<DeviceInfo>(), 160);
    assert_eq!(offset_of!(DeviceInfo, serial), 31);
    assert_eq!(size_of::<DeviceList>(), 2576);
    assert_eq!(size_of::<EepromImage>(), 144);
    assert_eq!(size_of::<DeviceStats>(), 64);
    assert_eq!(offset_of!(DeviceStats, eeprom_read_count), 48);
    assert_eq!(size_of::<ParallelConfig>(), 40);
    assert_eq!(size_of::<ParallelInputSnapshot>(), 16);
    assert_eq!(size_of::<ParallelOutputAction>(), 24);
    assert_eq!(size_of::<ParallelInvertingPulseAction>(), 20);
    assert_eq!(size_of::<ParallelScheduledInvertingPulseAction>(), 20);
    assert_eq!(size_of::<ParallelStats>(), 48);
    assert_eq!(size_of::<InvertingPulseAction>(), 24);
    assert_eq!(size_of::<ScheduledInvertingPulseAction>(), 28);
    assert_eq!(
        offset_of!(AdapterDescriptor, device_publish_inverting_pulse),
        152
    );
    assert_eq!(
        offset_of!(AdapterDescriptor, parallel_publish_inverting_pulse),
        160
    );
    assert_eq!(
        offset_of!(AdapterDescriptor, device_schedule_inverting_pulse),
        168
    );
    assert_eq!(
        offset_of!(AdapterDescriptor, parallel_schedule_inverting_pulse),
        176
    );
    assert_eq!(
        offset_of!(AdapterDescriptor, parallel_set_binary_channel),
        184
    );
    assert_eq!(offset_of!(AdapterDescriptor, parallel_program_rtx), 192);
    assert_eq!(
        offset_of!(AdapterDescriptor, parallel_clear_rtx_transmit),
        200
    );
    assert_eq!(size_of::<AdapterDescriptor>(), 208);
}

/// Verify the default C-Media family selector and optional interface suffix normalize safely.
#[test]
fn configuration_uses_a_stable_topology_and_default_cmedia_family() {
    let config = ffi_config();
    let validated = validated(&config);
    assert_eq!(validated.vendor_id, CMEDIA_VENDOR_ID);
    assert_eq!(validated.product_id, None);
    assert_eq!(
        validated.usb_port_path,
        UsbPortPath {
            bus: 3,
            ports: vec![1]
        }
    );
    assert!(product_matches(None, 0x0008));
    assert!(product_matches(None, 0x013c));
    assert!(product_matches(None, 0x6a42));
    assert!(!product_matches(None, 0x9999));
    assert!(product_matches(Some(0x0008), 0x0008));
    assert!(!product_matches(Some(0x0008), 0x000c));
}

/// Reject values that could drive a profile's reserved PTT or unavailable GPIO bit.
#[test]
fn configuration_rejects_unsafe_or_malformed_gpio_selection() {
    let mut config = ffi_config();
    config.usb_port_path = c"3-0".as_ptr();
    // SAFETY: test configuration is readable and intentionally invalid.
    assert!(unsafe { ValidatedConfig::from_ffi(&config) }.is_none());

    config = ffi_config();
    config.profile = 99;
    // SAFETY: test configuration is readable and intentionally invalid.
    assert!(unsafe { ValidatedConfig::from_ffi(&config) }.is_none());

    config = ffi_config();
    config.gpio_output_enable_mask = 0x04;
    // SAFETY: profile zero reserves this bit for PTT.
    assert!(unsafe { ValidatedConfig::from_ffi(&config) }.is_none());

    config = ffi_config();
    config.gpio_output_initial_mask = 0x02;
    // SAFETY: initial bits must be a subset of the enabled output mask.
    assert!(unsafe { ValidatedConfig::from_ffi(&config) }.is_none());
}

/// Prove the service owner applies the newest atomic action and decodes active-low inputs.
#[test]
fn service_translates_logical_ptt_gpio_cor_and_ctcss_without_a_lock() {
    let config = ffi_config();
    let (device, state) = fake_device(&config);
    state
        .lock()
        .expect("test state lock")
        .reads
        .push_back(Ok([0x00, 0xa5, 0x00, 0x00]));

    assert_eq!(device.service(), GPIO_OK);
    let action = OutputAction {
        struct_size: size_of::<OutputAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_asserted: 1,
        gpio_output_mask: 0,
    };
    assert_eq!(device.publish_outputs(&action), GPIO_OK);
    assert_eq!(device.service(), GPIO_OK);

    let writes = state.lock().expect("test state lock").writes.clone();
    assert_eq!(writes, vec![[0, 0x01, 0x05, 0], [0, 0x04, 0x05, 0]]);

    let mut inputs = InputSnapshot {
        struct_size: size_of::<InputSnapshot>() as u32,
        abi_version: 0,
        online: 0,
        cor_active: 0,
        ctcss_active: 0,
        gpio_input_mask: 0,
        hid_report: [0; HID_REPORT_BYTES],
    };
    assert_eq!(device.inputs(&mut inputs), GPIO_OK);
    assert_eq!(inputs.abi_version, ABI_VERSION);
    assert_eq!(inputs.online, 1);
    assert_eq!(inputs.cor_active, 0);
    assert_eq!(inputs.ctcss_active, 0);
    assert_eq!(inputs.gpio_input_mask, u32::from(u8::MAX));
    assert_eq!(inputs.hid_report, [u8::MAX; HID_REPORT_BYTES]);

    let mut stats = DeviceStats {
        struct_size: size_of::<DeviceStats>() as u32,
        abi_version: 0,
        input_read_count: 0,
        output_apply_count: 0,
        usb_error_count: 0,
        ptt_applied: 0,
        online: 0,
        last_usb_error: 0,
        eeprom_read_count: 0,
        eeprom_write_count: 0,
    };
    assert_eq!(device.stats(&mut stats), GPIO_OK);
    assert_eq!(stats.input_read_count, 2);
    assert_eq!(stats.output_apply_count, 2);
    assert_eq!(stats.usb_error_count, 0);
    assert_eq!(stats.ptt_applied, 1);
    assert_eq!(stats.online, 1);
}

/// Apply, cancel, and expire one CM119 XOR pulse without HID I/O from its publisher.
#[test]
fn cm119_inverting_pulse_flips_ptt_and_gpio_then_restores_the_baseline() {
    let config = ffi_config();
    let (device, state) = fake_device(&config);
    let start = Instant::now();

    assert_eq!(device.service_at(start), GPIO_OK);
    let pulse = InvertingPulseAction {
        struct_size: size_of::<InvertingPulseAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_invert: 1,
        gpio_invert_mask: 1,
        pulse_duration_milliseconds: 10,
        cancel_pulse: 0,
    };
    assert_eq!(device.publish_inverting_pulse(&pulse), GPIO_OK);
    assert_eq!(device.service_at(start + Duration::from_millis(1)), GPIO_OK);

    let mut stats = DeviceStats {
        struct_size: size_of::<DeviceStats>() as u32,
        abi_version: 0,
        input_read_count: 0,
        output_apply_count: 0,
        usb_error_count: 0,
        ptt_applied: 0,
        online: 0,
        last_usb_error: 0,
        eeprom_read_count: 0,
        eeprom_write_count: 0,
    };
    assert_eq!(device.stats(&mut stats), GPIO_OK);
    assert_eq!(stats.ptt_applied, 1);
    let cancel = InvertingPulseAction {
        struct_size: size_of::<InvertingPulseAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_invert: 0,
        gpio_invert_mask: 0,
        pulse_duration_milliseconds: 0,
        cancel_pulse: 1,
    };
    assert_eq!(device.publish_inverting_pulse(&cancel), GPIO_OK);
    assert_eq!(device.service_at(start + Duration::from_millis(2)), GPIO_OK);
    assert_eq!(device.stats(&mut stats), GPIO_OK);
    assert_eq!(stats.ptt_applied, 0);
    assert_eq!(device.publish_inverting_pulse(&pulse), GPIO_OK);
    assert_eq!(device.service_at(start + Duration::from_millis(3)), GPIO_OK);
    assert_eq!(
        device.service_at(start + Duration::from_millis(14)),
        GPIO_OK
    );
    assert_eq!(device.stats(&mut stats), GPIO_OK);
    assert_eq!(stats.ptt_applied, 0);
    assert_eq!(
        state.lock().expect("test state lock").writes,
        [
            [0, 0x01, 0x05, 0],
            [0, 0x04, 0x05, 0],
            [0, 0x01, 0x05, 0],
            [0, 0x04, 0x05, 0],
            [0, 0x01, 0x05, 0],
        ]
    );
}

/// Keep independent CM119 pulse deadlines and selected cancellation separate.
#[test]
fn cm119_scheduled_inverting_pulses_overlap_and_expire_independently() {
    let config = ffi_config();
    let (device, state) = fake_device(&config);
    let start = Instant::now();

    assert_eq!(device.service_at(start), GPIO_OK);
    let gpio = ScheduledInvertingPulseAction {
        struct_size: size_of::<ScheduledInvertingPulseAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_invert: 0,
        gpio_invert_mask: 1,
        pulse_duration_milliseconds: 10,
        ptt_cancel: 0,
        gpio_cancel_mask: 0,
    };
    assert_eq!(device.schedule_inverting_pulse(&gpio), GPIO_OK);
    let ptt = ScheduledInvertingPulseAction {
        struct_size: size_of::<ScheduledInvertingPulseAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_invert: 1,
        gpio_invert_mask: 0,
        pulse_duration_milliseconds: 20,
        ptt_cancel: 0,
        gpio_cancel_mask: 0,
    };
    assert_eq!(device.schedule_inverting_pulse(&ptt), GPIO_OK);
    assert_eq!(device.service_at(start + Duration::from_millis(1)), GPIO_OK);

    let cancel_ptt = ScheduledInvertingPulseAction {
        struct_size: size_of::<ScheduledInvertingPulseAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_invert: 0,
        gpio_invert_mask: 0,
        pulse_duration_milliseconds: 0,
        ptt_cancel: 1,
        gpio_cancel_mask: 0,
    };
    assert_eq!(device.schedule_inverting_pulse(&cancel_ptt), GPIO_OK);
    assert_eq!(device.service_at(start + Duration::from_millis(2)), GPIO_OK);
    assert_eq!(
        device.service_at(start + Duration::from_millis(11)),
        GPIO_OK
    );

    assert_eq!(
        state.lock().expect("test state lock").writes,
        [
            [0, 0x01, 0x05, 0],
            [0, 0x04, 0x05, 0],
            [0, 0x00, 0x05, 0],
            [0, 0x01, 0x05, 0],
        ]
    );
}

/// Apply scheduled logical PTT inversion before physical active-low conversion.
#[test]
fn cm119_scheduled_pulse_preserves_active_low_ptt_wiring() {
    let mut config = ffi_config();
    config.ptt_inverted = 1;
    let (device, state) = fake_device(&config);
    let start = Instant::now();
    let pulse = ScheduledInvertingPulseAction {
        struct_size: size_of::<ScheduledInvertingPulseAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_invert: 1,
        gpio_invert_mask: 0,
        pulse_duration_milliseconds: 10,
        ptt_cancel: 0,
        gpio_cancel_mask: 0,
    };

    assert_eq!(device.service_at(start), GPIO_OK);
    assert_eq!(device.schedule_inverting_pulse(&pulse), GPIO_OK);
    assert_eq!(device.service_at(start + Duration::from_millis(1)), GPIO_OK);
    assert_eq!(
        device.service_at(start + Duration::from_millis(11)),
        GPIO_OK
    );
    assert_eq!(
        state.lock().expect("test state lock").writes,
        [[0, 0x05, 0x05, 0], [0, 0x01, 0x05, 0], [0, 0x05, 0x05, 0]]
    );
}

/// Reject malformed independent CM119 schedule requests before publication.
#[test]
fn cm119_scheduled_inverting_pulse_validates_selected_bits_and_deadlines() {
    let config = ffi_config();
    let (device, _) = fake_device(&config);
    let mut action = ScheduledInvertingPulseAction {
        struct_size: size_of::<ScheduledInvertingPulseAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_invert: 0,
        gpio_invert_mask: 1,
        pulse_duration_milliseconds: 10,
        ptt_cancel: 0,
        gpio_cancel_mask: 0,
    };
    action.struct_size = 0;
    assert_eq!(
        device.schedule_inverting_pulse(&action),
        GPIO_INVALID_ARGUMENT
    );
    action.struct_size = size_of::<ScheduledInvertingPulseAction>() as u32;
    action.abi_version = ABI_VERSION + 1;
    assert_eq!(
        device.schedule_inverting_pulse(&action),
        GPIO_INVALID_ARGUMENT
    );
    action.abi_version = ABI_VERSION;
    action.ptt_invert = 2;
    assert_eq!(
        device.schedule_inverting_pulse(&action),
        GPIO_INVALID_ARGUMENT
    );
    action.ptt_invert = 0;
    action.gpio_invert_mask = 2;
    assert_eq!(
        device.schedule_inverting_pulse(&action),
        GPIO_INVALID_ARGUMENT
    );
    action.gpio_invert_mask = 1;
    action.gpio_cancel_mask = 1;
    assert_eq!(
        device.schedule_inverting_pulse(&action),
        GPIO_INVALID_ARGUMENT
    );
    action.gpio_cancel_mask = 0;
    action.pulse_duration_milliseconds = 0;
    assert_eq!(
        device.schedule_inverting_pulse(&action),
        GPIO_INVALID_ARGUMENT
    );
    action.gpio_invert_mask = 0;
    action.ptt_cancel = 2;
    assert_eq!(
        device.schedule_inverting_pulse(&action),
        GPIO_INVALID_ARGUMENT
    );
    action.ptt_cancel = 0;
    assert_eq!(device.schedule_inverting_pulse(&action), GPIO_OK);
}

/// Reject malformed CM119 pulse actions before mutating a pending pulse request.
#[test]
fn cm119_inverting_pulse_validates_its_mask_duration_and_cancel_contract() {
    let config = ffi_config();
    let (device, state) = fake_device(&config);
    let mut pulse = InvertingPulseAction {
        struct_size: size_of::<InvertingPulseAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_invert: 0,
        gpio_invert_mask: 1,
        pulse_duration_milliseconds: 10,
        cancel_pulse: 0,
    };
    pulse.struct_size = 0;
    assert_eq!(
        device.publish_inverting_pulse(&pulse),
        GPIO_INVALID_ARGUMENT
    );
    pulse.struct_size = size_of::<InvertingPulseAction>() as u32;
    pulse.abi_version = ABI_VERSION + 1;
    assert_eq!(
        device.publish_inverting_pulse(&pulse),
        GPIO_INVALID_ARGUMENT
    );
    pulse.abi_version = ABI_VERSION;
    pulse.ptt_invert = 2;
    assert_eq!(
        device.publish_inverting_pulse(&pulse),
        GPIO_INVALID_ARGUMENT
    );
    pulse.ptt_invert = 0;
    pulse.gpio_invert_mask = 2;
    assert_eq!(
        device.publish_inverting_pulse(&pulse),
        GPIO_INVALID_ARGUMENT
    );
    pulse.gpio_invert_mask = 1;
    pulse.pulse_duration_milliseconds = 0;
    assert_eq!(
        device.publish_inverting_pulse(&pulse),
        GPIO_INVALID_ARGUMENT
    );
    pulse.pulse_duration_milliseconds = 10;
    pulse.cancel_pulse = 1;
    assert_eq!(
        device.publish_inverting_pulse(&pulse),
        GPIO_INVALID_ARGUMENT
    );
    pulse.gpio_invert_mask = 0;
    pulse.pulse_duration_milliseconds = 0;
    assert_eq!(device.publish_inverting_pulse(&pulse), GPIO_OK);

    assert_eq!(device.service(), GPIO_OK);
    assert_eq!(
        state.lock().expect("test state lock").writes,
        [[0, 0x01, 0x05, 0]]
    );
}

/// Verify active-low COR and CTCSS measurements from the same HID report are retained atomically.
#[test]
fn service_publishes_active_low_hardware_signaling() {
    let config = ffi_config();
    let (device, state) = fake_device(&config);
    state
        .lock()
        .expect("test state lock")
        .reads
        .push_back(Ok([0xfc, 0x5a, 0x00, 0x00]));
    assert_eq!(device.service(), GPIO_OK);

    let mut inputs = InputSnapshot {
        struct_size: size_of::<InputSnapshot>() as u32,
        abi_version: 0,
        online: 0,
        cor_active: 0,
        ctcss_active: 0,
        gpio_input_mask: 0,
        hid_report: [0; HID_REPORT_BYTES],
    };
    assert_eq!(device.inputs(&mut inputs), GPIO_OK);
    assert_eq!(inputs.cor_active, 1);
    assert_eq!(inputs.ctcss_active, 1);
    assert_eq!(inputs.gpio_input_mask, 0x5a);
    assert_eq!(inputs.hid_report, [0xfc, 0x5a, 0x00, 0x00]);
}

/// Normalize CM108AH's active-low HOOK input to the legacy logical GPIO2 bit.
#[test]
fn cm108ah_hook_is_normalized_as_logical_gpio2() {
    let config = ffi_config();
    let (device, state) = fake_device_with_product(&config, CM108AH_PRODUCT_ID);
    let mut inputs = InputSnapshot {
        struct_size: size_of::<InputSnapshot>() as u32,
        abi_version: 0,
        online: 0,
        cor_active: 0,
        ctcss_active: 0,
        gpio_input_mask: 0,
        hid_report: [0; HID_REPORT_BYTES],
    };

    state
        .lock()
        .expect("test state lock")
        .reads
        .push_back(Ok([0x00, 0x00, 0x00, 0x00]));
    assert_eq!(device.service(), GPIO_OK);
    assert_eq!(device.inputs(&mut inputs), GPIO_OK);
    assert_eq!(inputs.gpio_input_mask, 0x02);
    assert_eq!(inputs.hid_report, [0x00, 0x00, 0x00, 0x00]);

    state
        .lock()
        .expect("test state lock")
        .reads
        .push_back(Ok([0x10, 0xff, 0x00, 0x00]));
    assert_eq!(device.service(), GPIO_OK);
    assert_eq!(device.inputs(&mut inputs), GPIO_OK);
    assert_eq!(inputs.gpio_input_mask, 0xfd);
    assert_eq!(inputs.hid_report, [0x10, 0xff, 0x00, 0x00]);
}

/// Ensure bad action data never reaches the HID service owner.
#[test]
fn output_publication_validates_before_an_atomic_update() {
    let config = ffi_config();
    let (device, state) = fake_device(&config);
    let invalid = OutputAction {
        struct_size: size_of::<OutputAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_asserted: 2,
        gpio_output_mask: 0,
    };
    assert_eq!(device.publish_outputs(&invalid), GPIO_INVALID_ARGUMENT);
    assert_eq!(device.service(), GPIO_OK);
    assert_eq!(
        state.lock().expect("test state lock").writes,
        vec![[0, 0x01, 0x05, 0]]
    );
}

/// Make output and input transport failures visible without losing the latest requested action.
#[test]
fn service_reports_transport_failures_and_recovers_on_the_next_service() {
    let config = ffi_config();
    let state = Arc::new(Mutex::new(FakeState::default()));
    let transport = FakeTransport {
        state: Arc::clone(&state),
        next_write_error: Some(-71),
    };
    let device = GpioDevice::with_transport(validated(&config), Box::new(transport), 0x0008);
    assert_eq!(device.service(), GPIO_USB_ERROR);
    assert_eq!(device.service(), GPIO_OK);
    state
        .lock()
        .expect("test state lock")
        .reads
        .push_back(Err(-72));
    assert_eq!(device.service(), GPIO_USB_ERROR);

    let mut stats = DeviceStats {
        struct_size: size_of::<DeviceStats>() as u32,
        abi_version: 0,
        input_read_count: 0,
        output_apply_count: 0,
        usb_error_count: 0,
        ptt_applied: 0,
        online: 0,
        last_usb_error: 0,
        eeprom_read_count: 0,
        eeprom_write_count: 0,
    };
    assert_eq!(device.stats(&mut stats), GPIO_OK);
    assert_eq!(stats.output_apply_count, 2);
    assert_eq!(stats.input_read_count, 2);
    assert_eq!(stats.usb_error_count, 2);
    assert_eq!(stats.last_usb_error, -72);
}

/// Closing a device must submit an unkey action before the transport is released.
#[test]
fn close_unkeys_and_marks_the_device_offline() {
    let config = ffi_config();
    let (mut device, state) = fake_device(&config);
    let key = OutputAction {
        struct_size: size_of::<OutputAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_asserted: 1,
        gpio_output_mask: 0,
    };
    assert_eq!(device.publish_outputs(&key), GPIO_OK);
    assert_eq!(device.service(), GPIO_OK);
    let scheduled_unkey = ScheduledInvertingPulseAction {
        struct_size: size_of::<ScheduledInvertingPulseAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_invert: 1,
        gpio_invert_mask: 0,
        pulse_duration_milliseconds: 1_000,
        ptt_cancel: 0,
        gpio_cancel_mask: 0,
    };
    assert_eq!(device.schedule_inverting_pulse(&scheduled_unkey), GPIO_OK);
    assert_eq!(device.service(), GPIO_OK);
    device.close();
    let writes = state.lock().expect("test state lock").writes.clone();
    assert_eq!(writes, vec![[0, 0x04, 0x05, 0], [0, 0x00, 0x05, 0]]);
    let mut inputs = InputSnapshot {
        struct_size: size_of::<InputSnapshot>() as u32,
        abi_version: 0,
        online: 0,
        cor_active: 0,
        ctcss_active: 0,
        gpio_input_mask: 0,
        hid_report: [0; HID_REPORT_BYTES],
    };
    assert_eq!(device.inputs(&mut inputs), GPIO_OK);
    assert_eq!(inputs.online, 0);
}

/// Preserve ASL3's user EEPROM checksum without touching manufacturer words.
#[test]
fn eeprom_read_reports_validity_and_restores_latest_gpio_outputs() {
    let config = ffi_config();
    let (device, state) = fake_device(&config);
    let mut expected = [0; EEPROM_WORD_COUNT];
    expected[EEPROM_MAGIC_WORD] = EEPROM_MAGIC;
    expected[EEPROM_START_WORD + 1] = 711;
    expected[EEPROM_START_WORD + 2] = 412;
    expected[EEPROM_CHECKSUM_WORD] = eeprom_checksum_word(&expected);
    queue_eeprom_reads(&state, &expected);

    let mut image = eeprom_image(0);
    assert_eq!(device.read_eeprom(&mut image), GPIO_OK);
    assert_eq!(image.abi_version, ABI_VERSION);
    assert_eq!(image.words, expected);
    assert_eq!(image.magic_valid, 1);
    assert_eq!(image.checksum_valid, 1);

    let writes = state.lock().expect("test state lock").writes.clone();
    assert_eq!(writes.len(), EEPROM_CHECKSUM_WORD - EEPROM_START_WORD + 2);
    for (offset, report) in writes[..=EEPROM_CHECKSUM_WORD - EEPROM_START_WORD]
        .iter()
        .enumerate()
    {
        assert_eq!(
            *report,
            [0x80, 0, 0, 0x80 | (EEPROM_START_WORD + offset) as u8]
        );
    }
    assert_eq!(writes.last(), Some(&[0, 0x01, 0x05, 0]));

    let mut stats = DeviceStats {
        struct_size: size_of::<DeviceStats>() as u32,
        abi_version: 0,
        input_read_count: 0,
        output_apply_count: 0,
        usb_error_count: 0,
        ptt_applied: 0,
        online: 0,
        last_usb_error: 0,
        eeprom_read_count: 0,
        eeprom_write_count: 0,
    };
    assert_eq!(device.stats(&mut stats), GPIO_OK);
    assert_eq!(stats.eeprom_read_count, 13);
    assert_eq!(stats.eeprom_write_count, 0);
    assert_eq!(stats.input_read_count, 13);
    assert_eq!(stats.output_apply_count, 14);
}

/// Keep corrupt EEPROM contents observable rather than silently accepting tuning values.
#[test]
fn eeprom_read_marks_a_corrupt_image_invalid() {
    let config = ffi_config();
    let (device, state) = fake_device(&config);
    queue_eeprom_reads(&state, &[0; EEPROM_WORD_COUNT]);
    let mut image = eeprom_image(0);
    assert_eq!(device.read_eeprom(&mut image), GPIO_OK);
    assert_eq!(image.magic_valid, 0);
    assert_eq!(image.checksum_valid, 0);
}

/// Program only ASL3's user tuning region and retain its magic and checksum.
#[test]
fn eeprom_write_stamps_a_valid_image_and_restores_gpio_outputs() {
    let config = ffi_config();
    let (device, state) = fake_device(&config);
    let mut image = eeprom_image(ABI_VERSION);
    image.words[EEPROM_START_WORD + 1] = 711;
    image.words[EEPROM_START_WORD + 2] = 412;
    assert_eq!(device.write_eeprom(&mut image), GPIO_OK);
    assert_eq!(image.words[EEPROM_MAGIC_WORD], EEPROM_MAGIC);
    assert!(eeprom_checksum_is_valid(&image.words));
    assert_eq!(image.magic_valid, 1);
    assert_eq!(image.checksum_valid, 1);

    let writes = state.lock().expect("test state lock").writes.clone();
    assert_eq!(writes.len(), EEPROM_CHECKSUM_WORD - EEPROM_START_WORD + 2);
    for (offset, report) in writes[..=EEPROM_CHECKSUM_WORD - EEPROM_START_WORD]
        .iter()
        .enumerate()
    {
        assert_eq!(report[0], 0x80);
        assert_eq!(report[3], 0xc0 | (EEPROM_START_WORD + offset) as u8);
    }
    assert_eq!(writes.last(), Some(&[0, 0x01, 0x05, 0]));
}

/// Surface EEPROM transport failure while restoring the latest safe GPIO output action.
#[test]
fn eeprom_failure_does_not_leave_control_traffic_on_the_gpio_pins() {
    let config = ffi_config();
    let state = Arc::new(Mutex::new(FakeState::default()));
    let transport = FakeTransport {
        state: Arc::clone(&state),
        next_write_error: Some(-71),
    };
    let device = GpioDevice::with_transport(validated(&config), Box::new(transport), 0x0008);
    let mut image = eeprom_image(ABI_VERSION);
    assert_eq!(device.write_eeprom(&mut image), GPIO_USB_ERROR);
    assert_eq!(
        state.lock().expect("test state lock").writes,
        vec![[0, 0x01, 0x05, 0]]
    );
}

/// Surface an output-restoration failure even after EEPROM transfer data succeeds.
#[test]
fn eeprom_restore_failure_is_not_masked_by_a_successful_transfer() {
    let config = ffi_config();
    let (device, state) = fake_device_failing_on_write(&config, 14);
    let mut expected = [0; EEPROM_WORD_COUNT];
    expected[EEPROM_MAGIC_WORD] = EEPROM_MAGIC;
    expected[EEPROM_CHECKSUM_WORD] = eeprom_checksum_word(&expected);
    queue_eeprom_reads(&state, &expected);
    let mut image = eeprom_image(0);
    assert_eq!(device.read_eeprom(&mut image), GPIO_USB_ERROR);
    assert_eq!(image.words, expected);

    let (device, _) = fake_device_failing_on_write(&config, 14);
    let mut image = eeprom_image(ABI_VERSION);
    assert_eq!(device.write_eeprom(&mut image), GPIO_USB_ERROR);
    assert_eq!(image.words[EEPROM_MAGIC_WORD], EEPROM_MAGIC);
}

/// Maintain ABI-1 statistics-prefix compatibility when a caller lacks EEPROM fields.
#[test]
fn statistics_accept_the_original_abi_prefix_without_overwriting_extension_storage() {
    let config = ffi_config();
    let (device, _) = fake_device(&config);
    let mut stats = DeviceStats {
        struct_size: offset_of!(DeviceStats, eeprom_read_count) as u32,
        abi_version: 0,
        input_read_count: 0,
        output_apply_count: 0,
        usb_error_count: 0,
        ptt_applied: 0,
        online: 0,
        last_usb_error: 0,
        eeprom_read_count: 123,
        eeprom_write_count: 456,
    };
    assert_eq!(device.stats(&mut stats), GPIO_OK);
    assert_eq!(stats.eeprom_read_count, 123);
    assert_eq!(stats.eeprom_write_count, 456);
}

/// Exercise every public CM119 wiring map and the strict configuration boundary.
#[test]
fn configuration_accepts_each_profile_and_rejects_each_invalid_prefix() {
    for (profile, gpio_mask, gpio_control, ptt_mask) in [
        (0, 1, 0x04, 0x04),
        (1, 1, 0x08, 0x08),
        (2, 0, 0x04, 0x04),
        (3, 1, 0x0c, 0x04),
    ] {
        let mut config = ffi_config();
        config.profile = profile;
        config.gpio_output_enable_mask = gpio_mask;
        config.gpio_output_initial_mask = gpio_mask;
        let validated = validated(&config);
        assert_eq!(validated.profile.gpio_control, gpio_control);
        assert_eq!(validated.profile.ptt_mask, ptt_mask);
    }

    let mut explicit = ffi_config();
    explicit.vendor_id = 0x1234;
    explicit.product_id = 0x5678;
    explicit.ptt_inverted = 1;
    let explicit = validated(&explicit);
    assert_eq!(explicit.vendor_id, 0x1234);
    assert_eq!(explicit.product_id, Some(0x5678));
    assert!(explicit.ptt_inverted);

    let mut invalid = ffi_config();
    invalid.struct_size = 0;
    // SAFETY: each malformed test configuration is readable for its full declared type.
    assert!(unsafe { ValidatedConfig::from_ffi(&invalid) }.is_none());
    invalid = ffi_config();
    invalid.abi_version = ABI_VERSION + 1;
    // SAFETY: each malformed test configuration is readable for its full declared type.
    assert!(unsafe { ValidatedConfig::from_ffi(&invalid) }.is_none());
    invalid = ffi_config();
    invalid.usb_port_path = std::ptr::null();
    // SAFETY: each malformed test configuration is readable for its full declared type.
    assert!(unsafe { ValidatedConfig::from_ffi(&invalid) }.is_none());
    invalid = ffi_config();
    invalid.ptt_inverted = 2;
    // SAFETY: each malformed test configuration is readable for its full declared type.
    assert!(unsafe { ValidatedConfig::from_ffi(&invalid) }.is_none());
    invalid = ffi_config();
    invalid.gpio_output_enable_mask = u32::from(u8::MAX) + 1;
    // SAFETY: each malformed test configuration is readable for its full declared type.
    assert!(unsafe { ValidatedConfig::from_ffi(&invalid) }.is_none());
    invalid = ffi_config();
    invalid.gpio_output_initial_mask = u32::from(u8::MAX) + 1;
    // SAFETY: each malformed test configuration is readable for its full declared type.
    assert!(unsafe { ValidatedConfig::from_ffi(&invalid) }.is_none());
    // SAFETY: a null configuration is expressly rejected before dereference.
    assert!(unsafe { ValidatedConfig::from_ffi(std::ptr::null()) }.is_none());
}

/// Reject every malformed USB topology form before it can reach libusb.
#[test]
fn usb_topology_parser_accepts_only_a_stable_nonzero_port_chain() {
    assert_eq!(
        UsbPortPath::parse(c"12-1.2:1.0"),
        Some(UsbPortPath {
            bus: 12,
            ports: vec![1, 2],
        })
    );
    for value in [
        c"0-1",
        c"1",
        c"1-",
        c"1-0",
        c"1-1.0",
        c"1-1.2.3.4.5.6.7.8",
        c"x-1",
        c"1-x",
    ] {
        assert_eq!(UsbPortPath::parse(value), None, "{value:?}");
    }
    let invalid_utf8 =
        CStr::from_bytes_with_nul(&[b'1', b'-', 0xff, 0]).expect("explicit NUL terminator");
    assert_eq!(UsbPortPath::parse(invalid_utf8), None);
}

/// Keep profile-specific report encoding and input normalization deterministic.
#[test]
fn profiles_encode_ptt_polarity_and_decode_inputs() {
    let profile = Cm119Profile::from_public(1).expect("profile one");
    assert_eq!(
        profile.encode_outputs(false, 1, pack_output(true, 1)),
        [0, 0x09, 0x09, 0]
    );
    assert_eq!(
        profile.encode_outputs(true, 1, pack_output(true, 1)),
        [0, 0x01, 0x09, 0]
    );
    assert_eq!(
        profile.decode_inputs([0xfb, 0xa5, 0, 0], false),
        2 | (0xa5_u32 << 8)
    );
    assert!(Cm119Profile::from_public(4).is_none());
    assert_eq!(unpack_gpio(pack_output(false, 0xa5)), 0xa5);
}

/// Validate snapshots, statistics, and output actions before a service owner sees them.
#[test]
fn device_accessors_reject_short_prefixes_and_invalid_output_forms() {
    let mut config = ffi_config();
    config.gpio_output_enable_mask = 3;
    config.gpio_output_initial_mask = 0;
    let (device, _) = fake_device(&config);
    let valid = OutputAction {
        struct_size: size_of::<OutputAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_asserted: 0,
        gpio_output_mask: 3,
    };
    assert_eq!(device.publish_outputs(&valid), GPIO_OK);

    let mut invalid = OutputAction {
        struct_size: 0,
        abi_version: ABI_VERSION,
        ptt_asserted: 0,
        gpio_output_mask: 3,
    };
    assert_eq!(device.publish_outputs(&invalid), GPIO_INVALID_ARGUMENT);
    invalid = OutputAction {
        struct_size: size_of::<OutputAction>() as u32,
        abi_version: ABI_VERSION + 1,
        ptt_asserted: 0,
        gpio_output_mask: 3,
    };
    assert_eq!(device.publish_outputs(&invalid), GPIO_INVALID_ARGUMENT);
    invalid = OutputAction {
        struct_size: size_of::<OutputAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_asserted: 2,
        gpio_output_mask: 3,
    };
    assert_eq!(device.publish_outputs(&invalid), GPIO_INVALID_ARGUMENT);
    invalid = OutputAction {
        struct_size: size_of::<OutputAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_asserted: 0,
        gpio_output_mask: u32::from(u8::MAX) + 1,
    };
    assert_eq!(device.publish_outputs(&invalid), GPIO_INVALID_ARGUMENT);
    invalid = OutputAction {
        struct_size: size_of::<OutputAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_asserted: 0,
        gpio_output_mask: 4,
    };
    assert_eq!(device.publish_outputs(&invalid), GPIO_INVALID_ARGUMENT);

    let mut inputs = InputSnapshot {
        struct_size: 0,
        abi_version: 0,
        online: 0,
        cor_active: 0,
        ctcss_active: 0,
        gpio_input_mask: 0,
        hid_report: [0; HID_REPORT_BYTES],
    };
    assert_eq!(device.inputs(&mut inputs), GPIO_INVALID_ARGUMENT);
    let mut stats = DeviceStats {
        struct_size: 0,
        abi_version: 0,
        input_read_count: 0,
        output_apply_count: 0,
        usb_error_count: 0,
        ptt_applied: 0,
        online: 0,
        last_usb_error: 0,
        eeprom_read_count: 0,
        eeprom_write_count: 0,
    };
    assert_eq!(device.stats(&mut stats), GPIO_INVALID_ARGUMENT);

    let mut image = eeprom_image(ABI_VERSION);
    image.struct_size = 0;
    assert_eq!(device.read_eeprom(&mut image), GPIO_INVALID_ARGUMENT);
    assert_eq!(device.write_eeprom(&mut image), GPIO_INVALID_ARGUMENT);
    image = eeprom_image(ABI_VERSION + 1);
    assert_eq!(device.write_eeprom(&mut image), GPIO_INVALID_ARGUMENT);
}

/// Keep device information helpers ABI-safe without requiring an enumerated USB device.
#[test]
fn device_info_helpers_reset_prefixes_and_fill_a_complete_match() {
    let mut info = empty_device_info();
    assert_eq!(info.struct_size as usize, size_of::<DeviceInfo>());
    assert_eq!(info.abi_version, ABI_VERSION);
    info.present = 1;
    info.serial[0] = b'x' as c_char;
    info.struct_size = offset_of!(DeviceInfo, serial) as u32;
    clear_device_info(&mut info);
    assert_eq!(info.present, 0);
    assert_eq!(info.serial[0], b'x' as c_char);

    info.struct_size = size_of::<DeviceInfo>() as u32;
    let topology = UsbPortPath {
        bus: 2,
        ports: vec![3, 4],
    };
    let descriptor = ffi::LibusbDeviceDescriptor {
        b_length: 18,
        b_descriptor_type: 1,
        bcd_usb: 0x0200,
        b_device_class: 0,
        b_device_sub_class: 0,
        b_device_protocol: 0,
        b_max_packet_size_0: 64,
        id_vendor: CMEDIA_VENDOR_ID,
        id_product: 0x0008,
        bcd_device: 0x0100,
        i_manufacturer: 1,
        i_product: 2,
        i_serial_number: 3,
        b_num_configurations: 1,
    };
    let mut serial = [0; DEVICE_SERIAL_CAPACITY];
    serial[..3].copy_from_slice(&[b'a' as c_char, b'b' as c_char, 0]);
    fill_device_info(&mut info, &topology, descriptor, serial);
    assert_eq!(info.present, 1);
    assert_eq!(info.usb_bus, 2);
    assert_eq!(info.usb_port_number_count, 2);
    assert_eq!(&info.usb_port_numbers[..2], &[3, 4]);
    assert_eq!(info.serial[..3], serial[..3]);
    info.struct_size = offset_of!(DeviceInfo, serial) as u32;
    info.serial[0] = b'x' as c_char;
    fill_device_info(
        &mut info,
        &topology,
        descriptor,
        [0; DEVICE_SERIAL_CAPACITY],
    );
    assert_eq!(info.serial[0], b'x' as c_char);
    assert!(supported_cm119_descriptor(descriptor));
    assert!(!supported_cm119_descriptor(ffi::LibusbDeviceDescriptor {
        id_vendor: 0x1234,
        ..descriptor
    }));
}

/// The C ABI performs only pointer/prefix validation before it opens real hardware.
#[test]
fn c_abi_rejects_invalid_arguments_without_opening_hardware() {
    let config = ffi_config();
    let mut info = empty_device_info();
    assert_eq!(
        device_probe(std::ptr::null(), std::ptr::null_mut()),
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(
        device_probe(std::ptr::null(), &mut info),
        GPIO_INVALID_ARGUMENT
    );
    info.struct_size = 0;
    assert_eq!(device_probe(&config, &mut info), GPIO_INVALID_ARGUMENT);

    let mut list = DeviceList {
        struct_size: 0,
        abi_version: 0,
        matching_device_count: 0,
        returned_device_count: 0,
        devices: std::array::from_fn(|_| empty_device_info()),
    };
    assert_eq!(device_discover(std::ptr::null_mut()), GPIO_INVALID_ARGUMENT);
    assert_eq!(device_discover(&mut list), GPIO_INVALID_ARGUMENT);

    let mut handle = 1_usize as *mut GpioDevice;
    assert_eq!(
        device_open(std::ptr::null(), &mut handle),
        GPIO_INVALID_ARGUMENT
    );
    assert!(handle.is_null());
    assert_eq!(
        device_open(&config, std::ptr::null_mut()),
        GPIO_INVALID_ARGUMENT
    );

    assert_eq!(
        device_publish_outputs(std::ptr::null_mut(), std::ptr::null()),
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(
        device_publish_inverting_pulse(std::ptr::null_mut(), std::ptr::null()),
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(
        device_schedule_inverting_pulse(std::ptr::null_mut(), std::ptr::null()),
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(device_service(std::ptr::null_mut()), GPIO_INVALID_ARGUMENT);
    assert_eq!(
        device_get_inputs(std::ptr::null(), std::ptr::null_mut()),
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(
        device_get_stats(std::ptr::null(), std::ptr::null_mut()),
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(
        device_read_eeprom(std::ptr::null_mut(), std::ptr::null_mut()),
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(
        device_write_eeprom(std::ptr::null_mut(), std::ptr::null_mut()),
        GPIO_INVALID_ARGUMENT
    );
    device_close(std::ptr::null_mut());
}

/// The C ABI operates correctly over the same deterministic transport used by core tests.
#[test]
fn c_abi_operates_a_fake_device_and_transfers_ownership_on_close() {
    let config = ffi_config();
    let (device, state) = fake_device(&config);
    let device = Box::into_raw(Box::new(device));
    let action = OutputAction {
        struct_size: size_of::<OutputAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_asserted: 1,
        gpio_output_mask: 0,
    };
    assert_eq!(device_publish_outputs(device, &action), GPIO_OK);
    assert_eq!(device_service(device), GPIO_OK);
    let pulse = InvertingPulseAction {
        struct_size: size_of::<InvertingPulseAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_invert: 1,
        gpio_invert_mask: 0,
        pulse_duration_milliseconds: 1,
        cancel_pulse: 0,
    };
    assert_eq!(device_publish_inverting_pulse(device, &pulse), GPIO_OK);
    assert_eq!(device_service(device), GPIO_OK);

    let mut inputs = InputSnapshot {
        struct_size: size_of::<InputSnapshot>() as u32,
        abi_version: 0,
        online: 0,
        cor_active: 0,
        ctcss_active: 0,
        gpio_input_mask: 0,
        hid_report: [0; HID_REPORT_BYTES],
    };
    assert_eq!(device_get_inputs(device, &mut inputs), GPIO_OK);
    assert_eq!(inputs.online, 1);
    let mut stats = DeviceStats {
        struct_size: size_of::<DeviceStats>() as u32,
        abi_version: 0,
        input_read_count: 0,
        output_apply_count: 0,
        usb_error_count: 0,
        ptt_applied: 0,
        online: 0,
        last_usb_error: 0,
        eeprom_read_count: 0,
        eeprom_write_count: 0,
    };
    assert_eq!(device_get_stats(device, &mut stats), GPIO_OK);
    assert_eq!(stats.ptt_applied, 0);

    let mut expected = [0; EEPROM_WORD_COUNT];
    expected[EEPROM_MAGIC_WORD] = EEPROM_MAGIC;
    expected[EEPROM_CHECKSUM_WORD] = eeprom_checksum_word(&expected);
    queue_eeprom_reads(&state, &expected);
    let mut image = eeprom_image(0);
    assert_eq!(device_read_eeprom(device, &mut image), GPIO_OK);
    image = eeprom_image(ABI_VERSION);
    assert_eq!(device_write_eeprom(device, &mut image), GPIO_OK);

    device_close(device);
    let writes = state.lock().expect("test state lock").writes.clone();
    assert_eq!(writes.last(), Some(&[0, 0, 0x05, 0]));
}

/// Exercise per-bit CM119 pulse publication through the exported C ABI.
#[test]
fn c_abi_schedules_a_cm119_baseline_xor_pulse() {
    let config = ffi_config();
    let (device, state) = fake_device(&config);
    let device = Box::into_raw(Box::new(device));
    let action = ScheduledInvertingPulseAction {
        struct_size: size_of::<ScheduledInvertingPulseAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_invert: 0,
        gpio_invert_mask: 1,
        pulse_duration_milliseconds: 10,
        ptt_cancel: 0,
        gpio_cancel_mask: 0,
    };

    assert_eq!(device_schedule_inverting_pulse(device, &action), GPIO_OK);
    assert_eq!(device_service(device), GPIO_OK);
    device_close(device);
    assert_eq!(
        state.lock().expect("test state lock").writes,
        [[0, 0x00, 0x05, 0], [0, 0x01, 0x05, 0]]
    );
}

/// Dropping an unopened libusb wrapper is a no-op and never touches an FFI handle.
#[test]
fn unopened_libusb_transport_drops_without_ffi_calls() {
    let unopened = LibusbHid {
        context: std::ptr::null_mut(),
        handle: std::ptr::null_mut(),
        claimed: false,
    };
    drop(unopened);
}

/// Drive discovery, opening, HID exchange, and release through a deterministic libusb backend.
#[test]
fn libusb_backend_covers_successful_discovery_open_exchange_and_release() {
    let (backend_guard, state) = install_fake_libusb();
    let config = ffi_config();
    let mut info = empty_device_info();
    assert_eq!(device_probe(&config, &mut info), GPIO_OK);
    assert_eq!(info.present, 1);
    assert_eq!(
        info.serial[..10]
            .iter()
            .map(|value| *value as u8)
            .collect::<Vec<_>>(),
        b"CM119-TEST"
    );
    assert_eq!(info.serial[10], 0);

    let mut list = DeviceList {
        struct_size: size_of::<DeviceList>() as u32,
        abi_version: 0,
        matching_device_count: 0,
        returned_device_count: 0,
        devices: std::array::from_fn(|_| empty_device_info()),
    };
    assert_eq!(device_discover(&mut list), GPIO_OK);
    assert_eq!(list.matching_device_count, 1);
    assert_eq!(list.returned_device_count, 1);
    assert_eq!(list.devices[0].product_id, 0x0008);

    let mut device = std::ptr::null_mut();
    assert_eq!(device_open(&config, &mut device), GPIO_OK);
    assert!(!device.is_null());
    let action = OutputAction {
        struct_size: size_of::<OutputAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_asserted: 1,
        gpio_output_mask: 0,
    };
    assert_eq!(device_publish_outputs(device, &action), GPIO_OK);
    assert_eq!(device_service(device), GPIO_OK);
    device_close(device);
    drop(backend_guard);

    let state = state.lock().expect("test state lock");
    assert_eq!(state.init_calls, 3);
    assert_eq!(state.exit_calls, 3);
    assert_eq!(state.list_calls, 3);
    assert_eq!(state.free_list_calls, 3);
    assert_eq!(state.open_calls, 3);
    assert_eq!(state.serial_calls, 2);
    assert_eq!(state.claim_calls, 1);
    assert_eq!(state.release_calls, 1);
    assert_eq!(state.close_calls, 3);
    assert!(state.transfer_calls >= 4);
    assert_eq!(state.writes.first(), Some(&[0, 0x04, 0x05, 0]));
}

/// Exercise the established kernel-driver detach and retry sequence during HID claiming.
#[test]
fn libusb_backend_retries_claim_after_detaching_an_active_kernel_driver() {
    let state = Arc::new(Mutex::new(FakeLibusbState::default()));
    let mut backend = FakeLibusbBackend::new(Arc::clone(&state));
    backend.claim_results.extend([-7, ffi::LIBUSB_SUCCESS]);
    backend.kernel_active_result = 1;
    let backend_guard = ffi::install_test_backend(Box::new(backend));
    let config = ffi_config();
    let mut device = std::ptr::null_mut();
    assert_eq!(device_open(&config, &mut device), GPIO_OK);
    device_close(device);
    drop(backend_guard);

    let state = state.lock().expect("test state lock");
    assert_eq!(state.claim_calls, 2);
    assert_eq!(state.kernel_active_calls, 1);
    assert_eq!(state.detach_calls, 1);
    assert_eq!(state.release_calls, 1);
}

/// Keep malformed libusb discovery data fail-closed before a device can be claimed.
#[test]
fn libusb_backend_rejects_malformed_topology_and_descriptor_results() {
    let config = ffi_config();
    for (bus, ports, port_result, descriptor_result) in [
        (0, vec![1], None, ffi::LIBUSB_SUCCESS),
        (3, vec![1], Some(0), ffi::LIBUSB_SUCCESS),
        (
            3,
            vec![1],
            Some((PORT_CHAIN_CAPACITY + 1) as c_int),
            ffi::LIBUSB_SUCCESS,
        ),
        (3, vec![0], None, ffi::LIBUSB_SUCCESS),
        (3, vec![1], None, -8),
    ] {
        let state = Arc::new(Mutex::new(FakeLibusbState::default()));
        let mut backend = FakeLibusbBackend::new(state);
        backend.bus = bus;
        backend.ports = ports;
        backend.port_result = port_result;
        backend.descriptor_result = descriptor_result;
        let backend_guard = ffi::install_test_backend(Box::new(backend));
        let mut device = std::ptr::null_mut();
        assert_eq!(device_open(&config, &mut device), GPIO_UNSUPPORTED);
        assert!(device.is_null());
        drop(backend_guard);
    }
}

/// Surface every initialization, enumeration, open, and claim failure without retaining resources.
#[test]
fn libusb_backend_reports_open_failures_and_releases_partial_resources() {
    let config = ffi_config();
    for (
        init_result,
        null_context,
        list_result,
        open_result,
        null_handle,
        claim_result,
        active,
        detach,
    ) in [
        (-1, false, 1, 0, false, 0, 0, 0),
        (0, true, 1, 0, false, 0, 0, 0),
        (0, false, -2, 0, false, 0, 0, 0),
        (0, false, 1, -3, false, 0, 0, 0),
        (0, false, 1, 0, true, 0, 0, 0),
        (0, false, 1, 0, false, -4, 0, 0),
        (0, false, 1, 0, false, -5, 1, -6),
    ] {
        let state = Arc::new(Mutex::new(FakeLibusbState::default()));
        let mut backend = FakeLibusbBackend::new(state);
        backend.init_result = init_result;
        backend.null_context_on_init = null_context;
        backend.device_list_result = list_result;
        backend.open_results.push_back(open_result);
        backend.null_handle_on_open = null_handle;
        backend.claim_results.push_back(claim_result);
        backend.kernel_active_result = active;
        backend.detach_result = detach;
        let backend_guard = ffi::install_test_backend(Box::new(backend));
        let mut device = std::ptr::null_mut();
        assert_eq!(device_open(&config, &mut device), GPIO_USB_ERROR);
        assert!(device.is_null());
        drop(backend_guard);
    }
}

/// Make short and failed HID transfers observable as the established public USB error.
#[test]
fn libusb_backend_rejects_short_and_failed_hid_transfers() {
    let config = ffi_config();
    for (output_result, input_result, input_return) in [
        (
            HID_REPORT_BYTES as c_int - 1,
            Ok([u8::MAX; HID_REPORT_BYTES]),
            None,
        ),
        (-9, Ok([u8::MAX; HID_REPORT_BYTES]), None),
        (HID_REPORT_BYTES as c_int, Err(-10), None),
        (
            HID_REPORT_BYTES as c_int,
            Ok([u8::MAX; HID_REPORT_BYTES]),
            Some(HID_REPORT_BYTES as c_int - 1),
        ),
    ] {
        let state = Arc::new(Mutex::new(FakeLibusbState::default()));
        let mut backend = FakeLibusbBackend::new(state);
        backend.output_results.push_back(output_result);
        backend.input_results.push_back(input_result);
        if let Some(input_return) = input_return {
            backend.input_return_results.push_back(input_return);
        }
        let backend_guard = ffi::install_test_backend(Box::new(backend));
        let mut device = std::ptr::null_mut();
        assert_eq!(device_open(&config, &mut device), GPIO_OK);
        assert_eq!(device_service(device), GPIO_USB_ERROR);
        device_close(device);
        drop(backend_guard);
    }
}

/// Verify serial-number and descriptor helpers reject incomplete temporary libusb operations.
#[test]
fn libusb_backend_handles_missing_and_failed_serial_queries() {
    let device = 0x1000_usize as *mut ffi::LibusbDevice;
    let state = Arc::new(Mutex::new(FakeLibusbState::default()));
    let mut backend = FakeLibusbBackend::new(state);
    backend.descriptor.i_serial_number = 0;
    let backend_guard = ffi::install_test_backend(Box::new(backend));
    let descriptor = ffi::LibusbDeviceDescriptor {
        i_serial_number: 0,
        ..FakeLibusbBackend::new(Arc::new(Mutex::new(FakeLibusbState::default()))).descriptor
    };
    assert_eq!(
        device_serial(device, descriptor),
        [0; DEVICE_SERIAL_CAPACITY]
    );
    drop(backend_guard);

    let state = Arc::new(Mutex::new(FakeLibusbState::default()));
    let mut backend = FakeLibusbBackend::new(state);
    backend.open_results.push_back(-11);
    let descriptor = backend.descriptor;
    let backend_guard = ffi::install_test_backend(Box::new(backend));
    assert_eq!(
        device_serial(device, descriptor),
        [0; DEVICE_SERIAL_CAPACITY]
    );
    drop(backend_guard);

    let state = Arc::new(Mutex::new(FakeLibusbState::default()));
    let mut backend = FakeLibusbBackend::new(state);
    backend.serial_result = Some(0);
    let descriptor = backend.descriptor;
    let backend_guard = ffi::install_test_backend(Box::new(backend));
    assert_eq!(
        device_serial(device, descriptor),
        [0; DEVICE_SERIAL_CAPACITY]
    );
    drop(backend_guard);
}

/// Exercise probe and discovery initialization/enumeration failures without an actual USB bus.
#[test]
fn libusb_backend_reports_probe_and_discovery_transport_failures() {
    let config = ffi_config();
    for (init_result, list_result) in [(-1, 1), (ffi::LIBUSB_SUCCESS, -2)] {
        let state = Arc::new(Mutex::new(FakeLibusbState::default()));
        let mut backend = FakeLibusbBackend::new(state);
        backend.init_result = init_result;
        backend.device_list_result = list_result;
        let backend_guard = ffi::install_test_backend(Box::new(backend));
        let mut info = empty_device_info();
        assert_eq!(device_probe(&config, &mut info), GPIO_USB_ERROR);
        let mut list = DeviceList {
            struct_size: size_of::<DeviceList>() as u32,
            abi_version: 0,
            matching_device_count: 0,
            returned_device_count: 0,
            devices: std::array::from_fn(|_| empty_device_info()),
        };
        assert_eq!(device_discover(&mut list), GPIO_USB_ERROR);
        drop(backend_guard);
    }
}

/// Keep probe and opening fail-closed when enumeration changes between descriptor reads.
#[test]
fn libusb_backend_handles_nonmatching_and_disappearing_devices() {
    let config = ffi_config();
    let state = Arc::new(Mutex::new(FakeLibusbState::default()));
    let mut backend = FakeLibusbBackend::new(state);
    backend.descriptor.id_vendor = 0x1234;
    let backend_guard = ffi::install_test_backend(Box::new(backend));
    let mut info = empty_device_info();
    assert_eq!(device_probe(&config, &mut info), GPIO_UNSUPPORTED);
    drop(backend_guard);

    let state = Arc::new(Mutex::new(FakeLibusbState::default()));
    let mut backend = FakeLibusbBackend::new(state);
    backend
        .descriptor_results
        .extend([ffi::LIBUSB_SUCCESS, -12]);
    let backend_guard = ffi::install_test_backend(Box::new(backend));
    let mut info = empty_device_info();
    assert_eq!(device_probe(&config, &mut info), GPIO_UNSUPPORTED);
    drop(backend_guard);

    let state = Arc::new(Mutex::new(FakeLibusbState::default()));
    let mut backend = FakeLibusbBackend::new(state);
    backend
        .descriptor_results
        .extend([ffi::LIBUSB_SUCCESS, -13]);
    let backend_guard = ffi::install_test_backend(Box::new(backend));
    let mut device = std::ptr::null_mut();
    assert_eq!(device_open(&config, &mut device), GPIO_UNSUPPORTED);
    assert!(device.is_null());
    drop(backend_guard);
}

/// Cover discovery's null, unsupported, malformed, and bounded-list candidate cases.
#[test]
fn libusb_backend_discovery_skips_bad_candidates_and_bounds_the_snapshot() {
    let cases: Vec<Box<dyn FnOnce() -> FakeLibusbBackend>> = vec![
        Box::new(|| {
            let state = Arc::new(Mutex::new(FakeLibusbState::default()));
            let mut backend = FakeLibusbBackend::new(state);
            backend.device_list[0] = std::ptr::null_mut();
            backend
        }),
        Box::new(|| {
            let state = Arc::new(Mutex::new(FakeLibusbState::default()));
            let mut backend = FakeLibusbBackend::new(state);
            backend.descriptor_result = -14;
            backend
        }),
        Box::new(|| {
            let state = Arc::new(Mutex::new(FakeLibusbState::default()));
            let mut backend = FakeLibusbBackend::new(state);
            backend.descriptor.id_product = 0xffff;
            backend
        }),
        Box::new(|| {
            let state = Arc::new(Mutex::new(FakeLibusbState::default()));
            let mut backend = FakeLibusbBackend::new(state);
            backend.bus = 0;
            backend
        }),
        Box::new(|| {
            let state = Arc::new(Mutex::new(FakeLibusbState::default()));
            let mut backend = FakeLibusbBackend::new(state);
            backend.set_device_count(DEVICE_LIST_CAPACITY + 1);
            backend
        }),
    ];
    for make_backend in cases {
        let backend_guard = ffi::install_test_backend(Box::new(make_backend()));
        let mut list = DeviceList {
            struct_size: size_of::<DeviceList>() as u32,
            abi_version: 0,
            matching_device_count: 0,
            returned_device_count: 0,
            devices: std::array::from_fn(|_| empty_device_info()),
        };
        assert_eq!(device_discover(&mut list), GPIO_OK);
        assert!(list.returned_device_count <= DEVICE_LIST_CAPACITY as u32);
        drop(backend_guard);
    }
}

/// Validate every C ABI pointer pair independently once a fake device exists.
#[test]
fn c_abi_checks_the_second_pointer_after_a_valid_device_handle() {
    let config = ffi_config();
    let (device, _) = fake_device(&config);
    let device = Box::into_raw(Box::new(device));
    let action = OutputAction {
        struct_size: size_of::<OutputAction>() as u32,
        abi_version: ABI_VERSION,
        ptt_asserted: 0,
        gpio_output_mask: 0,
    };
    assert_eq!(
        device_publish_outputs(device, std::ptr::null()),
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(
        device_get_inputs(device, std::ptr::null_mut()),
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(
        device_get_stats(device, std::ptr::null_mut()),
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(
        device_read_eeprom(device, std::ptr::null_mut()),
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(
        device_write_eeprom(device, std::ptr::null_mut()),
        GPIO_INVALID_ARGUMENT
    );
    assert_eq!(device_publish_outputs(device, &action), GPIO_OK);
    device_close(device);
}

/// Preserve a failed EEPROM read as an error even when output restoration succeeds.
#[test]
fn eeprom_read_failure_remains_visible_after_output_restoration() {
    let config = ffi_config();
    let (device, state) = fake_device(&config);
    state
        .lock()
        .expect("test state lock")
        .reads
        .push_back(Err(-74));
    let mut image = eeprom_image(0);
    assert_eq!(device.read_eeprom(&mut image), GPIO_USB_ERROR);
}

/// Cover the remaining null-handle and short-circuit variants of libusb-backed helpers.
#[test]
fn libusb_backend_covers_null_context_handle_and_candidate_short_circuits() {
    let config = ffi_config();
    let state = Arc::new(Mutex::new(FakeLibusbState::default()));
    let mut backend = FakeLibusbBackend::new(state);
    backend.null_context_on_init = true;
    let backend_guard = ffi::install_test_backend(Box::new(backend));
    let mut info = empty_device_info();
    assert_eq!(device_probe(&config, &mut info), GPIO_USB_ERROR);
    drop(backend_guard);

    let state = Arc::new(Mutex::new(FakeLibusbState::default()));
    let mut backend = FakeLibusbBackend::new(state);
    backend.null_context_on_init = true;
    let backend_guard = ffi::install_test_backend(Box::new(backend));
    let mut list = DeviceList {
        struct_size: size_of::<DeviceList>() as u32,
        abi_version: 0,
        matching_device_count: 0,
        returned_device_count: 0,
        devices: std::array::from_fn(|_| empty_device_info()),
    };
    assert_eq!(device_discover(&mut list), GPIO_USB_ERROR);
    drop(backend_guard);

    let state = Arc::new(Mutex::new(FakeLibusbState::default()));
    let backend = FakeLibusbBackend::new(state);
    let backend_guard = ffi::install_test_backend(Box::new(backend));
    let mut wrong_product = ffi_config();
    wrong_product.product_id = 0x000c;
    let mut info = empty_device_info();
    assert_eq!(device_probe(&wrong_product, &mut info), GPIO_UNSUPPORTED);
    drop(backend_guard);

    let state = Arc::new(Mutex::new(FakeLibusbState::default()));
    let mut backend = FakeLibusbBackend::new(state);
    backend.device_list[0] = std::ptr::null_mut();
    let backend_guard = ffi::install_test_backend(Box::new(backend));
    let mut info = empty_device_info();
    assert_eq!(device_probe(&config, &mut info), GPIO_UNSUPPORTED);
    let mut device = std::ptr::null_mut();
    assert_eq!(device_open(&config, &mut device), GPIO_UNSUPPORTED);
    assert!(device.is_null());
    drop(backend_guard);

    let state = Arc::new(Mutex::new(FakeLibusbState::default()));
    let mut backend = FakeLibusbBackend::new(state);
    backend.null_handle_on_open = true;
    let descriptor = backend.descriptor;
    let backend_guard = ffi::install_test_backend(Box::new(backend));
    assert_eq!(
        device_serial(0x1000_usize as *mut ffi::LibusbDevice, descriptor),
        [0; DEVICE_SERIAL_CAPACITY]
    );
    drop(backend_guard);
}
