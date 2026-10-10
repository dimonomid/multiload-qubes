/* Load the actual shared plugin using XFCE's module entry point. */
#include <gtk/gtk.h>
#include <gmodule.h>
#include <libxfce4panel/libxfce4panel.h>

typedef XfcePanelPlugin *(*Construct)(const gchar *, gint, const gchar *,
                                    const gchar *, gchar **, GdkScreen *);

static gboolean finish(gpointer unused) {
    (void)unused;
    gtk_main_quit();
    return G_SOURCE_REMOVE;
}

int main(int argc, char **argv) {
    gtk_init(&argc, &argv);
    g_assert_cmpint(argc, ==, 2);
    GModule *module = g_module_open(argv[1], G_MODULE_BIND_LOCAL);
    if (!module) g_error("Cannot load panel plugin: %s", g_module_error());
    Construct construct = NULL;
    g_assert_true(g_module_symbol(module, "xfce_panel_module_construct", (gpointer *)&construct));
    XfcePanelPlugin *plugin = construct("multiload-qubes", 43, "Multiload for Qubes",
                                      "CPU and RAM", NULL, gdk_screen_get_default());
    g_assert_nonnull(plugin);
    GtkWidget *window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_container_add(GTK_CONTAINER(window), GTK_WIDGET(plugin));
    gtk_widget_show_all(window);
    g_timeout_add_seconds(2, finish, NULL);
    gtk_main();
    GList *children = gtk_container_get_children(GTK_CONTAINER(plugin));
    g_assert_cmpuint(g_list_length(children), ==, 1);
    g_assert_true(GTK_IS_DRAWING_AREA(children->data));
    g_assert_true(gtk_widget_get_has_tooltip(children->data));
    g_list_free(children);
    /* XFCE emits free-data as part of the widget's teardown. */
    gtk_widget_destroy(window);
    g_module_close(module);
    g_print("Shared XFCE plugin load, realization, draw and teardown: passed\n");
    return 0;
}
