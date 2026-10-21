#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
/* P3 Task 6 host harness: the real "P3 Task 6" rows of payload/src/mgmt_table.def (extracted by build.rs
 * into mgmt_t6.def) over stub handlers that answer with the reply shapes of the runtime.c handlers
 * (same JSON keys, same ERROR tokens, same `{"ok":false,...}` bodies). The dispatcher, the runner
 * helpers, the flags and the method numbers are the payload's own; only the handlers are stubs
 * (runtime.c needs the PS5 SDK and is not compiled on the host).
 *
 * The Sony stubs take the payload's own `sony_api_lock` (src/sony_api_lock.c) the way the real
 * handlers do, and count how many callers are inside at once, so a test can check that two
 * concurrent Sony-lock calls never overlap, and (control) that they do overlap when no lock is taken.
 */
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "ava1_gen.h"
#include "mgmt_rpc.h"
#include "sony_api_lock.h"

#define T6_FRAME_ERROR 3u

static int g_use_lock = 1;
static int g_hold_ms = 30;
static int g_inside, g_peak;
static unsigned g_list_n = 3;
static int g_list_trunc;
static unsigned g_search_bytes = 200;
static int g_index_building;
static char g_last_body[1024];
static unsigned g_calls[256];

static int t6_send(uint16_t type, const void *body, uint64_t len) {
    if (mgmt_capture_active()) return mgmt_capture_frame(type, body, len);
    return -1;
}

static void note(unsigned slot, const char *b) {
    __atomic_add_fetch(&g_calls[slot & 255u], 1, __ATOMIC_SEQ_CST);
    if (b) snprintf(g_last_body, sizeof g_last_body, "%s", b);
}

/* One "Sony API call": under sony_api_lock when g_use_lock, counting who is inside. */
static void sony_work(void) {
    struct timespec ts;
    int now, peak;
    if (g_use_lock) pthread_mutex_lock(&sony_api_lock);
    now = __atomic_add_fetch(&g_inside, 1, __ATOMIC_SEQ_CST);
    peak = __atomic_load_n(&g_peak, __ATOMIC_SEQ_CST);
    while (now > peak && !__atomic_compare_exchange_n(&g_peak, &peak, now, 0, __ATOMIC_SEQ_CST, __ATOMIC_SEQ_CST)) {
    }
    ts.tv_sec = 0;
    ts.tv_nsec = (long)g_hold_ms * 1000000L;
    nanosleep(&ts, NULL);
    __atomic_sub_fetch(&g_inside, 1, __ATOMIC_SEQ_CST);
    if (g_use_lock) pthread_mutex_unlock(&sony_api_lock);
}

static int err_frame(const char *tok) { return t6_send(T6_FRAME_ERROR, tok, strlen(tok)); }
static int ok_body(const char *s) { return t6_send(2, s, strlen(s)); }

static int has_key(const char *b, const char *key) {
    char needle[48];
    snprintf(needle, sizeof needle, "\"%s\"", key);
    return strstr(b, needle) != NULL;
}

/* A list of g_list_n entries under `array_key`, closed like the runtime.c handlers do. */
static int list_reply(const char *array_key, int items) {
    size_t cap = 64 + (size_t)g_list_n * 260, off = 0;
    unsigned i;
    char *buf = malloc(cap);
    int rc;
    if (!buf) return -1;
    off += (size_t)snprintf(buf + off, cap - off, "{\"%s\":[", array_key);
    for (i = 0; i < g_list_n; i++) {
        if (i) buf[off++] = ',';
        if (items)
            off += (size_t)snprintf(buf + off, cap - off,
                                    "{\"path\":\"/user/av_contents/photo/1/%u/shot \\\"%u\\\" [x].jxr\",\"size\":%u,\"mtime\":1700000000}", i, i,
                                    1000 + i);
        else
            off += (size_t)snprintf(buf + off, cap - off,
                                    "{\"title_id\":\"CUSA%05u\",\"user_id\":1,\"path\":\"/user/home/1/savedata/CUSA%05u\",\"size\":%u,"
                                    "\"mtime\":1700000000,\"kind\":\"ps4\"}",
                                    i, i, 4096 + i);
    }
    if (g_list_trunc)
        off += (size_t)snprintf(buf + off, cap - off, "],\"truncated\":true}");
    else
        off += (size_t)snprintf(buf + off, cap - off, "]}");
    rc = t6_send(2, buf, off);
    free(buf);
    return rc;
}

