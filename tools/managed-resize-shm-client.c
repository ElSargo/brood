#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>
#include <wayland-client.h>

#include "fractional-scale-v1-client-protocol.h"
#include "viewporter-client-protocol.h"
#include "xdg-shell-client-protocol.h"

enum commit_mode {
    MODE_NORMAL,
    MODE_STALL,
    MODE_OLD_ONCE,
    MODE_WRONG_ONCE,
    MODE_DELAY,
};

struct buffer {
    struct wl_buffer *object;
    void *pixels;
    size_t bytes;
    int fd;
};

struct client {
    struct wl_display *display;
    struct wl_registry *registry;
    struct wl_compositor *compositor;
    struct wl_shm *shm;
    struct xdg_wm_base *wm_base;
    struct wp_viewporter *viewporter;
    struct wp_fractional_scale_manager_v1 *fractional_manager;
    struct wl_surface *surface;
    struct xdg_surface *xdg_surface;
    struct xdg_toplevel *toplevel;
    struct wp_viewport *viewport;
    struct wp_fractional_scale_v1 *fractional_scale;
    struct buffer *attached;
    FILE *log;
    int control_fd;
    char control_buffer[4096];
    size_t control_used;
    const char *app_id;
    unsigned role;
    enum commit_mode mode;
    uint32_t scale_120;
    int logical_width;
    int logical_height;
    int previous_pixel_width;
    int previous_pixel_height;
    uint32_t configure_serial;
    uint64_t configure_index;
    uint64_t commit_index;
    uint64_t delayed_at_us;
    unsigned delay_ms;
    bool configured;
    bool running;
};

static uint64_t monotonic_us(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
        return 0;
    }
    return (uint64_t)now.tv_sec * 1000000u + (uint64_t)now.tv_nsec / 1000u;
}

static void log_event(struct client *client, const char *event, const char *detail) {
    fprintf(client->log,
            "time_us=%llu app=%s event=%s configure_index=%llu serial=%u "
            "logical=%dx%d scale_120=%u %s\n",
            (unsigned long long)monotonic_us(), client->app_id, event,
            (unsigned long long)client->configure_index, client->configure_serial,
            client->logical_width, client->logical_height, client->scale_120,
            detail == NULL ? "" : detail);
    fflush(client->log);
}

