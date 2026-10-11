/* SPDX-License-Identifier: GPL-2.0-or-later */
#include "themes.h"
#include <glib/gstdio.h>
#include <errno.h>
#include <string.h>

static const char *groups[] = {"CPU", "RAM", "Network", "Swap", "Disk", "PerQubeRAM", "PerQubeCPU"};
static const char *keys[] = {"Border", "BackgroundTop", "BackgroundBottom", "Series1", "Series2", "Series3", "Missing", "BorderWidth"};
static char *themes_dir(void) {
    return g_build_filename(g_get_user_config_dir(), "multiload-qubes", "themes", NULL);
}
static char *theme_path(const char *id) {
    if (!id || !g_uuid_string_is_valid(id)) return NULL;
    g_autofree char *dir = themes_dir();
    g_autofree char *file = g_strconcat(id, ".theme", NULL);
    return g_build_filename(dir, file, NULL);
}
void saved_theme_free(SavedTheme *t) {
    if (!t) return;
    g_free(t->id); g_free(t->name); g_free(t);
}
gboolean theme_name_valid(const char *name) {
    if (!name || !*name || !g_utf8_validate(name, -1, NULL) || g_utf8_strlen(name, -1) > 128) return FALSE;
    for (const char *p = name; *p; p = g_utf8_next_char(p))
        if (g_unichar_iscntrl(g_utf8_get_char(p))) return FALSE;
    g_autofree char *trimmed = g_strdup(name);
    return *g_strstrip(trimmed) && g_str_equal(trimmed, name);
}
SavedTheme *themes_find_id(GPtrArray *themes, const char *id) {
    for (guint i = 0; id && i < themes->len; i++) {
        SavedTheme *t = g_ptr_array_index(themes, i);
        if (g_str_equal(t->id, id)) return t;
    }
    return NULL;
}
SavedTheme *themes_find_name(GPtrArray *themes, const char *name) {
    for (guint i = 0; i < themes->len; i++) {
        SavedTheme *t = g_ptr_array_index(themes, i);
        if (g_str_equal(t->name, name)) return t;
    }
    return NULL;
}
static SavedTheme *theme_read(const char *id) {
    g_autofree char *path = theme_path(id);
    GStatBuf info;
    if (!path || g_stat(path, &info) || !S_ISREG(info.st_mode) || info.st_size > 65536) return NULL;
    g_autoptr(GKeyFile) file = g_key_file_new();
    if (!g_key_file_load_from_file(file, path, G_KEY_FILE_NONE, NULL) ||
        g_key_file_get_integer(file, "Theme", "Version", NULL) != 1) return NULL;
    g_autofree char *name = g_key_file_get_string(file, "Theme", "Name", NULL);
    if (!theme_name_valid(name)) return NULL;
    ThemePalette palette;
    for (unsigned k = 0; k < 7; k++) for (unsigned f = 0; f < 8; f++) {
        g_autofree char *value = g_key_file_get_string(file, groups[k], keys[f], NULL);
        if (!value) return NULL;
        char *end;
        guint64 number;
        if (f == 7) {
            if (!g_ascii_isdigit(*value)) return NULL;
            number = g_ascii_strtoull(value, &end, 10);
            if (*end || number > 16) return NULL;
        } else {
            if (strlen(value) != 9 || value[0] != '#') return NULL;
            for (unsigned c = 1; c < 9; c++) if (!g_ascii_isxdigit(value[c])) return NULL;
            number = g_ascii_strtoull(value + 1, &end, 16);
            if (*end || number > UINT32_MAX) return NULL;
        }
        palette.values[k][f] = (uint32_t)number;
    }
    SavedTheme *theme = g_new0(SavedTheme, 1);
    *theme = (SavedTheme){g_strdup(id), g_steal_pointer(&name), palette};
    return theme;
}
static gint theme_compare(gconstpointer a, gconstpointer b) {
    return g_utf8_collate((*(SavedTheme * const *)a)->name, (*(SavedTheme * const *)b)->name);
}
GPtrArray *themes_load(guint *invalid, GError **error) {
    GPtrArray *themes = g_ptr_array_new_with_free_func((GDestroyNotify)saved_theme_free);
    *invalid = 0;
    g_autofree char *path = themes_dir();
    g_autoptr(GError) local = NULL;
    g_autoptr(GDir) dir = g_dir_open(path, 0, &local);
    if (!dir) {
        if (!g_error_matches(local, G_FILE_ERROR, G_FILE_ERROR_NOENT)) g_propagate_error(error, g_steal_pointer(&local));
        return themes;
    }
    const char *file;
    while ((file = g_dir_read_name(dir))) {
        if (!g_str_has_suffix(file, ".theme")) continue;
        g_autofree char *id = g_strndup(file, strlen(file) - strlen(".theme"));
        SavedTheme *theme = theme_read(id);
        if (!theme || themes_find_name(themes, theme->name)) {
            (*invalid)++; saved_theme_free(theme);
        } else g_ptr_array_add(themes, theme);
    }
    g_ptr_array_sort(themes, theme_compare);
    return themes;
}
gboolean theme_write(SavedTheme *theme, GError **error) {
    g_autofree char *path = theme_path(theme->id);
    if (!path || !theme_name_valid(theme->name)) {
        g_set_error_literal(error, G_FILE_ERROR, G_FILE_ERROR_INVAL, "Invalid theme name or identifier."); return FALSE;
    }
    g_autofree char *dir = themes_dir();
    if (g_mkdir_with_parents(dir, 0700)) {
        g_set_error(error, G_FILE_ERROR, g_file_error_from_errno(errno), "Cannot create theme directory: %s", g_strerror(errno)); return FALSE;
    }
    g_autoptr(GKeyFile) file = g_key_file_new();
    g_key_file_set_integer(file, "Theme", "Version", 1);
    g_key_file_set_string(file, "Theme", "Name", theme->name);
    for (unsigned k = 0; k < 7; k++) for (unsigned f = 0; f < 8; f++) {
        uint32_t value = theme->palette.values[k][f];
        if (f == 7) {
            if (value > 16) {
                g_set_error_literal(error, G_FILE_ERROR, G_FILE_ERROR_INVAL, "Invalid border width."); return FALSE;
            }
            g_key_file_set_integer(file, groups[k], keys[f], value);
        } else {
            char rgba[10];
            g_snprintf(rgba, sizeof rgba, "#%08X", value);
            g_key_file_set_string(file, groups[k], keys[f], rgba);
        }
    }
    gsize size;
    g_autofree char *data = g_key_file_to_data(file, &size, NULL);
    return g_file_set_contents_full(path, data, size, G_FILE_SET_CONTENTS_CONSISTENT | G_FILE_SET_CONTENTS_DURABLE, 0600, error);
}
gboolean theme_delete(const char *id, GError **error) {
    g_autofree char *path = theme_path(id);
    if (!path) { g_set_error_literal(error, G_FILE_ERROR, G_FILE_ERROR_INVAL, "Invalid theme identifier."); return FALSE; }
    if (!g_unlink(path) || errno == ENOENT) return TRUE;
    g_set_error(error, G_FILE_ERROR, g_file_error_from_errno(errno), "Cannot delete theme: %s", g_strerror(errno));
    return FALSE;
}
