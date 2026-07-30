#define _GNU_SOURCE

#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include <linux/input-event-codes.h>
#include <wayland-client.h>

struct virtual_pointer_manager;
struct virtual_pointer;

static const struct wl_interface virtual_pointer_interface;

static const struct wl_interface *manager_create_types[] = {
    &wl_seat_interface,
    &virtual_pointer_interface,
};

static const struct wl_message manager_requests[] = {
    {"create_virtual_pointer", "?on", manager_create_types},
    {"destroy", "", NULL},
};

static const struct wl_interface virtual_pointer_manager_interface = {
    "zwlr_virtual_pointer_manager_v1",
    1,
    2,
    manager_requests,
    0,
    NULL,
};

static const struct wl_message pointer_requests[] = {
    {"motion", "uff", NULL},
    {"motion_absolute", "uuuuu", NULL},
    {"button", "uuu", NULL},
    {"axis", "uuf", NULL},
    {"frame", "", NULL},
    {"axis_source", "u", NULL},
    {"axis_stop", "uu", NULL},
    {"axis_discrete", "uufi", NULL},
    {"destroy", "", NULL},
};

static const struct wl_interface virtual_pointer_interface = {
    "zwlr_virtual_pointer_v1",
    1,
    9,
    pointer_requests,
    0,
    NULL,
};

struct globals {
    struct wl_seat *seat;
    struct virtual_pointer_manager *manager;
};

static void registry_global(
    void *data,
    struct wl_registry *registry,
    uint32_t name,
    const char *interface,
    uint32_t version
) {
    struct globals *globals = data;
    if (strcmp(interface, wl_seat_interface.name) == 0 && globals->seat == NULL) {
        globals->seat = wl_registry_bind(registry, name, &wl_seat_interface, version < 7 ? version : 7);
    } else if (
        strcmp(interface, virtual_pointer_manager_interface.name) == 0 &&
        globals->manager == NULL
    ) {
        globals->manager = wl_registry_bind(
            registry,
            name,
            &virtual_pointer_manager_interface,
            1
        );
    }
}

static void registry_remove(void *data, struct wl_registry *registry, uint32_t name) {
    (void)data;
    (void)registry;
    (void)name;
}

static const struct wl_registry_listener registry_listener = {
    .global = registry_global,
    .global_remove = registry_remove,
};

static void fail(const char *message) {
    fprintf(stderr, "virtual-pointer-input: %s\n", message);
    exit(1);
}

static uint32_t parse_u32(const char *value, const char *name) {
    char *end = NULL;
    unsigned long parsed = strtoul(value, &end, 10);
    if (end == value || *end != '\0' || parsed > UINT32_MAX) {
        fprintf(stderr, "virtual-pointer-input: %s must be an unsigned 32-bit integer\n", name);
        exit(1);
    }
    return (uint32_t)parsed;
}

static double parse_double(const char *value, const char *name) {
    char *end = NULL;
    double parsed = strtod(value, &end);
    if (end == value || *end != '\0') {
        fprintf(stderr, "virtual-pointer-input: %s must be a number\n", name);
        exit(1);
    }
    return parsed;
}

static uint32_t monotonic_milliseconds(void) {
    struct timespec now = {0};
    if (clock_gettime(CLOCK_MONOTONIC, &now) < 0) {
        fail("could not read the monotonic clock");
    }
    uint64_t milliseconds =
        (uint64_t)now.tv_sec * UINT64_C(1000) + (uint64_t)now.tv_nsec / UINT64_C(1000000);
    return (uint32_t)milliseconds;
}