static int create_shm_file(size_t bytes) {
    char name[] = "/tmp/brood-managed-shm.XXXXXX";
    int fd = mkstemp(name);
    if (fd < 0) {
        return -1;
    }
    unlink(name);
    if (ftruncate(fd, (off_t)bytes) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static void buffer_release(void *data, struct wl_buffer *object) {
    struct buffer *buffer = data;
    (void)object;
    wl_buffer_destroy(buffer->object);
    munmap(buffer->pixels, buffer->bytes);
    close(buffer->fd);
    free(buffer);
}

static const struct wl_buffer_listener buffer_listener = {
    .release = buffer_release,
};

static uint32_t fixture_pixel(struct client *client, int x, int y, int width,
                              int height, uint64_t content) {
    uint8_t red = client->role == 1 ? 0x28 : 0x24;
    uint8_t green = client->role == 1 ? 0x5c : 0x28;
    uint8_t blue = client->role == 1 ? 0x9a : 0xa4;
    if (y < 11) {
        unsigned bit = (unsigned)((uint64_t)x * 64u / (uint64_t)width);
        bool one = ((content >> (bit % 64u)) & 1u) != 0;
        red = green = blue = one ? 0xea : 0x12;
    } else if (x < 3 || y < 3 || x >= width - 3 || y >= height - 3) {
        red = green = blue = 0xf4;
    } else if (x >= width / 2 - 2 && x <= width / 2 + 2) {
        red = client->role == 1 ? 0x28 : 0xe8;
        green = client->role == 1 ? 0xe8 : 0x70;
        blue = client->role == 1 ? 0xe8 : 0x34;
    }
    return 0xff000000u | (uint32_t)red << 16u | (uint32_t)green << 8u |
           (uint32_t)blue;
}

static struct buffer *make_buffer(struct client *client, int width, int height,
                                  uint64_t content) {
    if (width <= 0 || height <= 0 || width > 16384 || height > 16384) {
        return NULL;
    }
    size_t stride = (size_t)width * 4u;
    size_t bytes = stride * (size_t)height;
    int fd = create_shm_file(bytes);
    if (fd < 0) {
        return NULL;
    }
    void *pixels = mmap(NULL, bytes, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (pixels == MAP_FAILED) {
        close(fd);
        return NULL;
    }
    uint32_t *words = pixels;
    for (int y = 0; y < height; y += 1) {
        for (int x = 0; x < width; x += 1) {
            words[(size_t)y * (size_t)width + (size_t)x] =
                fixture_pixel(client, x, y, width, height, content);
        }
    }
    struct wl_shm_pool *pool = wl_shm_create_pool(client->shm, fd, (int)bytes);
    struct buffer *buffer = calloc(1, sizeof(*buffer));
    if (pool == NULL || buffer == NULL) {
        if (pool != NULL) {
            wl_shm_pool_destroy(pool);
        }
        munmap(pixels, bytes);
        close(fd);
        free(buffer);
        return NULL;
    }
    buffer->object = wl_shm_pool_create_buffer(pool, 0, width, height, (int)stride,
                                                WL_SHM_FORMAT_XRGB8888);
    wl_shm_pool_destroy(pool);
    if (buffer->object == NULL) {
        munmap(pixels, bytes);
        close(fd);
        free(buffer);
        return NULL;
    }
    buffer->pixels = pixels;
    buffer->bytes = bytes;
    buffer->fd = fd;
    wl_buffer_add_listener(buffer->object, &buffer_listener, buffer);
    return buffer;
}

static int scaled_extent(int logical, uint32_t scale_120) {
    uint64_t scaled = (uint64_t)(unsigned)logical * (uint64_t)scale_120;
    return (int)((scaled + 60u) / 120u);
}

static bool insufficient_extent(int target, int *result) {
    if (target <= 1) {
        return false;
    }
    *result = target - 1;
    return true;
}

static bool commit_buffer(struct client *client, int pixel_width, int pixel_height,
                          const char *kind) {
    struct buffer *buffer = make_buffer(client, pixel_width, pixel_height,
                                        ++client->commit_index);
    if (buffer == NULL) {
        log_event(client, "fatal", "reason=buffer-allocation");
        client->running = false;
        return false;
    }
    wp_viewport_set_destination(client->viewport, client->logical_width,
                                client->logical_height);
    wl_surface_attach(client->surface, buffer->object, 0, 0);
    wl_surface_damage_buffer(client->surface, 0, 0, pixel_width, pixel_height);
    wl_surface_commit(client->surface);
    client->attached = buffer;
    client->previous_pixel_width = pixel_width;
    client->previous_pixel_height = pixel_height;
    char detail[192];
    snprintf(detail, sizeof(detail), "kind=%s pixel=%dx%d content=%llu", kind,
             pixel_width, pixel_height, (unsigned long long)client->commit_index);
    log_event(client, "commit", detail);
    return true;
}

static void commit_exact(struct client *client, const char *kind) {
    if (!client->configured || client->scale_120 == 0) {
        return;
    }
    commit_buffer(client, scaled_extent(client->logical_width, client->scale_120),
                  scaled_extent(client->logical_height, client->scale_120), kind);
}

static void satisfy_configure(struct client *client) {
    if (!client->configured || client->scale_120 == 0) {
        return;
    }
    switch (client->mode) {
    case MODE_NORMAL:
        commit_exact(client, "exact");
        break;
    case MODE_STALL:
        log_event(client, "withhold", "kind=stall");
        break;
    case MODE_OLD_ONCE: {
        int width = client->previous_pixel_width > 0
                        ? client->previous_pixel_width
                        : scaled_extent(client->logical_width, client->scale_120) - 3;
        int height = client->previous_pixel_height > 0
                         ? client->previous_pixel_height
                         : scaled_extent(client->logical_height, client->scale_120);
        commit_buffer(client, width, height, "old-after-ack");
        client->mode = MODE_STALL;
        break;
    }
    case MODE_WRONG_ONCE: {
        int width;
        int target_width = scaled_extent(client->logical_width, client->scale_120);
        if (!insufficient_extent(target_width, &width)) {
            log_event(client, "fatal", "reason=target-width-too-small");
            client->running = false;
            break;
        }
        commit_buffer(client, width,
                      scaled_extent(client->logical_height, client->scale_120),
                      "wrong-after-ack");
        client->mode = MODE_STALL;
        break;
    }
    case MODE_DELAY:
        client->delayed_at_us = monotonic_us() + (uint64_t)client->delay_ms * 1000u;
        log_event(client, "withhold", "kind=delay");
        break;
    }
}

static void wm_ping(void *data, struct xdg_wm_base *wm_base, uint32_t serial) {
    (void)data;
    xdg_wm_base_pong(wm_base, serial);
}

static const struct xdg_wm_base_listener wm_listener = {.ping = wm_ping};

static void toplevel_configure(void *data, struct xdg_toplevel *toplevel,
                               int32_t width, int32_t height,
                               struct wl_array *states) {
    struct client *client = data;
    (void)toplevel;
    (void)states;
    if (width > 0 && height > 0) {
        client->logical_width = width;
        client->logical_height = height;
    }
}

static void toplevel_close(void *data, struct xdg_toplevel *toplevel) {
    struct client *client = data;
    (void)toplevel;
    log_event(client, "close", "source=compositor");
    client->running = false;
}

static void toplevel_bounds(void *data, struct xdg_toplevel *toplevel, int32_t width,
                            int32_t height) {
    (void)data;
    (void)toplevel;
    (void)width;
    (void)height;
}

static void toplevel_capabilities(void *data, struct xdg_toplevel *toplevel,
                                  struct wl_array *capabilities) {
    (void)data;
    (void)toplevel;
    (void)capabilities;
}

static const struct xdg_toplevel_listener toplevel_listener = {
    .configure = toplevel_configure,
    .close = toplevel_close,
    .configure_bounds = toplevel_bounds,
    .wm_capabilities = toplevel_capabilities,
};

static void surface_configure(void *data, struct xdg_surface *surface, uint32_t serial) {
    struct client *client = data;
    client->configure_serial = serial;
    client->configure_index += 1;
    client->configured = true;
    xdg_surface_ack_configure(surface, serial);
    log_event(client, "configure-ack", "");
    satisfy_configure(client);
}

static const struct xdg_surface_listener surface_listener = {
    .configure = surface_configure,
};

static void preferred_scale(void *data, struct wp_fractional_scale_v1 *fractional,
                            uint32_t scale) {
    struct client *client = data;
    (void)fractional;
    client->scale_120 = scale;
    log_event(client, "preferred-scale", "");
    satisfy_configure(client);
}

static const struct wp_fractional_scale_v1_listener fractional_listener = {
    .preferred_scale = preferred_scale,
};

static void registry_global(void *data, struct wl_registry *registry, uint32_t name,
                            const char *interface, uint32_t version) {
    struct client *client = data;
    if (strcmp(interface, wl_compositor_interface.name) == 0) {
        client->compositor = wl_registry_bind(registry, name, &wl_compositor_interface,
                                              version < 6 ? version : 6);
    } else if (strcmp(interface, wl_shm_interface.name) == 0) {
        client->shm = wl_registry_bind(registry, name, &wl_shm_interface, 1);
    } else if (strcmp(interface, xdg_wm_base_interface.name) == 0) {
        client->wm_base = wl_registry_bind(registry, name, &xdg_wm_base_interface, 1);
        xdg_wm_base_add_listener(client->wm_base, &wm_listener, client);
    } else if (strcmp(interface, wp_viewporter_interface.name) == 0) {
        client->viewporter = wl_registry_bind(registry, name, &wp_viewporter_interface, 1);
    } else if (strcmp(interface, wp_fractional_scale_manager_v1_interface.name) == 0) {
        client->fractional_manager = wl_registry_bind(
            registry, name, &wp_fractional_scale_manager_v1_interface, 1);
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

static void handle_command(struct client *client, char *command) {
    while (*command == ' ' || *command == '\t') {
        command += 1;
    }
    if (strcmp(command, "normal") == 0) {
        client->mode = MODE_NORMAL;
    } else if (strcmp(command, "stall") == 0) {
        client->mode = MODE_STALL;
    } else if (strcmp(command, "old-once") == 0) {
        client->mode = MODE_OLD_ONCE;
    } else if (strcmp(command, "wrong-once") == 0) {
        client->mode = MODE_WRONG_ONCE;
    } else if (strncmp(command, "delay ", 6) == 0) {
        client->mode = MODE_DELAY;
        client->delay_ms = (unsigned)strtoul(command + 6, NULL, 10);
    } else if (strcmp(command, "exact") == 0) {
        client->mode = MODE_STALL;
        commit_exact(client, "exact-command");
    } else if (strcmp(command, "ordinary") == 0) {
        commit_exact(client, "ordinary-command");
    } else if (strcmp(command, "close") == 0) {
        log_event(client, "close", "source=control");
        client->running = false;
    } else {
        log_event(client, "unknown-command", command);
        return;
    }
    log_event(client, "command", command);
}

static void read_commands(struct client *client) {
    char input[1024];
    ssize_t count = read(client->control_fd, input, sizeof(input));
    if (count <= 0) {
        return;
    }
    if ((size_t)count > sizeof(client->control_buffer) - client->control_used - 1u) {
        client->control_used = 0;
        log_event(client, "command-overflow", "");
    }
    memcpy(client->control_buffer + client->control_used, input, (size_t)count);
    client->control_used += (size_t)count;
    client->control_buffer[client->control_used] = '\0';
    char *start = client->control_buffer;
    char *newline;
    while ((newline = strchr(start, '\n')) != NULL) {
        *newline = '\0';
        handle_command(client, start);
        start = newline + 1;
    }
    size_t remaining = client->control_buffer + client->control_used - start;
    memmove(client->control_buffer, start, remaining);
    client->control_used = remaining;
}

static void destroy_client(struct client *client) {
    if (client->fractional_scale != NULL) {
        wp_fractional_scale_v1_destroy(client->fractional_scale);
    }
    if (client->viewport != NULL) {
        wp_viewport_destroy(client->viewport);
    }
    if (client->toplevel != NULL) {
        xdg_toplevel_destroy(client->toplevel);
    }
    if (client->xdg_surface != NULL) {
        xdg_surface_destroy(client->xdg_surface);
    }
    if (client->surface != NULL) {
        wl_surface_destroy(client->surface);
    }
    if (client->fractional_manager != NULL) {
        wp_fractional_scale_manager_v1_destroy(client->fractional_manager);
    }
    if (client->viewporter != NULL) {
        wp_viewporter_destroy(client->viewporter);
    }
    if (client->wm_base != NULL) {
        xdg_wm_base_destroy(client->wm_base);
    }
    if (client->shm != NULL) {
        wl_shm_destroy(client->shm);
    }
    if (client->compositor != NULL) {
        wl_compositor_destroy(client->compositor);
    }
    if (client->registry != NULL) {
        wl_registry_destroy(client->registry);
    }
    if (client->display != NULL) {
        wl_display_disconnect(client->display);
    }
    if (client->control_fd >= 0) {
        close(client->control_fd);
    }
    if (client->log != NULL) {
        fclose(client->log);
    }
}

int main(int argc, char **argv) {
    if (argc == 2 && strcmp(argv[1], "--self-test") == 0) {
        int result = 0;
        struct client test_client = {.role = 1};
        if (!insufficient_extent(545, &result) || result != 544 ||
            !insufficient_extent(2, &result) || result != 1 ||
            insufficient_extent(1, &result) ||
            (fixture_pixel(&test_client, 0, 6, 320, 32, UINT64_C(1)) & 0x00ffffffu) !=
                0x00eaeaeau ||
            (fixture_pixel(&test_client, 319, 6, 320, 32,
                           UINT64_C(1) << 63u) &
             0x00ffffffu) != 0x00eaeaeau ||
            (fixture_pixel(&test_client, 0, 12, 320, 32, 0) & 0x00ffffffu) !=
                0x00f4f4f4u) {
            fprintf(stderr, "managed resize fixture self-test failed\n");
            return 1;
        }
        puts("managed_resize_fixture_self_test=passed");
        return 0;
    }
    if (argc != 5) {
        fprintf(stderr, "usage: %s APP_ID ROLE CONTROL_FIFO LOG\n", argv[0]);
        return 2;
    }
    struct client client = {
        .app_id = argv[1],
        .role = (unsigned)strtoul(argv[2], NULL, 10),
        .mode = MODE_NORMAL,
        .logical_width = 320,
        .logical_height = 240,
        .control_fd = -1,
        .running = true,
    };
    client.log = fopen(argv[4], "a");
    if (client.log == NULL) {
        perror("open log");
        return 1;
    }
    client.control_fd = open(argv[3], O_RDWR | O_NONBLOCK | O_CLOEXEC);
    client.display = wl_display_connect(NULL);
    if (client.control_fd < 0 || client.display == NULL) {
        log_event(&client, "fatal", "reason=connect");
        destroy_client(&client);
        return 1;
    }
    client.registry = wl_display_get_registry(client.display);
    wl_registry_add_listener(client.registry, &registry_listener, &client);
    if (wl_display_roundtrip(client.display) < 0 || client.compositor == NULL ||
        client.shm == NULL || client.wm_base == NULL || client.viewporter == NULL ||
        client.fractional_manager == NULL) {
        log_event(&client, "fatal", "reason=missing-global");
        destroy_client(&client);
        return 1;
    }
    client.surface = wl_compositor_create_surface(client.compositor);
    client.xdg_surface = xdg_wm_base_get_xdg_surface(client.wm_base, client.surface);
    xdg_surface_add_listener(client.xdg_surface, &surface_listener, &client);
    client.toplevel = xdg_surface_get_toplevel(client.xdg_surface);
    xdg_toplevel_add_listener(client.toplevel, &toplevel_listener, &client);
    xdg_toplevel_set_title(client.toplevel, client.app_id);
    xdg_toplevel_set_app_id(client.toplevel, client.app_id);
    client.viewport = wp_viewporter_get_viewport(client.viewporter, client.surface);
    client.fractional_scale = wp_fractional_scale_manager_v1_get_fractional_scale(
        client.fractional_manager, client.surface);
    wp_fractional_scale_v1_add_listener(client.fractional_scale, &fractional_listener,
                                        &client);
    wl_surface_commit(client.surface);
    log_event(&client, "started", "");

    while (client.running) {
        struct pollfd fds[2] = {
            {.fd = wl_display_get_fd(client.display), .events = POLLIN},
            {.fd = client.control_fd, .events = POLLIN},
        };
        wl_display_dispatch_pending(client.display);
        wl_display_flush(client.display);
        int timeout = -1;
        if (client.delayed_at_us != 0) {
            uint64_t now = monotonic_us();
            timeout = now >= client.delayed_at_us
                          ? 0
                          : (int)((client.delayed_at_us - now + 999u) / 1000u);
        }
        int ready = poll(fds, 2, timeout);
        if (ready < 0 && errno != EINTR) {
            break;
        }
        if (fds[0].revents & (POLLERR | POLLHUP)) {
            break;
        }
        if (fds[0].revents & POLLIN && wl_display_dispatch(client.display) < 0) {
            break;
        }
        if (fds[1].revents & POLLIN) {
            read_commands(&client);
        }
        if (client.delayed_at_us != 0 && monotonic_us() >= client.delayed_at_us) {
            client.delayed_at_us = 0;
            client.mode = MODE_STALL;
            commit_exact(&client, "exact-delay");
        }
    }
    log_event(&client, "stopped", "");
    destroy_client(&client);
    return 0;
}
