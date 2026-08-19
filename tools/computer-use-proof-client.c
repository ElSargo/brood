#include <gtk/gtk.h>
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct {
    const char *state_path;
    GtkWidget *canvas;
    unsigned revision;
    unsigned click_count;
    unsigned double_clicks;
    unsigned scroll_events;
    unsigned shortcut_count;
    double drag_distance;
    double pointer_x;
    double pointer_y;
    gboolean button_down;
    gboolean control_down;
    gint64 last_press_us;
    double last_press_x;
    double last_press_y;
    char typed_text[256];
    size_t typed_len;
} ProofState;

static void write_state(ProofState *state) {
    FILE *file = fopen(state->state_path, "w");
    if (file == NULL) {
        return;
    }
    state->revision++;
    fprintf(file,
            "{\"revision\":%u,\"click_count\":%u,\"double_clicks\":%u,"
            "\"scroll_events\":%u,\"shortcut_count\":%u,"
            "\"drag_distance\":%.3f,\"pointer_x\":%.3f,\"pointer_y\":%.3f,"
            "\"button_down\":%s,\"control_down\":%s,\"typed_text\":\"%s\"}\n",
            state->revision, state->click_count, state->double_clicks,
            state->scroll_events, state->shortcut_count, state->drag_distance,
            state->pointer_x, state->pointer_y,
            state->button_down ? "true" : "false",
            state->control_down ? "true" : "false", state->typed_text);
    fclose(file);
    gtk_widget_queue_draw(state->canvas);
}

static void draw_text(cairo_t *cr, double x, double y, const char *text) {
    cairo_move_to(cr, x, y);
    cairo_show_text(cr, text);
}

static void draw_canvas(GtkDrawingArea *area, cairo_t *cr, int width, int height,
                        gpointer data) {
    (void)area;
    ProofState *state = data;
    cairo_set_source_rgb(cr, 0.06, 0.08, 0.12);
    cairo_paint(cr);

    cairo_set_source_rgb(cr, 0.12, 0.56, 0.92);
    cairo_rectangle(cr, 30, 30, 250, 100);
    cairo_fill(cr);
    cairo_set_source_rgb(cr, 0.94, 0.41, 0.18);
    cairo_rectangle(cr, 30, 165, 300, 190);
    cairo_fill(cr);
    cairo_set_source_rgb(cr, 0.20, 0.72, 0.38);
    cairo_rectangle(cr, 380, 165, 250, 190);
    cairo_fill(cr);

    cairo_select_font_face(cr, "sans", CAIRO_FONT_SLANT_NORMAL,
                           CAIRO_FONT_WEIGHT_BOLD);
    cairo_set_font_size(cr, 22);
    cairo_set_source_rgb(cr, 1.0, 1.0, 1.0);
    char line[320];
    snprintf(line, sizeof(line), "clicks %u   double %u", state->click_count,
             state->double_clicks);
    draw_text(cr, 45, 72, line);
    snprintf(line, sizeof(line), "drag %.0f px", state->drag_distance);
    draw_text(cr, 45, 215, line);
    snprintf(line, sizeof(line), "scroll %u", state->scroll_events);
    draw_text(cr, 395, 215, line);
    snprintf(line, sizeof(line), "typed: %s", state->typed_text);
    draw_text(cr, 45, 405, line);
    snprintf(line, sizeof(line), "Ctrl+L: %u", state->shortcut_count);
    draw_text(cr, 395, 405, line);
    snprintf(line, sizeof(line), "input held: button=%s ctrl=%s",
             state->button_down ? "yes" : "no",
             state->control_down ? "yes" : "no");
    draw_text(cr, 45, 455, line);

    cairo_set_source_rgb(cr, 1.0, 0.9, 0.2);
    cairo_arc(cr, state->pointer_x, state->pointer_y, 8, 0, 2 * G_PI);
    cairo_fill(cr);
    (void)width;
    (void)height;
}

static void pointer_motion(GtkEventControllerMotion *controller, double x, double y,
                           gpointer data) {
    (void)controller;
    ProofState *state = data;
    state->pointer_x = x;
    state->pointer_y = y;
    write_state(state);
}

static void click_pressed(GtkGestureClick *gesture, int presses, double x, double y,
                          gpointer data) {
    (void)gesture;
    ProofState *state = data;
    state->click_count++;
    gint64 now = g_get_monotonic_time();
    if (presses == 2 ||
        (state->last_press_us != 0 && now - state->last_press_us <= 500000 &&
         fabs(x - state->last_press_x) <= 10.0 &&
         fabs(y - state->last_press_y) <= 10.0)) {
        state->double_clicks++;
    }
    state->last_press_us = now;
    state->last_press_x = x;
    state->last_press_y = y;
    state->button_down = TRUE;
    state->pointer_x = x;
    state->pointer_y = y;
    gtk_widget_grab_focus(state->canvas);
    write_state(state);
}

