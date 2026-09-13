/**
 * @file rptadv_gpio_adapter.h
 * @brief Stable C ABI for CM119 HID GPIO used by rpt_advanced.
 *
 * This adapter owns CM119 HID signaling, the optional CM119 tuning EEPROM,
 * configured Linux parallel-port GPIO, and established parallel-port binary
 * channel and RTX serial-radio protocols. PCM, mixer controls, DSP,
 * Asterisk, and Hamlib remain separate adapter or core responsibilities.
 */

#ifndef RPTADV_GPIO_ADAPTER_H
#define RPTADV_GPIO_ADAPTER_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/** @brief ABI implemented by this adapter descriptor. */
#define RPTADV_GPIO_ADAPTER_ABI_VERSION 1U

/** @brief C-Media's USB vendor identifier selected by a zero config value. */
#define RPTADV_GPIO_CM119_DEFAULT_VENDOR_ID 0x0d8cU

/** @brief Match the supported CM108/CM119 product family when configured as zero. */
#define RPTADV_GPIO_CM119_ANY_SUPPORTED_PRODUCT 0U

/** @brief Number of CM119 GPIO bits represented by the ABI masks. */
#define RPTADV_GPIO_CM119_PIN_COUNT 8U

/** @brief Maximum C string bytes returned for one USB serial number. */
#define RPTADV_GPIO_DEVICE_SERIAL_CAPACITY 128U

/** @brief Maximum CM119 candidates returned by one discovery snapshot. */
#define RPTADV_GPIO_DEVICE_LIST_CAPACITY 16U

/** @brief Number of physical 16-bit words addressable by the CM119 EEPROM. */
#define RPTADV_GPIO_CM119_EEPROM_WORD_COUNT 64U

/** @brief First physical EEPROM word reserved for the established ASL3 tuning image. */
#define RPTADV_GPIO_CM119_EEPROM_START_WORD 51U

/** @brief EEPROM word containing the established tuning-image magic value. */
#define RPTADV_GPIO_CM119_EEPROM_MAGIC_WORD 51U

/** @brief Established CM119 tuning-image magic value. */
#define RPTADV_GPIO_CM119_EEPROM_MAGIC 34329U

/** @brief EEPROM word containing the established tuning-image checksum. */
#define RPTADV_GPIO_CM119_EEPROM_CHECKSUM_WORD 63U

/** @brief Opaque, exclusively owned CM119 HID device. */
struct rptadv_gpio_device;

/** @brief Result returned by one adapter operation. */
enum rptadv_gpio_result {
	/** Operation completed. */
	RPTADV_GPIO_OK = 0,
	/** A required pointer, structure size, or value was invalid. */
	RPTADV_GPIO_INVALID_ARGUMENT = -1,
	/** Setup could not reserve required memory. */
	RPTADV_GPIO_NO_MEMORY = -2,
	/** libusb could not enumerate, claim, or transfer to the interface. */
	RPTADV_GPIO_USB_ERROR = -3,
	/** The selected CM119 identity or wiring profile is unavailable. */
	RPTADV_GPIO_UNSUPPORTED = -4,
	/** A selected ppdev or raw-I/O parallel transport failed after opening. */
	RPTADV_GPIO_IO_ERROR = -5,
};

/** @brief CM119 wiring profile matching the established USBRadioPlus values. */
enum rptadv_gpio_cm119_profile {
	/** Standard DudeUSB/URI-style CM119 GPIO wiring. */
	RPTADV_GPIO_CM119_DUDEUSB = 0,
	/** SPH USB interface wiring. */
	RPTADV_GPIO_CM119_SPHUSB = 1,
	/** NHRC/N1KDO interface wiring. */
	RPTADV_GPIO_CM119_NHRC = 2,
	/** Custom interface wiring used by the existing CM119 profile. */
	RPTADV_GPIO_CM119_CUSTOM = 3,
};

