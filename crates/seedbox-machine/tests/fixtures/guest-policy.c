/* Host-only parser fixture. No mount, VM, guest, socket or process supervisor. */
#include "guest_policy.h"
#include <stdio.h>
int main(int argc, char **argv) {
    if (argc != 3) return 3;
    int enabled = -1;
    if (sbx_control_mode(argv[2], argv[1][0] == '1', &enabled)) {
        puts("invalid"); return 2;
    }
    printf("%d\n", enabled);
    return 0;
}
