/* SPDX-License-Identifier: GPL-2.0-or-later
 * GTK/XFCE adapter. All history, snapshot parsing and graph drawing live in Rust.
 */
#include <gtk/gtk.h>
#include <libxfce4panel/libxfce4panel.h>
#include <glib/gstdio.h>
#include <stdint.h>
#include "themes.h"

typedef struct Graphs Graphs;
extern Graphs *mq_new(void);
extern void mq_free(Graphs *);
extern void mq_tick(Graphs *, int);
extern const char *mq_tooltip(Graphs *, int, int, int);
extern void mq_draw(const Graphs *, cairo_t *, int, int, int, int, int, int, int, int, int, int);

extern uint32_t mq_palette_get(const Graphs *, unsigned, unsigned);
extern void mq_palette_set(Graphs *, unsigned, unsigned, uint32_t);
extern uint32_t mq_palette_preset(unsigned, unsigned, unsigned);
static const int chart_kinds[] = {0, 6, 1, 5, 2, 3, 4};
static const char *chart_names[] = {"CPU", "Per-qube CPU", "RAM", "Per-qube RAM", "Network", "Per-qube swap", "Disk I/O"};
static const char *palette_keys[] = {"border", "background_top", "background_bottom", "series1", "series2", "series3", "missing", "border_width"};

/* Stable saved IDs; built-in values also cross the Rust palette ABI. */
typedef enum {
    THEME_ORIGINAL = 0,
    THEME_TANGO = 1,
    THEME_SOLARIZED_DARK = 2,
    THEME_SOLARIZED_LIGHT = 3,
    THEME_SOLARIZED_LIGHT_GRADIENT = 4,
    THEME_SAVED = 999,
    THEME_CUSTOM_DRAFT = 1000
} Theme;

static Theme theme_from_id(int id) {
    switch (id) {
    case THEME_ORIGINAL: case THEME_TANGO: case THEME_SOLARIZED_DARK:
    case THEME_SAVED: case THEME_SOLARIZED_LIGHT: case THEME_CUSTOM_DRAFT: case THEME_SOLARIZED_LIGHT_GRADIENT:
        return (Theme)id;
    default: return THEME_ORIGINAL;
    }
}
static void select_theme(GtkWidget *combo, Theme theme) {
    g_autoptr(GtkWidget) keep = g_object_ref(combo);
    char id[12];
    g_snprintf(id, sizeof id, "%d", theme);
    gtk_combo_box_set_active_id(GTK_COMBO_BOX(combo), id);
}

typedef struct {
    XfcePanelPlugin *plugin;
    GtkWidget *area, *dialog;
    Graphs *graphs;
    guint timer;
    int width, interval;
    Theme theme;
    char *custom_theme_id;
    gboolean cpu, qube_cpu, ram, qube_ram, network, swap, disk, demo, vertical;
} Plugin;

typedef struct {
    Plugin *owner;
    GtkWidget *width, *interval, *cpu, *qube_cpu, *ram, *qube_ram, *network, *swap, *disk;
    GtkWidget *theme, *chart, *colors[7], *labels[7], *border_width, *transparent, *preview;
    Graphs *preview_graphs;
    uint32_t original[7][8];
    Theme original_theme;
    char *original_custom_id;
    GPtrArray *saved_themes;
    GtkWidget *save_as, *save_changes, *rename_theme, *delete_theme, *theme_status;
    gboolean updating, accepted, closed;
    unsigned references;
} Settings;

static Settings *settings_ref(Settings *s) { s->references++; return s; }
static void settings_unref(Settings *s) {
    if (--s->references) return;
    mq_free(s->preview_graphs);
    g_clear_pointer(&s->saved_themes, g_ptr_array_unref);
    g_free(s->original_custom_id);
    g_free(s);
}
G_DEFINE_AUTOPTR_CLEANUP_FUNC(Settings, settings_unref)

static ThemePalette palette_snapshot(const Graphs *g) {
    ThemePalette palette;
    for (unsigned k = 0; k < 7; k++) for (unsigned f = 0; f < 8; f++)
        palette.values[k][f] = mq_palette_get(g, k, f);
    return palette;
}
static void palette_apply(Graphs *g, const ThemePalette *palette) {
    for (unsigned k = 0; k < 7; k++) for (unsigned f = 0; f < 8; f++)
        mq_palette_set(g, k, f, palette->values[k][f]);
}
static void custom_id_set(Plugin *p, const char *id) {
    char *copy = g_strdup(id);
    g_free(p->custom_theme_id); p->custom_theme_id = copy;
}
static void theme_selection_validate(Settings *s) {
    if (s->owner->theme != THEME_SAVED) return;
    SavedTheme *theme = themes_find_id(s->saved_themes, s->owner->custom_theme_id);
    ThemePalette current = palette_snapshot(s->owner->graphs);
    if (!theme || memcmp(&theme->palette, &current, sizeof current)) s->owner->theme = THEME_CUSTOM_DRAFT;
}
static const struct { Theme id; const char *name; } builtin_themes[] = {
    {THEME_ORIGINAL, "Original"}, {THEME_TANGO, "Tango"},
    {THEME_SOLARIZED_DARK, "Solarized Dark"}, {THEME_SOLARIZED_LIGHT, "Solarized Light"},
    {THEME_SOLARIZED_LIGHT_GRADIENT, "Solarized Light Gradient"}
};
static gboolean builtin_name(const char *name) {
    if (g_str_equal(name, "Custom draft")) return TRUE;
    for (unsigned i = 0; i < G_N_ELEMENTS(builtin_themes); i++)
        if (g_str_equal(name, builtin_themes[i].name)) return TRUE;
    return FALSE;
}

