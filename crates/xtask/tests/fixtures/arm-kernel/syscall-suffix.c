
static void reset(u64 cost, u64 bound, u64 span, u64 replacement) {
    storage = (struct seedbox_page){.syscall_ns = cost, .bound = bound};
    window = span;
    next_cost = replacement;
    writes = 0;
}
int main(void) {
    /* Ordinary clock reads still cost one, independently of CPU throttle. */
    for (u64 cost = 1000; cost <= 4000; cost *= 4) {
        reset(cost, UINT64_MAX, 0, 0);
        assert(seedbox_read() == 1);
        seedbox_syscall_progress();
        assert(page->last == cost + 1 && writes == 0);
        assert(irq_flags == 0);
    }
    reset(1000000, UINT64_MAX, 0, 0);
    seedbox_syscall_progress();
    assert(page->last == 1000000 && writes == 0);

    /* Throttling halfway through affects the next syscall, not owed work. */
    reset(1000, 500, 500, 4000);
    seedbox_syscall_progress();
    assert(page->last == 1000 && writes == 1 && page->syscall_ns == 4000);
    seedbox_syscall_progress();
    assert(page->last == 5000 && writes == 9);

    /* Healing halfway through likewise preserves the original charge. */
    reset(4000, 2000, 2000, 1000);
    seedbox_syscall_progress();
    assert(page->last == 4000 && writes == 1 && page->syscall_ns == 1000);
    seedbox_syscall_progress();
    assert(page->last == 5000 && writes == 2);

    /* Multiple bounded rendezvous neither lose nor duplicate work. */
    reset(1000, 250, 250, 0);
    seedbox_syscall_progress();
    assert(page->last == 1000 && writes == 3 && irq_flags == 0);

    /* Preserve the pre-initialization syscall behavior. */
    page = NULL;
    seedbox_syscall_progress();
    assert(early_last == 1000 && irq_flags == 0);
}
