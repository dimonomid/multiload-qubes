/* SPDX-License-Identifier: GPL-2.0-or-later
 * XFCE registration and API bridge only. GTK UI and application state are Rust.
 */
#include <libxfce4panel/libxfce4panel.h>

typedef struct {
    int (*size)(XfcePanelPlugin *);
    int (*vertical)(XfcePanelPlugin *);
    void (*attach)(XfcePanelPlugin *, GtkWidget *);
    char *(*path)(XfcePanelPlugin *, int);
    void (*block)(XfcePanelPlugin *, int);
} PanelApi;
extern const void *mq_panel_new(void *, const PanelApi *);
extern void mq_panel_free(const void *);
extern void mq_panel_save(const void *);
extern void mq_panel_configure(const void *);
extern void mq_panel_resize(const void *, int);
extern void mq_panel_about(void);

static int mq_xfce_size(XfcePanelPlugin *plugin) { return xfce_panel_plugin_get_size(plugin); }
static int mq_xfce_vertical(XfcePanelPlugin *plugin) {
    return xfce_panel_plugin_get_orientation(plugin) == GTK_ORIENTATION_VERTICAL;
}
static char *mq_xfce_path(XfcePanelPlugin *plugin, int save) {
    return save ? xfce_panel_plugin_save_location(plugin, TRUE) : xfce_panel_plugin_lookup_rc_file(plugin);
}
static void mq_xfce_block(XfcePanelPlugin *plugin, int block) {
    if (block) xfce_panel_plugin_block_menu(plugin); else xfce_panel_plugin_unblock_menu(plugin);
}
static void mq_xfce_attach(XfcePanelPlugin *plugin, GtkWidget *widget) {
    gtk_container_add(GTK_CONTAINER(plugin), widget);
    xfce_panel_plugin_set_small(plugin, FALSE);
    xfce_panel_plugin_add_action_widget(plugin, widget);
    xfce_panel_plugin_menu_show_configure(plugin);
    xfce_panel_plugin_menu_show_about(plugin);
}
static void released(XfcePanelPlugin *plugin, const void *state) { (void)plugin; mq_panel_free(state); }
static void save(XfcePanelPlugin *plugin, const void *state) { (void)plugin; mq_panel_save(state); }
static void configure(XfcePanelPlugin *plugin, const void *state) { (void)plugin; mq_panel_configure(state); }
static void about(XfcePanelPlugin *plugin) { (void)plugin; mq_panel_about(); }
static gboolean size_changed(XfcePanelPlugin *plugin, int size, const void *state) {
    (void)plugin; (void)size; mq_panel_resize(state, -1); return TRUE;
}
static void orientation_changed(XfcePanelPlugin *plugin, GtkOrientation orientation, const void *state) {
    (void)plugin; mq_panel_resize(state, orientation == GTK_ORIENTATION_VERTICAL);
}
static void construct(XfcePanelPlugin *plugin) {
    static const PanelApi api = {mq_xfce_size, mq_xfce_vertical, mq_xfce_attach, mq_xfce_path, mq_xfce_block};
    const void *state = mq_panel_new(plugin, &api);
    if (!state) return;
    g_signal_connect(plugin, "free-data", G_CALLBACK(released), (void *)state);
    g_signal_connect(plugin, "save", G_CALLBACK(save), (void *)state);
    g_signal_connect(plugin, "size-changed", G_CALLBACK(size_changed), (void *)state);
    g_signal_connect(plugin, "orientation-changed", G_CALLBACK(orientation_changed), (void *)state);
    g_signal_connect(plugin, "configure-plugin", G_CALLBACK(configure), (void *)state);
    g_signal_connect(plugin, "about", G_CALLBACK(about), NULL);
}
XFCE_PANEL_PLUGIN_REGISTER(construct)