/* ---- handlers (the names the real table uses) ---- */
static int handle_app_register(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    (void)st; (void)fd; (void)t; (void)l;
    note(AVA1_METHOD_APP_REGISTER, b);
    if (!has_key(b, "src_path") || strstr(b, "\"src_path\":\"\"")) return err_frame("register_src_path_missing");
    if (strstr(b, "/forbidden")) return err_frame("register_src_path_not_allowed");
    sony_work();
    return ok_body("{\"title_id\":\"PPSA01234\",\"title_name\":\"Stub \\\"Game\\\"\",\"used_nullfs\":true}");
}
static int handle_app_unregister(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    (void)st; (void)fd; (void)t; (void)l;
    note(AVA1_METHOD_APP_UNREGISTER, b);
    if (!has_key(b, "title_id")) return err_frame("unregister_title_id_missing");
    sony_work();
    return ok_body(strstr(b, "REFUSED") ? "{\"sony_uninstall_rc\":2158631682}" : "{\"sony_uninstall_rc\":0}");
}
static int handle_app_launch(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    (void)st; (void)fd; (void)t; (void)l;
    note(AVA1_METHOD_APP_LAUNCH, b);
    if (!has_key(b, "title_id")) return err_frame("launch_title_id_missing");
    sony_work();
    if (strstr(b, "FAIL")) return err_frame("launch_all_strategies_failed: param=1 null=1 sys=1");
    return t6_send(61, NULL, 0);
}
/* app.list: g_list_n entries, `{"apps":[...]}` (the dispatcher pages it). */
static int handle_app_list_registered(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    size_t cap = 64 + (size_t)g_list_n * 160, off = 0;
    unsigned i;
    char *buf = malloc(cap);
    int rc;
    (void)st; (void)fd; (void)t; (void)l;
    note(AVA1_METHOD_APP_LIST, b);
    if (!buf) return -1;
    off += (size_t)snprintf(buf + off, cap - off, "{\"apps\":[");
    for (i = 0; i < g_list_n; i++) {
        if (i) buf[off++] = ',';
        off += (size_t)snprintf(buf + off, cap - off, "{\"title_id\":\"PPSA%05u\",\"title_name\":\"PPSA%05u\",\"src\":\"/data/g%u\",\"image_backed\":false}", i, i, i);
    }
    off += (size_t)snprintf(buf + off, cap - off, "]}");
    rc = t6_send(63, buf, off);
    free(buf);
    return rc;
}
static int handle_process_list(void *st, int fd, uint64_t t) {
    (void)st; (void)fd; (void)t;
    note(AVA1_METHOD_PROC_PROCESS_LIST, NULL);
    return ok_body("{\"procs\":[{\"pid\":101,\"name\":\"payload.elf\",\"kind\":\"payload\",\"is_self\":true}]}");
}
static int handle_app_launch_browser(void *st, int fd, uint64_t t) {
    (void)st; (void)fd; (void)t;
    note(AVA1_METHOD_APP_LAUNCH_BROWSER, NULL);
    sony_work();
    return t6_send(71, NULL, 0);
}
static int handle_app_lifecycle(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    (void)st; (void)fd; (void)t; (void)l;
    note(AVA1_METHOD_APP_LIFECYCLE, b);
    if (!strstr(b, "\"action\":\"")) return ok_body("{\"ok\":false,\"err\":\"bad_action\"}");
    sony_work();
    if (strstr(b, "\"list\"")) return ok_body("{\"ok\":true,\"action\":\"list\",\"apps\":[{\"app_id\":57368}]}");
    if (strstr(b, "\"app_id\":666")) return ok_body("{\"ok\":false,\"action\":\"kill\",\"app_id\":666,\"code\":-2146369530}");
    return ok_body("{\"ok\":true,\"action\":\"suspend\",\"app_id\":57368,\"code\":0}");
}
static int handle_appinfo_query(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    (void)st; (void)fd; (void)t; (void)l;
    note(AVA1_METHOD_APP_INFO_QUERY, b);
    if (!has_key(b, "title_id") || strstr(b, "\"title_id\":\"\"")) return ok_body("{\"ok\":false,\"error\":\"title_id is required\"}");
    return ok_body("{\"ok\":true,\"title_id\":\"CUSA00001\",\"rows\":[{\"key\":\"TITLE\",\"val\":\"Stub\"}],\"source\":\"sql\"}");
}
static int handle_appinfo_set(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    (void)st; (void)fd; (void)t; (void)l;
    note(AVA1_METHOD_APP_INFO_SET, b);
    if (strstr(b, "RUNNING")) return ok_body("{\"ok\":false,\"err\":\"title is running\"}");
    return ok_body("{\"ok\":true,\"err\":null}");
}
static int handle_appdb_query(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    (void)st; (void)fd; (void)t; (void)l;
    note(AVA1_METHOD_APP_DB_QUERY, b);
    return ok_body("{\"apps\":[{\"title_id\":\"CUSA00001\",\"app_id\":57368,\"name\":\"Stub\"}],\"source\":\"sql\"}");
}
static int handle_focus_probe(void *st, int fd, uint64_t t) {
    (void)st; (void)fd; (void)t;
    note(AVA1_METHOD_PROC_FOCUS, NULL);
    return ok_body("{\"ok\":true,\"apis\":{\"sceSystemServiceGetAppFocusedAppId\":false},\"big_app_id\":57368}");
}
static int handle_proc_list(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    (void)st; (void)fd; (void)t; (void)l;
    note(AVA1_METHOD_PROC_LIST, b);
    return ok_body("{\"procs\":[{\"pid\":1,\"name\":\"init\"},{\"pid\":101,\"name\":\"payload.elf\"}]}");
}
static int handle_process_kill(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    (void)st; (void)fd; (void)t; (void)l;
    note(AVA1_METHOD_PROC_KILL, b);
    if (strstr(b, "\"pid\":1}"))
        return ok_body("{\"ok\":false,\"pid\":1,\"err\":\"kill_failed\",\"errno\":1,\"reason\":\"Operation not permitted\"}");
    if (strstr(b, "\"pid\":99999}"))
        return ok_body("{\"ok\":false,\"pid\":99999,\"err\":\"kill_failed\",\"errno\":3,\"reason\":\"No such process\"}");
    return ok_body("{\"ok\":true,\"pid\":4242}");
}
static int handle_proc_modules(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    (void)st; (void)fd; (void)t; (void)l;
    note(AVA1_METHOD_PROC_MODULES, b);
    return ok_body("{\"modules\":[{\"name\":\"libc.sprx\"}]}");
}
static int handle_list_saves(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    (void)st; (void)fd; (void)t; (void)l;
    note(AVA1_METHOD_SAVES_LIST, b);
    return list_reply("saves", 0);
}
static int handle_list_screenshots(void *st, int fd, uint64_t t) {
    (void)st; (void)fd; (void)t;
    note(AVA1_METHOD_SHOTS_LIST, NULL);
    return list_reply("items", 1);
}
static int handle_list_videos(void *st, int fd, uint64_t t) {
    (void)st; (void)fd; (void)t;
    note(AVA1_METHOD_VIDEOS_LIST, NULL);
    return list_reply("items", 1);
}
static int handle_index_start(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    (void)st; (void)fd; (void)t; (void)l;
    note(AVA1_METHOD_INDEX_START, b);
    if (g_index_building) return ok_body("{\"started\":false,\"err\":\"already_building\"}");
    g_index_building = 1;
    return ok_body("{\"started\":true}");
}
static int handle_index_status(void *st, int fd, uint64_t t) {
    (void)st; (void)fd; (void)t;
    note(AVA1_METHOD_INDEX_STATUS, NULL);
    return ok_body(g_index_building ? "{\"phase\":\"building\",\"files\":12,\"truncated\":false,\"started_at\":1,\"completed_at\":0}"
                                    : "{\"phase\":\"idle\",\"files\":0,\"truncated\":false,\"started_at\":0,\"completed_at\":0}");
}
/* index.search: hits until the reply is about g_search_bytes long (the real handler stops at its buffer). */
static int handle_search_index(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    size_t cap = (size_t)g_search_bytes + 4096, off = 0;
    char *buf = malloc(cap);
    unsigned i = 0;
    int rc;
    (void)st; (void)fd; (void)t; (void)l;
    note(AVA1_METHOD_INDEX_SEARCH, b);
    if (!buf) return -1;
    off += (size_t)snprintf(buf + off, cap - off, "{\"results\":[");
    while (off < g_search_bytes) {
        if (i) buf[off++] = ',';
        off += (size_t)snprintf(buf + off, cap - off, "{\"path\":\"/data/games/dir%u/file%u.pkg\",\"size\":%u}", i % 97, i, 1000 + i);
        i++;
    }
    off += (size_t)snprintf(buf + off, cap - off, "]}");
    rc = t6_send(101, buf, off);
    free(buf);
    return rc;
}
static int handle_index_cancel(void *st, int fd, uint64_t t) {
    (void)st; (void)fd; (void)t;
    note(AVA1_METHOD_INDEX_CANCEL, NULL);
    g_index_building = 0;
    return ok_body("{\"cancelled\":true}");
}

