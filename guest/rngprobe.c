/* Print 16 bytes from getrandom(2) as hex. */
#include <stdio.h>
#include <sys/random.h>

int main(void) {
    unsigned char bytes[16];
    if (getrandom(bytes, sizeof(bytes), 0) != sizeof(bytes)) { perror("getrandom"); return 1; }
    for (size_t i = 0; i < sizeof(bytes); i++) printf("%02x", bytes[i]);
    printf("\n");
    return 0;
}
