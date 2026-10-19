/* Source-derived oracle tests; LICENSE and provenance.json identify originals.
 * Synthetic observations test the retained model, not live DR behavior. */
#include "oracle.h"
#include "storm_model.h"
static void observations(const itcase_t *c, model_trace_t *t)
{
    memset(t, 0, sizeof(*t));
    t->report = 1;
    itarm_t arm = {0};
    int64_t now = 1000000;
    for (int i = 0; i <= c->nops; ++i) {
        itop_t final = { IT_SET, 0, 0, 0, 0, it_late_tol(c->which) };
        const itop_t *o = i < c->nops ? &c->op[i] : &final;
        model_op_t *m = &t->op[i];
        m->t0 = m->t1 = now;
        int64_t remaining = it_rem(&arm, arm.s0, now);
        m->old.it_value.tv_sec = remaining / 1000000;
        m->old.it_value.tv_usec = remaining % 1000000;
        m->old.it_interval.tv_sec = arm.interval / 1000000;
        m->old.it_interval.tv_usec = arm.interval % 1000000;
        m->alarm_old = it_alarm_secs(remaining);
        if (o->op != IT_GET) {
            arm.armed = o->op == IT_ALARM ? o->alarm_sec > 0 : o->value > 0;
            arm.value = o->op == IT_ALARM ? (int64_t)o->alarm_sec * 1000000 : o->value;
            arm.interval = o->op == IT_ALARM ? 0 : o->interval;
            arm.s0 = arm.s1 = now;
        }
        if (arm.armed) {
            for (int64_t expiry = arm.s0 + arm.value; expiry < now + o->wait; expiry += arm.interval) {
                if (expiry >= now) {
                    REQUIRE(t->nlog < IT_MAX_LOG);
                    t->log[t->nlog++] = expiry;
                }
                if (!arm.interval) break;
            }
        }
        now += o->wait;
    }
}
int main(void)
{
    REQUIRE(storm_accounted(0, 100, 900, 1000, 100, 10, 1));
    REQUIRE(storm_accounted(0, 100, 900, 1000, 100, 6, 1));
    REQUIRE(storm_accounted(0, 100, 900, 1000, 100, 12, 1));
    REQUIRE(!storm_accounted(0, 100, 900, 1000, 100, 5, 1));
    REQUIRE(!storm_accounted(0, 100, 900, 1000, 100, 13, 1));
    REQUIRE(!storm_accounted(0, 100, 900, 1000, 100, 10, 0));
    unsigned checked = 0, rejected = 0;
    for (int profile = 0; profile < 3; ++profile) {
        memset(it_mux, 0, sizeof(it_mux));
        if (profile == 1) { it_mux[0] = 3000; it_mux[1] = 4000; it_mux[2] = 5000; }
        if (profile == 2) it_mux[1] = 10000;
        for (int i = 0; i < 7; ++i) {
            itcase_t c;
            model_trace_t t;
            itres_t r = {0};
            gen_itcase(1 + i, &c);
            observations(&c, &t);
            REQUIRE(model_check(&c, &t, &r, 0) == 0 && r.valid && !r.stalled);
            checked++;
            t.report = 0;
            t.wrong_thread = 1;
            REQUIRE(model_check(&c, &t, &r, 0) > 0);
            rejected++;
            t.wrong_thread = 0;
            /* A delivery predating every arming must fail even with mux slack. */
            REQUIRE(t.nlog < IT_MAX_LOG);
            t.log[t.nlog++] = 0;
            REQUIRE(model_check(&c, &t, &r, 0) > 0);
            rejected++;
        }
    }
    printf("synthetic-model cases=%u negative=%u scope=synthetic\n", checked, rejected);
    return 0;
}
