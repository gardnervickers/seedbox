
int main(void) {
    for (int masked = 0; masked != 2; ++masked) {
        caller_flags = irq_flags = masked ? 0x3c0 : 0x340;
        storage.now = storage.last = storage.bound = 100;
        storage.deferred_irq_pending = 0;
        writes = synchronized = 0;
        assert(seedbox_read() == (masked ? 101 : 106));
        assert(irq_flags == caller_flags && writes == 1);
        assert(seedbox_read() == (masked ? 102 : 107));
        assert(irq_flags == caller_flags && writes == 1);
        assert(storage.deferred_irq_pending == (u64)masked);
        if (masked) {
            /* No idle or other MMIO: the next unmasked read delivers it. */
            caller_flags = irq_flags = 0x340;
            assert(seedbox_read() == 106);
            assert(irq_flags == caller_flags && writes == 2);
            assert(storage.deferred_irq_pending == 0);
            assert(seedbox_read() == 107 && writes == 2);
        }
    }
}