static void pointer_request(
    struct virtual_pointer *pointer,
    uint32_t opcode,
    uint32_t first,
    uint32_t second,
    uint32_t third,
    uint32_t fourth,
    uint32_t fifth
) {
    uint32_t version = wl_proxy_get_version((struct wl_proxy *)pointer);
    switch (opcode) {
        case 1:
            wl_proxy_marshal_flags(
                (struct wl_proxy *)pointer,
                opcode,
                NULL,
                version,
                0,
                first,
                second,
                third,
                fourth,
                fifth
            );
            break;
        case 2:
            wl_proxy_marshal_flags(
                (struct wl_proxy *)pointer,
                opcode,
                NULL,
                version,
                0,
                first,
                second,
                third
            );
            break;
        default:
            fail("unsupported pointer request");
    }
}

static void absolute_motion(
    struct virtual_pointer *pointer,
    uint32_t x,
    uint32_t y,
    uint32_t extent_x,
    uint32_t extent_y
) {
    pointer_request(
        pointer,
        1,
        monotonic_milliseconds(),
        x,
        y,
        extent_x,
        extent_y
    );
    wl_proxy_marshal_flags(
        (struct wl_proxy *)pointer,
        4,
        NULL,
        wl_proxy_get_version((struct wl_proxy *)pointer),
        0
    );
}

static void pointer_button(struct virtual_pointer *pointer, uint32_t state) {
    pointer_request(pointer, 2, monotonic_milliseconds(), BTN_LEFT, state, 0, 0);
    wl_proxy_marshal_flags(
        (struct wl_proxy *)pointer,
        4,
        NULL,
        wl_proxy_get_version((struct wl_proxy *)pointer),
        0
    );
}

static void pointer_axis(struct virtual_pointer *pointer, double delta) {
    wl_proxy_marshal_flags(
        (struct wl_proxy *)pointer,
        3,
        NULL,
        wl_proxy_get_version((struct wl_proxy *)pointer),
        0,
        monotonic_milliseconds(),
        WL_POINTER_AXIS_VERTICAL_SCROLL,
        wl_fixed_from_double(delta)
    );
    wl_proxy_marshal_flags(
        (struct wl_proxy *)pointer,
        4,
        NULL,
        wl_proxy_get_version((struct wl_proxy *)pointer),
        0
    );
}

