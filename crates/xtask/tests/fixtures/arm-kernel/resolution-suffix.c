
int main(int argc, char **argv) {
    assert(argc == 2);
    resolution_ns = strtol(argv[1], NULL, 10);
    require_high_resolution();
    return 0;
}
