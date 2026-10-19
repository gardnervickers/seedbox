# Parent owns find_package(DynamoRIO) and add_subdirectory(fault_context).
add_executable(unix_fault_context fault.c)
set_target_properties(unix_fault_context PROPERTIES OUTPUT_NAME unix-fault-context)
target_compile_options(unix_fault_context PRIVATE -Wall -Wextra -Werror)
install(TARGETS unix_fault_context RUNTIME DESTINATION bin)

set(DynamoRIO_USE_LIBC OFF)
add_library(unix_fault_context_client SHARED client.c)
set_target_properties(unix_fault_context_client PROPERTIES OUTPUT_NAME unix-fault-context)
configure_DynamoRIO_client(unix_fault_context_client)
install(TARGETS unix_fault_context_client LIBRARY DESTINATION lib)
