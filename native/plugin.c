/* SPDX-License-Identifier: GPL-2.0-or-later
 * XFCE registration and API bridge only. GTK UI and application state are Rust.
 */
#include <libxfce4panel/libxfce4panel.h>

#include "panel-api.h"

static int mq_xfce_size(void *host) { return xfce_panel_plugin_get_size(XFCE_PANEL_PLUGIN(host)); }
static int mq_xfce_vertical(void *host) {
    return xfce_panel_plugin_get_orientation(XFCE_PANEL_PLUGIN(host)) == GTK_ORIENTATION_VERTICAL;
}
/* Returned filenames are GLib-owned allocations; Rust takes and frees them. */
static char *mq_xfce_path(void *host, int save) {
    XfcePanelPlugin *plugin = XFCE_PANEL_PLUGIN(host);
    return save ? xfce_panel_plugin_save_location(plugin, TRUE) : xfce_panel_plugin_lookup_rc_file(plugin);
}
static void mq_xfce_block(void *host, int block) {
    XfcePanelPlugin *plugin = XFCE_PANEL_PLUGIN(host);
    if (block) xfce_panel_plugin_block_menu(plugin); else xfce_panel_plugin_unblock_menu(plugin);
}
/* Register the drawing area for XFCE button events as well as adding it to the container. */
static void mq_xfce_attach(void *host, GtkWidget *widget) {
    XfcePanelPlugin *plugin = XFCE_PANEL_PLUGIN(host);
    gtk_container_add(GTK_CONTAINER(plugin), widget);
    xfce_panel_plugin_set_small(plugin, FALSE);
    xfce_panel_plugin_add_action_widget(plugin, widget);
    xfce_panel_plugin_menu_show_configure(plugin);
    xfce_panel_plugin_menu_show_about(plugin);
}
/* C holds one Rust reference until free-data; the other callbacks only borrow it. */
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
    /* Rust keeps a pointer to this table for the plugin's whole lifetime. */
    static const HostApi api = {
        .size = mq_xfce_size, .vertical = mq_xfce_vertical,
        .attach = mq_xfce_attach, .path = mq_xfce_path, .block = mq_xfce_block
    };
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
