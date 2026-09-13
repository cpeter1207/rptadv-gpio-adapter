/**
 * @file descriptor_smoke.c
 * @brief Verify that a C consumer can load the GPIO adapter descriptor.
 */

#include <assert.h>
#include <stddef.h>
#include <string.h>

#include "rptadv_gpio_adapter/rptadv_gpio_adapter.h"

_Static_assert(sizeof(struct rptadv_gpio_device_config) == 40,
	       "unexpected CM119 configuration ABI");
_Static_assert(sizeof(struct rptadv_gpio_device_info) == 160,
	       "unexpected CM119 device-info ABI");
_Static_assert(offsetof(struct rptadv_gpio_device_info, serial) == 31,
	       "unexpected CM119 serial offset");
_Static_assert(sizeof(struct rptadv_gpio_device_list) == 2576,
	       "unexpected CM119 discovery ABI");
_Static_assert(sizeof(struct rptadv_gpio_eeprom_image) == 144,
	       "unexpected CM119 EEPROM ABI");
_Static_assert(RPTADV_GPIO_CM119_EEPROM_START_WORD == 51U,
	       "unexpected ASL3 EEPROM user-region start");
_Static_assert(RPTADV_GPIO_CM119_EEPROM_MAGIC_WORD == 51U,
	       "unexpected ASL3 EEPROM magic word");
_Static_assert(RPTADV_GPIO_CM119_EEPROM_CHECKSUM_WORD == 63U,
	       "unexpected ASL3 EEPROM checksum word");
_Static_assert(sizeof(struct rptadv_gpio_device_stats) == 64,
	       "unexpected CM119 statistics ABI");
_Static_assert(offsetof(struct rptadv_gpio_device_stats, eeprom_read_count) == 48,
	       "unexpected CM119 EEPROM statistics offset");
_Static_assert(sizeof(struct rptadv_gpio_parallel_config) == 40,
	       "unexpected parallel configuration ABI");
_Static_assert(sizeof(struct rptadv_gpio_parallel_input_snapshot) == 16,
	       "unexpected parallel input ABI");
_Static_assert(sizeof(struct rptadv_gpio_parallel_output_action) == 24,
	       "unexpected parallel output ABI");
_Static_assert(sizeof(struct rptadv_gpio_parallel_inverting_pulse_action) == 20,
	       "unexpected parallel XOR pulse ABI");
_Static_assert(sizeof(struct rptadv_gpio_parallel_scheduled_inverting_pulse_action) == 20,
	       "unexpected scheduled parallel XOR pulse ABI");
_Static_assert(sizeof(struct rptadv_gpio_parallel_stats) == 48,
	       "unexpected parallel statistics ABI");
_Static_assert(sizeof(struct rptadv_gpio_cm119_inverting_pulse_action) == 24,
	       "unexpected CM119 XOR pulse ABI");
_Static_assert(sizeof(struct rptadv_gpio_cm119_scheduled_inverting_pulse_action) == 28,
	       "unexpected scheduled CM119 XOR pulse ABI");
_Static_assert(offsetof(struct rptadv_gpio_adapter_descriptor,
			device_publish_inverting_pulse) == 152,
	       "CM119 XOR pulse must append to the original descriptor");
_Static_assert(offsetof(struct rptadv_gpio_adapter_descriptor,
			parallel_publish_inverting_pulse) == 160,
	       "parallel XOR pulse must append after the CM119 extension");

_Static_assert(offsetof(struct rptadv_gpio_adapter_descriptor,
			device_schedule_inverting_pulse) == 168,
	       "scheduled CM119 pulse must append after existing ABI-1 entries");
_Static_assert(offsetof(struct rptadv_gpio_adapter_descriptor,
			parallel_schedule_inverting_pulse) == 176,
	       "scheduled parallel pulse must append after the CM119 schedule");
_Static_assert(offsetof(struct rptadv_gpio_adapter_descriptor,
			parallel_set_binary_channel) == 184,
	       "binary channel selection must append after ABI-1 pulse entries");
_Static_assert(offsetof(struct rptadv_gpio_adapter_descriptor,
			parallel_program_rtx) == 192,
	       "RTX programming must append after binary channel selection");
_Static_assert(offsetof(struct rptadv_gpio_adapter_descriptor,
			parallel_clear_rtx_transmit) == 200,
	       "RTX transmit clear must append after RTX programming");
_Static_assert(sizeof(struct rptadv_gpio_adapter_descriptor) == 208,
	       "unexpected CM119 descriptor ABI");

