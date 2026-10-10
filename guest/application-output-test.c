#define _POSIX_C_SOURCE 200809L
#include "application-output.h"
#include <assert.h>
#include <stdio.h>

int main(void) {
    struct application_output output = {-1, -1};
    assert(!application_output_open(&output));
    assert(fcntl(output.read_fd, F_GETFL) & O_NONBLOCK);
    assert(!(fcntl(output.write_fd, F_GETFL) & O_NONBLOCK));
    char frame[2 * APPLICATION_OUTPUT_BYTES + 16];
    assert(application_output_frame(&output, 0, APPLICATION_OUTPUT_BYTES, frame) == 0);
    unsigned char bytes[256];
    for (unsigned i = 0; i < sizeof(bytes); ++i) bytes[i] = (unsigned char)i;
    for (unsigned stream = 0; stream < 2; ++stream) {
        assert(write(output.write_fd, bytes, sizeof(bytes)) == sizeof(bytes));
        ssize_t length = application_output_frame(&output, stream, APPLICATION_OUTPUT_BYTES, frame);
        assert(length > 0);
        assert(fwrite(frame, 1, (size_t)length, stdout) == (size_t)length);
    }
    close(output.write_fd); output.write_fd = -1;
    assert(!application_output_frame(&output, 0, APPLICATION_OUTPUT_BYTES, frame));
    assert(output.read_fd == -1);
    application_output_close(&output);
}
