//! Minimal libusb declarations contained by the CM119 HID adapter.

use std::ffi::{c_char, c_int, c_uchar, c_uint, c_ulong, c_ushort};

#[cfg(test)]
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Opaque libusb process context.
#[repr(C)]
pub(crate) struct LibusbContext {
    _private: [u8; 0],
}

/// Opaque libusb device object.
#[repr(C)]
pub(crate) struct LibusbDevice {
    _private: [u8; 0],
}

/// Opaque libusb device handle.
#[repr(C)]
pub(crate) struct LibusbDeviceHandle {
    _private: [u8; 0],
}

/// USB device descriptor returned by libusb.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct LibusbDeviceDescriptor {
    pub(crate) b_length: c_uchar,
    pub(crate) b_descriptor_type: c_uchar,
    pub(crate) bcd_usb: c_ushort,
    pub(crate) b_device_class: c_uchar,
    pub(crate) b_device_sub_class: c_uchar,
    pub(crate) b_device_protocol: c_uchar,
    pub(crate) b_max_packet_size_0: c_uchar,
    pub(crate) id_vendor: c_ushort,
    pub(crate) id_product: c_ushort,
    pub(crate) bcd_device: c_ushort,
    pub(crate) i_manufacturer: c_uchar,
    pub(crate) i_product: c_uchar,
    pub(crate) i_serial_number: c_uchar,
    pub(crate) b_num_configurations: c_uchar,
}

/// Success return code from libusb.
pub(crate) const LIBUSB_SUCCESS: c_int = 0;

/// HID endpoint direction and request constants for the CM119 report interface.
pub(crate) const LIBUSB_ENDPOINT_IN: c_uchar = 0x80;
pub(crate) const LIBUSB_ENDPOINT_OUT: c_uchar = 0x00;
pub(crate) const LIBUSB_REQUEST_TYPE_CLASS: c_uchar = 0x20;
pub(crate) const LIBUSB_RECIPIENT_INTERFACE: c_uchar = 0x01;
pub(crate) const HID_REPORT_GET: c_uchar = 0x01;
pub(crate) const HID_REPORT_SET: c_uchar = 0x09;
pub(crate) const HID_REPORT_INPUT: c_ushort = 0x0100;
pub(crate) const HID_REPORT_OUTPUT: c_ushort = 0x0200;

/// One complete libusb HID control-transfer request.
///
/// Grouping the FFI arguments keeps production and deterministic test backends
/// aligned without exposing this internal transport structure through the ABI.
pub(crate) struct LibusbControlTransfer {
    /// Exclusive libusb device handle.
    pub(crate) handle: *mut LibusbDeviceHandle,
    /// Endpoint and request-type bits.
    pub(crate) request_type: c_uchar,
    /// HID request selector.
    pub(crate) request: c_uchar,
    /// HID report type and identifier.
    pub(crate) value: c_ushort,
    /// CM119 HID interface number.
    pub(crate) index: c_ushort,
    /// Caller-owned report storage.
    pub(crate) data: *mut c_uchar,
    /// Exact report-storage width.
    pub(crate) length: c_ushort,
    /// Bounded control-transfer timeout.
    pub(crate) timeout_milliseconds: c_uint,
}