static void resize(Plugin *p) {
    int size = p->plugin ? xfce_panel_plugin_get_size(p->plugin) : 48;
    int width = p->width ? p->width : size;
    int count = p->cpu + p->qube_cpu + p->ram + p->qube_ram + p->network + p->swap + p->disk;
    gtk_widget_set_size_request(p->area, p->vertical ? size : width * count,
                               p->vertical ? width * count : size);
}
static gboolean tick(gpointer data) {
    Plugin *p = data;
    mq_tick(p->graphs, p->demo);
    gtk_widget_trigger_tooltip_query(p->area);
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
            gtk_widget_get_allocated_height(widget), p->cpu, p->qube_cpu, p->ram, p->qube_ram, p->network, p->swap, p->disk, p->vertical);
    return FALSE;
}
/* Match mq_draw's integer chart sizes, including any unused trailing pixels. */
static int chart_at(const Plugin *p, int width, int height, int x, int y, GdkRectangle *area) {
    int enabled[] = {p->cpu, p->qube_cpu, p->ram, p->qube_ram, p->network, p->swap, p->disk};
    int count = p->cpu + p->qube_cpu + p->ram + p->qube_ram + p->network + p->swap + p->disk;
    if (!count || x < 0 || y < 0 || x >= width || y >= height) return -1;
    int cw = p->vertical ? width : width / count;
    int ch = p->vertical ? height / count : height;
    if (cw < 4 || ch < 4) return -1;
    int index = p->vertical ? y / ch : x / cw;
    int kinds[] = {0, 6, 1, 5, 2, 3, 4};
    int slot = 0;
    for (int position = 0; position < 7; position++) {
        if (!enabled[position]) continue;
        if (slot++ == index) {
            *area = (GdkRectangle){p->vertical ? 0 : index * cw,
                p->vertical ? index * ch : 0, cw, ch};
            return kinds[position];
        }
    }
    return -1;
}
static gboolean query_tooltip(GtkWidget *widget, gint x, gint y, gboolean keyboard,
                              GtkTooltip *tooltip, gpointer data) {
    if (keyboard) return FALSE;
    Plugin *p = data;
    GdkRectangle area;
    int kind = chart_at(p, gtk_widget_get_allocated_width(widget),
                       gtk_widget_get_allocated_height(widget), x, y, &area);
    if (kind < 0) return FALSE;
    gtk_tooltip_set_text(tooltip, mq_tooltip(p->graphs, kind, area.width, area.height));
    gtk_tooltip_set_tip_area(tooltip, &area);
    return TRUE;
}
static void save(XfcePanelPlugin *plugin, Plugin *p) {
    char *path = xfce_panel_plugin_save_location(plugin, TRUE);
    if (!path) return;
    GKeyFile *key = g_key_file_new();
    g_key_file_set_integer(key, "Graphs", "width", p->width);
    g_key_file_set_integer(key, "Graphs", "interval_ms", p->interval);
    g_key_file_set_boolean(key, "Graphs", "cpu", p->cpu);
    g_key_file_set_boolean(key, "Graphs", "qube_cpu", p->qube_cpu);
    g_key_file_set_boolean(key, "Graphs", "ram", p->ram);
    g_key_file_set_boolean(key, "Graphs", "qube_ram", p->qube_ram);
    g_key_file_set_boolean(key, "Graphs", "network", p->network);
    g_key_file_set_boolean(key, "Graphs", "swap", p->swap);
    g_key_file_set_boolean(key, "Graphs", "disk", p->disk);
    Settings *editing = p->dialog ? g_object_get_data(G_OBJECT(p->dialog), "settings") : NULL;
    gboolean draft = editing && !editing->accepted;
    g_key_file_set_integer(key, "Appearance", "theme", draft ? editing->original_theme : p->theme);
    const char *custom_id = draft ? editing->original_custom_id : p->custom_theme_id;
    if (custom_id) g_key_file_set_string(key, "Appearance", "custom_theme_id", custom_id);
    for (unsigned kind = 0; kind < 7; kind++) {
        char group[32];
        g_snprintf(group, sizeof group, "Appearance-%u", kind);
        for (unsigned field = 0; field < 8; field++)
            g_key_file_set_uint64(key, group, palette_keys[field], draft ? editing->original[kind][field] : mq_palette_get(p->graphs, kind, field));
    }
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
        p->theme = theme_from_id(g_key_file_get_integer(key, "Appearance", "theme", NULL));
        g_free(p->custom_theme_id);
        p->custom_theme_id = g_key_file_get_string(key, "Appearance", "custom_theme_id", NULL);
        if (p->custom_theme_id && !g_uuid_string_is_valid(p->custom_theme_id)) g_clear_pointer(&p->custom_theme_id, g_free);
        for (unsigned kind = 0; kind < 7; kind++) {
            char group[32];
            g_snprintf(group, sizeof group, "Appearance-%u", kind);
            for (unsigned field = 0; field < 8; field++) {
                GError *error = NULL;
                guint64 value = g_key_file_get_uint64(key, group, palette_keys[field], &error);
                if (!error && value <= UINT32_MAX)
                    mq_palette_set(p->graphs, kind, field, (uint32_t)value);
                g_clear_error(&error);
            }
        }
        p->width = CLAMP(g_key_file_get_integer(key, "Graphs", "width", NULL), 0, 512);
        if (p->width > 0 && p->width < 4) p->width = 4;
        p->interval = CLAMP(g_key_file_get_integer(key, "Graphs", "interval_ms", NULL), 250, 10000);
        p->cpu = g_key_file_get_boolean(key, "Graphs", "cpu", NULL);
        p->qube_cpu = !g_key_file_has_key(key, "Graphs", "qube_cpu", NULL) || g_key_file_get_boolean(key, "Graphs", "qube_cpu", NULL);
        p->ram = g_key_file_get_boolean(key, "Graphs", "ram", NULL);
        p->qube_ram = !g_key_file_has_key(key, "Graphs", "qube_ram", NULL) || g_key_file_get_boolean(key, "Graphs", "qube_ram", NULL);
        p->network = !g_key_file_has_key(key, "Graphs", "network", NULL) || g_key_file_get_boolean(key, "Graphs", "network", NULL);
        p->swap = !g_key_file_has_key(key, "Graphs", "swap", NULL) || g_key_file_get_boolean(key, "Graphs", "swap", NULL);
        p->disk = !g_key_file_has_key(key, "Graphs", "disk", NULL) || g_key_file_get_boolean(key, "Graphs", "disk", NULL);
        if (!p->cpu && !p->qube_cpu && !p->ram && !p->qube_ram && !p->network && !p->swap && !p->disk) p->cpu = p->qube_cpu = p->ram = TRUE;
    }
    g_key_file_unref(key);
    g_free(path);
}
static void free_plugin(XfcePanelPlugin *plugin, Plugin *p) {
    (void)plugin;
    if (p->dialog) gtk_widget_destroy(p->dialog);
    if (p->timer) g_source_remove(p->timer);
    mq_free(p->graphs);
    g_free(p->custom_theme_id);
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
    if (!s->accepted) {
        for (unsigned k = 0; k < 7; k++)
            for (unsigned f = 0; f < 8; f++)
                mq_palette_set(s->owner->graphs, k, f, s->original[k][f]);
        s->owner->theme = s->original_theme;
        custom_id_set(s->owner, s->original_custom_id);
        theme_selection_validate(s);
        gtk_widget_queue_draw(s->owner->area);
    }
    s->closed = TRUE;
    s->owner->dialog = NULL;
    xfce_panel_plugin_unblock_menu(s->owner->plugin);
    settings_unref(s);
}
static void settings_response(GtkDialog *dialog, gint response, Settings *s) {
    Plugin *p = s->owner;
    if (response == GTK_RESPONSE_OK) {
        p->width = gtk_spin_button_get_value_as_int(GTK_SPIN_BUTTON(s->width));
        if (p->width > 0 && p->width < 4) p->width = 4;
        p->interval = gtk_spin_button_get_value_as_int(GTK_SPIN_BUTTON(s->interval));
        p->cpu = gtk_toggle_button_get_active(GTK_TOGGLE_BUTTON(s->cpu));
        p->qube_cpu = gtk_toggle_button_get_active(GTK_TOGGLE_BUTTON(s->qube_cpu));
        p->ram = gtk_toggle_button_get_active(GTK_TOGGLE_BUTTON(s->ram));
        p->qube_ram = gtk_toggle_button_get_active(GTK_TOGGLE_BUTTON(s->qube_ram));
        p->network = gtk_toggle_button_get_active(GTK_TOGGLE_BUTTON(s->network));
        p->swap = gtk_toggle_button_get_active(GTK_TOGGLE_BUTTON(s->swap));
        p->disk = gtk_toggle_button_get_active(GTK_TOGGLE_BUTTON(s->disk));
        if (!p->cpu && !p->qube_cpu && !p->ram && !p->qube_ram && !p->network && !p->swap && !p->disk) p->cpu = p->qube_cpu = p->ram = TRUE;
        s->accepted = TRUE;
        resize(p); restart_timer(p); save(p->plugin, p);
    }
    gtk_widget_destroy(GTK_WIDGET(dialog));
}
static GdkRGBA unpack_color(uint32_t c) {
    return (GdkRGBA){(c >> 24) / 255.0, ((c >> 16) & 255) / 255.0,
                     ((c >> 8) & 255) / 255.0, (c & 255) / 255.0};
}
static uint32_t pack_color(const GdkRGBA *c) {
    return ((uint32_t)(c->red * 255 + 0.5) << 24) |
           ((uint32_t)(c->green * 255 + 0.5) << 16) |
           ((uint32_t)(c->blue * 255 + 0.5) << 8) | (uint32_t)(c->alpha * 255 + 0.5);
}
static int selected_chart(Settings *s) {
    return chart_kinds[MAX(0, gtk_combo_box_get_active(GTK_COMBO_BOX(s->chart)))];
}
static void appearance_refresh(Settings *s) {
    s->updating = TRUE;
    int kind = selected_chart(s);
    const char *first = kind == 1 ? "Used" : kind == 2 ? "Incoming" : kind == 4 ? "Read" :
                        (kind == 3 || kind == 5 || kind == 6) ? "Base line" : "Usage";
    const char *second = kind == 1 ? "Allocated spare" : kind == 2 ? "Outgoing" : kind == 4 ? "Write" : "Maximum fill";
    gtk_label_set_text(GTK_LABEL(s->labels[3]), first);
    gtk_label_set_text(GTK_LABEL(s->labels[4]), second);
    for (unsigned f = 0; f < 7; f++) {
        GdkRGBA c = unpack_color(mq_palette_get(s->owner->graphs, kind, f));
        gtk_color_chooser_set_rgba(GTK_COLOR_CHOOSER(s->colors[f]), &c);
        gtk_color_button_set_title(GTK_COLOR_BUTTON(s->colors[f]), gtk_label_get_text(GTK_LABEL(s->labels[f])));
        gboolean visible = !(f == 4 && kind == 0) && !(f == 5 && kind != 1);
        gtk_widget_set_visible(s->colors[f], visible);
        gtk_widget_set_visible(s->labels[f], visible);
    }
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(s->border_width), mq_palette_get(s->owner->graphs, kind, 7));
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(s->transparent),
        !(mq_palette_get(s->owner->graphs, kind, 1) & 255) && !(mq_palette_get(s->owner->graphs, kind, 2) & 255));
    if (s->owner->theme == THEME_SAVED && s->owner->custom_theme_id) {
        g_autofree char *id = g_strconcat("saved:", s->owner->custom_theme_id, NULL);
        gtk_combo_box_set_active_id(GTK_COMBO_BOX(s->theme), id);
    } else select_theme(s->theme, s->owner->theme);
    SavedTheme *source = themes_find_id(s->saved_themes, s->owner->custom_theme_id);
    gtk_widget_set_sensitive(s->save_changes, s->owner->theme == THEME_CUSTOM_DRAFT && source);
    gtk_widget_set_sensitive(s->rename_theme, s->owner->theme == THEME_SAVED && source);
    gtk_widget_set_sensitive(s->delete_theme, s->owner->theme == THEME_SAVED && source);
    for (unsigned k = 0; k < 7; k++)
        for (unsigned f = 0; f < 8; f++)
            mq_palette_set(s->preview_graphs, k, f, mq_palette_get(s->owner->graphs, k, f));
    gtk_widget_queue_draw(s->preview);
    gtk_widget_queue_draw(s->owner->area);
    s->updating = FALSE;
}
static void chart_selected(GtkComboBox *combo, Settings *s) {
    (void)combo;
    if (!s->updating) appearance_refresh(s);
}
static void color_changed(GtkColorButton *button, Settings *s) {
    if (s->updating) return;
    for (unsigned f = 0; f < 7; f++) if (s->colors[f] == GTK_WIDGET(button)) {
        GdkRGBA c;
        gtk_color_chooser_get_rgba(GTK_COLOR_CHOOSER(button), &c);
        mq_palette_set(s->owner->graphs, selected_chart(s), f, pack_color(&c));
    }
    s->owner->theme = THEME_CUSTOM_DRAFT;
    appearance_refresh(s);
}
static void border_changed(GtkSpinButton *button, Settings *s) {
    if (s->updating) return;
    mq_palette_set(s->owner->graphs, selected_chart(s), 7, gtk_spin_button_get_value_as_int(button));
    s->owner->theme = THEME_CUSTOM_DRAFT;
    appearance_refresh(s);
}
static void transparency_changed(GtkToggleButton *button, Settings *s) {
    if (s->updating) return;
    int kind = selected_chart(s);
    for (unsigned f = 1; f <= 2; f++) {
        uint32_t c = mq_palette_get(s->owner->graphs, kind, f);
        mq_palette_set(s->owner->graphs, kind, f, (c & ~255u) | (gtk_toggle_button_get_active(button) ? 0 : 255));
    }
    s->owner->theme = THEME_CUSTOM_DRAFT;
    appearance_refresh(s);
}
/* Callers hold a Settings reference across nested modal loops: removing the panel
 * item destroys these dialogs, but their callbacks must not touch a freed owner. */
