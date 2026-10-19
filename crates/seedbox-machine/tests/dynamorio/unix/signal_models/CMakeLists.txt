# Included by the Unix fixture owner, after finding the same-source public SDK.
foreach(case storm itimer model)
    add_executable(unix_signal_models_${case} ${case}.c)
    set_target_properties(unix_signal_models_${case} PROPERTIES
        OUTPUT_NAME unix-signal-models-${case} C_STANDARD 11 C_STANDARD_REQUIRED YES)
    target_compile_options(unix_signal_models_${case} PRIVATE -O2 -g -Wall -Wextra)
    target_link_libraries(unix_signal_models_${case} PRIVATE pthread rt)
    install(TARGETS unix_signal_models_${case} RUNTIME DESTINATION bin)
endforeach()
add_library(unix_signal_models_client SHARED client.c)
set_target_properties(unix_signal_models_client PROPERTIES OUTPUT_NAME unix-signal-models-client
    C_STANDARD 11 C_STANDARD_REQUIRED YES)
# This C client calls only public DR APIs. Use the SDK's documented libc-free
# client contract so compiler-added CRT/libc constructors are not privately loaded.
# This subdirectory-local setting does not change the application link contract.
set(DynamoRIO_USE_LIBC OFF)
configure_DynamoRIO_client(unix_signal_models_client)
install(TARGETS unix_signal_models_client LIBRARY DESTINATION lib)