int main(void)
{
	const struct rptadv_gpio_adapter_descriptor *descriptor =
		rptadv_gpio_adapter_descriptor();
	struct rptadv_gpio_device_config config = {
		.struct_size = sizeof(config),
		.abi_version = RPTADV_GPIO_ADAPTER_ABI_VERSION,
		.usb_port_path = "3-1",
		.vendor_id = 0,
		.product_id = RPTADV_GPIO_CM119_ANY_SUPPORTED_PRODUCT,
		.profile = RPTADV_GPIO_CM119_DUDEUSB,
	};
	struct rptadv_gpio_input_snapshot inputs = {
		.struct_size = sizeof(inputs),
	};
	struct rptadv_gpio_output_action output = {
		.struct_size = sizeof(output),
		.abi_version = RPTADV_GPIO_ADAPTER_ABI_VERSION,
	};
	struct rptadv_gpio_device_stats stats = {
		.struct_size = sizeof(stats),
	};
	struct rptadv_gpio_device_list list = {
		.struct_size = sizeof(list),
	};
	struct rptadv_gpio_eeprom_image eeprom = {
		.struct_size = sizeof(eeprom),
		.abi_version = RPTADV_GPIO_ADAPTER_ABI_VERSION,
	};
	struct rptadv_gpio_parallel_config parallel_config = {
		.struct_size = sizeof(parallel_config),
		.abi_version = RPTADV_GPIO_ADAPTER_ABI_VERSION,
		.transport = RPTADV_GPIO_PARALLEL_TRANSPORT_PPDEV,
		.ppdev_path = "/dev/parport0",
		.output_enable_mask = 0xff,
	};
	struct rptadv_gpio_parallel_output_action parallel_output = {
		.struct_size = sizeof(parallel_output),
		.abi_version = RPTADV_GPIO_ADAPTER_ABI_VERSION,
	};
	struct rptadv_gpio_cm119_inverting_pulse_action cm119_pulse = {
		.struct_size = sizeof(cm119_pulse),
		.abi_version = RPTADV_GPIO_ADAPTER_ABI_VERSION,
	};
	struct rptadv_gpio_parallel_inverting_pulse_action parallel_pulse = {
		.struct_size = sizeof(parallel_pulse),
		.abi_version = RPTADV_GPIO_ADAPTER_ABI_VERSION,
	};
	struct rptadv_gpio_cm119_scheduled_inverting_pulse_action cm119_scheduled_pulse = {
		.struct_size = sizeof(cm119_scheduled_pulse),
		.abi_version = RPTADV_GPIO_ADAPTER_ABI_VERSION,
	};
	struct rptadv_gpio_parallel_scheduled_inverting_pulse_action parallel_scheduled_pulse = {
		.struct_size = sizeof(parallel_scheduled_pulse),
		.abi_version = RPTADV_GPIO_ADAPTER_ABI_VERSION,
	};

	assert(descriptor != NULL);
	assert(descriptor->abi_version == RPTADV_GPIO_ADAPTER_ABI_VERSION);
	assert(descriptor->struct_size == sizeof(*descriptor));
	assert(strcmp(descriptor->capability_name, "rptadv.cm119-hid-gpio") == 0);
	assert(descriptor->device_probe != NULL);
	assert(descriptor->device_open != NULL);
	assert(descriptor->device_publish_outputs != NULL);
	assert(descriptor->device_service != NULL);
	assert(descriptor->device_get_inputs != NULL);
	assert(descriptor->device_get_stats != NULL);
	assert(descriptor->device_close != NULL);
	assert(descriptor->device_discover != NULL);
	assert(descriptor->device_read_eeprom != NULL);
	assert(descriptor->device_write_eeprom != NULL);
	assert(descriptor->parallel_open != NULL);
	assert(descriptor->parallel_publish_outputs != NULL);
	assert(descriptor->parallel_service != NULL);
	assert(descriptor->parallel_control_write_data != NULL);
	assert(descriptor->parallel_get_inputs != NULL);
	assert(descriptor->parallel_get_stats != NULL);
	assert(descriptor->parallel_close != NULL);
	assert(descriptor->device_publish_inverting_pulse != NULL);
	assert(descriptor->parallel_publish_inverting_pulse != NULL);
	assert(descriptor->device_schedule_inverting_pulse != NULL);
	assert(descriptor->parallel_schedule_inverting_pulse != NULL);
	assert(descriptor->parallel_set_binary_channel != NULL);
	assert(descriptor->parallel_program_rtx != NULL);
	assert(descriptor->parallel_clear_rtx_transmit != NULL);
	assert(descriptor->parallel_set_binary_channel(NULL, 0) ==
	       RPTADV_GPIO_INVALID_ARGUMENT);
	assert(descriptor->parallel_program_rtx(NULL, 0, 0, 0, 0) ==
	       RPTADV_GPIO_INVALID_ARGUMENT);
	assert(descriptor->parallel_clear_rtx_transmit(NULL) ==
	       RPTADV_GPIO_INVALID_ARGUMENT);
	assert(config.vendor_id == 0);
	assert(inputs.abi_version == 0);
	assert(output.ptt_asserted == 0);
	assert(stats.input_read_count == 0);
	assert(list.returned_device_count == 0);
	assert(eeprom.words[RPTADV_GPIO_CM119_EEPROM_MAGIC_WORD] == 0);
	assert(parallel_config.output_enable_mask == 0xff);
	assert(parallel_output.pulse_duration_milliseconds == 0);
	assert(cm119_pulse.ptt_invert == 0);
	assert(parallel_pulse.invert_mask == 0);
	assert(cm119_scheduled_pulse.gpio_cancel_mask == 0);
	assert(parallel_scheduled_pulse.cancel_mask == 0);
	return 0;
}
