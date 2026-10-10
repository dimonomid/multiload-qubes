/* SPDX-License-Identifier: GPL-2.0-or-later
 * GTK/XFCE adapter. All history, snapshot parsing and graph drawing live in Rust.
 */
#include <gtk/gtk.h>
#include <libxfce4panel/libxfce4panel.h>
#include <glib/gstdio.h>

typedef struct Graphs Graphs;
extern Graphs *mq_new(void);
extern void mq_free(Graphs *);
extern void mq_tick(Graphs *, int);
extern const char *mq_tooltip(const Graphs *);
extern void mq_draw(const Graphs *, cairo_t *, int, int, int, int, int, int);

typedef struct {
    XfcePanelPlugin *plugin;
    GtkWidget *area, *dialog;
    Graphs *graphs;
    guint timer;
    int width, interval;
    gboolean cpu, ram, network, demo, vertical;
} Plugin;

typedef struct {
    Plugin *owner;
    GtkWidget *width, *interval, *cpu, *ram, *network;
} Settings;

static void resize(Plugin *p) {
    int size = p->plugin ? xfce_panel_plugin_get_size(p->plugin) : 48;
    int width = p->width ? p->width : size;
    int count = p->cpu + p->ram + p->network;
    gtk_widget_set_size_request(p->area, p->vertical ? size : width * count,
                               p->vertical ? width * count : size);
}
static gboolean tick(gpointer data) {
    Plugin *p = data;
    mq_tick(p->graphs, p->demo);
    gtk_widget_set_tooltip_text(p->area, mq_tooltip(p->graphs));
    gtk_widget_queue_draw(p->area);
    return G_SOURCE_CONTINUE;
}
static void restart_timer(Plugin *p) {
    if (p->timer) g_source_remove(p->timer);
    p->timer = g_timeout_add(p->interval, tick, p);
}
static gboolean draw(GtkWidget *widget, cairo_t *cr, gpointer data) {
    Plugin *p = data;
    mq_draw(p->graphs, cr, gtk_widget_get_allocated_width(widget),
            gtk_widget_get_allocated_height(widget), p->cpu, p->ram, p->network, p->vertical);
    return FALSE;
}
static void save(XfcePanelPlugin *plugin, Plugin *p) {
    char *path = xfce_panel_plugin_save_location(plugin, TRUE);
    if (!path) return;
    GKeyFile *key = g_key_file_new();
    g_key_file_set_integer(key, "Graphs", "width", p->width);
    g_key_file_set_integer(key, "Graphs", "interval_ms", p->interval);
    g_key_file_set_boolean(key, "Graphs", "cpu", p->cpu);
    g_key_file_set_boolean(key, "Graphs", "ram", p->ram);
    g_key_file_set_boolean(key, "Graphs", "network", p->network);
    GError *error = NULL;
    if (!g_key_file_save_to_file(key, path, &error)) {
        g_warning("Cannot save Multiload settings: %s", error->message);
        g_clear_error(&error);
    }
    g_key_file_unref(key);
    g_free(path);
}
static void load(Plugin *p) {
    char *path = xfce_panel_plugin_lookup_rc_file(p->plugin);
    if (!path) return;
    GKeyFile *key = g_key_file_new();
    if (g_key_file_load_from_file(key, path, G_KEY_FILE_NONE, NULL)) {
        p->width = CLAMP(g_key_file_get_integer(key, "Graphs", "width", NULL), 0, 512);
        if (p->width > 0 && p->width < 4) p->width = 4;
        p->interval = CLAMP(g_key_file_get_integer(key, "Graphs", "interval_ms", NULL), 250, 10000);
        p->cpu = g_key_file_get_boolean(key, "Graphs", "cpu", NULL);
        p->ram = g_key_file_get_boolean(key, "Graphs", "ram", NULL);
        p->network = !g_key_file_has_key(key, "Graphs", "network", NULL) || g_key_file_get_boolean(key, "Graphs", "network", NULL);
        if (!p->cpu && !p->ram && !p->network) p->cpu = p->ram = TRUE;
    }
    g_key_file_unref(key);
    g_free(path);
}
static void free_plugin(XfcePanelPlugin *plugin, Plugin *p) {
    (void)plugin;
    if (p->dialog) gtk_widget_destroy(p->dialog);
    if (p->timer) g_source_remove(p->timer);
    mq_free(p->graphs);
    g_free(p);
}
static gboolean size_changed(XfcePanelPlugin *plugin, int size, Plugin *p) {
    (void)plugin; (void)size;
    resize(p);
    return TRUE;
}
static void orientation_changed(XfcePanelPlugin *plugin, GtkOrientation orientation, Plugin *p) {
    (void)plugin;
    p->vertical = orientation == GTK_ORIENTATION_VERTICAL;
    resize(p);
}
static void settings_closed(GtkWidget *dialog, Settings *s) {
    (void)dialog;
    s->owner->dialog = NULL;
    xfce_panel_plugin_unblock_menu(s->owner->plugin);
    g_free(s);
}
static void settings_response(GtkDialog *dialog, gint response, Settings *s) {
    Plugin *p = s->owner;
    if (response == GTK_RESPONSE_OK) {
        p->width = gtk_spin_button_get_value_as_int(GTK_SPIN_BUTTON(s->width));
        if (p->width > 0 && p->width < 4) p->width = 4;
        p->interval = gtk_spin_button_get_value_as_int(GTK_SPIN_BUTTON(s->interval));
        p->cpu = gtk_toggle_button_get_active(GTK_TOGGLE_BUTTON(s->cpu));
        p->ram = gtk_toggle_button_get_active(GTK_TOGGLE_BUTTON(s->ram));
        p->network = gtk_toggle_button_get_active(GTK_TOGGLE_BUTTON(s->network));
        if (!p->cpu && !p->ram && !p->network) p->cpu = p->ram = TRUE;
        resize(p); restart_timer(p); save(p->plugin, p);
    }
    gtk_widget_destroy(GTK_WIDGET(dialog));
}
static void configure(XfcePanelPlugin *plugin, Plugin *p) {
    if (p->dialog) { gtk_window_present(GTK_WINDOW(p->dialog)); return; }
    xfce_panel_plugin_block_menu(plugin);
    p->dialog = gtk_dialog_new_with_buttons("Multiload for Qubes", NULL, GTK_DIALOG_DESTROY_WITH_PARENT,
            "_Cancel", GTK_RESPONSE_CANCEL, "_Apply", GTK_RESPONSE_OK, NULL);
    GtkWidget *grid = gtk_grid_new();
    gtk_container_set_border_width(GTK_CONTAINER(grid), 12);
    gtk_grid_set_row_spacing(GTK_GRID(grid), 8);
    gtk_grid_set_column_spacing(GTK_GRID(grid), 12);
    GtkWidget *width = gtk_spin_button_new_with_range(0, 512, 1);
    GtkWidget *interval = gtk_spin_button_new_with_range(250, 10000, 250);
    GtkWidget *cpu = gtk_check_button_new_with_label("CPU");
    GtkWidget *ram = gtk_check_button_new_with_label("RAM");
    GtkWidget *network = gtk_check_button_new_with_label("Network");
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(width), p->width);
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(interval), p->interval);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(cpu), p->cpu);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(ram), p->ram);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(network), p->network);
    gtk_grid_attach(GTK_GRID(grid), gtk_label_new("Chart width (0 = square)"), 0, 0, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), width, 1, 0, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), gtk_label_new("Update interval (ms)"), 0, 1, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), interval, 1, 1, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), cpu, 0, 2, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), ram, 1, 2, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), network, 0, 3, 2, 1);
    gtk_grid_attach(GTK_GRID(grid), gtk_label_new("RAM: green = used; light green = allocated spare\nRed = allocation whose usage is unknown"), 0, 4, 2, 1);
    gtk_container_add(GTK_CONTAINER(gtk_dialog_get_content_area(GTK_DIALOG(p->dialog))), grid);
    Settings *s = g_new0(Settings, 1);
    *s = (Settings){p, width, interval, cpu, ram, network};
    g_object_set_data(G_OBJECT(p->dialog), "settings", s);
    g_signal_connect(p->dialog, "response", G_CALLBACK(settings_response), s);
    g_signal_connect(p->dialog, "destroy", G_CALLBACK(settings_closed), s);
    gtk_widget_show_all(p->dialog);
}
static void about(XfcePanelPlugin *plugin, Plugin *p) {
    (void)plugin; (void)p;
    gtk_show_about_dialog(NULL, "program-name", "Multiload for Qubes", "version", "0.3.0",
        "comments", "Whole-machine CPU, RAM and network graphs.\nPalette from multiload-ng.",
        "license-type", GTK_LICENSE_GPL_2_0, NULL);
}
static Plugin *create(void) {
    Plugin *p = g_new0(Plugin, 1);
    p->cpu = p->ram = p->network = TRUE; p->interval = 1000;
    p->graphs = mq_new();
    p->area = gtk_drawing_area_new();
    gtk_widget_add_events(p->area, GDK_BUTTON_PRESS_MASK);
    g_signal_connect(p->area, "draw", G_CALLBACK(draw), p);
    return p;
}
static void construct(XfcePanelPlugin *plugin) {
    Plugin *p = create();
    p->plugin = plugin;
    p->vertical = xfce_panel_plugin_get_orientation(plugin) == GTK_ORIENTATION_VERTICAL;
    load(p);
    gtk_container_add(GTK_CONTAINER(plugin), p->area);
    xfce_panel_plugin_set_small(plugin, FALSE);
    xfce_panel_plugin_add_action_widget(plugin, p->area);
    g_signal_connect(plugin, "free-data", G_CALLBACK(free_plugin), p);
    g_signal_connect(plugin, "save", G_CALLBACK(save), p);
    g_signal_connect(plugin, "size-changed", G_CALLBACK(size_changed), p);
    g_signal_connect(plugin, "orientation-changed", G_CALLBACK(orientation_changed), p);
    g_signal_connect(plugin, "configure-plugin", G_CALLBACK(configure), p);
    g_signal_connect(plugin, "about", G_CALLBACK(about), p);
    xfce_panel_plugin_menu_show_configure(plugin);
    xfce_panel_plugin_menu_show_about(plugin);
    resize(p); tick(p); restart_timer(p);
    gtk_widget_show(p->area);
}
XFCE_PANEL_PLUGIN_REGISTER(construct)