/**
 * @brief Stable CM119 HID selection and wiring setup.
 *
 * @p usb_port_path is a Linux USB topology such as `3-1` or `3-1.2`.  An
 * optional interface suffix such as `:1.0` is ignored.  The path is
 * compared with libusb's bus and port chain and avoids an ephemeral USB device
 * address.  The caller must resolve one unique device before opening it.
 */
struct rptadv_gpio_device_config {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Required descriptor ABI version. */
	uint32_t abi_version;
	/** Required stable Linux USB topology identity. */
	const char *usb_port_path;
	/** USB vendor ID, or zero to select @ref RPTADV_GPIO_CM119_DEFAULT_VENDOR_ID. */
	uint16_t vendor_id;
	/** USB product ID, or zero for the supported CM108/CM119 family. */
	uint16_t product_id;
	/** One value from @ref rptadv_gpio_cm119_profile. */
	uint32_t profile;
	/** Nonzero inverts the logical PTT output. */
	uint32_t ptt_inverted;
	/** Logical GPIO bits enabled as ordinary outputs. */
	uint32_t gpio_output_enable_mask;
	/** Initial logical values for enabled ordinary GPIO outputs. */
	uint32_t gpio_output_initial_mask;
};

/** @brief Non-owning result from a stable CM119 device probe. */
struct rptadv_gpio_device_info {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Descriptor ABI that populated this snapshot. */
	uint32_t abi_version;
	/** Nonzero when the requested device is currently enumerated. */
	uint32_t present;
	/** Matched USB vendor ID. */
	uint16_t vendor_id;
	/** Matched USB product ID. */
	uint16_t product_id;
	/** USB bus number represented by the selected topology. */
	uint32_t usb_bus;
	/** Number of populated entries in the port-chain array below. */
	uint32_t usb_port_number_count;
	/** libusb port chain matching the selected stable topology. */
	uint8_t usb_port_numbers[7];
	/** Optional NUL-terminated USB serial number; empty when unavailable. */
	char serial[RPTADV_GPIO_DEVICE_SERIAL_CAPACITY];
};

/**
 * @brief Bounded CM119 discovery snapshot.
 *
 * The adapter never chooses a device automatically.  The composition uses a
 * returned serial and topology to resolve one explicit device before open.
 * If @ref matching_device_count exceeds @ref returned_device_count, the
 * caller must refine its selection rather than treating the snapshot as a
 * complete inventory.
 */
struct rptadv_gpio_device_list {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Descriptor ABI that populated this snapshot. */
	uint32_t abi_version;
	/** Number of supported CM119 candidates presently enumerated. */
	uint32_t matching_device_count;
	/** Number of entries written to @ref devices. */
	uint32_t returned_device_count;
	/** First bounded set of supported CM119 candidates. */
	struct rptadv_gpio_device_info devices[RPTADV_GPIO_DEVICE_LIST_CAPACITY];
};

/**
 * @brief Established CM119 user tuning EEPROM image in physical-word form.
 *
 * @ref words uses physical EEPROM addresses so it remains convenient for C
 * compatibility code. The adapter reads and writes only the ASL3 user tuning
 * region from @ref RPTADV_GPIO_CM119_EEPROM_START_WORD through
 * @ref RPTADV_GPIO_CM119_EEPROM_CHECKSUM_WORD; all manufacturer-reserved
 * words remain zero in a read result and are never programmed. A read reports
 * validity through @ref checksum_valid and @ref magic_valid rather than
 * silently accepting a corrupt image. A write restores the established magic
 * and checksum before programming the user region.
 */
struct rptadv_gpio_eeprom_image {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Descriptor ABI that populated or validates this image. */
	uint32_t abi_version;
	/** Nonzero when the established additive checksum is valid. */
	uint32_t checksum_valid;
	/** Nonzero when the established tuning-image magic is present. */
	uint32_t magic_valid;
    /** CM119 user tuning words indexed by physical address; reserved words are zero. */
	uint16_t words[RPTADV_GPIO_CM119_EEPROM_WORD_COUNT];
};