/* Test-only entries (methods Task 6 does not own): fs.mount_pkg answers its request body verbatim
 * (to pin how legacy bodies map to statuses); fs.mount sends a progress frame, then the result. */
static int handle_echo(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    (void)st; (void)fd; (void)t;
    return t6_send(2, b, l);
}
static int handle_progress_then_result(void *st, int fd, uint64_t t, const char *b, uint64_t l) {
    (void)st; (void)fd; (void)t; (void)l;
    ok_body("{\"progress\":50}");
    if (strstr(b, "FAIL")) return ok_body("{\"result\":1,\"ok\":false,\"err\":\"final_failed\"}");
    return ok_body("{\"result\":1,\"ok\":true}");
}
static int r_echo(const uint8_t *q, uint32_t n, mgmt_ctx_t *cx) { return mgmt_call_text(q, n, cx, handle_echo); }
static int r_progress(const uint8_t *q, uint32_t n, mgmt_ctx_t *cx) { return mgmt_call_text(q, n, cx, handle_progress_then_result); }

/* ---- the table: the real rows, with the legacy frame numbers left out (unused here) ---- */
#define MGMT_H0(m, f, a, fl, h, helper)                                                                  \
    static int w_##m(void *st, int fd, uint64_t t, const char *b, uint64_t l) {                          \
        (void)b;                                                                                         \
        (void)l;                                                                                         \
        return h(st, fd, t);                                                                             \
    }                                                                                                    \
    static int r_##m(const uint8_t *q, uint32_t n, mgmt_ctx_t *cx) { return helper(q, n, cx, w_##m); }