#ifdef PREVIEW
static gboolean finish(gpointer unused) { (void)unused; gtk_main_quit(); return G_SOURCE_REMOVE; }
static int self_test(void) {
    Plugin *p = create();
    p->plugin = g_object_new(XFCE_TYPE_PANEL_PLUGIN, "name", "multiload-qubes",
        "unique-id", 42, "display-name", "Multiload for Qubes", NULL);
    g_object_ref_sink(p->plugin);
    gtk_container_add(GTK_CONTAINER(p->plugin), p->area);
    resize(p);
    configure(p->plugin, p);
    Settings *s = g_object_get_data(G_OBJECT(p->dialog), "settings");
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(s->width), 64);
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(s->interval), 500);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(s->cpu), FALSE);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(s->network), FALSE);
    gtk_dialog_response(GTK_DIALOG(p->dialog), GTK_RESPONSE_OK);
    g_assert_null(p->dialog);
    g_assert_cmpint(p->width, ==, 64);
    g_assert_cmpint(p->interval, ==, 500);
    g_assert_false(p->cpu);
    g_assert_false(p->network);
    p->width = 0; p->interval = 1000; p->cpu = p->network = TRUE;
    load(p);
    g_assert_cmpint(p->width, ==, 64);
    g_assert_cmpint(p->interval, ==, 500);
    g_assert_false(p->cpu);
    g_assert_false(p->network);
    orientation_changed(p->plugin, GTK_ORIENTATION_VERTICAL, p);
    g_assert_true(p->vertical);
    configure(p->plugin, p);
    XfcePanelPlugin *plugin = p->plugin;
    free_plugin(plugin, p); /* Removing an item while Properties is open. */
    gtk_widget_destroy(GTK_WIDGET(plugin));
    g_object_unref(plugin);
    g_print("GTK settings, persistence, orientation and teardown: passed\n");
    return 0;
}
int main(int argc, char **argv) {
    if (argc == 3 && g_str_equal(argv[1], "--render")) {
        Graphs *graphs = mq_new();
        for (int i = 0; i < 400; i++) mq_tick(graphs, TRUE);
        cairo_surface_t *surface = cairo_image_surface_create(CAIRO_FORMAT_ARGB32, 480, 64);
        cairo_t *cr = cairo_create(surface);
        mq_draw(graphs, cr, 480, 64, TRUE, TRUE, TRUE, FALSE);
        cairo_status_t status = cairo_surface_write_to_png(surface, argv[2]);
        cairo_destroy(cr); cairo_surface_destroy(surface); mq_free(graphs);
        return status == CAIRO_STATUS_SUCCESS ? 0 : 1;
    }
    gtk_init(&argc, &argv);
    if (argc == 2 && g_str_equal(argv[1], "--self-test")) return self_test();
    Plugin *p = create();
    p->demo = TRUE;
    for (int i = 0; i < 160; i++) mq_tick(p->graphs, TRUE);
    GtkWidget *window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_window_set_title(GTK_WINDOW(window), "Multiload for Qubes — DEMO");
    gtk_container_add(GTK_CONTAINER(window), p->area);
    p->width = 96;
    resize(p); tick(p); restart_timer(p);
    g_signal_connect(window, "destroy", G_CALLBACK(gtk_main_quit), NULL);
    gtk_widget_show_all(window);
    if (g_getenv("MQ_PREVIEW_SECONDS")) g_timeout_add_seconds(atoi(g_getenv("MQ_PREVIEW_SECONDS")), finish, NULL);
    gtk_main();
    free_plugin(NULL, p);
    return 0;
}
#endif
