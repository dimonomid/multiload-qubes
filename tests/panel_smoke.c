/* Load the actual shared plugin using XFCE's module entry point. */
#include <gtk/gtk.h>
#include <gmodule.h>
#include <glib/gstdio.h>
#include <libxfce4panel/libxfce4panel.h>

typedef XfcePanelPlugin *(*Construct)(const gchar *, gint, const gchar *,
                                    const gchar *, gchar **, GdkScreen *);

/* The real panel unlocks its provider before allowing configuration. */
extern void xfce_panel_plugin_provider_set_locked(gpointer provider, gboolean locked);

static gboolean finish(gpointer unused) {
    (void)unused;
    gtk_main_quit();
    return G_SOURCE_REMOVE;
}

static GtkWidget *visible_menu(GtkWidget *widget) {
    if (GTK_IS_MENU(widget) && gtk_widget_get_visible(widget)) return widget;
    if (!GTK_IS_CONTAINER(widget)) return NULL;
    GList *children = gtk_container_get_children(GTK_CONTAINER(widget));
    GtkWidget *result = NULL;
    for (GList *child = children; child && !result; child = child->next)
        result = visible_menu(child->data);
    g_list_free(children);
    return result;
}
static GtkWidget *open_menu(void) {
    GList *windows = gtk_window_list_toplevels();
    GtkWidget *menu = NULL;
    for (GList *window = windows; window && !menu; window = window->next)
        menu = visible_menu(window->data);
    g_list_free(windows);
    return menu;
}
static gboolean has_label(GtkWidget *widget, const char *text) {
    if (GTK_IS_LABEL(widget) && g_strrstr(gtk_label_get_text(GTK_LABEL(widget)), text)) return TRUE;
    if (!GTK_IS_CONTAINER(widget)) return FALSE;
    GList *children = gtk_container_get_children(GTK_CONTAINER(widget));
    gboolean found = FALSE;
    for (GList *child = children; child && !found; child = child->next)
        found = has_label(child->data, text);
    g_list_free(children);
    return found;
}
/* Find the real Rust dialog; no application state is mocked for these checks. */
static GtkWidget *preferences_dialog(void) {
    GList *windows = gtk_window_list_toplevels();
    GtkWidget *dialog = NULL;
    for (GList *item = windows; item; item = item->next)
        if (GTK_IS_DIALOG(item->data) && !GTK_IS_MESSAGE_DIALOG(item->data) &&
            gtk_widget_get_visible(item->data)) dialog = item->data;
    g_list_free(windows);
    return dialog;
}
static GtkWidget *preferences_page(GtkWidget *dialog, int page) {
    GList *children = gtk_container_get_children(GTK_CONTAINER(gtk_dialog_get_content_area(GTK_DIALOG(dialog))));
    GtkWidget *result = NULL;
    for (GList *item = children; item; item = item->next)
        if (GTK_IS_NOTEBOOK(item->data)) result = gtk_notebook_get_nth_page(GTK_NOTEBOOK(item->data), page);
    g_list_free(children);
    g_assert_true(GTK_IS_GRID(result));
    return result;
}
static gboolean dismiss_save_error(gpointer data) {
    gboolean *seen = data;
    GList *windows = gtk_window_list_toplevels();
    for (GList *item = windows; item; item = item->next) {
        if (!GTK_IS_MESSAGE_DIALOG(item->data) || !gtk_widget_get_visible(item->data)) continue;
        char *text = NULL;
        g_object_get(item->data, "text", &text, NULL);
        g_assert_nonnull(g_strrstr(text, "Could not save panel settings."));
        g_free(text);
        *seen = TRUE;
        gtk_dialog_response(GTK_DIALOG(item->data), GTK_RESPONSE_CLOSE);
        g_list_free(windows);
        return G_SOURCE_REMOVE;
    }
    g_list_free(windows);
    return G_SOURCE_CONTINUE;
}
static void failed_apply(GtkWidget *dialog, const char *path, const char *backup) {
    g_assert_cmpint(g_rename(path, backup), ==, 0);
    /* A directory at the settings filename reliably rejects atomic replacement. */
    g_assert_cmpint(g_mkdir(path, 0700), ==, 0);
    gboolean seen = FALSE;
    g_idle_add(dismiss_save_error, &seen);
    gtk_dialog_response(GTK_DIALOG(dialog), GTK_RESPONSE_OK);
    g_assert_true(seen);
    g_assert_true(gtk_widget_get_visible(dialog));
    g_assert_cmpint(g_rmdir(path), ==, 0);
    g_assert_cmpint(g_rename(backup, path), ==, 0);
}
static void apply_save_failure_test(XfcePanelPlugin *plugin, const char *path) {
    GKeyFile *file = g_key_file_new();
    g_assert_true(g_key_file_load_from_file(file, path, G_KEY_FILE_NONE, NULL));
    int saved_theme = g_key_file_get_integer(file, "Appearance", "theme", NULL);
    char *backup = g_strconcat(path, ".before-failed-save", NULL);
    GtkWidget *dialog = preferences_dialog();
    g_assert_nonnull(dialog);
    g_object_ref(dialog);
    GtkWidget *width = gtk_grid_get_child_at(GTK_GRID(preferences_page(dialog, 0)), 1, 0);
    GtkWidget *color = gtk_grid_get_child_at(GTK_GRID(preferences_page(dialog, 1)), 1, 6);
    g_assert_true(GTK_IS_SPIN_BUTTON(width));
    g_assert_true(GTK_IS_COLOR_BUTTON(color));
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(width), 64);
    GdkRGBA edited = {0.2, 0.4, 0.6, 0.5};
    gtk_color_chooser_set_rgba(GTK_COLOR_CHOOSER(color), &edited);
    g_signal_emit_by_name(color, "color-set");
    failed_apply(dialog, path, backup);
    g_assert_cmpint(gtk_spin_button_get_value_as_int(GTK_SPIN_BUTTON(width)), ==, 64);
    /* Autosave still sees committed settings, not the failed candidate. */
    g_signal_emit_by_name(plugin, "save");
    g_assert_true(g_key_file_load_from_file(file, path, G_KEY_FILE_NONE, NULL));
    g_assert_cmpint(g_key_file_get_integer(file, "Graphs", "width", NULL), ==, 0);
    g_assert_cmpint(g_key_file_get_integer(file, "Appearance", "theme", NULL), ==, saved_theme);
    /* Fixing the filesystem permits Apply again, with all edits preserved. */
    gtk_dialog_response(GTK_DIALOG(dialog), GTK_RESPONSE_OK);
    g_assert_false(gtk_widget_get_visible(dialog));
    g_object_unref(dialog);
    g_assert_true(g_key_file_load_from_file(file, path, G_KEY_FILE_NONE, NULL));
    g_assert_cmpint(g_key_file_get_integer(file, "Graphs", "width", NULL), ==, 64);
    g_assert_cmpuint(g_key_file_get_uint64(file, "Appearance-0", "series1", NULL), ==, 0x33669980u);
    /* A failed subsequent Apply must still allow Cancel to discard its edits. */
    g_signal_emit_by_name(plugin, "configure-plugin");
    dialog = preferences_dialog(); g_assert_nonnull(dialog); g_object_ref(dialog);
    width = gtk_grid_get_child_at(GTK_GRID(preferences_page(dialog, 0)), 1, 0);
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(width), 96);
    failed_apply(dialog, path, backup);
    gtk_dialog_response(GTK_DIALOG(dialog), GTK_RESPONSE_CANCEL);
    g_assert_false(gtk_widget_get_visible(dialog)); g_object_unref(dialog);
    g_signal_emit_by_name(plugin, "save");
    g_assert_true(g_key_file_load_from_file(file, path, G_KEY_FILE_NONE, NULL));
    g_assert_cmpint(g_key_file_get_integer(file, "Graphs", "width", NULL), ==, 64);
    g_key_file_unref(file); g_free(backup);
    /* Leave Preferences open for the existing removal/lifecycle check. */
    g_signal_emit_by_name(plugin, "configure-plugin");
    g_assert_nonnull(preferences_dialog());
    g_print("Failed Apply: error shown, edits retained, autosave unchanged, retry and Cancel: passed\n");
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
    xfce_panel_plugin_provider_set_locked(plugin, FALSE);
    g_assert_false(xfce_panel_plugin_get_locked(plugin));
    GtkWidget *window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    gtk_window_set_default_size(GTK_WINDOW(window), 840, 64);
    gtk_container_add(GTK_CONTAINER(window), GTK_WIDGET(plugin));
    gtk_widget_show_all(window);
    g_timeout_add_seconds(2, finish, NULL);
    gtk_main();
    GList *children = gtk_container_get_children(GTK_CONTAINER(plugin));
    g_assert_cmpuint(g_list_length(children), ==, 1);
    g_assert_true(GTK_IS_DRAWING_AREA(children->data));
    g_assert_true(gtk_widget_get_has_tooltip(children->data));
    GtkWidget *area = children->data;
    g_list_free(children);
    /* Check event subscription, then dispatch a GTK right-click through the
     * actual action widget and verify the resulting XFCE menu. */
    g_assert_true(gtk_widget_get_events(area) & GDK_BUTTON_PRESS_MASK);
    GdkEvent *click = gdk_event_new(GDK_BUTTON_PRESS);
    click->button.window = g_object_ref(gtk_widget_get_window(area));
    click->button.send_event = TRUE;
    click->button.time = GDK_CURRENT_TIME;
    click->button.x = 4; click->button.y = 4; click->button.button = 3;
    gdk_event_set_device(click, gdk_seat_get_pointer(gdk_display_get_default_seat(gdk_display_get_default())));
    gtk_main_do_event(click);
    gdk_event_free(click);
    while (gtk_events_pending()) gtk_main_iteration();
    GtkWidget *menu = open_menu();
    g_assert_nonnull(menu);
    GtkWidget *properties = NULL;
    GList *items = gtk_container_get_children(GTK_CONTAINER(menu));
    for (GList *item = items; item; item = item->next) {
        if (!GTK_IS_MENU_ITEM(item->data)) continue;
        if (has_label(item->data, "Properties")) properties = item->data;
    }
    g_assert_nonnull(properties);
    g_assert_true(gtk_widget_get_sensitive(properties));
    gtk_menu_popdown(GTK_MENU(menu));
    gtk_menu_item_activate(GTK_MENU_ITEM(properties));
    g_list_free(items);
    gboolean preferences_open = FALSE;
    GList *windows = gtk_window_list_toplevels();
    for (GList *item = windows; item; item = item->next)
        if (GTK_IS_DIALOG(item->data) && gtk_widget_get_visible(item->data)) preferences_open = TRUE;
    g_list_free(windows);
    g_assert_true(preferences_open);
    /* Exercise the actual XFCE callbacks and settings path, then remove the
     * plugin while its Rust Preferences dialog is still open. */
    g_signal_emit_by_name(plugin, "save");
    char *path = xfce_panel_plugin_lookup_rc_file(plugin);
    g_assert_nonnull(path);
    GKeyFile *settings = g_key_file_new();
    g_assert_true(g_key_file_load_from_file(settings, path, G_KEY_FILE_NONE, NULL));
    GtkWidget *selector = gtk_grid_get_child_at(GTK_GRID(preferences_page(preferences_dialog(), 1)), 1, 0);
    g_assert_true(GTK_IS_COMBO_BOX_TEXT(selector));
    g_autofree char *theme_name = gtk_combo_box_text_get_active_text(GTK_COMBO_BOX_TEXT(selector));
    g_assert_cmpstr(theme_name, ==, "Light Gradient");
    g_assert_cmpint(g_key_file_get_integer(settings, "Appearance", "theme", NULL), ==,
                   g_ascii_strtoll(gtk_combo_box_get_active_id(GTK_COMBO_BOX(selector)), NULL, 10));
    g_key_file_unref(settings);
    apply_save_failure_test(plugin, path);
    g_free(path);
    /* XFCE emits free-data as part of the widget's teardown. */
    gtk_widget_destroy(window);
    g_module_close(module);
    g_print("Shared XFCE plugin: load, draw, right-click menu, Properties, save and teardown: passed\n");
    return 0;
}