int main(int argc, char **argv) {
    bool drag = argc == 9 && strcmp(argv[1], "drag") == 0;
    bool scroll = argc == 8 && strcmp(argv[1], "scroll") == 0;
    bool click = argc == 6 && strcmp(argv[1], "click") == 0;
    bool hover = argc == 7 && strcmp(argv[1], "hover") == 0;
    if (!drag && !scroll && !click && !hover) {
        fail(
            "usage: virtual-pointer-input drag <start-x> <start-y> <end-x> "
            "<end-y> <steps> <extent-x> <extent-y> | scroll <x> <y> "
            "<delta> <steps> <extent-x> <extent-y> | click <x> <y> "
            "<extent-x> <extent-y> | hover <x> <y> <hold-ms> <extent-x> "
            "<extent-y>"
        );
    }

    uint32_t x = parse_u32(argv[2], "x");
    uint32_t y = parse_u32(argv[3], "y");
    double end_or_delta =
        click || hover ? 0.0 : parse_double(argv[4], drag ? "end-x" : "delta");
    uint32_t end_y = drag ? parse_u32(argv[5], "end-y") : 0;
    uint32_t steps =
        click || hover ? 1 : parse_u32(argv[drag ? 6 : 5], "steps");
    uint32_t extent_x =
        parse_u32(argv[click ? 4 : (hover ? 5 : (drag ? 7 : 6))], "extent-x");
    uint32_t extent_y =
        parse_u32(argv[click ? 5 : (hover ? 6 : (drag ? 8 : 7))], "extent-y");
    if (steps == 0 || extent_x == 0 || extent_y == 0) {
        fail("steps and extents must be non-zero");
    }

    struct wl_display *display = wl_display_connect(NULL);
    if (display == NULL) {
        fail("could not connect to the Wayland compositor");
    }
    struct globals globals = {0};
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, &globals);
    if (wl_display_roundtrip(display) < 0 || globals.seat == NULL || globals.manager == NULL) {
        fail("the compositor does not expose a seat and virtual-pointer manager");
    }

    struct virtual_pointer *pointer = (struct virtual_pointer *)wl_proxy_marshal_flags(
        (struct wl_proxy *)globals.manager,
        0,
        &virtual_pointer_interface,
        wl_proxy_get_version((struct wl_proxy *)globals.manager),
        0,
        globals.seat,
        NULL
    );
    if (wl_display_roundtrip(display) < 0) {
        fail("the compositor rejected the virtual pointer");
    }

    absolute_motion(pointer, x, y, extent_x, extent_y);
    if (wl_display_roundtrip(display) < 0) {
        fail("the compositor rejected initial pointer motion");
    }
    usleep(50000);
    if (hover) {
        uint32_t hold_milliseconds = parse_u32(argv[4], "hold milliseconds");
        usleep((useconds_t)hold_milliseconds * 1000);
    } else if (click) {
        pointer_button(pointer, WL_POINTER_BUTTON_STATE_PRESSED);
        if (wl_display_roundtrip(display) < 0) {
            fail("the compositor rejected the pointer press");
        }
        usleep(20000);
        pointer_button(pointer, WL_POINTER_BUTTON_STATE_RELEASED);
        if (wl_display_roundtrip(display) < 0) {
            fail("the compositor rejected the pointer release");
        }
        usleep(50000);
    } else if (drag) {
        uint32_t end_x = (uint32_t)end_or_delta;
        pointer_button(pointer, WL_POINTER_BUTTON_STATE_PRESSED);
        if (wl_display_roundtrip(display) < 0) {
            fail("the compositor rejected the pointer press");
        }
        usleep(20000);
        for (uint32_t step = 1; step <= steps; ++step) {
            uint32_t next_x = x + (end_x - x) * step / steps;
            uint32_t next_y = y + (end_y - y) * step / steps;
            absolute_motion(pointer, next_x, next_y, extent_x, extent_y);
            if (wl_display_roundtrip(display) < 0) {
                fail("the compositor rejected pointer drag motion");
            }
            usleep(16000);
        }
        pointer_button(pointer, WL_POINTER_BUTTON_STATE_RELEASED);
        if (wl_display_roundtrip(display) < 0) {
            fail("the compositor rejected the pointer release");
        }
        usleep(50000);
    } else {
        wl_proxy_marshal_flags(
            (struct wl_proxy *)pointer,
            5,
            NULL,
            wl_proxy_get_version((struct wl_proxy *)pointer),
            0,
            WL_POINTER_AXIS_SOURCE_FINGER
        );
        for (uint32_t step = 0; step < steps; ++step) {
            pointer_axis(pointer, end_or_delta);
            wl_display_flush(display);
            usleep(4000);
        }
        wl_proxy_marshal_flags(
            (struct wl_proxy *)pointer,
            6,
            NULL,
            wl_proxy_get_version((struct wl_proxy *)pointer),
            0,
            monotonic_milliseconds(),
            WL_POINTER_AXIS_VERTICAL_SCROLL
        );
        wl_proxy_marshal_flags(
            (struct wl_proxy *)pointer,
            4,
            NULL,
            wl_proxy_get_version((struct wl_proxy *)pointer),
            0
        );
    }

    if (wl_display_roundtrip(display) < 0) {
        fail("the compositor rejected virtual pointer input");
    }
    usleep(100000);
    wl_proxy_marshal_flags(
        (struct wl_proxy *)pointer,
        8,
        NULL,
        wl_proxy_get_version((struct wl_proxy *)pointer),
        WL_MARSHAL_FLAG_DESTROY
    );
    wl_proxy_destroy((struct wl_proxy *)globals.manager);
    wl_seat_destroy(globals.seat);
    wl_registry_destroy(registry);
    wl_display_disconnect(display);
    return 0;
}
