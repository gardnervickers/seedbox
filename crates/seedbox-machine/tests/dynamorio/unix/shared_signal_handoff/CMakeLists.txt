# Frozen public shared-signal control; application sources are unchanged.
add_library(unix_handoff_client SHARED client.c)
set_target_properties(unix_handoff_client PROPERTIES OUTPUT_NAME unix-handoff-client)
configure_DynamoRIO_client(unix_handoff_client)
use_DynamoRIO_extension(unix_handoff_client drmgr)
target_link_options(unix_handoff_client PRIVATE -nostdlib)
install(TARGETS unix_handoff_client LIBRARY DESTINATION lib)

foreach(case IN ITEMS handoff handoff_syscall)
  add_executable(unix_${case} handoff.c)
  set_target_properties(unix_${case} PROPERTIES OUTPUT_NAME unix-${case})
  target_compile_options(unix_${case} PRIVATE -O2)
  target_link_options(unix_${case} PRIVATE -rdynamic)
  target_link_libraries(unix_${case} PRIVATE pthread)
  install(TARGETS unix_${case} RUNTIME DESTINATION bin)
endforeach()
target_compile_definitions(unix_handoff_syscall PRIVATE LOOP_SYSCALL)
# The observer is already built/installed by translator/CMakeLists.txt for both ISAs.