static void click_released(GtkGestureClick *gesture, int presses, double x, double y,
                           gpointer data) {
    (void)gesture;
    (void)presses;
    ProofState *state = data;
    state->button_down = FALSE;
    state->pointer_x = x;
    state->pointer_y = y;
    write_state(state);
}

static void drag_end(GtkGestureDrag *gesture, double offset_x, double offset_y,
                     gpointer data) {
    (void)gesture;
    ProofState *state = data;
    state->drag_distance += hypot(offset_x, offset_y);
    write_state(state);
}

static gboolean scroll_event(GtkEventControllerScroll *controller, double dx, double dy,
                             gpointer data) {
    (void)controller;
    ProofState *state = data;
    if (dx != 0.0 || dy != 0.0) {
        state->scroll_events++;
        write_state(state);
    }
    return TRUE;
}

static gboolean key_pressed(GtkEventControllerKey *controller, guint keyval, guint keycode,
                            GdkModifierType modifiers, gpointer data) {
    (void)controller;
    (void)keycode;
    ProofState *state = data;
    if (keyval == GDK_KEY_Control_L || keyval == GDK_KEY_Control_R) {
        state->control_down = TRUE;
    }
    if ((modifiers & GDK_CONTROL_MASK) != 0 &&
        (keyval == GDK_KEY_l || keyval == GDK_KEY_L)) {
        state->shortcut_count++;
    } else {
        gunichar character = gdk_keyval_to_unicode(keyval);
        if (character >= 0x20 && character <= 0x7e &&
            state->typed_len + 1 < sizeof(state->typed_text)) {
            state->typed_text[state->typed_len++] = (char)character;
            state->typed_text[state->typed_len] = '\0';
        }
    }
    write_state(state);
    return TRUE;
}

static void key_released(GtkEventControllerKey *controller, guint keyval, guint keycode,
                         GdkModifierType modifiers, gpointer data) {
    (void)controller;
    (void)keycode;
    (void)modifiers;
    ProofState *state = data;
    if (keyval == GDK_KEY_Control_L || keyval == GDK_KEY_Control_R) {
        state->control_down = FALSE;
        write_state(state);
    }
}

static void activate(GtkApplication *application, gpointer data) {
    ProofState *state = data;
    GtkWidget *window = gtk_application_window_new(application);
    gtk_window_set_title(GTK_WINDOW(window), "Brood computer-use proof");
    gtk_window_set_default_size(GTK_WINDOW(window), 700, 500);

    state->canvas = gtk_drawing_area_new();
    gtk_widget_set_focusable(state->canvas, TRUE);
    gtk_drawing_area_set_draw_func(GTK_DRAWING_AREA(state->canvas), draw_canvas, state,
                                   NULL);
    gtk_window_set_child(GTK_WINDOW(window), state->canvas);

    GtkEventController *motion = gtk_event_controller_motion_new();
    g_signal_connect(motion, "motion", G_CALLBACK(pointer_motion), state);
    gtk_widget_add_controller(state->canvas, motion);

    GtkGesture *click = gtk_gesture_click_new();
    gtk_gesture_single_set_button(GTK_GESTURE_SINGLE(click), GDK_BUTTON_PRIMARY);
    g_signal_connect(click, "pressed", G_CALLBACK(click_pressed), state);
    g_signal_connect(click, "released", G_CALLBACK(click_released), state);
    gtk_widget_add_controller(state->canvas, GTK_EVENT_CONTROLLER(click));

    GtkGesture *drag = gtk_gesture_drag_new();
    gtk_gesture_single_set_button(GTK_GESTURE_SINGLE(drag), GDK_BUTTON_PRIMARY);
    g_signal_connect(drag, "drag-end", G_CALLBACK(drag_end), state);
    gtk_widget_add_controller(state->canvas, GTK_EVENT_CONTROLLER(drag));

    GtkEventController *scroll = gtk_event_controller_scroll_new(
        GTK_EVENT_CONTROLLER_SCROLL_BOTH_AXES);
    g_signal_connect(scroll, "scroll", G_CALLBACK(scroll_event), state);
    gtk_widget_add_controller(state->canvas, scroll);

    GtkEventController *keys = gtk_event_controller_key_new();
    g_signal_connect(keys, "key-pressed", G_CALLBACK(key_pressed), state);
    g_signal_connect(keys, "key-released", G_CALLBACK(key_released), state);
    gtk_widget_add_controller(state->canvas, keys);

    gtk_window_present(GTK_WINDOW(window));
    gtk_widget_grab_focus(state->canvas);
    write_state(state);
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s STATE_JSON\n", argv[0]);
        return 2;
    }
    ProofState state = {.state_path = argv[1], .pointer_x = 650, .pointer_y = 450};
    GtkApplication *application = gtk_application_new(
        "org.brood.ComputerUseProof", G_APPLICATION_NON_UNIQUE);
    g_signal_connect(application, "activate", G_CALLBACK(activate), &state);
    int status = g_application_run(G_APPLICATION(application), 1, argv);
    g_object_unref(application);
    return status;
}
