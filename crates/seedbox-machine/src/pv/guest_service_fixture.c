/* Native acceptance application only, packaged as /application. */
#define _POSIX_C_SOURCE 200809L
#include <signal.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>
static void cleanup(int signal) {
    (void)signal;
    (void)write(STDOUT_FILENO, "cleaned\n", 8);
    _exit(0);
}
int main(void) {
    /* The ordinary default machine must expose control before this app starts,
     * and again after app Stop/Start. No kernel command-line opt-in is supplied. */
    struct sockaddr_un control = {.sun_family = AF_UNIX};
    const char path[] = "/run/seedbox/control.sock";
    for (unsigned i = 0; i < sizeof(path); ++i) control.sun_path[i] = path[i];
    int channel = socket(AF_UNIX, SOCK_STREAM, 0);
    if (channel < 0 || connect(channel, (struct sockaddr *)&control, sizeof(control))) {
        const char failure[] = "control socket unavailable\n";
        (void)write(STDERR_FILENO, failure, sizeof(failure) - 1);
        return 124;
    }
    /* Keep one accepted call outstanding throughout this app lifetime. Stop,
     * exit and shutdown must abandon its socket without killing supervision;
     * the next app session starts its own public request sequence at one. */
    const char request[] = "{\"version\":1,\"id\":1,\"op\":\"now\"}\n";
    if (write(channel, request, sizeof(request) - 1) != (ssize_t)(sizeof(request) - 1)) return 124;
    struct sigaction action = {.sa_handler = cleanup};
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGTERM, &action, NULL)) return 125;
    if (write(STDOUT_FILENO, "app\0out\n", 8) != 8 ||
        write(STDERR_FILENO, "\xff\0z", 3) != 3) return 125;
    struct timespec delay = {.tv_sec = 1};
    while (nanosleep(&delay, &delay)) {}
    return 23;
}