/** @brief Opaque, exclusively owned Linux parallel-port GPIO device. */
struct rptadv_gpio_parallel_device;

/** @brief Try ppdev then a configured raw I/O range. */
#define RPTADV_GPIO_PARALLEL_TRANSPORT_AUTO 0U
/** @brief Use only the configured Linux ppdev device node. */
#define RPTADV_GPIO_PARALLEL_TRANSPORT_PPDEV 1U
/** @brief Use only the explicitly configured raw x86 I/O range. */
#define RPTADV_GPIO_PARALLEL_TRANSPORT_RAW_IO 2U

/**
 * @brief Stable setup for one parallel-port GPIO transport.
 *
 * The ppdev path and raw-I/O base are explicit; automatic selection never
 * scans arbitrary host ports. Raw I/O is available only on x86 Linux when the
 * process has permission for the configured two-port range. It is rejected on
 * unsupported architectures rather than silently using another device.
 */
struct rptadv_gpio_parallel_config {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Required descriptor ABI version. */
	uint32_t abi_version;
	/** One RPTADV_GPIO_PARALLEL_TRANSPORT_* value. */
	uint32_t transport;
	/** Optional ppdev node such as `/dev/parport0`. */
	const char *ppdev_path;
	/** Optional raw I/O base, such as `0x378`. */
	uint32_t raw_io_base;
	/** Data-register bits available to a caller. */
	uint32_t output_enable_mask;
	/** Initial persistent data-register values. */
	uint32_t output_initial_mask;
};

/** @brief Lock-free parallel status-register snapshot. */
struct rptadv_gpio_parallel_input_snapshot {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Descriptor ABI that produced this snapshot. */
	uint32_t abi_version;
	/** Nonzero while the selected parallel transport remains open. */
	uint32_t online;
	/** Latest raw IEEE 1284 status-register byte. */
	uint32_t status_mask;
};

/**
 * @brief One prepared parallel data-register action.
 *
 * A nonzero pulse duration atomically asks the service owner to OR
 * @ref pulse_mask into the persistent data output until the monotonic
 * deadline. Setting @ref cancel_pulse cancels an active pulse without
 * reconfiguring the device. All pin polarity belongs to the caller's
 * configured pin mapping, not this raw-register adapter.
 */
struct rptadv_gpio_parallel_output_action {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Required descriptor ABI version. */
	uint32_t abi_version;
	/** Complete desired persistent data-register byte. */
	uint32_t output_mask;
	/** Bits to assert for a new timed active-high pulse. */
	uint32_t pulse_mask;
	/** Duration of the new pulse in milliseconds; zero when no new pulse is requested. */
	uint32_t pulse_duration_milliseconds;
	/** Nonzero cancels the active pulse. */
	uint32_t cancel_pulse;
};

/**
 * @brief One timed parallel output pulse that XORs the persistent data byte.
 *
 * This is the compatibility form used by legacy parallel-port pulse code:
 * selected bits invert relative to the latest persistent data byte instead of
 * being forced high. A new pulse or cancel request supersedes an active pulse
 * from either parallel pulse API. Pin polarity remains the caller's mapping.
 */
struct rptadv_gpio_parallel_inverting_pulse_action {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Required descriptor ABI version. */
	uint32_t abi_version;
	/** Enabled data-register bits to invert for a new timed pulse. */
	uint32_t invert_mask;
	/** Duration of the new pulse in milliseconds; zero when no new pulse is requested. */
	uint32_t pulse_duration_milliseconds;
	/** Nonzero cancels an active pulse without starting another one. */
	uint32_t cancel_pulse;
};

