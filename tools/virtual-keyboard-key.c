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
    int fd = memfd_create("brood-test-keymap", MFD_CLOEXEC);
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

static int ascii_key(unsigned char character, uint32_t *keycode, int *shifted) {
    static const uint32_t digit_keycodes[] = {11, 2, 3, 4, 5, 6, 7, 8, 9, 10};
    static const uint32_t letter_keycodes[] = {
        30, 48, 46, 32, 18, 33, 34, 35, 23, 36, 37, 38, 50,
        49, 24, 25, 16, 19, 31, 20, 22, 47, 17, 45, 21, 44,
    };
    static const struct {
        unsigned char character;
        uint32_t keycode;
        int shifted;
    } punctuation[] = {
        {'-', 12, 0}, {'_', 12, 1}, {'=', 13, 0}, {'+', 13, 1},
        {'[', 26, 0}, {'{', 26, 1}, {']', 27, 0}, {'}', 27, 1},
        {';', 39, 0}, {':', 39, 1}, {'\'', 40, 0}, {'"', 40, 1},
        {'`', 41, 0}, {'~', 41, 1}, {'\\', 43, 0}, {'|', 43, 1},
        {',', 51, 0}, {'<', 51, 1}, {'.', 52, 0}, {'>', 52, 1},
        {'/', 53, 0}, {'?', 53, 1}, {'!', 2, 1}, {'@', 3, 1},
        {'#', 4, 1}, {'$', 5, 1}, {'%', 6, 1}, {'^', 7, 1},
        {'&', 8, 1}, {'*', 9, 1}, {'(', 10, 1}, {')', 11, 1},
    };

    *shifted = 0;
    if (character >= 'a' && character <= 'z') {
        *keycode = letter_keycodes[character - 'a'];
        return 1;
    }
    if (character >= 'A' && character <= 'Z') {
        *keycode = letter_keycodes[character - 'A'];
        *shifted = 1;
        return 1;
    }
    if (character >= '0' && character <= '9') {
        *keycode = digit_keycodes[character - '0'];
        return 1;
    }
    if (character == ' ') {
        *keycode = 57;
        return 1;
    }
    if (character == '\t') {
        *keycode = 15;
        return 1;
    }
    if (character == '\n' || character == '\r') {
        *keycode = 28;
        return 1;
    }
    for (size_t index = 0; index < sizeof(punctuation) / sizeof(punctuation[0]); index++) {
        if (punctuation[index].character == character) {
            *keycode = punctuation[index].keycode;
            *shifted = punctuation[index].shifted;
            return 1;
        }
    }
    return 0;
}

static void send_modifiers(struct virtual_keyboard *keyboard, uint32_t modifiers) {
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
}

static void send_key(struct virtual_keyboard *keyboard, uint32_t keycode) {
    uint32_t time = monotonic_milliseconds();
    wl_proxy_marshal_flags(
        (struct wl_proxy *)keyboard,
        1,
        NULL,
        wl_proxy_get_version((struct wl_proxy *)keyboard),
        0,
        time,
        keycode,
        WL_KEYBOARD_KEY_STATE_PRESSED
    );
    wl_proxy_marshal_flags(
        (struct wl_proxy *)keyboard,
        1,
        NULL,
        wl_proxy_get_version((struct wl_proxy *)keyboard),
        0,
        time + 1,
        keycode,
        WL_KEYBOARD_KEY_STATE_RELEASED
    );
}

