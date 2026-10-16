
int main(void) {
    irq_flags = 0x3c0;
    seedbox_idle();
    assert(irq_flags == 0x3c0 && writes == 1);
    irq_flags = 0x340;
    seedbox_idle();
    assert(irq_flags == 0x340 && writes == 2);
}
