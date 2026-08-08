#define _POSIX_C_SOURCE 200809L

#include <gio/gio.h>
#include <gtk/gtk.h>
#include <string.h>

static const char *image_path;

static void drag_begin(GtkDragSource *source, GdkDrag *drag, gpointer user_data) {
    (void)source;
    (void)drag;
    (void)user_data;
    g_printerr("drag begin\n");
}

static void drag_end(
    GtkDragSource *source,
    GdkDrag *drag,
    gboolean delete_data,
    gpointer user_data
) {
    (void)source;
    (void)drag;
    (void)delete_data;
    (void)user_data;
    g_printerr("drag end\n");
}

static gboolean drag_cancel(
    GtkDragSource *source,
    GdkDrag *drag,
    GdkDragCancelReason reason,
    gpointer user_data
) {
    (void)source;
    (void)drag;
    (void)user_data;
    g_printerr("drag cancelled reason=%d\n", reason);
    return FALSE;
}

static void activate(GtkApplication *application, gpointer user_data) {
    (void)user_data;
    GtkWidget *window = gtk_application_window_new(application);
    gtk_window_set_title(GTK_WINDOW(window), "Brood image drag source");
    gtk_window_set_default_size(GTK_WINDOW(window), 240, 120);

    GtkWidget *label = gtk_label_new("Drag this image into Brood");
    gtk_widget_set_hexpand(label, TRUE);
    gtk_widget_set_vexpand(label, TRUE);
    gtk_window_set_child(GTK_WINDOW(window), label);

    GFile *file = g_file_new_for_path(image_path);
    char *uri = g_file_get_uri(file);
    char *uri_list = g_strconcat(uri, "\r\n", NULL);
    GBytes *bytes = g_bytes_new(uri_list, strlen(uri_list));
    GdkContentProvider *provider =
        gdk_content_provider_new_for_bytes("text/uri-list", bytes);
    g_bytes_unref(bytes);
    g_free(uri_list);
    g_free(uri);
    g_object_unref(file);

    GtkDragSource *source = gtk_drag_source_new();
    gtk_drag_source_set_actions(source, GDK_ACTION_COPY);
    gtk_drag_source_set_content(source, provider);
    g_signal_connect(source, "drag-begin", G_CALLBACK(drag_begin), NULL);
    g_signal_connect(source, "drag-end", G_CALLBACK(drag_end), NULL);
    g_signal_connect(source, "drag-cancel", G_CALLBACK(drag_cancel), NULL);
    g_object_unref(provider);
    gtk_widget_add_controller(label, GTK_EVENT_CONTROLLER(source));

    gtk_window_present(GTK_WINDOW(window));
}

int main(int argc, char **argv) {
    if (argc != 2) {
        g_printerr("usage: image-drag-source IMAGE\n");
        return 2;
    }
    image_path = argv[1];
    GtkApplication *application = gtk_application_new(
        "org.brood.ImageDragSource",
        G_APPLICATION_DEFAULT_FLAGS
    );
    g_signal_connect(application, "activate", G_CALLBACK(activate), NULL);
    const int status = g_application_run(G_APPLICATION(application), 1, argv);
    g_object_unref(application);
    return status;
}