#[cfg(not(test))]
#[link(name = "usb-1.0")]
unsafe extern "C" {
    /// Initialize a libusb process context.
    #[link_name = "libusb_init"]
    fn libusb_init_system(context: *mut *mut LibusbContext) -> c_int;
    /// Shut down a libusb process context.
    #[link_name = "libusb_exit"]
    fn libusb_exit_system(context: *mut LibusbContext);
    /// Obtain the process-visible USB device list.
    #[link_name = "libusb_get_device_list"]
    fn libusb_get_device_list_system(
        context: *mut LibusbContext,
        devices: *mut *mut *mut LibusbDevice,
    ) -> isize;
    /// Release a list returned by `libusb_get_device_list`.
    #[link_name = "libusb_free_device_list"]
    fn libusb_free_device_list_system(devices: *mut *mut LibusbDevice, unref_devices: c_int);
    /// Obtain one device descriptor.
    #[link_name = "libusb_get_device_descriptor"]
    fn libusb_get_device_descriptor_system(
        device: *mut LibusbDevice,
        descriptor: *mut LibusbDeviceDescriptor,
    ) -> c_int;
    /// Obtain the USB bus number for a device.
    #[link_name = "libusb_get_bus_number"]
    fn libusb_get_bus_number_system(device: *mut LibusbDevice) -> c_uchar;
    /// Obtain the stable physical port chain for a device.
    #[link_name = "libusb_get_port_numbers"]
    fn libusb_get_port_numbers_system(
        device: *mut LibusbDevice,
        ports: *mut c_uchar,
        port_count: c_int,
    ) -> c_int;
    /// Open one enumerated USB device.
    #[link_name = "libusb_open"]
    fn libusb_open_system(device: *mut LibusbDevice, handle: *mut *mut LibusbDeviceHandle)
    -> c_int;
    /// Read one USB string descriptor as bounded ASCII bytes.
    #[link_name = "libusb_get_string_descriptor_ascii"]
    fn libusb_get_string_descriptor_ascii_system(
        handle: *mut LibusbDeviceHandle,
        descriptor_index: c_uchar,
        data: *mut c_uchar,
        length: c_int,
    ) -> c_int;
    /// Close a previously opened USB device handle.
    #[link_name = "libusb_close"]
    fn libusb_close_system(handle: *mut LibusbDeviceHandle);
    /// Claim an interface for exclusive application control.
    #[link_name = "libusb_claim_interface"]
    fn libusb_claim_interface_system(handle: *mut LibusbDeviceHandle, interface: c_int) -> c_int;
    /// Release a claimed interface.
    #[link_name = "libusb_release_interface"]
    fn libusb_release_interface_system(handle: *mut LibusbDeviceHandle, interface: c_int) -> c_int;
    /// Report whether a kernel driver currently owns an interface.
    #[link_name = "libusb_kernel_driver_active"]
    fn libusb_kernel_driver_active_system(
        handle: *mut LibusbDeviceHandle,
        interface: c_int,
    ) -> c_int;
    /// Detach a kernel driver before claiming an interface.
    #[link_name = "libusb_detach_kernel_driver"]
    fn libusb_detach_kernel_driver_system(
        handle: *mut LibusbDeviceHandle,
        interface: c_int,
    ) -> c_int;
    /// Perform one class control transfer.
    #[link_name = "libusb_control_transfer"]
    fn libusb_control_transfer_system(
        handle: *mut LibusbDeviceHandle,
        request_type: c_uchar,
        request: c_uchar,
        value: c_ushort,
        index: c_ushort,
        data: *mut c_uchar,
        length: c_ushort,
        timeout_milliseconds: c_uint,
    ) -> c_int;
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_init(context: *mut *mut LibusbContext) -> c_int {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe { libusb_init_system(context) }
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_exit(context: *mut LibusbContext) {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe { libusb_exit_system(context) };
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_get_device_list(
    context: *mut LibusbContext,
    devices: *mut *mut *mut LibusbDevice,
) -> isize {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe { libusb_get_device_list_system(context, devices) }
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_free_device_list(
    devices: *mut *mut LibusbDevice,
    unref_devices: c_int,
) {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe { libusb_free_device_list_system(devices, unref_devices) };
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_get_device_descriptor(
    device: *mut LibusbDevice,
    descriptor: *mut LibusbDeviceDescriptor,
) -> c_int {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe { libusb_get_device_descriptor_system(device, descriptor) }
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_get_bus_number(device: *mut LibusbDevice) -> c_uchar {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe { libusb_get_bus_number_system(device) }
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_get_port_numbers(
    device: *mut LibusbDevice,
    ports: *mut c_uchar,
    port_count: c_int,
) -> c_int {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe { libusb_get_port_numbers_system(device, ports, port_count) }
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_open(
    device: *mut LibusbDevice,
    handle: *mut *mut LibusbDeviceHandle,
) -> c_int {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe { libusb_open_system(device, handle) }
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_get_string_descriptor_ascii(
    handle: *mut LibusbDeviceHandle,
    descriptor_index: c_uchar,
    data: *mut c_uchar,
    length: c_int,
) -> c_int {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe { libusb_get_string_descriptor_ascii_system(handle, descriptor_index, data, length) }
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_close(handle: *mut LibusbDeviceHandle) {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe { libusb_close_system(handle) };
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_claim_interface(
    handle: *mut LibusbDeviceHandle,
    interface: c_int,
) -> c_int {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe { libusb_claim_interface_system(handle, interface) }
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_release_interface(
    handle: *mut LibusbDeviceHandle,
    interface: c_int,
) -> c_int {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe { libusb_release_interface_system(handle, interface) }
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_kernel_driver_active(
    handle: *mut LibusbDeviceHandle,
    interface: c_int,
) -> c_int {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe { libusb_kernel_driver_active_system(handle, interface) }
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_detach_kernel_driver(
    handle: *mut LibusbDeviceHandle,
    interface: c_int,
) -> c_int {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe { libusb_detach_kernel_driver_system(handle, interface) }
}

/// Call the production libusb symbol or, in tests, the installed deterministic backend.
#[cfg(not(test))]
pub(crate) unsafe fn libusb_control_transfer(transfer: LibusbControlTransfer) -> c_int {
    // SAFETY: this wrapper preserves libusb's public argument contract.
    unsafe {
        libusb_control_transfer_system(
            transfer.handle,
            transfer.request_type,
            transfer.request,
            transfer.value,
            transfer.index,
            transfer.data,
            transfer.length,
            transfer.timeout_milliseconds,
        )
    }
}

/// Deterministic libusb surface used only by unit tests.
///
/// Production builds directly call the dynamically linked libusb symbols above.
#[cfg(test)]
pub(crate) trait TestLibusbBackend: Send {
    /// Initialize one synthetic libusb context.
    unsafe fn init(&mut self, context: *mut *mut LibusbContext) -> c_int;
    /// Dispose one synthetic libusb context.
    unsafe fn exit(&mut self, context: *mut LibusbContext);
    /// Return one synthetic device-list pointer and count.
    unsafe fn get_device_list(
        &mut self,
        context: *mut LibusbContext,
        devices: *mut *mut *mut LibusbDevice,
    ) -> isize;
    /// Release one synthetic device list.
    unsafe fn free_device_list(&mut self, devices: *mut *mut LibusbDevice, unref_devices: c_int);
    /// Copy one synthetic USB device descriptor.
    unsafe fn get_device_descriptor(
        &mut self,
        device: *mut LibusbDevice,
        descriptor: *mut LibusbDeviceDescriptor,
    ) -> c_int;
    /// Return the synthetic USB bus number.
    unsafe fn get_bus_number(&mut self, device: *mut LibusbDevice) -> c_uchar;
    /// Copy the synthetic USB hub-port chain.
    unsafe fn get_port_numbers(
        &mut self,
        device: *mut LibusbDevice,
        ports: *mut c_uchar,
        port_count: c_int,
    ) -> c_int;
    /// Open one synthetic USB device.
    unsafe fn open(
        &mut self,
        device: *mut LibusbDevice,
        handle: *mut *mut LibusbDeviceHandle,
    ) -> c_int;
    /// Copy one synthetic USB serial string.
    unsafe fn get_string_descriptor_ascii(
        &mut self,
        handle: *mut LibusbDeviceHandle,
        descriptor_index: c_uchar,
        data: *mut c_uchar,
        length: c_int,
    ) -> c_int;
    /// Close one synthetic USB handle.
    unsafe fn close(&mut self, handle: *mut LibusbDeviceHandle);
    /// Claim the synthetic HID interface.
    unsafe fn claim_interface(
        &mut self,
        handle: *mut LibusbDeviceHandle,
        interface: c_int,
    ) -> c_int;
    /// Release the synthetic HID interface.
    unsafe fn release_interface(
        &mut self,
        handle: *mut LibusbDeviceHandle,
        interface: c_int,
    ) -> c_int;
    /// Report synthetic kernel-driver ownership.
    unsafe fn kernel_driver_active(
        &mut self,
        handle: *mut LibusbDeviceHandle,
        interface: c_int,
    ) -> c_int;
    /// Detach the synthetic kernel driver.
    unsafe fn detach_kernel_driver(
        &mut self,
        handle: *mut LibusbDeviceHandle,
        interface: c_int,
    ) -> c_int;
    /// Process one synthetic HID control transfer.
    unsafe fn control_transfer(&mut self, transfer: LibusbControlTransfer) -> c_int;
}

/// One test-only installed backend, serialized so parallel unit tests cannot cross-contaminate.
#[cfg(test)]
fn test_backend_slot() -> &'static Mutex<Option<Box<dyn TestLibusbBackend>>> {
    static SLOT: OnceLock<Mutex<Option<Box<dyn TestLibusbBackend>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Serialize tests that replace the libusb backend.
#[cfg(test)]
fn test_backend_serial_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Recover a test mutex after a deliberately asserted failure without leaking a backend.
#[cfg(test)]
#[cfg_attr(coverage, coverage(off))]
fn test_lock<T>(lock: &'static Mutex<T>) -> MutexGuard<'static, T> {
    match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// RAII installation token for one deterministic libusb backend.
#[cfg(test)]
pub(crate) struct TestBackendGuard {
    /// Holds exclusive test ownership until the backend slot is cleared.
    _serial: MutexGuard<'static, ()>,
}

#[cfg(test)]
impl Drop for TestBackendGuard {
    fn drop(&mut self) {
        *test_lock(test_backend_slot()) = None;
    }
}

/// Install one deterministic backend for the duration of a unit test.
#[cfg(test)]
pub(crate) fn install_test_backend(backend: Box<dyn TestLibusbBackend>) -> TestBackendGuard {
    let serial = test_lock(test_backend_serial_lock());
    *test_lock(test_backend_slot()) = Some(backend);
    TestBackendGuard { _serial: serial }
}

/// Invoke the installed test backend.
#[cfg(test)]
fn with_test_backend<T>(operation: impl FnOnce(&mut dyn TestLibusbBackend) -> T) -> T {
    let mut slot = test_lock(test_backend_slot());
    let backend = slot
        .as_deref_mut()
        .expect("libusb operation needs an installed deterministic test backend");
    operation(backend)
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_init(context: *mut *mut LibusbContext) -> c_int {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.init(context) }
    })
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_exit(context: *mut LibusbContext) {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.exit(context) };
    });
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_get_device_list(
    context: *mut LibusbContext,
    devices: *mut *mut *mut LibusbDevice,
) -> isize {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.get_device_list(context, devices) }
    })
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_free_device_list(
    devices: *mut *mut LibusbDevice,
    unref_devices: c_int,
) {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.free_device_list(devices, unref_devices) };
    });
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_get_device_descriptor(
    device: *mut LibusbDevice,
    descriptor: *mut LibusbDeviceDescriptor,
) -> c_int {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.get_device_descriptor(device, descriptor) }
    })
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_get_bus_number(device: *mut LibusbDevice) -> c_uchar {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.get_bus_number(device) }
    })
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_get_port_numbers(
    device: *mut LibusbDevice,
    ports: *mut c_uchar,
    port_count: c_int,
) -> c_int {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.get_port_numbers(device, ports, port_count) }
    })
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_open(
    device: *mut LibusbDevice,
    handle: *mut *mut LibusbDeviceHandle,
) -> c_int {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.open(device, handle) }
    })
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_get_string_descriptor_ascii(
    handle: *mut LibusbDeviceHandle,
    descriptor_index: c_uchar,
    data: *mut c_uchar,
    length: c_int,
) -> c_int {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.get_string_descriptor_ascii(handle, descriptor_index, data, length) }
    })
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_close(handle: *mut LibusbDeviceHandle) {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.close(handle) };
    });
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_claim_interface(
    handle: *mut LibusbDeviceHandle,
    interface: c_int,
) -> c_int {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.claim_interface(handle, interface) }
    })
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_release_interface(
    handle: *mut LibusbDeviceHandle,
    interface: c_int,
) -> c_int {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.release_interface(handle, interface) }
    })
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_kernel_driver_active(
    handle: *mut LibusbDeviceHandle,
    interface: c_int,
) -> c_int {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.kernel_driver_active(handle, interface) }
    })
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_detach_kernel_driver(
    handle: *mut LibusbDeviceHandle,
    interface: c_int,
) -> c_int {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.detach_kernel_driver(handle, interface) }
    })
}