/**
 * @brief Schedule independent timed baseline-XOR pulses on parallel data pins.
 *
 * Every bit in @ref invert_mask receives its own monotonic deadline. The
 * configured service owner starts that duration when it first observes the
 * request. A later request replaces deadlines only for its selected bits, so
 * overlapping pins may expire independently. Disjoint requests made before
 * one service cycle are retained; a later request for the same pin wins.
 * @ref cancel_mask removes deadlines only for its selected bits. A request may
 * schedule and cancel different pins together, but their masks must not
 * overlap. A zero duration schedules no bits.
 *
 * This supplements, rather than changes, the single-pulse API above. The
 * service owner applies its legacy single pulse first and this scheduled XOR
 * mask second. Callers should not use both pulse APIs for the same pin.
 */
struct rptadv_gpio_parallel_scheduled_inverting_pulse_action {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Required descriptor ABI version. */
	uint32_t abi_version;
	/** Enabled data-register bits to invert until their individual deadlines. */
	uint32_t invert_mask;
	/** Duration for every bit in @ref invert_mask; zero schedules no bits. */
	uint32_t pulse_duration_milliseconds;
	/** Enabled data-register bits whose outstanding deadlines are cancelled. */
	uint32_t cancel_mask;
};

/** @brief Lock-free best-effort parallel transport statistics. */
struct rptadv_gpio_parallel_stats {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Descriptor ABI that produced this snapshot. */
	uint32_t abi_version;
	/** Status-register reads attempted by the service owner. */
	uint64_t input_read_count;
	/** Data-register writes attempted by the service owner. */
	uint64_t output_apply_count;
	/** ppdev or raw-I/O failures after the port opened. */
	uint64_t io_error_count;
	/** Nonzero while the selected parallel transport remains open. */
	uint32_t online;
	/** Latest operating-system error code, or zero after success. */
	int32_t last_io_error;
	/** Latest successfully applied raw data-register byte. */
	uint32_t applied_output_mask;
};

/** @brief Input levels sampled by the CM119 HID service owner. */
struct rptadv_gpio_input_snapshot {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Descriptor ABI that populated this snapshot. */
	uint32_t abi_version;
	/** Nonzero while the exclusive CM119 HID interface is open. */
	uint32_t online;
	/** Nonzero when the profile's active-low COR input is asserted. */
	uint32_t cor_active;
	/** Nonzero when the profile's active-low external CTCSS input is asserted. */
	uint32_t ctcss_active;
	/** Logical GPIO byte; CM108AH HOOK is normalized as legacy GPIO2. */
	uint32_t gpio_input_mask;
	/** Complete four-byte HID input report for diagnostics. */
	uint8_t hid_report[4];
};

/** @brief One prepared logical CM119 output action. */
struct rptadv_gpio_output_action {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Required descriptor ABI version. */
	uint32_t abi_version;
	/** Nonzero requests logical PTT assertion. */
	uint32_t ptt_asserted;
	/** Logical values for the configured ordinary GPIO output bits. */
	uint32_t gpio_output_mask;
};

/**
 * @brief One timed CM119 output pulse that XORs the logical output baseline.
 *
 * The service owner inverts the requested logical PTT and GPIO values until
 * the monotonic deadline. This retains the established CM119 pulse behavior
 * for PTT and a clip LED with either configured PTT polarity. A new pulse or
 * cancel request supersedes any active CM119 pulse; ordinary output actions
 * update the baseline without cancelling it.
 */
struct rptadv_gpio_cm119_inverting_pulse_action {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Required descriptor ABI version. */
	uint32_t abi_version;
	/** Nonzero inverts logical PTT for a new timed pulse. */
	uint32_t ptt_invert;
	/** Configured ordinary GPIO bits to invert for a new timed pulse. */
	uint32_t gpio_invert_mask;
	/** Duration of the new pulse in milliseconds; zero when no new pulse is requested. */
	uint32_t pulse_duration_milliseconds;
	/** Nonzero cancels an active pulse without starting another one. */
	uint32_t cancel_pulse;
};

