# Ordinary Linux apps plus a public callback control for normal fork/exec.
foreach(case truncate signalfd launch)
    add_executable(unix_fd_limits_${case} ${case}.c)
    set_target_properties(unix_fd_limits_${case} PROPERTIES OUTPUT_NAME unix-fd-limits-${case})
    target_compile_options(unix_fd_limits_${case} PRIVATE -Wall -Wextra -Werror)
    install(TARGETS unix_fd_limits_${case} RUNTIME DESTINATION bin)
endforeach()

set(DynamoRIO_USE_LIBC OFF)
add_library(unix_fd_limits_client SHARED client.c)
set_target_properties(unix_fd_limits_client PROPERTIES OUTPUT_NAME unix-fd-limits)
configure_DynamoRIO_client(unix_fd_limits_client)
target_link_options(unix_fd_limits_client PRIVATE -nostdlib)
install(TARGETS unix_fd_limits_client LIBRARY DESTINATION lib)
