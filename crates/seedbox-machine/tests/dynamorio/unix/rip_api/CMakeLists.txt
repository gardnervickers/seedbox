# X86 RIP-relative data references are deliberately not registered on AArch64.
if(CMAKE_SYSTEM_PROCESSOR MATCHES "^(x86_64|amd64|AMD64)$")
    add_executable(unix_rip_api_control control.c)
    set_target_properties(unix_rip_api_control PROPERTIES OUTPUT_NAME unix-rip-api-control)
    install(TARGETS unix_rip_api_control RUNTIME DESTINATION bin)
    add_library(unix_rip_api_client SHARED client.c)
    set_target_properties(unix_rip_api_client PROPERTIES OUTPUT_NAME unix-rip-api)
    # Public SDK C-client contract: no libc/CRT dependency before client entry.
    set(DynamoRIO_USE_LIBC OFF)
    configure_DynamoRIO_client(unix_rip_api_client)
    install(TARGETS unix_rip_api_client LIBRARY DESTINATION lib)
endif()
