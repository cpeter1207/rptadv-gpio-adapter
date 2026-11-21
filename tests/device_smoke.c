/**
 * @file device_smoke.c
 * @brief Exercise the actual DSO through the public USB and ppdev operations.
 */
#include <assert.h>
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "rptadv_gpio_adapter/rptadv_gpio_adapter.h"

int main(void)
{
    const struct rptadv_gpio_adapter_descriptor *api = rptadv_gpio_adapter_descriptor();
    unsigned (*calls)(void) = dlsym(RTLD_DEFAULT, "rpt_test_usb_calls");
    assert(calls);
    struct rptadv_gpio_device_config config = {
        .struct_size = sizeof(config), .abi_version = 1, .usb_port_path = "3-1"
    };
    struct rptadv_gpio_device_info info = {.struct_size = sizeof(info)};
    struct rptadv_gpio_device_list list = {.struct_size = sizeof(list)};
    assert(api->device_discover(&list) == 0 && list.returned_device_count == 1);
    assert(strcmp(list.devices[0].serial, "fixture") == 0);
    assert(api->device_probe(&config, &info) == 0 && info.present);
    for (unsigned no_ptt = 0; no_ptt < 2; ++no_ptt) {
        struct rptadv_gpio_device *device = NULL;
        int result = no_ptt ? api->device_open_without_ptt(&config, &device)
                            : api->device_open(&config, &device);
        assert(result == 0 && device);
        assert(api->device_service(device) == 0);
        struct rptadv_gpio_device_stats stats = {.struct_size = sizeof(stats)};
        assert(api->device_get_stats(device, &stats) == 0 && stats.online);
        struct rptadv_gpio_eeprom_image image = {.struct_size = sizeof(image), .abi_version = 1};
        assert(api->device_read_eeprom(device, &image) == 0);
        assert(api->device_write_eeprom(device, &image) == 0);
        api->device_close(device);
    }
    assert(calls() == 0x7fff);
    assert(setenv("RPT_TEST_PARALLEL", "/dev/null", 1) == 0);
    struct rptadv_gpio_parallel_config parallel = {
        .struct_size = sizeof(parallel), .abi_version = 1,
        .transport = RPTADV_GPIO_PARALLEL_TRANSPORT_PPDEV,
        .ppdev_path = "/dev/null", .output_enable_mask = 0xff
    };
    struct rptadv_gpio_parallel_device *port = NULL;
    assert(api->parallel_open(&parallel, &port) == 0 && port);
    assert(api->parallel_service(port) == 0);
    api->parallel_close(port);
    puts("Exported USB lifecycle and ppdev syscall boundaries passed.");
    return 0;
}
