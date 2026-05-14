/*
 * Simple libusb test - enumerate all USB devices
 */
#include <stdio.h>
#include <stdlib.h>
#include <libusb.h>

int main(void) {
    struct libusb_context *ctx = NULL;
    struct libusb_device **devs = NULL;
    ssize_t cnt;
    int r;

    printf("=== libusb test ===\n");

    r = libusb_init(&ctx);
    if (r < 0) {
        fprintf(stderr, "libusb_init failed: %d\n", r);
        return 1;
    }

    // Set debug level
    libusb_set_option(ctx, LIBUSB_OPTION_LOG_LEVEL, 3);

    printf("Enumerating USB devices...\n");
    cnt = libusb_get_device_list(ctx, &devs);
    printf("libusb_get_device_list returned: %zd\n", cnt);

    if (cnt < 0) {
        fprintf(stderr, "libusb_get_device_list error: %d\n", (int)cnt);
        libusb_exit(ctx);
        return 1;
    }

    printf("Found %zd USB device(s)\n", cnt);

    for (ssize_t i = 0; i < cnt; i++) {
        struct libusb_device *dev = devs[i];
        struct libusb_device_descriptor desc;

        r = libusb_get_device_descriptor(dev, &desc);
        if (r < 0) {
            printf("  [%zd] Failed to get descriptor\n", i);
            continue;
        }

        printf("  [%zd] Bus %03d Device %03d: VID=%04x PID=%04x Class=%02x/%02x\n",
               i,
               libusb_get_bus_number(dev),
               libusb_get_device_address(dev),
               desc.idVendor,
               desc.idProduct,
               desc.bDeviceClass,
               desc.bDeviceSubClass);

        // Check if this is a Video device (UVC)
        if (desc.bDeviceClass == 0xEF ||  // Interface-specific
            desc.idVendor == 0x046d ||   // Logitech
            desc.idVendor == 0x5986) {   // Generic
            printf("      -> This looks like a video device!\n");
        }
    }

    libusb_free_device_list(devs, 1);
    libusb_exit(ctx);

    return 0;
}