/// Invoke the deterministic test backend.
#[cfg(test)]
pub(crate) unsafe fn libusb_control_transfer(transfer: LibusbControlTransfer) -> c_int {
    with_test_backend(|backend| {
        // SAFETY: the caller preserves libusb's public argument contract.
        unsafe { backend.control_transfer(transfer) }
    })
}

/* Linux control-plane calls used by the optional parallel-port GPIO backend.
 * The CM119 HID contract remains independent of these calls.  They are kept
 * here so the public GPIO ABI never exposes a libc or ppdev type. */
#[cfg(all(target_os = "linux", not(test)))]
#[link(name = "c")]
unsafe extern "C" {
    /// Open a configured ppdev device node.
    #[link_name = "open"]
    fn parallel_open_system(pathname: *const c_char, flags: c_int, mode: c_int) -> c_int;
    /// Close one ppdev device descriptor.
    #[link_name = "close"]
    fn parallel_close_system(file_descriptor: c_int) -> c_int;
    /// Invoke a Linux ppdev ioctl with an integer or pointer-sized argument.
    #[link_name = "ioctl"]
    fn parallel_ioctl_system(file_descriptor: c_int, request: c_ulong, argument: usize) -> c_int;
}

/// Open a configured ppdev device node through the production libc boundary.
#[cfg(all(target_os = "linux", not(test)))]
pub(crate) unsafe fn open(pathname: *const c_char, flags: c_int, mode: c_int) -> c_int {
    // SAFETY: this wrapper preserves libc's public argument contract.
    unsafe { parallel_open_system(pathname, flags, mode) }
}

