#define _GNU_SOURCE

#include <errno.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

#include <wayland-client.h>
#include <xkbcommon/xkbcommon.h>

struct virtual_keyboard_manager;
struct virtual_keyboard;

static const struct wl_interface virtual_keyboard_interface;

static const struct wl_interface *manager_create_types[] = {
    &wl_seat_interface,
    &virtual_keyboard_interface,
};

static const struct wl_message manager_requests[] = {
    {"create_virtual_keyboard", "on", manager_create_types},
};

static const struct wl_interface virtual_keyboard_manager_interface = {
    "zwp_virtual_keyboard_manager_v1",
    1,
    1,
    manager_requests,
    0,
    NULL,
};

static const struct wl_message virtual_keyboard_requests[] = {
    {"keymap", "uhu", NULL},
    {"key", "uuu", NULL},
    {"modifiers", "uuuu", NULL},
    {"destroy", "", NULL},
};

static const struct wl_interface virtual_keyboard_interface = {
    "zwp_virtual_keyboard_v1",
    1,
    4,
    virtual_keyboard_requests,
    0,
    NULL,
};

struct globals {
    struct wl_seat *seat;
    struct virtual_keyboard_manager *manager;
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
        strcmp(interface, virtual_keyboard_manager_interface.name) == 0 &&
        globals->manager == NULL
    ) {
        globals->manager = wl_registry_bind(
            registry,
            name,
            &virtual_keyboard_manager_interface,
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
    fprintf(stderr, "virtual-keyboard-key: %s\n", message);
    exit(1);
}

static int create_keymap_fd(struct xkb_keymap *keymap, uint32_t *size_out) {
    char *text = xkb_keymap_get_as_string(keymap, XKB_KEYMAP_FORMAT_TEXT_V1);
    if (text == NULL) {
        fail("could not serialize the XKB keymap");
    }
    size_t size = strlen(text) + 1;
    if (size > UINT32_MAX) {
        free(text);
        fail("serialized XKB keymap is too large");
    }
    int fd = memfd_create("manifold-test-keymap", MFD_CLOEXEC);
    if (fd < 0 || ftruncate(fd, (off_t)size) < 0) {
        free(text);
        fail("could not allocate the XKB keymap file");
    }
    size_t written = 0;
    while (written < size) {
        ssize_t result = write(fd, text + written, size - written);
        if (result < 0 && errno == EINTR) {
            continue;
        }
        if (result <= 0) {
            close(fd);
            free(text);
            fail("could not write the XKB keymap file");
        }
        written += (size_t)result;
    }
    free(text);
    *size_out = (uint32_t)size;
    return fd;
}

static uint32_t modifier_mask(struct xkb_keymap *keymap, const char *name) {
    xkb_mod_index_t index = xkb_keymap_mod_get_index(keymap, name);
    if (index == XKB_MOD_INVALID || index >= 32) {
        fail("required XKB modifier is unavailable");
    }
    return UINT32_C(1) << index;
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

int main(int argc, char **argv) {
    bool ctrl_only = false;
    const char *keycode = NULL;
    if (argc == 2) {
        keycode = argv[1];
    } else if (argc == 3 && strcmp(argv[1], "--ctrl-only") == 0) {
        ctrl_only = true;
        keycode = argv[2];
    } else {
        fail("usage: virtual-keyboard-key [--ctrl-only] <linux-evdev-keycode>");
    }
    char *end = NULL;
    unsigned long parsed = strtoul(keycode, &end, 10);
    if (end == keycode || *end != '\0' || parsed > UINT32_MAX) {
        fail("the keycode must be an unsigned 32-bit integer");
    }

    struct wl_display *display = wl_display_connect(NULL);
    if (display == NULL) {
        fail("could not connect to the Wayland compositor");
    }
    struct globals globals = {0};
    struct wl_registry *registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, &globals);
    if (wl_display_roundtrip(display) < 0 || globals.seat == NULL || globals.manager == NULL) {
        fail("the compositor does not expose a seat and virtual-keyboard manager");
    }

    struct xkb_context *context = xkb_context_new(XKB_CONTEXT_NO_FLAGS);
    if (context == NULL) {
        fail("could not create the XKB context");
    }
    struct xkb_keymap *keymap = xkb_keymap_new_from_names(
        context,
        NULL,
        XKB_KEYMAP_COMPILE_NO_FLAGS
    );
    if (keymap == NULL) {
        fail("could not create the default XKB keymap");
    }
    uint32_t keymap_size = 0;
    int keymap_fd = create_keymap_fd(keymap, &keymap_size);

    struct virtual_keyboard *keyboard = (struct virtual_keyboard *)wl_proxy_marshal_flags(
        (struct wl_proxy *)globals.manager,
        0,
        &virtual_keyboard_interface,
        wl_proxy_get_version((struct wl_proxy *)globals.manager),
        0,
        globals.seat,
        NULL
    );
    wl_proxy_marshal_flags(
        (struct wl_proxy *)keyboard,
        0,
        NULL,
        wl_proxy_get_version((struct wl_proxy *)keyboard),
        0,
        WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1,
        keymap_fd,
        keymap_size
    );
    close(keymap_fd);
    if (wl_display_roundtrip(display) < 0) {
        fail("the compositor rejected the virtual keyboard keymap");
    }
    usleep(10000);
    uint32_t modifiers = modifier_mask(keymap, XKB_MOD_NAME_CTRL);
    if (!ctrl_only) {
        modifiers |= modifier_mask(keymap, XKB_MOD_NAME_ALT);
    }
    uint32_t time = monotonic_milliseconds();
    wl_proxy_marshal_flags(
        (struct wl_proxy *)keyboard,
        2,
        NULL,
        wl_proxy_get_version((struct wl_proxy *)keyboard),
        0,
        modifiers,
        0,
        0,
        0
    );
    if (wl_display_roundtrip(display) < 0) {
        fail("the compositor rejected the virtual keyboard modifiers");
    }
    usleep(10000);
    wl_proxy_marshal_flags(
        (struct wl_proxy *)keyboard,
        1,
        NULL,
        wl_proxy_get_version((struct wl_proxy *)keyboard),
        0,
        time,
        (uint32_t)parsed,
        WL_KEYBOARD_KEY_STATE_PRESSED
    );
    if (wl_display_roundtrip(display) < 0) {
        fail("the virtual keyboard key press failed");
    }
    usleep(10000);
    wl_proxy_marshal_flags(
        (struct wl_proxy *)keyboard,
        1,
        NULL,
        wl_proxy_get_version((struct wl_proxy *)keyboard),
        0,
        time + 1,
        (uint32_t)parsed,
        WL_KEYBOARD_KEY_STATE_RELEASED
    );
    wl_proxy_marshal_flags(
        (struct wl_proxy *)keyboard,
        2,
        NULL,
        wl_proxy_get_version((struct wl_proxy *)keyboard),
        0,
        0,
        0,
        0,
        0
    );
    if (wl_display_roundtrip(display) < 0) {
        fail("the virtual keyboard request failed");
    }
    usleep(10000);
    wl_proxy_marshal_flags(
        (struct wl_proxy *)keyboard,
        3,
        NULL,
        wl_proxy_get_version((struct wl_proxy *)keyboard),
        WL_MARSHAL_FLAG_DESTROY
    );
    wl_proxy_destroy((struct wl_proxy *)globals.manager);
    wl_seat_destroy(globals.seat);
    wl_registry_destroy(registry);
    xkb_keymap_unref(keymap);
    xkb_context_unref(context);
    wl_display_disconnect(display);
    return 0;
}
