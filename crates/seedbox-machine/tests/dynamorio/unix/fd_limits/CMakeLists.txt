# Ordinary Linux applications: no client or DR SDK linkage is needed.
foreach(case truncate signalfd)
    add_executable(unix_fd_limits_${case} ${case}.c)
    set_target_properties(unix_fd_limits_${case} PROPERTIES OUTPUT_NAME unix-fd-limits-${case})
    target_compile_options(unix_fd_limits_${case} PRIVATE -Wall -Wextra -Werror)
    install(TARGETS unix_fd_limits_${case} RUNTIME DESTINATION bin)
endforeach()
