#include "progress/budget.h"
#include <stdio.h>
int main(int argc, char **argv) {
    uint32_t budget = 0;
    if (argc != 3 || !seedbox_progress_budget(argv[1], argv[2], &budget)) return 2;
    printf("%u\n", budget);
    return 0;
}
