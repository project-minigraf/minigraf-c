/* The consumer contract every binding checks (spec §4): in memory, file-backed
 * across a reopen, and the error path for invalid Datalog.
 * Usage: contract <empty directory for the database file> */
#include "minigraf.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static int failures = 0;

static void check(int ok, const char *what) {
    printf("%s: %s\n", ok ? "ok" : "FAIL", what);
    if (!ok) failures++;
}

/* Runs a query and reports whether its JSON result contains `want`. */
static int query_has_q(MiniGrafDb *db, const char *query, const char *want) {
    char *r = minigraf_execute(db, query);
    int found = r != NULL && strstr(r, want) != NULL;
    if (r) printf("  %s\n", r);
    minigraf_string_free(r);
    return found;
}

static int query_has(MiniGrafDb *db, const char *want) {
    return query_has_q(db, "(query [:find ?n :where [?e :name ?n]])", want);
}

static int transact(MiniGrafDb *db, const char *datalog) {
    char *r = minigraf_execute(db, datalog);
    int ok = r != NULL && strstr(r, "\"transacted\"") != NULL;
    minigraf_string_free(r);
    return ok;
}

int main(int argc, char **argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s <dir>\n", argv[0]);
        return 2;
    }

    MiniGrafDb *mem = minigraf_open_in_memory();
    check(mem != NULL, "open in memory");
    check(transact(mem, "(transact [[:alice :name \"Alice\"]])"), "transact in memory");
    check(query_has(mem, "[[\"Alice\"]]"), "query in memory returns [[\"Alice\"]]");

    char *bad = minigraf_execute(mem, "(query [:find");
    check(bad == NULL, "invalid Datalog returns NULL");
    const char *err = minigraf_last_error(mem);
    check(err != NULL && err[0] != '\0', "invalid Datalog sets minigraf_last_error");
    if (err) printf("  %s\n", err);
    minigraf_string_free(bad);

    check(transact(mem, "(transact [[:bob :name \"Bob\"]])"), "transact Bob");
    MiniGrafPrepared *pq = minigraf_prepare(mem, "(query [:find ?n :where [?e :name $name] [?e :name ?n]])");
    check(pq != NULL, "prepare");
    char *a = minigraf_prepared_execute(pq, "{\"name\": \"Alice\"}");
    check(a != NULL && strstr(a, "[[\"Alice\"]]") != NULL, "prepared query finds Alice");
    char *b = minigraf_prepared_execute(pq, "{\"name\": \"Bob\"}");
    check(b != NULL && strstr(b, "[[\"Bob\"]]") != NULL, "prepared query finds Bob");
    char *t = minigraf_prepared_execute(pq, "{\"name\": {\"type\": \"string\", \"value\": \"Alice\"}}");
    check(t != NULL && strstr(t, "[[\"Alice\"]]") != NULL, "prepared query takes a typed value");
    minigraf_string_free(a);
    minigraf_string_free(b);
    minigraf_string_free(t);

    check(transact(mem, "(transact [[:alice :tags #{1 \"x\"}]])"), "transact a set");
    check(query_has_q(mem, "(query [:find ?t :where [?e :tags ?t]])", "[[[\"x\",1]]]"),
          "a set value comes back as an array");
    MiniGrafPrepared *member = minigraf_prepare(mem, "(query [:find ?t :where [?e :tags ?t] [(in? 1 $s)]])");
    char *m = minigraf_prepared_execute(member, "{\"s\": {\"type\": \"set\", \"value\": [1, 2]}}");
    check(m != NULL && strstr(m, "[[[\"x\",1]]]") != NULL, "a set binds");
    minigraf_string_free(m);
    minigraf_prepared_free(member);

    minigraf_close(mem);
    minigraf_prepared_free(pq);

    char path[4096];
    snprintf(path, sizeof path, "%s/test.graph", argv[1]);
    MiniGrafDb *db = minigraf_open(path);
    check(db != NULL, "open file-backed");
    check(transact(db, "(transact [[:bob :name \"Bob\"]])"), "transact file-backed");
    check(minigraf_checkpoint(db) == 0, "checkpoint");
    minigraf_close(db);

    MiniGrafDb *again = minigraf_open(path);
    check(again != NULL, "reopen file-backed");
    check(query_has(again, "[[\"Bob\"]]"), "query after reopen returns [[\"Bob\"]]");
    minigraf_close(again);

    if (failures) fprintf(stderr, "%d check(s) failed\n", failures);
    return failures ? 1 : 0;
}
