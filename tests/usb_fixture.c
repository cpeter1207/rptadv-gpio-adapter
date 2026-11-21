/**
 * @file usb_fixture.c
 * @brief Interpose libusb only for the exported-DSO device contract test.
 */
#include <assert.h>
#include <libusb-1.0/libusb.h>
#include <string.h>

struct libusb_context { unsigned operations; };
struct libusb_device { unsigned operations; };
struct libusb_device_handle { unsigned operations; };
static struct libusb_context context_token;
static struct libusb_device device_token;
static struct libusb_device_handle handle_token;
static libusb_device *devices[] = {(libusb_device *)&device_token, NULL};
static int claimed;
int libusb_init(libusb_context **context)
{
    context_token.operations |= 1U << 0;
    *context = (libusb_context *)&context_token;
    return 0;
}
void libusb_exit(libusb_context *context)
{
    assert(context == (libusb_context *)&context_token);
    context->operations |= 1U << 1;
}
ssize_t libusb_get_device_list(libusb_context *context, libusb_device ***list)
{
    assert(context == (libusb_context *)&context_token);
    context->operations |= 1U << 2;
    *list = devices;
    return 1;
}
void libusb_free_device_list(libusb_device **list, int unref)
{
    assert(list == devices && unref == 1);
    list[0]->operations |= 1U << 3;
}
int libusb_get_device_descriptor(libusb_device *device, struct libusb_device_descriptor *out)
{
    assert(device == devices[0]);
    device->operations |= 1U << 4;
    memset(out, 0, sizeof(*out));
    out->idVendor = 0x0d8c;
    out->idProduct = 0x000c;
    out->iSerialNumber = 1;
    return 0;
}
uint8_t libusb_get_bus_number(libusb_device *device)
{
    assert(device == devices[0]);
    device->operations |= 1U << 5;
    return 3;
}
int libusb_get_port_numbers(libusb_device *device, uint8_t *ports, int capacity)
{
    assert(device == devices[0] && capacity >= 1);
    device->operations |= 1U << 6;
    ports[0] = 1;
    return 1;
}
int libusb_open(libusb_device *device, libusb_device_handle **handle)
{
    assert(device == devices[0]);
    device->operations |= 1U << 7;
    *handle = (libusb_device_handle *)&handle_token;
    return 0;
}
int libusb_get_string_descriptor_ascii(libusb_device_handle *handle, uint8_t index,
                                       unsigned char *out, int capacity)
{
    assert(handle == (libusb_device_handle *)&handle_token && index == 1 && capacity >= 7);
    handle->operations |= 1U << 8;
    memcpy(out, "fixture", 7);
    return 7;
}
void libusb_close(libusb_device_handle *handle)
{
    assert(handle == (libusb_device_handle *)&handle_token);
    handle->operations |= 1U << 9;
}
int libusb_claim_interface(libusb_device_handle *handle, int interface)
{
    assert(handle == (libusb_device_handle *)&handle_token && interface == 3);
    handle->operations |= 1U << 10;
    return claimed++ ? 0 : LIBUSB_ERROR_BUSY;
}
int libusb_release_interface(libusb_device_handle *handle, int interface)
{
    assert(handle == (libusb_device_handle *)&handle_token && interface == 3);
    handle->operations |= 1U << 11;
    return 0;
}
int libusb_kernel_driver_active(libusb_device_handle *handle, int interface)
{
    assert(handle == (libusb_device_handle *)&handle_token && interface == 3);
    handle->operations |= 1U << 12;
    return 1;
}
int libusb_detach_kernel_driver(libusb_device_handle *handle, int interface)
{
    assert(handle == (libusb_device_handle *)&handle_token && interface == 3);
    handle->operations |= 1U << 13;
    return 0;
}
int libusb_control_transfer(libusb_device_handle *handle, uint8_t type, uint8_t request,
                           uint16_t value, uint16_t index, unsigned char *data,
                           uint16_t length, unsigned timeout)
{
    assert(handle == (libusb_device_handle *)&handle_token && length == 4 && index == 3);
    assert(timeout && (request == 1 || request == 9) && (value == 0x100 || value == 0x200));
    handle->operations |= 1U << 14;
    if (type & LIBUSB_ENDPOINT_IN)
        memset(data, 0, length);
    return length;
}
unsigned rpt_test_usb_calls(void)
{
    return context_token.operations | device_token.operations | handle_token.operations;
}