/// Close one ppdev descriptor through the production libc boundary.
#[cfg(all(target_os = "linux", not(test)))]
pub(crate) unsafe fn close(file_descriptor: c_int) -> c_int {
    // SAFETY: this wrapper preserves libc's public argument contract.
    unsafe { parallel_close_system(file_descriptor) }
}

/// Invoke one ppdev ioctl through the production libc boundary.
#[cfg(all(target_os = "linux", not(test)))]
pub(crate) unsafe fn ioctl(file_descriptor: c_int, request: c_ulong, argument: usize) -> c_int {
    // SAFETY: this wrapper preserves libc's public argument contract.
    unsafe { parallel_ioctl_system(file_descriptor, request, argument) }
}

/// Deterministic ppdev/libc surface used only by parallel-port unit tests.
#[cfg(all(test, target_os = "linux"))]
pub(crate) trait TestParallelBackend: Send {
    /// Open one configured synthetic device node.
    unsafe fn open(&mut self, pathname: *const c_char, flags: c_int, mode: c_int) -> c_int;
    /// Close one synthetic descriptor.
    unsafe fn close(&mut self, file_descriptor: c_int) -> c_int;
    /// Process one synthetic ppdev ioctl.
    unsafe fn ioctl(&mut self, file_descriptor: c_int, request: c_ulong, argument: usize) -> c_int;
}

