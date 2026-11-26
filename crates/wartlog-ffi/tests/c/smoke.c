/* Drives libwartlog through the public header: private writer, reads, queries, errors, verify.
 * Usage: smoke <empty dir>. Exits 0 on success; prints the failing check otherwise. */
#include "wartlog.h"
#include <stdio.h>
#include <string.h>

static int failures = 0;
#define CHECK(cond)                                                                    \
    do {                                                                               \
        if (!(cond)) {                                                                 \
            fprintf(stderr, "%s:%d: CHECK(%s) failed; last error: %s\n", __FILE__,      \
                    __LINE__, #cond, wl_last_error());                                 \
            failures++;                                                                \
        }                                                                              \
    } while (0)
#define S(x) x, strlen(x)

int main(int argc, char **argv) {
    if (argc < 2) return 2;
    char root[4096], missing[4096];
    snprintf(root, sizeof root, "%s/app.wartlogx", argv[1]);
    snprintf(missing, sizeof missing, "%s/nope", argv[1]);

    CHECK(wl_abi_version() >= WL_ABI_VERSION);
    CHECK(strlen(wl_version()) > 0);

    /* ---- write in private mode ---- */
    wl_writer *w = NULL;
    wl_writer_opts wo;
    memset(&wo, 0, sizeof wo);
    wo.size = sizeof wo;
    wo.creator = "smoke-test 1.0";
    wo.default_source = "json-lines";
    CHECK(wl_writer_open(root, WL_WRITER_PRIVATE, &wo, &w) == WL_OK);

    wl_source *src = NULL;
    CHECK(wl_source_new(WL_SOURCE_NDJSON, &src) == WL_OK);
    CHECK(wl_source_set_attr(src, S("source.name"), S("smoke")) == WL_OK);

    uint64_t seq = 0, last = 0;
    for (int i = 0; i < 50; i++) {
        wl_record_builder *b = wl_record_new(src);
        CHECK(b != NULL);
        CHECK(wl_record_set_time(b, 1700000000000000000LL + (int64_t)i * 1000000) == WL_OK);
        CHECK(wl_record_set_severity(b, i % 10 == 0 ? 17 : 9, NULL, 0) == WL_OK);
        CHECK(wl_record_set_template(b, S("Order {OrderId} shipped in {Ms} ms")) == WL_OK);
        CHECK(wl_record_set_attr_i64(b, S("OrderId"), i) == WL_OK);
        CHECK(wl_record_set_attr_f64(b, S("Ms"), i * 1.5) == WL_OK);
        CHECK(wl_record_set_attr_bool(b, S("odd"), i % 2) == WL_OK);
        CHECK(wl_record_set_attr_str(b, S("region"), S("eu")) == WL_OK);
        uint8_t trace[16] = {1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, (uint8_t)i};
        uint8_t span[8] = {9, 9, 9, 9, 9, 9, 9, (uint8_t)i};
        CHECK(wl_record_set_trace(b, trace, span) == WL_OK);
        CHECK(wl_writer_push(w, b, &seq) == WL_OK); /* consumes b */
        CHECK(seq == (uint64_t)i + 1);
    }
    wl_source_free(src);
    const char *line = "{\"@t\":\"2026-11-26T10:00:00Z\",\"@mt\":\"User {User} logged in\",\"User\":\"tim\"}";
    CHECK(wl_writer_push_json(w, S(line), &seq) == WL_OK);
    CHECK(seq == 51);
    CHECK(wl_writer_push_json(w, S("plain text line"), &seq) == WL_OK);
    CHECK(wl_writer_flush(w, &last) == WL_OK);
    CHECK(last >= 52);

    /* the store is owned: exclusive mode is locked out while the writer is open */
    wl_writer *w2 = NULL;
    CHECK(wl_writer_open(root, WL_WRITER_EXCLUSIVE, NULL, &w2) == WL_E_LOCKED);
    CHECK(strstr(wl_last_error(), "another process") != NULL);
    /* shared mode refuses an external store */
    CHECK(wl_writer_open(root, WL_WRITER_SHARED, NULL, &w2) == WL_E_NO_DAEMON);
    CHECK(wl_writer_close(w) == WL_OK);

    /* ---- read ---- */
    wl_snapshot *s = NULL;
    CHECK(wl_snapshot_open(missing, &s) == WL_E_NOT_FOUND);
    CHECK(wl_snapshot_open(root, &s) == WL_OK);
    uint64_t max_seq = 0, records = 0;
    CHECK(wl_snapshot_stats(s, &max_seq, &records) == WL_OK);
    CHECK(max_seq == 52 && records == 52);

    wl_cursor *c = NULL;
    CHECK(wl_query(s, "logs | where Source == 'smoke' and Level == 'error' | count", NULL, &c) == WL_OK);
    wl_row *row = NULL;
    CHECK(wl_cursor_next(c, &row) == 1);
    int64_t n = 0;
    CHECK(wl_row_get_i64(row, 0, &n) == WL_OK);
    CHECK(n == 5);
    CHECK(wl_cursor_next(c, &row) == 0);
    wl_cursor_free(c);

    wl_query_opts qo;
    memset(&qo, 0, sizeof qo);
    qo.size = sizeof qo;
    qo.threads = 1;
    CHECK(wl_query(s, "logs | where OrderId >= 47 | sort by OrderId asc | project OrderId, Body, Ms, odd, Timestamp, Id", &qo, &c) == WL_OK);
    uint32_t cols = 0;
    CHECK(wl_cursor_columns(c, &cols) == WL_OK);
    CHECK(cols == 6);
    const char *name;
    size_t len;
    CHECK(wl_cursor_column_name(c, 1, &name, &len) == WL_OK);
    CHECK(len == 4 && memcmp(name, "Body", 4) == 0);
    int got = 0;
    while (wl_cursor_next(c, &row) == 1) {
        int64_t order = -1, ts = 0, id = 0;
        double ms = 0;
        int odd = -1;
        wl_value_type t;
        CHECK(wl_row_type(row, 0, &t) == WL_OK && t == WL_I64);
        CHECK(wl_row_get_i64(row, 0, &order) == WL_OK);
        CHECK(wl_row_get_str(row, 1, &name, &len) == WL_OK);
        char want[64];
        snprintf(want, sizeof want, "Order %lld shipped in", (long long)order);
        CHECK(len > strlen(want) && memcmp(name, want, strlen(want)) == 0);
        CHECK(wl_row_get_f64(row, 2, &ms) == WL_OK && ms == order * 1.5);
        CHECK(wl_row_get_bool(row, 3, &odd) == WL_OK && odd == (int)(order % 2));
        CHECK(wl_row_get_i64(row, 4, &ts) == WL_OK && ts == 1700000000000000000LL + order * 1000000);
        CHECK(wl_row_get_bool(row, 0, &odd) == WL_E_TYPE);
        CHECK(wl_row_get_i64(row, 99, &ts) == WL_E_INVALID);
        CHECK(wl_row_get_i64(row, 5, &id) == WL_OK && id == order + 1);
        const char *j = wl_row_json(row);
        CHECK(j != NULL && strstr(j, "\"OrderId\"") != NULL);
        got++;
    }
    CHECK(got == 3);
    wl_cursor_free(c);

    /* a whole-record row has an id; record lookup by id */
    CHECK(wl_query(s, "logs | where User == 'tim'", NULL, &c) == WL_OK);
    CHECK(wl_cursor_next(c, &row) == 1);
    uint64_t rid = 0;
    CHECK(wl_row_id(row, &rid) == 1 && rid == 51);
    wl_cursor_free(c);
    wl_record *rec = NULL;
    CHECK(wl_record_get(s, 51, &rec) == 1);
    CHECK(strstr(wl_record_json(rec), "User {User} logged in") != NULL);
    wl_record_free(rec);
    CHECK(wl_record_get(s, 9999, &rec) == 0);

    /* query errors point into the text */
    size_t a = 0, z = 0;
    CHECK(wl_query(s, "logs | frobnicate", NULL, &c) == WL_E_QUERY);
    CHECK(wl_last_error_span(&a, &z) == 1 && z > a);

    /* invalid arguments never crash */
    CHECK(wl_query(NULL, "logs", NULL, &c) == WL_E_INVALID);
    CHECK(wl_writer_open(root, 42, NULL, &w2) == WL_E_INVALID);
    CHECK(wl_record_new(NULL) == NULL);
    CHECK(wl_writer_push(NULL, NULL, &seq) == WL_E_INVALID);
    wl_snapshot_free(s);

    /* ---- verify ---- */
    uint64_t findings = 99;
    char *report = NULL;
    CHECK(wl_store_verify(root, 0, &findings, &report) == WL_OK);
    CHECK(findings == 0);
    CHECK(report != NULL && strstr(report, "\"ok\":true") != NULL);
    CHECK(strstr(report, "private") != NULL);
    wl_string_free(report);
    CHECK(wl_store_verify(missing, 1, &findings, NULL) == WL_E_NOT_FOUND);

    /* reopening the private store continues the seqs */
    CHECK(wl_writer_open(root, WL_WRITER_PRIVATE, NULL, &w) == WL_OK);
    CHECK(wl_writer_push_json(w, S("{\"msg\":\"again\"}"), &seq) == WL_OK);
    CHECK(seq == 53);
    CHECK(wl_writer_close(w) == WL_OK);

    if (failures) {
        fprintf(stderr, "%d check(s) failed\n", failures);
        return 1;
    }
    printf("ok\n");
    return 0;
}
