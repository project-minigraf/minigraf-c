/* Compiled against include/minigraf.h and the static library by CI (c-smoke job):
 * read-only open, a cursor, the fact log and a log writer through the C API. */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "minigraf.h"

static int failures = 0;
#define CHECK(cond, what) do { if (!(cond)) { fprintf(stderr, "FAIL %s:%d %s\n", __FILE__, __LINE__, what); failures++; } } while (0)

static int starts_with(const char *s, const char *prefix) {
    return s != NULL && strncmp(s, prefix, strlen(prefix)) == 0;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s <dir>\n", argv[0]);
        return 2;
    }
    char src_path[4096], out_path[4096];
    snprintf(src_path, sizeof src_path, "%s/src.graph", argv[1]);
    snprintf(out_path, sizeof out_path, "%s/out.graph", argv[1]);

    MiniGrafDb *db = minigraf_open(src_path);
    CHECK(db != NULL, "open");
    char *r = minigraf_execute(db, "(transact [[:a :n 1] [:b :n 2]])");
    minigraf_string_free(r);
    minigraf_close(db);

    char *err = NULL;
    MiniGrafDb *ro = minigraf_open_with_options(src_path, "{\"read_only\": true}", &err);
    CHECK(ro != NULL && err == NULL, "read-only open");
    CHECK(minigraf_execute(ro, "(transact [[:c :n 3]])") == NULL, "write refused");
    CHECK(starts_with(minigraf_last_error(ro), "[API-014]"), "API-014");

    MiniGrafCursor *cursor = minigraf_query(ro, "(query [:find ?n :where [?e :n ?n]])");
    CHECK(cursor != NULL, "query");
    size_t batches = 0;
    char *batch;
    while ((batch = minigraf_cursor_next_batch(cursor, 1)) != NULL) {
        batches++;
        minigraf_string_free(batch);
    }
    CHECK(minigraf_cursor_last_error(cursor) == NULL, "cursor ended cleanly");
    CHECK(batches == 2, "two batches of one row");
    minigraf_cursor_free(cursor);

    MiniGrafFactLog *log = minigraf_fact_log(ro, NULL);
    CHECK(log != NULL, "fact log");
    MiniGrafLogWriter *w = minigraf_log_writer_create(out_path, NULL, &err);
    CHECK(w != NULL, "writer");
    while ((batch = minigraf_fact_log_next_batch(log, 100)) != NULL) {
        CHECK(minigraf_log_writer_append_batch(w, batch) == 0, "append batch");
        minigraf_string_free(batch);
    }
    minigraf_fact_log_free(log);
    uint64_t tx = 0;
    CHECK(minigraf_current_tx_count(ro, &tx) == 0 && tx == 1, "tx count");
    CHECK(minigraf_log_writer_advance_tx_count(w, tx) == 0, "advance");
    CHECK(minigraf_log_writer_finish(w) == 0, "finish");
    CHECK(minigraf_log_writer_finish(w) == -1, "second finish");
    CHECK(starts_with(minigraf_log_writer_last_error(w), "[API-018]"), "API-018");
    minigraf_log_writer_free(w);
    minigraf_close(ro);

    MiniGrafDb *copy = minigraf_open_with_options(out_path, "{\"read_only\": true}", &err);
    CHECK(copy != NULL, "open copy");
    char *rows = minigraf_execute(copy, "(query [:find (count ?e) :where [?e :n _]])");
    CHECK(rows != NULL && strstr(rows, "[[2]]") != NULL, "copied facts");
    minigraf_string_free(rows);
    minigraf_close(copy);

    CHECK(minigraf_log_writer_create(out_path, NULL, &err) == NULL, "existing target refused");
    CHECK(starts_with(err, "[STG-043]"), "STG-043");
    minigraf_string_free(err);

    /* Prepared queries (minigraf#181). */
    char p_path[4096];
    snprintf(p_path, sizeof p_path, "%s/prepared.graph", argv[1]);
    MiniGrafDb *pdb = minigraf_open(p_path);
    CHECK(pdb != NULL, "open for prepare");
    minigraf_string_free(minigraf_execute(pdb, "(transact [[:a :n 1] [:b :n 2]])"));
    MiniGrafPrepared *pq = minigraf_prepare(pdb, "(query [:find ?x :where [?e :n $n] [?e :n ?x]])");
    CHECK(pq != NULL, "prepare");
    char *one = minigraf_prepared_execute(pq, "{\"n\": 1}");
    CHECK(one != NULL && strstr(one, "\"results\":[[1]]") != NULL, "prepared n=1");
    minigraf_string_free(one);
    char *two = minigraf_prepared_execute(pq, "{\"n\": {\"type\": \"integer\", \"value\": 2}}");
    CHECK(two != NULL && strstr(two, "\"results\":[[2]]") != NULL, "prepared n=2 typed");
    minigraf_string_free(two);
    MiniGrafCursor *pc = minigraf_prepared_query(pq, "{\"n\": 2}");
    CHECK(pc != NULL, "prepared cursor");
    char *pb = minigraf_cursor_next_batch(pc, 10);
    CHECK(pb != NULL && strcmp(pb, "[[2]]") == 0, "prepared cursor rows");
    minigraf_string_free(pb);
    minigraf_cursor_free(pc);
    CHECK(minigraf_prepared_execute(pq, "{}") == NULL, "missing slot refused");
    CHECK(starts_with(minigraf_prepared_last_error(pq), "[API-022]"), "API-022");
    minigraf_close(pdb);
    CHECK(minigraf_prepared_execute(pq, "{\"n\": 1}") == NULL, "closed database refused");
    CHECK(starts_with(minigraf_prepared_last_error(pq), "[API-018]"), "API-018");
    MiniGrafDb *reopened = minigraf_open(p_path);
    CHECK(reopened != NULL, "reopen after close with a prepared query alive");
    minigraf_close(reopened);
    minigraf_prepared_free(pq);

    if (failures > 0) {
        fprintf(stderr, "%d check(s) failed\n", failures);
        return 1;
    }
    printf("all C checks passed\n");
    return 0;
}