int main(int argc, char **argv) {
    enum modifier_mode {
        MODIFIERS_CTRL_ALT,
        MODIFIERS_CTRL,
        MODIFIERS_CTRL_SHIFT,
        MODIFIERS_SHIFT,
        MODIFIERS_NONE,
    } modifier_mode = MODIFIERS_CTRL_ALT;
    const char *text_input = NULL;
    const char *keycode = NULL;
    const char *repeat_count_value = NULL;
    const char *repeat_interval_value = NULL;
    const char *idle_proof_duration_value = NULL;
    if (argc == 2) {
        keycode = argv[1];
    } else if (argc == 3 && strcmp(argv[1], "--ctrl-only") == 0) {
        modifier_mode = MODIFIERS_CTRL;
        keycode = argv[2];
    } else if (argc == 3 && strcmp(argv[1], "--ctrl-shift-only") == 0) {
        modifier_mode = MODIFIERS_CTRL_SHIFT;
        keycode = argv[2];
    } else if (argc == 3 && strcmp(argv[1], "--shift-only") == 0) {
        modifier_mode = MODIFIERS_SHIFT;
        keycode = argv[2];
    } else if (argc == 3 && strcmp(argv[1], "--none") == 0) {
        modifier_mode = MODIFIERS_NONE;
        keycode = argv[2];
    } else if (argc == 3 && strcmp(argv[1], "--plain") == 0) {
        modifier_mode = MODIFIERS_NONE;
        keycode = argv[2];
    } else if (argc == 3 && strcmp(argv[1], "--text") == 0) {
        modifier_mode = MODIFIERS_NONE;
        text_input = argv[2];
    } else if (argc == 3 && strcmp(argv[1], "--idle-proof") == 0) {
        modifier_mode = MODIFIERS_NONE;
        idle_proof_duration_value = argv[2];
    } else if (argc == 5 && strcmp(argv[1], "--repeat") == 0) {
        modifier_mode = MODIFIERS_NONE;
        repeat_count_value = argv[2];
        repeat_interval_value = argv[3];
        keycode = argv[4];
    } else {
        fail(
            "usage: virtual-keyboard-key "
            "[--ctrl-only|--ctrl-shift-only|--shift-only|--none|--plain] "
            "<linux-evdev-keycode> "
            "| --repeat <count> <interval-us> <linux-evdev-keycode> "
            "| --idle-proof <duration-ms> "
            "| --text <ascii-text>"
        );
    }
    char *end = NULL;
    unsigned long parsed = 0;
    if (keycode != NULL) {
        parsed = strtoul(keycode, &end, 10);
        if (end == keycode || *end != '\0' || parsed > UINT32_MAX) {
            fail("the keycode must be an unsigned 32-bit integer");
        }
    }
    unsigned long repeat_count = 1;
    unsigned long repeat_interval_us = 0;
    unsigned long idle_proof_duration_ms = 0;
    if (repeat_count_value != NULL) {
        repeat_count = strtoul(repeat_count_value, &end, 10);
        if (end == repeat_count_value || *end != '\0' || repeat_count == 0) {
            fail("repeat count must be a positive integer");
        }
        repeat_interval_us = strtoul(repeat_interval_value, &end, 10);
        if (end == repeat_interval_value || *end != '\0' || repeat_interval_us == 0) {
            fail("repeat interval must be a positive number of microseconds");
        }
    }
    if (idle_proof_duration_value != NULL) {
        idle_proof_duration_ms = strtoul(idle_proof_duration_value, &end, 10);
        if (
            end == idle_proof_duration_value || *end != '\0' ||
            idle_proof_duration_ms == 0 || idle_proof_duration_ms > UINT32_MAX
        ) {
            fail("idle proof duration must be a positive 32-bit millisecond value");
        }
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
    uint32_t modifiers = 0;
    if (
        modifier_mode == MODIFIERS_CTRL_ALT ||
        modifier_mode == MODIFIERS_CTRL ||
        modifier_mode == MODIFIERS_CTRL_SHIFT
    ) {
        modifiers = modifier_mask(keymap, XKB_MOD_NAME_CTRL);
    }
    if (modifier_mode == MODIFIERS_CTRL_ALT) {
        modifiers |= modifier_mask(keymap, XKB_MOD_NAME_ALT);
    }
    if (modifier_mode == MODIFIERS_SHIFT) {
        modifiers = modifier_mask(keymap, XKB_MOD_NAME_SHIFT);
    }
    if (modifier_mode == MODIFIERS_CTRL_SHIFT) {
        modifiers |= modifier_mask(keymap, XKB_MOD_NAME_SHIFT);
    }
    send_modifiers(keyboard, modifiers);
    if (wl_display_roundtrip(display) < 0) {
        fail("the compositor rejected the virtual keyboard modifiers");
    }
    usleep(10000);
    if (idle_proof_duration_value != NULL) {
        // Keep one virtual-keyboard device alive across both proof boundaries.
        // Device/keymap setup and teardown can generate focus/reset traffic, so
        // neither belongs inside the interval that is asserting true idleness.
        usleep(500000);
        send_key(keyboard, 88);
        if (wl_display_roundtrip(display) < 0) {
            fail("the virtual keyboard idle proof start failed");
        }
        struct timespec delay = {
            .tv_sec = (time_t)(idle_proof_duration_ms / 1000),
            .tv_nsec = (long)(idle_proof_duration_ms % 1000) * 1000000L,
        };
        while (nanosleep(&delay, &delay) < 0 && errno == EINTR) {
        }
        send_key(keyboard, 87);
        if (wl_display_flush(display) < 0) {
            fail("the virtual keyboard idle proof finish failed");
        }
    } else if (text_input != NULL) {
        uint32_t shift_mask = modifier_mask(keymap, XKB_MOD_NAME_SHIFT);
        int shift_active = 0;
        for (const unsigned char *cursor = (const unsigned char *)text_input; *cursor; cursor++) {
            uint32_t text_keycode = 0;
            int shifted = 0;
            if (!ascii_key(*cursor, &text_keycode, &shifted)) {
                fail("--text only supports printable ASCII, tab, and newline");
            }
            if (shifted != shift_active) {
                send_modifiers(keyboard, shifted ? shift_mask : 0);
                shift_active = shifted;
            }
            send_key(keyboard, text_keycode);
            if (wl_display_flush(display) < 0) {
                fail("the virtual keyboard text request failed");
            }
            usleep(1000);
        }
    } else {
        struct timespec deadline = {0};
        if (clock_gettime(CLOCK_MONOTONIC, &deadline) < 0) {
            fail("could not establish the repeat deadline");
        }
        for (unsigned long index = 0; index < repeat_count; index++) {
            if (index > 0) {
                uint64_t next_nanoseconds =
                    (uint64_t)deadline.tv_nsec + (uint64_t)repeat_interval_us * UINT64_C(1000);
                deadline.tv_sec += (time_t)(next_nanoseconds / UINT64_C(1000000000));
                deadline.tv_nsec = (long)(next_nanoseconds % UINT64_C(1000000000));
                while (clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME, &deadline, NULL) == EINTR) {
                }
            }
            uint32_t time = monotonic_milliseconds();
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
            if (repeat_count_value == NULL) {
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
            }
            if (wl_display_flush(display) < 0) {
                fail("the virtual keyboard repeat failed");
            }
        }
        if (repeat_count_value != NULL) {
            wl_proxy_marshal_flags(
                (struct wl_proxy *)keyboard,
                1,
                NULL,
                wl_proxy_get_version((struct wl_proxy *)keyboard),
                0,
                monotonic_milliseconds(),
                (uint32_t)parsed,
                WL_KEYBOARD_KEY_STATE_RELEASED
            );
        }
    }
    send_modifiers(keyboard, 0);
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
