/* SPDX-License-Identifier: GPL-2.0-or-later */
#ifndef MQ_THEMES_H
#define MQ_THEMES_H
#include <glib.h>
#include <stdint.h>

typedef struct { uint32_t values[7][8]; } ThemePalette;
typedef struct { char *id, *name; ThemePalette palette; } SavedTheme;

/* IDs are generated UUIDs; user-visible names are never used as paths. */
void saved_theme_free(SavedTheme *theme);
GPtrArray *themes_load(guint *invalid, GError **error);
SavedTheme *themes_find_id(GPtrArray *themes, const char *id);
SavedTheme *themes_find_name(GPtrArray *themes, const char *name);
gboolean theme_name_valid(const char *name);
gboolean theme_write(SavedTheme *theme, GError **error);
gboolean theme_delete(const char *id, GError **error);
#endif
