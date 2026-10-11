/* SPDX-License-Identifier: GPL-2.0-or-later */
#include "../native/themes.h"
#include <glib/gstdio.h>
#include <string.h>
int main(void) {
    g_autofree char *tmp = g_dir_make_tmp("multiload-themes-XXXXXX", NULL);
    g_assert_nonnull(tmp);
    g_setenv("XDG_CONFIG_HOME", tmp, TRUE);
    guint invalid;
    g_autoptr(GError) error = NULL;
    g_autoptr(GPtrArray) empty = themes_load(&invalid, &error);
    g_assert_no_error(error); g_assert_cmpuint(empty->len, ==, 0);
    g_autofree char *id = g_uuid_string_random();
    SavedTheme original = {.id=id, .name="Cream / Ω"};
    for (unsigned k = 0; k < 7; k++) for (unsigned f = 0; f < 8; f++)
        original.palette.values[k][f] = f == 7 ? k : (0xF3E5C000u | (k * 8 + f));
    g_assert_true(theme_write(&original, &error)); g_assert_no_error(error);
    g_autoptr(GPtrArray) loaded = themes_load(&invalid, &error);
    g_assert_no_error(error); g_assert_cmpuint(invalid, ==, 0);
    g_assert_cmpuint(loaded->len, ==, 1);
    SavedTheme *copy = themes_find_name(loaded, original.name);
    g_assert_nonnull(copy);
    g_assert_cmpmem(&copy->palette, sizeof copy->palette, &original.palette, sizeof original.palette);
    /* Failed writes do not replace a previously valid file. */
    original.palette.values[0][7] = 17;
    g_assert_false(theme_write(&original, &error)); g_clear_error(&error);
    g_autoptr(GPtrArray) unchanged = themes_load(&invalid, &error);
    g_assert_no_error(error);
    g_assert_cmpuint(themes_find_id(unchanged, id)->palette.values[0][7], ==, 0);
    g_autofree char *file = g_strconcat(id, ".theme", NULL);
    g_autofree char *path = g_build_filename(tmp, "multiload-qubes", "themes", file, NULL);
    g_autoptr(GKeyFile) key = g_key_file_new();
    g_assert_true(g_key_file_load_from_file(key, path, G_KEY_FILE_NONE, NULL));
    g_key_file_set_integer(key, "Theme", "Version", 2);
    g_assert_true(g_key_file_save_to_file(key, path, NULL));
    g_autoptr(GPtrArray) future = themes_load(&invalid, &error);
    g_assert_cmpuint(future->len, ==, 0); g_assert_cmpuint(invalid, ==, 1);
    g_key_file_set_integer(key, "Theme", "Version", 1);
    g_key_file_set_string(key, "CPU", "Series1", "#000000G0");
    g_assert_true(g_key_file_save_to_file(key, path, NULL));
    g_autoptr(GPtrArray) corrupt = themes_load(&invalid, &error);
    g_assert_cmpuint(corrupt->len, ==, 0); g_assert_cmpuint(invalid, ==, 1);
    g_assert_false(theme_delete("../outside", &error)); g_clear_error(&error);
    g_assert_true(theme_delete(id, &error)); g_assert_no_error(error);
    g_assert_false(theme_name_valid("")); g_assert_false(theme_name_valid("bad\nname"));
    g_assert_false(theme_name_valid(" leading"));
    g_autofree char *dir = g_path_get_dirname(path);
    g_assert_cmpint(g_rmdir(dir), ==, 0);
    g_autofree char *app = g_path_get_dirname(dir);
    g_assert_cmpint(g_rmdir(app), ==, 0); g_assert_cmpint(g_rmdir(tmp), ==, 0);
    g_print("Theme file roundtrip, invalid files, versions and failed-write preservation: passed\n");
    return 0;
}