/**
 * @brief Schedule independent timed baseline-XOR pulses on CM119 PTT/GPIO.
 *
 * A selected logical PTT or GPIO bit receives its own monotonic deadline. The
 * configured service owner starts that duration when it first observes the
 * request. Later scheduling replaces only selected deadlines; selected
 * cancellation removes only those deadlines. Disjoint requests made before
 * one service cycle are retained; a later request for the same output wins.
 * A request may schedule and cancel different outputs together, but an output
 * cannot appear in both sets. A zero duration schedules no outputs. Logical
 * inversion retains the physical active-low or active-high behavior selected by
 * @ref rptadv_gpio_device_config.
 *
 * This supplements, rather than changes, the single-pulse API above. The
 * service owner applies its legacy single pulse first and this scheduled XOR
 * mask second. Callers should not use both pulse APIs for the same output.
 */
struct rptadv_gpio_cm119_scheduled_inverting_pulse_action {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Required descriptor ABI version. */
	uint32_t abi_version;
	/** Nonzero schedules logical PTT inversion until its individual deadline. */
	uint32_t ptt_invert;
	/** Configured ordinary GPIO bits to invert until individual deadlines. */
	uint32_t gpio_invert_mask;
	/** Duration for every selected scheduled output; zero schedules no outputs. */
	uint32_t pulse_duration_milliseconds;
	/** Nonzero cancels an outstanding logical PTT inversion. */
	uint32_t ptt_cancel;
	/** Configured ordinary GPIO bits whose outstanding deadlines are cancelled. */
	uint32_t gpio_cancel_mask;
};

/** @brief Lock-free, best-effort CM119 HID activity snapshot. */
struct rptadv_gpio_device_stats {
	/** Size of this structure supplied by the caller. */
	uint32_t struct_size;
	/** Descriptor ABI that populated this snapshot. */
	uint32_t abi_version;
	/** Successful or attempted HID input-report reads. */
	uint64_t input_read_count;
	/** Successful or attempted HID output-report writes. */
	uint64_t output_apply_count;
	/** HID transfer failures observed after a device opened. */
	uint64_t usb_error_count;
	/** Last logical PTT state successfully applied. */
	uint32_t ptt_applied;
	/** Nonzero while the exclusive HID device is open. */
	uint32_t online;
	/** Last libusb error code, or zero after successful I/O. */
	int32_t last_usb_error;
	/** EEPROM words successfully read by the service owner. */
	uint64_t eeprom_read_count;
	/** EEPROM words successfully written by the service owner. */
	uint64_t eeprom_write_count;
};

/**
 * @brief Versioned function table exported by the adapter shared object.
 *
 * Device open, service, and close calls are serialized control-plane
 * operations.  The selected HID service owner is the only code that accesses
 * libusb and it must never be a PCM callback.  Output publication and input
 * and status reads are lock-free best-effort operations that may run from a
 * native tick. ABI-1 additions are appended to this descriptor. A consumer
 * built against an earlier header must compare @ref struct_size before calling
 * an appended function pointer.
 */