/// One test-only installed ppdev backend.
#[cfg(all(test, target_os = "linux"))]
fn test_parallel_backend_slot() -> &'static Mutex<Option<Box<dyn TestParallelBackend>>> {
    static SLOT: OnceLock<Mutex<Option<Box<dyn TestParallelBackend>>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// RAII installation token for one deterministic ppdev backend.
#[cfg(all(test, target_os = "linux"))]
pub(crate) struct TestParallelBackendGuard {
    /// Holds exclusive test ownership until the backend slot is cleared.
    _serial: MutexGuard<'static, ()>,
}

#[cfg(all(test, target_os = "linux"))]
impl Drop for TestParallelBackendGuard {
    fn drop(&mut self) {
        *test_lock(test_parallel_backend_slot()) = None;
    }
}

/// Install one deterministic ppdev backend for the duration of a unit test.
#[cfg(all(test, target_os = "linux"))]
pub(crate) fn install_test_parallel_backend(
    backend: Box<dyn TestParallelBackend>,
) -> TestParallelBackendGuard {
    let serial = test_lock(test_backend_serial_lock());
    *test_lock(test_parallel_backend_slot()) = Some(backend);
    TestParallelBackendGuard { _serial: serial }
}

/// Invoke the installed ppdev backend.
#[cfg(all(test, target_os = "linux"))]
fn with_test_parallel_backend<T>(operation: impl FnOnce(&mut dyn TestParallelBackend) -> T) -> T {
    let mut slot = test_lock(test_parallel_backend_slot());
    let backend = slot
        .as_deref_mut()
        .expect("parallel operation needs an installed deterministic test backend");
    operation(backend)
}

/// Invoke the deterministic ppdev open operation.
#[cfg(all(test, target_os = "linux"))]
pub(crate) unsafe fn open(pathname: *const c_char, flags: c_int, mode: c_int) -> c_int {
    with_test_parallel_backend(|backend| {
        // SAFETY: the caller preserves libc's public argument contract.
        unsafe { backend.open(pathname, flags, mode) }
    })
}

/// Invoke the deterministic ppdev close operation.
#[cfg(all(test, target_os = "linux"))]
pub(crate) unsafe fn close(file_descriptor: c_int) -> c_int {
    with_test_parallel_backend(|backend| {
        // SAFETY: the caller preserves libc's public argument contract.
        unsafe { backend.close(file_descriptor) }
    })
}

/// Invoke the deterministic ppdev ioctl operation.
#[cfg(all(test, target_os = "linux"))]
pub(crate) unsafe fn ioctl(file_descriptor: c_int, request: c_ulong, argument: usize) -> c_int {
    with_test_parallel_backend(|backend| {
        // SAFETY: the caller preserves libc's public argument contract.
        unsafe { backend.ioctl(file_descriptor, request, argument) }
    })
}