#define MGMT_H1(m, f, a, fl, h, helper)                                                                  \
    static int w_##m(void *st, int fd, uint64_t t, const char *b, uint64_t l) { return h(st, fd, t, b, l); } \
    static int r_##m(const uint8_t *q, uint32_t n, mgmt_ctx_t *cx) { return helper(q, n, cx, w_##m); }
#include "mgmt_t6.def"
#undef MGMT_H0
#undef MGMT_H1

#define MGMT_H0(m, f, a, fl, h, helper) {(uint16_t)(m), 0, 0, (fl), r_##m},
#define MGMT_H1(m, f, a, fl, h, helper) {(uint16_t)(m), 0, 0, (fl), r_##m},
static const mgmt_entry_t k_t6_table[] = {
#include "mgmt_t6.def"
    {AVA1_METHOD_FS_MOUNT_PKG, 0, 0, 0, r_echo},
    {AVA1_METHOD_FS_MOUNT, 0, 0, 0, r_progress},
};
#undef MGMT_H0
#undef MGMT_H1

int ava1_t6_install(void) {
    memset(g_calls, 0, sizeof g_calls);
    g_inside = g_peak = 0;
    g_index_building = 0;
    return mgmt_rpc_install(k_t6_table, sizeof k_t6_table / sizeof k_t6_table[0], NULL, NULL, NULL);
}
void ava1_t6_uninstall(void) { (void)mgmt_rpc_install(NULL, 0, NULL, NULL, NULL); }
size_t ava1_t6_entries(uint16_t *methods, uint32_t *flags, size_t cap) {
    size_t i, n = sizeof k_t6_table / sizeof k_t6_table[0];
    for (i = 0; i < n && i < cap; i++) {
        methods[i] = k_t6_table[i].method;
        flags[i] = k_t6_table[i].flags;
    }
    return n;
}
void ava1_t6_set_sony(int use_lock, int hold_ms) {
    g_use_lock = use_lock;
    g_hold_ms = hold_ms;
}
int ava1_t6_peak(void) { return __atomic_load_n(&g_peak, __ATOMIC_SEQ_CST); }
void ava1_t6_reset_peak(void) { __atomic_store_n(&g_peak, 0, __ATOMIC_SEQ_CST); }
void ava1_t6_set_list(unsigned n, int truncated) {
    g_list_n = n;
    g_list_trunc = truncated;
}
void ava1_t6_set_search_bytes(unsigned n) { g_search_bytes = n; }
unsigned ava1_t6_calls(uint16_t method) { return __atomic_load_n(&g_calls[method & 255u], __ATOMIC_SEQ_CST); }
size_t ava1_t6_last_body(char *out, size_t cap) {
    size_t n = strlen(g_last_body);
    if (n >= cap) n = cap - 1;
    memcpy(out, g_last_body, n);
    out[n] = '\0';
    return n;
}