struct rptadv_gpio_adapter_descriptor {
	/** Size of this descriptor. */
	uint32_t struct_size;
	/** ABI implemented by every function in this table. */
	uint32_t abi_version;
	/** Stable capability name. */
	const char *capability_name;
	/** Probe a selected stable identity without claiming the HID interface. */
	enum rptadv_gpio_result (*device_probe)(
		const struct rptadv_gpio_device_config *config,
		struct rptadv_gpio_device_info *info);
	/** Open and exclusively claim one selected CM119 HID interface. */
	enum rptadv_gpio_result (*device_open)(
		const struct rptadv_gpio_device_config *config,
		struct rptadv_gpio_device **device);
	/** Lock-free publication of one prepared PTT and ordinary-GPIO output action. */
	enum rptadv_gpio_result (*device_publish_outputs)(
		struct rptadv_gpio_device *device,
		const struct rptadv_gpio_output_action *action);
	/**
	 * @brief Flush the newest published output and sample one HID input report.
	 *
	 * This is the sole libusb I/O entry and must be called by the single
	 * configured CM119 service worker, never by a PCM callback.
	 */
	enum rptadv_gpio_result (*device_service)(struct rptadv_gpio_device *device);
	/** Obtain a lock-free best-effort copy of the most recently serviced inputs. */
	enum rptadv_gpio_result (*device_get_inputs)(
		const struct rptadv_gpio_device *device,
		struct rptadv_gpio_input_snapshot *snapshot);
	/** Obtain a lock-free best-effort device snapshot. */
	enum rptadv_gpio_result (*device_get_stats)(
		const struct rptadv_gpio_device *device,
		struct rptadv_gpio_device_stats *stats);
	/** Unkey, release the HID interface, and free the device. */
	void (*device_close)(struct rptadv_gpio_device *device);
	/** Enumerate bounded CM119 candidates without claiming an HID interface. */
	enum rptadv_gpio_result (*device_discover)(struct rptadv_gpio_device_list *list);
	/**
	 * @brief Read the established CM119 tuning EEPROM through the service owner.
	 *
	 * This operation performs HID I/O and must be serialized with
	 * @ref device_service by the same non-real-time service owner.
	 */
	enum rptadv_gpio_result (*device_read_eeprom)(
		struct rptadv_gpio_device *device,
		struct rptadv_gpio_eeprom_image *image);
	/**
	 * @brief Program the established CM119 tuning EEPROM through the service owner.
	 *
	 * This operation performs HID I/O and must be serialized with
	 * @ref device_service by the same non-real-time service owner.
	 */
	enum rptadv_gpio_result (*device_write_eeprom)(
		struct rptadv_gpio_device *device,
		struct rptadv_gpio_eeprom_image *image);
	/** Open and exclusively claim one explicit parallel-port transport. */
	enum rptadv_gpio_result (*parallel_open)(
		const struct rptadv_gpio_parallel_config *config,
		struct rptadv_gpio_parallel_device **device);
	/** Publish a prepared raw data-register and optional pulse action without I/O. */
	enum rptadv_gpio_result (*parallel_publish_outputs)(
		struct rptadv_gpio_parallel_device *device,
		const struct rptadv_gpio_parallel_output_action *action);
	/**
	 * @brief Apply the newest output and sample one status byte.
	 *
	 * This is the sole ppdev/raw-I/O entry and must be called by the configured
	 * non-real-time GPIO service owner, never by a PCM callback.
	 */
	enum rptadv_gpio_result (*parallel_service)(struct rptadv_gpio_parallel_device *device);
	/**
	 * @brief Apply one immediate control-plane data byte for radio-programming sequences.
	 *
	 * The caller serializes this with @ref parallel_service. It is never callable
	 * from a native tick.
	 */
	enum rptadv_gpio_result (*parallel_control_write_data)(
		struct rptadv_gpio_parallel_device *device,
		uint32_t data);
	/** Obtain the latest lock-free parallel status-register snapshot. */
	enum rptadv_gpio_result (*parallel_get_inputs)(
		const struct rptadv_gpio_parallel_device *device,
		struct rptadv_gpio_parallel_input_snapshot *snapshot);
	/** Obtain lock-free best-effort parallel transport statistics. */
	enum rptadv_gpio_result (*parallel_get_stats)(
		const struct rptadv_gpio_parallel_device *device,
		struct rptadv_gpio_parallel_stats *stats);
	/** Deassert outputs, release the transport, and free the device. */
	void (*parallel_close)(struct rptadv_gpio_parallel_device *device);
	/**
	 * @brief Publish a timed CM119 PTT/GPIO baseline-XOR pulse without HID I/O.
	 *
	 * This ABI-1 extension is callable only when @ref struct_size reaches this
	 * member. The configured non-real-time device service owner applies it.
	 */
	enum rptadv_gpio_result (*device_publish_inverting_pulse)(
		struct rptadv_gpio_device *device,
		const struct rptadv_gpio_cm119_inverting_pulse_action *action);
	/**
	 * @brief Publish a timed parallel data-byte baseline-XOR pulse without I/O.
	 *
	 * This ABI-1 extension is callable only when @ref struct_size reaches this
	 * member. The configured non-real-time parallel service owner applies it.
	 */
	enum rptadv_gpio_result (*parallel_publish_inverting_pulse)(
		struct rptadv_gpio_parallel_device *device,
		const struct rptadv_gpio_parallel_inverting_pulse_action *action);
	/**
 * @brief Schedule per-bit CM119 baseline-XOR pulses without HID I/O.
 *
 * This ABI-1 extension is callable only when @ref struct_size reaches this
 * member. The configured non-real-time device service owner starts and
 * applies its monotonic deadlines.
	 */
	enum rptadv_gpio_result (*device_schedule_inverting_pulse)(
		struct rptadv_gpio_device *device,
		const struct rptadv_gpio_cm119_scheduled_inverting_pulse_action *action);
	/**
 * @brief Schedule per-bit parallel baseline-XOR pulses without port I/O.
 *
 * This ABI-1 extension is callable only when @ref struct_size reaches this
 * member. The configured non-real-time parallel service owner starts and
 * applies its monotonic deadlines.
	 */
	enum rptadv_gpio_result (*parallel_schedule_inverting_pulse)(
		struct rptadv_gpio_parallel_device *device,
		const struct rptadv_gpio_parallel_scheduled_inverting_pulse_action *action);
	/**
	 * @brief Latch an active-low four-bit binary channel selection.
	 *
	 * This compatibility operation first drives data bits 4 through 7 high,
	 * then clears the bits selected by @p channel. The parallel configuration
	 * must enable all four bits. The caller serializes this control-plane
	 * operation with @ref parallel_service; it is never callable from a native
	 * tick.
	 */
	enum rptadv_gpio_result (*parallel_set_binary_channel)(
		struct rptadv_gpio_parallel_device *device,
		uint8_t channel);
	/**
	 * @brief Program an established RTX synthesizer through the parallel port.
	 *
	 * The operation emits the legacy two-word, 20-bit, most-significant-bit
	 * first serial sequence and leaves the transmit bit asserted only when
	 * @p transmitting is nonzero. The legacy @p high_power argument is accepted
	 * for compatibility but does not assert the TX-power bit. The parallel
	 * configuration must enable data bits 0 through 4. The caller serializes
	 * this control-plane operation with @ref parallel_service; it is never
	 * callable from a native tick.
	 */
	enum rptadv_gpio_result (*parallel_program_rtx)(
		struct rptadv_gpio_parallel_device *device,
		uint32_t rx_frequency_hz,
		uint32_t tx_frequency_hz,
		uint32_t transmitting,
		uint32_t high_power);
	/**
	 * @brief Immediately deassert the established RTX transmit and power bits.
	 *
	 * This fail-safe control-plane operation preserves the programmed serial
	 * bits and does not replay a synthesizer sequence. The parallel
	 * configuration must enable data bits 3 and 4. The caller serializes this
	 * operation with @ref parallel_service; it is never callable from a native
	 * tick.
	 */
	enum rptadv_gpio_result (*parallel_clear_rtx_transmit)(
		struct rptadv_gpio_parallel_device *device);
};

/**
 * @brief Return the static descriptor for this shared-object ABI.
 * @return Never-null pointer valid for the lifetime of the loaded shared object.
 */
const struct rptadv_gpio_adapter_descriptor *rptadv_gpio_adapter_descriptor(void);

#ifdef __cplusplus
}
#endif

#endif
