#define _POSIX_C_SOURCE 200809L

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <wayland-client.h>

#include "wlr-virtual-pointer-unstable-v1-client-protocol.h"

static struct zwlr_virtual_pointer_manager_v1 *manager;
static struct wl_seat *seat;

static void registry_global(
    void *data,
    struct wl_registry *registry,
    uint32_t name,
    const char *interface,
    uint32_t version
) {
    (void)data;
    if (strcmp(interface, zwlr_virtual_pointer_manager_v1_interface.name) == 0) {
        const uint32_t bind_version = version < 2 ? version : 2;
        manager = wl_registry_bind(
            registry,
            name,
            &zwlr_virtual_pointer_manager_v1_interface,
            bind_version
        );
    } else if (strcmp(interface, wl_seat_interface.name) == 0) {
        const uint32_t bind_version = version < 7 ? version : 7;
        seat = wl_registry_bind(registry, name, &wl_seat_interface, bind_version);
    }
}

static void registry_global_remove(
    void *data,
    struct wl_registry *registry,
    uint32_t name
) {
    (void)data;
    (void)registry;
    (void)name;
}

static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_global_remove,
};

static uint32_t monotonic_msec(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return 0;
    }
    const uint64_t milliseconds =
        (uint64_t)now.tv_sec * 1000u + (uint64_t)now.tv_nsec / 1000000u;
    return (uint32_t)milliseconds;
}

static uint32_t parse_coordinate(const char *value, const char *name) {
    char *end = NULL;
    const unsigned long parsed = strtoul(value, &end, 10);
    if (end == value || *end != '\0' || parsed > UINT32_MAX) {
        fprintf(stderr, "invalid %s: %s\n", name, value);
        exit(2);
    }
    return (uint32_t)parsed;
}

int main(int argc, char **argv) {
    if (argc != 5) {
        fprintf(stderr, "usage: virtual-pointer-click X Y WIDTH HEIGHT\n");
        return 2;
    }
    const uint32_t x = parse_coordinate(argv[1], "x");
    const uint32_t y = parse_coordinate(argv[2], "y");
    const uint32_t width = parse_coordinate(argv[3], "width");
    const uint32_t height = parse_coordinate(argv[4], "height");
    if (width == 0 || height == 0) {
        fprintf(stderr, "pointer extents must be non-zero\n");
        return 2;
    }

    struct wl_display *display = wl_display_connect(NULL);
    if (display == NULL) {
        fprintf(stderr, "failed to connect to Wayland display\n");
        return 1;
    }
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    if (wl_display_roundtrip(display) < 0 || manager == NULL || seat == NULL) {
        fprintf(stderr, "compositor does not expose wlr-virtual-pointer and a seat\n");
        if (seat != NULL) {
            wl_seat_destroy(seat);
        }
        wl_registry_destroy(registry);
        wl_display_disconnect(display);
        return 1;
    }

    struct zwlr_virtual_pointer_v1 *pointer =
        zwlr_virtual_pointer_manager_v1_create_virtual_pointer(manager, seat);
    if (wl_display_roundtrip(display) < 0) {
        fprintf(stderr, "virtual pointer creation failed\n");
        zwlr_virtual_pointer_v1_destroy(pointer);
        zwlr_virtual_pointer_manager_v1_destroy(manager);
        wl_seat_destroy(seat);
        wl_registry_destroy(registry);
        wl_display_disconnect(display);
        return 1;
    }
    const struct timespec settle = {
        .tv_sec = 0,
        .tv_nsec = 100000000,
    };
    nanosleep(&settle, NULL);
    const uint32_t time = monotonic_msec();
    zwlr_virtual_pointer_v1_motion_absolute(pointer, time, x, y, width, height);
    zwlr_virtual_pointer_v1_frame(pointer);
    if (wl_display_roundtrip(display) < 0) {
        fprintf(stderr, "virtual pointer motion failed\n");
        zwlr_virtual_pointer_v1_destroy(pointer);
        zwlr_virtual_pointer_manager_v1_destroy(manager);
        wl_seat_destroy(seat);
        wl_registry_destroy(registry);
        wl_display_disconnect(display);
        return 1;
    }
    zwlr_virtual_pointer_v1_button(pointer, time + 1, 0x110, 1);
    zwlr_virtual_pointer_v1_frame(pointer);
    if (wl_display_roundtrip(display) < 0) {
        fprintf(stderr, "virtual pointer press failed\n");
        zwlr_virtual_pointer_v1_destroy(pointer);
        zwlr_virtual_pointer_manager_v1_destroy(manager);
        wl_seat_destroy(seat);
        wl_registry_destroy(registry);
        wl_display_disconnect(display);
        return 1;
    }
    zwlr_virtual_pointer_v1_button(pointer, time + 2, 0x110, 0);
    zwlr_virtual_pointer_v1_frame(pointer);
    if (wl_display_roundtrip(display) < 0) {
        fprintf(stderr, "virtual pointer click failed\n");
        zwlr_virtual_pointer_v1_destroy(pointer);
        zwlr_virtual_pointer_manager_v1_destroy(manager);
        wl_seat_destroy(seat);
        wl_registry_destroy(registry);
        wl_display_disconnect(display);
        return 1;
    }

    zwlr_virtual_pointer_v1_destroy(pointer);
    zwlr_virtual_pointer_manager_v1_destroy(manager);
    wl_seat_destroy(seat);
    wl_registry_destroy(registry);
    wl_display_disconnect(display);
    return 0;
}