static gboolean confirm(Settings *s, const char *text, const char *cancel, const char *accept) {
    GtkWidget *dialog = gtk_message_dialog_new(GTK_WINDOW(s->owner->dialog),
        GTK_DIALOG_MODAL | GTK_DIALOG_DESTROY_WITH_PARENT, GTK_MESSAGE_QUESTION, GTK_BUTTONS_NONE, "%s", text);
    gtk_dialog_add_buttons(GTK_DIALOG(dialog), cancel, GTK_RESPONSE_CANCEL, accept, GTK_RESPONSE_ACCEPT, NULL);
    gtk_dialog_set_default_response(GTK_DIALOG(dialog), GTK_RESPONSE_CANCEL);
    g_object_ref_sink(dialog);
    gint response = gtk_dialog_run(GTK_DIALOG(dialog));
    gtk_widget_destroy(dialog);
    g_object_unref(dialog);
    return !s->closed && response == GTK_RESPONSE_ACCEPT;
}
static void theme_error(Settings *s, const char *text) {
    if (s->closed) return;
    GtkWidget *dialog = gtk_message_dialog_new(GTK_WINDOW(s->owner->dialog),
        GTK_DIALOG_MODAL | GTK_DIALOG_DESTROY_WITH_PARENT, GTK_MESSAGE_ERROR, GTK_BUTTONS_CLOSE, "%s", text);
    g_object_ref_sink(dialog);
    gtk_dialog_run(GTK_DIALOG(dialog));
    gtk_widget_destroy(dialog);
    g_object_unref(dialog);
}
static gboolean discard_draft(Settings *s) {
    return s->owner->theme != THEME_CUSTOM_DRAFT ||
        confirm(s, "Your changes aren't saved. Discard them?", "Keep editing", "Discard");
}
static void theme_list_refresh(Settings *s) {
    s->updating = TRUE;
    gtk_combo_box_text_remove_all(GTK_COMBO_BOX_TEXT(s->theme));
    for (unsigned i = 0; i < G_N_ELEMENTS(builtin_themes); i++) {
        char id[12];
        g_snprintf(id, sizeof id, "%d", builtin_themes[i].id);
        gtk_combo_box_text_append(GTK_COMBO_BOX_TEXT(s->theme), id, builtin_themes[i].name);
    }
    for (guint i = 0; i < s->saved_themes->len; i++) {
        SavedTheme *t = g_ptr_array_index(s->saved_themes, i);
        if (builtin_name(t->name)) continue;
        g_autofree char *id = g_strconcat("saved:", t->id, NULL);
        gtk_combo_box_text_append(GTK_COMBO_BOX_TEXT(s->theme), id, t->name);
    }
    char draft[12];
    g_snprintf(draft, sizeof draft, "%d", THEME_CUSTOM_DRAFT);
    gtk_combo_box_text_append(GTK_COMBO_BOX_TEXT(s->theme), draft, "Custom draft");
    s->updating = FALSE;
}
static void theme_selected(GtkComboBox *combo, Settings *s) {
    const char *selected = gtk_combo_box_get_active_id(combo);
    if (s->updating || !selected) return;
    g_autoptr(Settings) keep = settings_ref(s);
    g_autofree char *id = g_strdup(selected);
    if (!g_str_has_prefix(id, "saved:") && theme_from_id((int)g_ascii_strtoll(id, NULL, 10)) == THEME_CUSTOM_DRAFT) { s->owner->theme = THEME_CUSTOM_DRAFT; appearance_refresh(s); return; }
    if (!discard_draft(s)) { if (!s->closed) appearance_refresh(s); return; }
    if (g_str_has_prefix(id, "saved:")) {
        SavedTheme *theme = themes_find_id(s->saved_themes, id + strlen("saved:"));
        if (!theme) { appearance_refresh(s); return; }
        palette_apply(s->owner->graphs, &theme->palette);
        custom_id_set(s->owner, theme->id);
        s->owner->theme = THEME_SAVED;
    } else {
        Theme theme = theme_from_id((int)g_ascii_strtoll(id, NULL, 10));
        custom_id_set(s->owner, NULL);
        s->owner->theme = theme;
        for (unsigned k = 0; k < 7; k++) for (unsigned f = 0; f < 8; f++)
            mq_palette_set(s->owner->graphs, k, f, mq_palette_preset(theme, k, f));
    }
    appearance_refresh(s);
}
static void reset_appearance(GtkButton *button, Settings *s) {
    (void)button;
    g_autoptr(Settings) keep = settings_ref(s);
    if (!discard_draft(s)) return;
    s->owner->theme = THEME_ORIGINAL;
    custom_id_set(s->owner, NULL);
    for (unsigned k = 0; k < 7; k++) for (unsigned f = 0; f < 8; f++)
        mq_palette_set(s->owner->graphs, k, f, mq_palette_preset(THEME_ORIGINAL, k, f));
    appearance_refresh(s);
}
static char *theme_name_prompt(Settings *s, const char *title, const char *initial, gboolean choices) {
    GtkWidget *dialog = gtk_dialog_new_with_buttons(title, GTK_WINDOW(s->owner->dialog),
        GTK_DIALOG_MODAL | GTK_DIALOG_DESTROY_WITH_PARENT, "Cancel", GTK_RESPONSE_CANCEL, "Save", GTK_RESPONSE_ACCEPT, NULL);
    GtkWidget *box = gtk_dialog_get_content_area(GTK_DIALOG(dialog));
    gtk_container_set_border_width(GTK_CONTAINER(box), 12);
    GtkWidget *combo = gtk_combo_box_text_new_with_entry();
    if (choices) for (guint i = 0; i < s->saved_themes->len; i++) {
        SavedTheme *theme = g_ptr_array_index(s->saved_themes, i);
        if (!builtin_name(theme->name)) gtk_combo_box_text_append_text(GTK_COMBO_BOX_TEXT(combo), theme->name);
    }
    GtkWidget *entry = gtk_bin_get_child(GTK_BIN(combo));
    gtk_entry_set_max_length(GTK_ENTRY(entry), 128);
    gtk_entry_set_text(GTK_ENTRY(entry), initial ? initial : "");
    gtk_entry_set_activates_default(GTK_ENTRY(entry), TRUE);
    gtk_dialog_set_default_response(GTK_DIALOG(dialog), GTK_RESPONSE_ACCEPT);
    gtk_container_add(GTK_CONTAINER(box), gtk_label_new("Theme name"));
    gtk_container_add(GTK_CONTAINER(box), combo);
    g_object_set_data(G_OBJECT(dialog), "theme-name-entry", entry);
    gtk_widget_show_all(dialog);
    char *name = NULL;
    g_object_ref_sink(dialog);
    if (gtk_dialog_run(GTK_DIALOG(dialog)) == GTK_RESPONSE_ACCEPT && !s->closed)
        name = g_strdup(gtk_entry_get_text(GTK_ENTRY(entry)));
    gtk_widget_destroy(dialog);
    g_object_unref(dialog);
    if (!name) return NULL;
    g_strstrip(name);
    if (!theme_name_valid(name) || builtin_name(name)) {
        theme_error(s, builtin_name(name) ? "Built-in theme names and Custom draft are reserved." :
                    "Enter a theme name of 1–128 characters without control characters.");
        g_free(name); return NULL;
    }
    return name;
}
static gboolean write_palette(Settings *s, const char *id, const char *name, const ThemePalette *palette) {
    SavedTheme theme = {(char *)id, (char *)name, *palette};
    g_autoptr(GError) error = NULL;
    if (!theme_write(&theme, &error)) { theme_error(s, error->message); return FALSE; }
    SavedTheme *existing = themes_find_id(s->saved_themes, id);
    if (existing) { g_free(existing->name); existing->name = g_strdup(name); existing->palette = *palette; }
    else {
        SavedTheme *copy = g_new0(SavedTheme, 1);
        *copy = (SavedTheme){g_strdup(id), g_strdup(name), *palette};
        g_ptr_array_add(s->saved_themes, copy);
    }
    return TRUE;
}
static gboolean overwrite_confirm(Settings *s, const char *name) {
    g_autofree char *question = g_strdup_printf("Overwrite theme ‘%s’?", name);
    return confirm(s, question, "Cancel", "Overwrite");
}
static void save_as_theme(GtkButton *button, Settings *s) {
    (void)button;
    g_autoptr(Settings) keep = settings_ref(s);
    SavedTheme *source = themes_find_id(s->saved_themes, s->owner->custom_theme_id);
    g_autofree char *name = theme_name_prompt(s, "Save theme as…", source ? source->name : NULL, TRUE);
    if (!name || s->closed) return;
    SavedTheme *existing = themes_find_name(s->saved_themes, name);
    if (existing && !overwrite_confirm(s, name)) return;
    g_autofree char *id = existing ? g_strdup(existing->id) : g_uuid_string_random();
    ThemePalette palette = palette_snapshot(s->owner->graphs);
    if (!write_palette(s, id, name, &palette)) return;
    custom_id_set(s->owner, id);
    s->owner->theme = THEME_SAVED;
    theme_list_refresh(s); appearance_refresh(s);
}
static void save_theme_changes(GtkButton *button, Settings *s) {
    (void)button;
    g_autoptr(Settings) keep = settings_ref(s);
    SavedTheme *theme = themes_find_id(s->saved_themes, s->owner->custom_theme_id);
    if (!theme || s->owner->theme != THEME_CUSTOM_DRAFT || !overwrite_confirm(s, theme->name)) return;
    g_autofree char *id = g_strdup(theme->id), *name = g_strdup(theme->name);
    ThemePalette palette = palette_snapshot(s->owner->graphs);
    if (!write_palette(s, id, name, &palette)) return;
    s->owner->theme = THEME_SAVED;
    theme_list_refresh(s); appearance_refresh(s);
}
static void rename_saved_theme(GtkButton *button, Settings *s) {
    (void)button;
    g_autoptr(Settings) keep = settings_ref(s);
    SavedTheme *theme = themes_find_id(s->saved_themes, s->owner->custom_theme_id);
    if (!theme || s->owner->theme != THEME_SAVED) return;
    g_autofree char *name = theme_name_prompt(s, "Rename theme", theme->name, FALSE);
    if (!name || s->closed || g_str_equal(name, theme->name)) return;
    if (themes_find_name(s->saved_themes, name)) { theme_error(s, "A theme with that name already exists."); return; }
    if (!write_palette(s, theme->id, name, &theme->palette)) return;
    theme_list_refresh(s); appearance_refresh(s);
}
static void delete_saved_theme(GtkButton *button, Settings *s) {
    (void)button;
    g_autoptr(Settings) keep = settings_ref(s);
    SavedTheme *theme = themes_find_id(s->saved_themes, s->owner->custom_theme_id);
    if (!theme || s->owner->theme != THEME_SAVED) return;
    g_autofree char *question = g_strdup_printf("Delete theme ‘%s’?", theme->name);
    if (!confirm(s, question, "Cancel", "Delete")) return;
    g_autoptr(GError) error = NULL;
    if (!theme_delete(theme->id, &error)) { theme_error(s, error->message); return; }
    g_ptr_array_remove(s->saved_themes, theme);
    custom_id_set(s->owner, NULL);
    s->owner->theme = THEME_CUSTOM_DRAFT; /* Keep the currently displayed colors. */
    theme_list_refresh(s); appearance_refresh(s);
}
static gboolean preview_draw(GtkWidget *widget, cairo_t *cr, Settings *s) {
    int w = gtk_widget_get_allocated_width(widget), h = gtk_widget_get_allocated_height(widget);
    /* Checkerboard makes transparency visible even on a dark desktop theme. */
    for (int y = 0; y < h; y += 8) for (int x = 0; x < w; x += 8) {
        double shade = ((x / 8 + y / 8) % 2) ? 0.75 : 0.9;
        cairo_set_source_rgb(cr, shade, shade, shade);
        cairo_rectangle(cr, x, y, 8, 8); cairo_fill(cr);
    }
    int k = selected_chart(s);
    mq_draw(s->preview_graphs, cr, w, h, k == 0, k == 6, k == 1, k == 5, k == 2, k == 3, k == 4, FALSE);
    return FALSE;
}
static GtkWidget *appearance_page(Settings *s) {
    GtkWidget *grid = gtk_grid_new();
    gtk_container_set_border_width(GTK_CONTAINER(grid), 12);
    gtk_grid_set_row_spacing(GTK_GRID(grid), 6);
    gtk_grid_set_column_spacing(GTK_GRID(grid), 12);
    s->theme = gtk_combo_box_text_new();
    theme_list_refresh(s);
    s->chart = gtk_combo_box_text_new();
    for (unsigned i = 0; i < G_N_ELEMENTS(chart_names); i++)
        gtk_combo_box_text_append_text(GTK_COMBO_BOX_TEXT(s->chart), chart_names[i]);
    gtk_combo_box_set_active(GTK_COMBO_BOX(s->chart), 0);
    gtk_grid_attach(GTK_GRID(grid), gtk_label_new("Theme (applies to all charts)"), 0, 0, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), s->theme, 1, 0, 2, 1);
    gtk_grid_attach(GTK_GRID(grid), gtk_label_new("Chart"), 0, 2, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), s->chart, 1, 2, 2, 1);
    const char *labels[] = {"Border", "Background top", "Background bottom", "Usage", "", "Unknown usage", "Missing data"};
    for (unsigned f = 0; f < 7; f++) {
        s->labels[f] = gtk_label_new(labels[f]);
        gtk_widget_set_halign(s->labels[f], GTK_ALIGN_START);
        s->colors[f] = gtk_color_button_new();
        gtk_color_chooser_set_use_alpha(GTK_COLOR_CHOOSER(s->colors[f]), TRUE);
        gtk_grid_attach(GTK_GRID(grid), s->labels[f], 0, 3 + f, 1, 1);
        gtk_grid_attach(GTK_GRID(grid), s->colors[f], 1, 3 + f, 1, 1);
        g_signal_connect(s->colors[f], "color-set", G_CALLBACK(color_changed), s);
    }
    s->border_width = gtk_spin_button_new_with_range(0, 16, 1);
    GtkWidget *border_box = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 6);
    gtk_box_pack_start(GTK_BOX(border_box), s->border_width, FALSE, FALSE, 0);
    gtk_box_pack_start(GTK_BOX(border_box), gtk_label_new("px (0 = none)"), FALSE, FALSE, 0);
    gtk_grid_attach(GTK_GRID(grid), border_box, 2, 3, 1, 1);
    s->transparent = gtk_check_button_new_with_label("Transparent background");
    gtk_grid_attach(GTK_GRID(grid), s->transparent, 0, 11, 3, 1);
    s->preview = gtk_drawing_area_new();
    gtk_widget_set_size_request(s->preview, 320, 64);
    gtk_grid_attach(GTK_GRID(grid), s->preview, 0, 12, 3, 1);
    GtkWidget *reset = gtk_button_new_with_label("Reset all appearance settings");
    gtk_grid_attach(GTK_GRID(grid), reset, 0, 13, 3, 1);
    GtkWidget *actions = gtk_box_new(GTK_ORIENTATION_HORIZONTAL, 6);
    s->save_as = gtk_button_new_with_label("Save as…");
    s->save_changes = gtk_button_new_with_label("Save changes");
    s->rename_theme = gtk_button_new_with_label("Rename…");
    s->delete_theme = gtk_button_new_with_label("Delete…");
    GtkWidget *buttons[] = {s->save_as, s->save_changes, s->rename_theme, s->delete_theme};
    for (unsigned i = 0; i < G_N_ELEMENTS(buttons); i++) gtk_box_pack_start(GTK_BOX(actions), buttons[i], FALSE, FALSE, 0);
    gtk_grid_attach(GTK_GRID(grid), actions, 0, 1, 3, 1);
    s->theme_status = gtk_label_new("Theme files are saved immediately. Apply saves the panel appearance.");
    gtk_label_set_line_wrap(GTK_LABEL(s->theme_status), TRUE);
    gtk_grid_attach(GTK_GRID(grid), s->theme_status, 0, 14, 3, 1);
    g_signal_connect(s->save_as, "clicked", G_CALLBACK(save_as_theme), s);
    g_signal_connect(s->save_changes, "clicked", G_CALLBACK(save_theme_changes), s);
    g_signal_connect(s->rename_theme, "clicked", G_CALLBACK(rename_saved_theme), s);
    g_signal_connect(s->delete_theme, "clicked", G_CALLBACK(delete_saved_theme), s);
    g_signal_connect(s->theme, "changed", G_CALLBACK(theme_selected), s);
    g_signal_connect(s->chart, "changed", G_CALLBACK(chart_selected), s);
    g_signal_connect(s->border_width, "value-changed", G_CALLBACK(border_changed), s);
    g_signal_connect(s->transparent, "toggled", G_CALLBACK(transparency_changed), s);
    g_signal_connect(s->preview, "draw", G_CALLBACK(preview_draw), s);
    g_signal_connect(reset, "clicked", G_CALLBACK(reset_appearance), s);
    return grid;
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
    GtkWidget *qube_cpu = gtk_check_button_new_with_label("Per-qube CPU");
    GtkWidget *ram = gtk_check_button_new_with_label("RAM");
    GtkWidget *qube_ram = gtk_check_button_new_with_label("Per-qube RAM");
    GtkWidget *network = gtk_check_button_new_with_label("Network");
    GtkWidget *swap = gtk_check_button_new_with_label("Per-qube swap");
    GtkWidget *disk = gtk_check_button_new_with_label("Disk I/O");
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(width), p->width);
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(interval), p->interval);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(cpu), p->cpu);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(qube_cpu), p->qube_cpu);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(ram), p->ram);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(qube_ram), p->qube_ram);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(network), p->network);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(swap), p->swap);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(disk), p->disk);
    gtk_grid_attach(GTK_GRID(grid), gtk_label_new("Chart width (0 = square)"), 0, 0, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), width, 1, 0, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), gtk_label_new("Update interval (ms)"), 0, 1, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), interval, 1, 1, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), cpu, 0, 2, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), qube_cpu, 1, 2, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), ram, 0, 3, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), qube_ram, 1, 3, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), network, 0, 4, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), swap, 1, 4, 1, 1);
    gtk_grid_attach(GTK_GRID(grid), disk, 0, 5, 2, 1);
    gtk_grid_attach(GTK_GRID(grid), gtk_label_new("Per-qube charts: 100% = one domain's capacity.\nColors can be changed in Appearance."), 0, 6, 2, 1);
    Settings *s = g_new0(Settings, 1);
    *s = (Settings){.owner=p, .width=width, .interval=interval, .cpu=cpu, .qube_cpu=qube_cpu,
        .ram=ram, .qube_ram=qube_ram, .network=network, .swap=swap, .disk=disk};
    s->references = 1;
    guint invalid;
    g_autoptr(GError) theme_load_error = NULL;
    s->saved_themes = themes_load(&invalid, &theme_load_error);
    for (guint i = 0; i < s->saved_themes->len;) {
        SavedTheme *theme = g_ptr_array_index(s->saved_themes, i);
        if (builtin_name(theme->name)) { g_ptr_array_remove_index(s->saved_themes, i); invalid++; }
        else i++;
    }
    theme_selection_validate(s);
    s->original_theme = p->theme;
    s->original_custom_id = g_strdup(p->custom_theme_id);
    for (unsigned k = 0; k < 7; k++)
        for (unsigned f = 0; f < 8; f++)
            s->original[k][f] = mq_palette_get(p->graphs, k, f);
    s->preview_graphs = mq_new();
    for (int i = 0; i < 400; i++) mq_tick(s->preview_graphs, TRUE);
    GtkWidget *notebook = gtk_notebook_new();
    gtk_notebook_append_page(GTK_NOTEBOOK(notebook), grid, gtk_label_new("General"));
    gtk_notebook_append_page(GTK_NOTEBOOK(notebook), appearance_page(s), gtk_label_new("Appearance"));
    gtk_container_add(GTK_CONTAINER(gtk_dialog_get_content_area(GTK_DIALOG(p->dialog))), notebook);
    g_object_set_data(G_OBJECT(p->dialog), "settings", s);
    g_signal_connect(p->dialog, "response", G_CALLBACK(settings_response), s);
    g_signal_connect(p->dialog, "destroy", G_CALLBACK(settings_closed), s);
    gtk_widget_show_all(p->dialog);
    appearance_refresh(s);
    if (theme_load_error || invalid)
        gtk_label_set_text(GTK_LABEL(s->theme_status), theme_load_error ? theme_load_error->message :
            "Some theme files could not be loaded (invalid format or duplicate name).");
}
static void about(XfcePanelPlugin *plugin, Plugin *p) {
    (void)plugin; (void)p;
    gtk_show_about_dialog(NULL, "program-name", "Multiload for Qubes", "version", "0.10.0",
        "comments", "CPU, RAM, network, swap and disk I/O graphs.\nPalette from multiload-ng.",
        "license-type", GTK_LICENSE_GPL_2_0, NULL);
}
static Plugin *create(void) {
    Plugin *p = g_new0(Plugin, 1);
    p->cpu = p->qube_cpu = p->ram = p->qube_ram = p->network = p->swap = p->disk = TRUE; p->interval = 1000;
    p->graphs = mq_new();
    p->area = gtk_drawing_area_new();
    gtk_widget_add_events(p->area, GDK_BUTTON_PRESS_MASK);
    gtk_widget_set_has_tooltip(p->area, TRUE);
    g_signal_connect(p->area, "draw", G_CALLBACK(draw), p);
    g_signal_connect(p->area, "query-tooltip", G_CALLBACK(query_tooltip), p);
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
typedef struct { const char *text, *name; gint response; } TestPrompt;
typedef struct { const TestPrompt *steps; unsigned count, next; } TestPrompts;
static gboolean answer_prompt(gpointer data) {
    TestPrompts *prompts = data;
    GList *windows = gtk_window_list_toplevels();
    for (GList *item = windows; item; item = item->next) {
        GtkWidget *window = item->data;
        if (!GTK_IS_DIALOG(window) || !gtk_window_get_modal(GTK_WINDOW(window)) || !gtk_widget_get_visible(window)) continue;
        const TestPrompt *step = &prompts->steps[prompts->next];
        GtkWidget *entry = g_object_get_data(G_OBJECT(window), "theme-name-entry");
        if (step->name) {
            if (!entry) continue;
            gtk_entry_set_text(GTK_ENTRY(entry), step->name);
        } else {
            if (!GTK_IS_MESSAGE_DIALOG(window)) continue;
            char *text = NULL;
            g_object_get(window, "text", &text, NULL);
            g_assert_nonnull(strstr(text, step->text));
            g_free(text);
        }
        prompts->next++;
        gtk_dialog_response(GTK_DIALOG(window), step->response);
        g_list_free(windows);
        return prompts->next < prompts->count;
    }
    g_list_free(windows);
    return G_SOURCE_CONTINUE;
}
static void queue_prompts(TestPrompts *prompts, const TestPrompt *steps, unsigned count) {
    *prompts = (TestPrompts){steps, count, 0};
    g_idle_add(answer_prompt, prompts);
}
static void named_theme_test(void) {
    Plugin *p = create();
    p->plugin = g_object_new(XFCE_TYPE_PANEL_PLUGIN, "name", "multiload-qubes", "unique-id", 44, NULL);
    g_object_ref_sink(p->plugin);
    gtk_container_add(GTK_CONTAINER(p->plugin), p->area);
    configure(p->plugin, p);
    Settings *s = g_object_get_data(G_OBJECT(p->dialog), "settings");
    TestPrompts prompts;
    const TestPrompt keep_editing[] = {{"Your changes aren't saved. Discard them?", NULL, GTK_RESPONSE_CANCEL}};
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(s->border_width), 2);
    queue_prompts(&prompts, keep_editing, G_N_ELEMENTS(keep_editing));
    select_theme(s->theme, THEME_TANGO);
    g_assert_cmpuint(prompts.next, ==, prompts.count);
    g_assert_cmpint(p->theme, ==, THEME_CUSTOM_DRAFT);
    g_assert_cmpuint(mq_palette_get(p->graphs, 0, 7), ==, 2);
    const TestPrompt save_first[] = {{NULL, "Cream / Ω", GTK_RESPONSE_ACCEPT}};
    queue_prompts(&prompts, save_first, G_N_ELEMENTS(save_first));
    save_as_theme(NULL, s);
    g_assert_cmpint(p->theme, ==, THEME_SAVED);
    g_assert_cmpuint(s->saved_themes->len, ==, 1);
    g_autofree char *first_id = g_strdup(p->custom_theme_id);
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(s->border_width), 3);
    g_assert_cmpuint(themes_find_id(s->saved_themes, first_id)->palette.values[0][7], ==, 2);
    const TestPrompt reject_overwrite[] = {{NULL, "Cream / Ω", GTK_RESPONSE_ACCEPT}, {"Overwrite theme ‘Cream / Ω’?", NULL, GTK_RESPONSE_CANCEL}};
    queue_prompts(&prompts, reject_overwrite, G_N_ELEMENTS(reject_overwrite));
    save_as_theme(NULL, s);
    g_assert_cmpuint(prompts.next, ==, prompts.count);
    g_assert_cmpint(p->theme, ==, THEME_CUSTOM_DRAFT);
    g_assert_cmpuint(themes_find_id(s->saved_themes, first_id)->palette.values[0][7], ==, 2);
    const TestPrompt overwrite[] = {{NULL, "Cream / Ω", GTK_RESPONSE_ACCEPT}, {"Overwrite theme ‘Cream / Ω’?", NULL, GTK_RESPONSE_ACCEPT}};
    queue_prompts(&prompts, overwrite, G_N_ELEMENTS(overwrite));
    save_as_theme(NULL, s);
    g_assert_cmpuint(prompts.next, ==, prompts.count);
    g_assert_cmpint(p->theme, ==, THEME_SAVED);
    g_assert_cmpstr(p->custom_theme_id, ==, first_id);
    g_assert_cmpuint(s->saved_themes->len, ==, 1);
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(s->border_width), 4);
    const TestPrompt update[] = {{"Overwrite theme ‘Cream / Ω’?", NULL, GTK_RESPONSE_ACCEPT}};
    queue_prompts(&prompts, update, G_N_ELEMENTS(update));
    save_theme_changes(NULL, s);
    g_assert_cmpint(p->theme, ==, THEME_SAVED);
    g_assert_cmpuint(themes_find_id(s->saved_themes, first_id)->palette.values[0][7], ==, 4);
    const TestPrompt rename_steps[] = {{NULL, "Renamed cream", GTK_RESPONSE_ACCEPT}};
    queue_prompts(&prompts, rename_steps, G_N_ELEMENTS(rename_steps));
    rename_saved_theme(NULL, s);
    g_assert_nonnull(themes_find_name(s->saved_themes, "Renamed cream"));
    const TestPrompt reserved[] = {{NULL, "Original", GTK_RESPONSE_ACCEPT}, {"reserved", NULL, GTK_RESPONSE_CLOSE}};
    queue_prompts(&prompts, reserved, G_N_ELEMENTS(reserved));
    save_as_theme(NULL, s);
    g_assert_cmpuint(prompts.next, ==, prompts.count);
    g_assert_cmpuint(s->saved_themes->len, ==, 1);
    /* A filesystem failure keeps both the editable draft and the saved palette. */
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(s->border_width), 5);
    g_autofree char *dir = g_build_filename(g_get_user_config_dir(), "multiload-qubes", "themes", NULL);
    g_autofree char *backup = g_strconcat(dir, ".backup", NULL);
    g_assert_cmpint(g_rename(dir, backup), ==, 0);
    g_assert_true(g_file_set_contents(dir, "blocked", -1, NULL));
    const TestPrompt failed[] = {{"Overwrite theme", NULL, GTK_RESPONSE_ACCEPT}, {"Cannot create theme directory", NULL, GTK_RESPONSE_CLOSE}};
    queue_prompts(&prompts, failed, G_N_ELEMENTS(failed));
    save_theme_changes(NULL, s);
    g_assert_cmpuint(prompts.next, ==, prompts.count);
    g_assert_cmpint(p->theme, ==, THEME_CUSTOM_DRAFT);
    g_assert_cmpuint(mq_palette_get(p->graphs, 0, 7), ==, 5);
    g_assert_cmpuint(themes_find_id(s->saved_themes, first_id)->palette.values[0][7], ==, 4);
    g_assert_cmpint(g_unlink(dir), ==, 0); g_assert_cmpint(g_rename(backup, dir), ==, 0);
    const TestPrompt save_second[] = {{NULL, "Second", GTK_RESPONSE_ACCEPT}};
    queue_prompts(&prompts, save_second, G_N_ELEMENTS(save_second));
    save_as_theme(NULL, s);
    g_assert_cmpuint(s->saved_themes->len, ==, 2);
    const TestPrompt delete[] = {{"Delete theme ‘Second’?", NULL, GTK_RESPONSE_ACCEPT}};
    queue_prompts(&prompts, delete, G_N_ELEMENTS(delete));
    delete_saved_theme(NULL, s);
    g_assert_cmpuint(s->saved_themes->len, ==, 1);
    g_assert_cmpint(p->theme, ==, THEME_CUSTOM_DRAFT);
    g_assert_cmpuint(mq_palette_get(p->graphs, 0, 7), ==, 5);
    const TestPrompt discard[] = {{"Your changes aren't saved. Discard them?", NULL, GTK_RESPONSE_ACCEPT}};
    queue_prompts(&prompts, discard, G_N_ELEMENTS(discard));
    g_autofree char *saved_id = g_strconcat("saved:", first_id, NULL);
    gtk_combo_box_set_active_id(GTK_COMBO_BOX(s->theme), saved_id);
    g_assert_cmpint(p->theme, ==, THEME_SAVED);
    g_assert_cmpuint(mq_palette_get(p->graphs, 0, 7), ==, 4);
    gtk_dialog_response(GTK_DIALOG(p->dialog), GTK_RESPONSE_OK);
    load(p);
    g_assert_cmpint(p->theme, ==, THEME_SAVED);
    g_assert_cmpstr(p->custom_theme_id, ==, first_id);
    configure(p->plugin, p);
    s = g_object_get_data(G_OBJECT(p->dialog), "settings");
    g_assert_cmpint(p->theme, ==, THEME_SAVED);
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(s->border_width), 6);
    gtk_dialog_response(GTK_DIALOG(p->dialog), GTK_RESPONSE_OK);
    load(p);
    g_assert_cmpint(p->theme, ==, THEME_CUSTOM_DRAFT);
    g_assert_cmpstr(p->custom_theme_id, ==, first_id);
    g_assert_cmpuint(mq_palette_get(p->graphs, 0, 7), ==, 6);
    guint invalid;
    g_autoptr(GPtrArray) disk = themes_load(&invalid, NULL);
    g_assert_cmpuint(invalid, ==, 0);
    g_assert_cmpuint(themes_find_id(disk, first_id)->palette.values[0][7], ==, 4);
    XfcePanelPlugin *plugin = p->plugin;
    free_plugin(plugin, p);
    gtk_widget_destroy(GTK_WIDGET(plugin)); g_object_unref(plugin);
    g_assert_true(theme_delete(first_id, NULL));
    g_print("Named themes: discard, overwrite, rename, delete, failures and draft persistence: passed\n");
}
static gboolean remove_during_prompt(gpointer data) {
    Plugin *p = data;
    XfcePanelPlugin *plugin = p->plugin;
    free_plugin(plugin, p);
    gtk_widget_destroy(GTK_WIDGET(plugin)); g_object_unref(plugin);
    return G_SOURCE_REMOVE;
}
static void modal_teardown_test(void) {
    for (int action = 0; action < 2; action++) {
        Plugin *p = create();
        p->plugin = g_object_new(XFCE_TYPE_PANEL_PLUGIN, "name", "multiload-qubes", "unique-id", 45, NULL);
        g_object_ref_sink(p->plugin);
        gtk_container_add(GTK_CONTAINER(p->plugin), p->area);
        configure(p->plugin, p);
        Settings *s = g_object_get_data(G_OBJECT(p->dialog), "settings");
        gtk_spin_button_set_value(GTK_SPIN_BUTTON(s->border_width), 2);
        g_idle_add(remove_during_prompt, p);
        if (action == 0) select_theme(s->theme, THEME_TANGO);
        else save_as_theme(NULL, s);
    }
    g_print("Removing panel item during discard and naming dialogs: passed\n");
}
static void rendering_test(void) {
    Graphs *g = mq_new();
    for (unsigned kind = 0; kind < 7; kind++) {
        for (unsigned f = 0; f < 7; f++) mq_palette_set(g, kind, f, 0);
        mq_palette_set(g, kind, 0, 0xFF000080); /* half-transparent red border */
        mq_palette_set(g, kind, 7, 3);
        cairo_surface_t *surface = cairo_image_surface_create(CAIRO_FORMAT_ARGB32, 20, 20);
        cairo_t *cr = cairo_create(surface);
        mq_draw(g, cr, 20, 20, kind == 0, kind == 6, kind == 1, kind == 5,
                kind == 2, kind == 3, kind == 4, FALSE);
        cairo_surface_flush(surface);
        uint32_t *pixels = (uint32_t *)cairo_image_surface_get_data(surface);
        g_assert_cmphex(pixels[0], ==, 0x80800000);
        g_assert_cmphex(pixels[2 * 20 + 2], ==, 0x80800000);
        g_assert_cmphex(pixels[3 * 20 + 3], ==, 0);
        g_assert_cmphex(pixels[10 * 20 + 10], ==, 0); /* no black underpaint */
        mq_palette_set(g, kind, 7, 0);
        cairo_set_operator(cr, CAIRO_OPERATOR_CLEAR); cairo_paint(cr);
        cairo_set_operator(cr, CAIRO_OPERATOR_OVER);
        mq_draw(g, cr, 20, 20, kind == 0, kind == 6, kind == 1, kind == 5,
                kind == 2, kind == 3, kind == 4, FALSE);
        cairo_surface_flush(surface);
        g_assert_cmphex(pixels[0], ==, 0);
        cairo_destroy(cr); cairo_surface_destroy(surface);
    }
    mq_free(g);
}
static int self_test(void) {
    rendering_test();
    named_theme_test();
    modal_teardown_test();
    Plugin *p = create();
    p->plugin = g_object_new(XFCE_TYPE_PANEL_PLUGIN, "name", "multiload-qubes",
        "unique-id", 42, "display-name", "Multiload for Qubes", NULL);
    g_object_ref_sink(p->plugin);
    gtk_container_add(GTK_CONTAINER(p->plugin), p->area);
    resize(p);
    p->disk = p->qube_ram = p->qube_cpu = FALSE;
    /* Hover regions follow rendering with hidden charts, vertical panels and rounding. */
    GdkRectangle region;
    g_assert_cmpint(chart_at(p, 130, 49, 0, 0, &region), ==, 0);
    g_assert_cmpint(chart_at(p, 130, 49, 32, 0, &region), ==, 1);
    g_assert_cmpint(region.x, ==, 32);
    g_assert_cmpint(region.width, ==, 32);
    g_assert_cmpint(chart_at(p, 130, 49, 96, 0, &region), ==, 3);
    g_assert_cmpint(chart_at(p, 130, 49, 128, 0, &region), ==, -1);
    g_assert_cmpint(chart_at(p, 130, 49, -1, 0, &region), ==, -1);
    p->vertical = TRUE;
    g_assert_cmpint(chart_at(p, 49, 130, 0, 64, &region), ==, 2);
    g_assert_cmpint(region.y, ==, 64);
    g_assert_cmpint(chart_at(p, 49, 130, 0, 129, &region), ==, -1);
    p->vertical = FALSE;
    p->cpu = p->network = FALSE;
    g_assert_cmpint(chart_at(p, 130, 49, 0, 0, &region), ==, 1);
    g_assert_cmpint(chart_at(p, 130, 49, 65, 0, &region), ==, 3);
    g_assert_cmpint(region.x, ==, 65);
    p->cpu = p->network = p->disk = TRUE;
    g_assert_cmpint(chart_at(p, 163, 49, 128, 0, &region), ==, 4);
    g_assert_cmpint(region.x, ==, 128);
    g_assert_cmpint(chart_at(p, 163, 49, 160, 0, &region), ==, -1);
    p->vertical = TRUE;
    g_assert_cmpint(chart_at(p, 49, 163, 0, 128, &region), ==, 4);
    p->vertical = FALSE;
    p->qube_ram = TRUE;
    /* Qube RAM is third, directly after host RAM, in both orientations. */
    g_assert_cmpint(chart_at(p, 195, 49, 32, 0, &region), ==, 1);
    g_assert_cmpint(chart_at(p, 195, 49, 64, 0, &region), ==, 5);
    g_assert_cmpint(chart_at(p, 195, 49, 96, 0, &region), ==, 2);
    p->vertical = TRUE;
    g_assert_cmpint(chart_at(p, 49, 195, 0, 64, &region), ==, 5);
    p->vertical = FALSE;
    p->qube_cpu = TRUE;
    g_assert_cmpint(chart_at(p, 227, 49, 0, 0, &region), ==, 0);
    g_assert_cmpint(chart_at(p, 227, 49, 32, 0, &region), ==, 6);
    g_assert_cmpint(chart_at(p, 227, 49, 64, 0, &region), ==, 1);
    p->vertical = TRUE;
    g_assert_cmpint(chart_at(p, 49, 227, 0, 32, &region), ==, 6);
    p->vertical = FALSE;
    configure(p->plugin, p);
    Settings *s = g_object_get_data(G_OBJECT(p->dialog), "settings");
    select_theme(s->theme, THEME_SOLARIZED_LIGHT);
    g_assert_cmpint(p->theme, ==, THEME_SOLARIZED_LIGHT);
    for (unsigned k = 0; k < 7; k++) {
        g_assert_cmphex(mq_palette_get(p->graphs, k, 1), ==, 0xFDF6E3FF);
        g_assert_cmphex(mq_palette_get(p->graphs, k, 2), ==, 0xFDF6E3FF);
    }
    select_theme(s->theme, THEME_SOLARIZED_LIGHT_GRADIENT);
    g_assert_cmpint(p->theme, ==, THEME_SOLARIZED_LIGHT_GRADIENT);
    for (unsigned k = 0; k < 7; k++) {
        g_assert_cmphex(mq_palette_get(p->graphs, k, 1), ==, 0xF3E5C0FF);
        g_assert_cmphex(mq_palette_get(p->graphs, k, 2), ==, 0xFDF6E300);
        g_assert_cmpuint(mq_palette_get(p->graphs, k, 7), ==, 0);
    }
    select_theme(s->theme, THEME_SOLARIZED_LIGHT);
    gtk_combo_box_set_active(GTK_COMBO_BOX(s->chart), 3); /* Per-qube RAM */
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(s->transparent), TRUE);
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(s->border_width), 3);
    GtkWidget *notebook = gtk_widget_get_parent(gtk_widget_get_parent(s->theme));
    g_assert_true(GTK_IS_NOTEBOOK(notebook));
    g_assert_cmpint(gtk_notebook_get_n_pages(GTK_NOTEBOOK(notebook)), ==, 2);
    gtk_notebook_set_current_page(GTK_NOTEBOOK(notebook), 1);
    while (gtk_events_pending()) gtk_main_iteration();
    cairo_surface_t *shot = cairo_image_surface_create(CAIRO_FORMAT_ARGB32,
        gtk_widget_get_allocated_width(p->dialog), gtk_widget_get_allocated_height(p->dialog));
    cairo_t *shot_cr = cairo_create(shot);
    gtk_widget_draw(p->dialog, shot_cr);
    g_assert_cmpint(cairo_surface_write_to_png(shot, "build/appearance-preferences.png"), ==, CAIRO_STATUS_SUCCESS);
    cairo_destroy(shot_cr); cairo_surface_destroy(shot);
    GdkRGBA edited_line = {0.2, 0.4, 0.6, 0.5};
    gtk_color_chooser_set_rgba(GTK_COLOR_CHOOSER(s->colors[3]), &edited_line);
    g_signal_emit_by_name(s->colors[3], "color-set");
    uint32_t saved_line = mq_palette_get(p->graphs, 5, 3);
    g_assert_cmphex(saved_line, ==, 0x33669980);
    g_assert_cmpuint(mq_palette_get(p->graphs, 5, 1) & 255, ==, 0);
    g_assert_cmpuint(mq_palette_get(p->graphs, 5, 2) & 255, ==, 0);
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(s->width), 64);
    gtk_spin_button_set_value(GTK_SPIN_BUTTON(s->interval), 500);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(s->cpu), FALSE);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(s->qube_cpu), FALSE);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(s->network), FALSE);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(s->swap), FALSE);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(s->disk), FALSE);
    gtk_toggle_button_set_active(GTK_TOGGLE_BUTTON(s->qube_ram), FALSE);
    gtk_dialog_response(GTK_DIALOG(p->dialog), GTK_RESPONSE_OK);
    g_assert_null(p->dialog);
    g_assert_cmpint(p->width, ==, 64);
    g_assert_cmpint(p->interval, ==, 500);
    g_assert_false(p->cpu);
    g_assert_false(p->qube_cpu);
    g_assert_false(p->network);
    g_assert_false(p->swap);
    g_assert_false(p->disk);
    g_assert_false(p->qube_ram);
    p->width = 0; p->interval = 1000; p->cpu = p->qube_cpu = p->network = p->swap = p->disk = p->qube_ram = TRUE;
    load(p);
    g_assert_cmpint(p->width, ==, 64);
    g_assert_cmpint(p->interval, ==, 500);
    g_assert_false(p->cpu);
    g_assert_false(p->qube_cpu);
    g_assert_false(p->network);
    g_assert_false(p->swap);
    g_assert_false(p->disk);
    g_assert_false(p->qube_ram);
    g_assert_cmpint(p->theme, ==, THEME_CUSTOM_DRAFT);
    g_assert_cmpuint(mq_palette_get(p->graphs, 5, 7), ==, 3);
    g_assert_cmpuint(mq_palette_get(p->graphs, 5, 1) & 255, ==, 0);
    g_assert_cmpuint(mq_palette_get(p->graphs, 5, 3), ==, saved_line);
    configure(p->plugin, p);
    s = g_object_get_data(G_OBJECT(p->dialog), "settings");
    TestPrompts prompts;
    const TestPrompt discard[] = {{"Your changes aren't saved. Discard them?", NULL, GTK_RESPONSE_ACCEPT}};
    queue_prompts(&prompts, discard, G_N_ELEMENTS(discard));
    select_theme(s->theme, THEME_TANGO);
    g_assert_cmpuint(prompts.next, ==, prompts.count);
    /* Panel save during editing must save committed settings, not the preview. */
    save(p->plugin, p);
    gtk_dialog_response(GTK_DIALOG(p->dialog), GTK_RESPONSE_CANCEL);
    g_assert_cmpint(p->theme, ==, THEME_CUSTOM_DRAFT);
    g_assert_cmpuint(mq_palette_get(p->graphs, 5, 7), ==, 3);
    g_assert_cmpuint(mq_palette_get(p->graphs, 5, 3), ==, saved_line);
    load(p);
    g_assert_cmpuint(mq_palette_get(p->graphs, 5, 7), ==, 3);
    /* Existing configurations should gain new charts without losing other settings. */
    char *legacy_path = xfce_panel_plugin_lookup_rc_file(p->plugin);
    GKeyFile *legacy = g_key_file_new();
    g_assert_true(g_key_file_load_from_file(legacy, legacy_path, G_KEY_FILE_NONE, NULL));
    g_assert_true(g_key_file_remove_key(legacy, "Graphs", "swap", NULL));
    g_assert_true(g_key_file_remove_key(legacy, "Graphs", "disk", NULL));
    g_assert_true(g_key_file_remove_key(legacy, "Graphs", "qube_ram", NULL));
    g_assert_true(g_key_file_remove_key(legacy, "Graphs", "qube_cpu", NULL));
    g_assert_true(g_key_file_save_to_file(legacy, legacy_path, NULL));
    g_key_file_unref(legacy);
    g_free(legacy_path);
    load(p);
    g_assert_true(p->swap);
    g_assert_true(p->disk);
    g_assert_true(p->qube_ram);
    g_assert_true(p->qube_cpu);
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
    if ((argc == 3 || argc == 4) && g_str_equal(argv[1], "--render")) {
        Graphs *graphs = mq_new();
        if (argc == 4)
            for (unsigned k = 0; k < 7; k++)
                for (unsigned f = 0; f < 8; f++)
                    mq_palette_set(graphs, k, f, mq_palette_preset((unsigned)g_ascii_strtoull(argv[3], NULL, 10), k, f));
        for (int i = 0; i < 400; i++) mq_tick(graphs, TRUE);
        cairo_surface_t *surface = cairo_image_surface_create(CAIRO_FORMAT_ARGB32, 840, 64);
        cairo_t *cr = cairo_create(surface);
        mq_draw(graphs, cr, 840, 64, TRUE, TRUE, TRUE, TRUE, TRUE, TRUE, TRUE, FALSE);
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
