set(CMAKE_C_STANDARD 11)
set(CMAKE_C_STANDARD_REQUIRED ON)
find_package(Threads REQUIRED)
add_executable(translator_memcache memcache_app.c)
target_link_libraries(translator_memcache PRIVATE Threads::Threads)
target_compile_options(translator_memcache PRIVATE -Wall -Wextra -Werror)
target_link_options(translator_memcache PRIVATE -rdynamic)
add_library(translator_memcache_client SHARED memcache_client.c)
configure_DynamoRIO_client(translator_memcache_client)
install(TARGETS translator_memcache translator_memcache_client RUNTIME DESTINATION bin LIBRARY DESTINATION lib)
# Included by the integrator's public-SDK fixture package.
add_executable(translator_flush flush_app.c)
target_compile_options(translator_flush PRIVATE -Wall -Wextra -Werror)
target_link_options(translator_flush PRIVATE -rdynamic)
add_library(translator_flush_client SHARED flush_client.c)
configure_DynamoRIO_client(translator_flush_client)
install(TARGETS translator_flush translator_flush_client RUNTIME DESTINATION bin LIBRARY DESTINATION lib)
if(CMAKE_SYSTEM_PROCESSOR MATCHES "aarch64|arm64")
  add_executable(translator_exclusive exclusive.c)
  target_link_libraries(translator_exclusive PRIVATE Threads::Threads)
  target_link_options(translator_exclusive PRIVATE -rdynamic)
  install(TARGETS translator_exclusive RUNTIME DESTINATION bin)
  add_library(translator_progress SHARED progress.c)
  configure_DynamoRIO_client(translator_progress)
  use_DynamoRIO_extension(translator_progress drmgr)
  use_DynamoRIO_extension(translator_progress drreg)
  add_executable(translator_codec codec.c)
  # The SDK's relative-RPATH helper requires an explicit executable directory.
  # Nix installs this executable separately from its immutable SDK, so retain
  # the selected imported runtime's absolute directory in the installed RPATH.
  set_target_properties(translator_codec PROPERTIES
    RUNTIME_OUTPUT_DIRECTORY "${CMAKE_CURRENT_BINARY_DIR}"
    INSTALL_RPATH "$<TARGET_FILE_DIR:dynamorio>")
  # The source dis-a64 test enables every encoder feature for offline vectors.
  # drdecode provides that contract without changing runtime CPU capabilities.
  configure_DynamoRIO_decoder(translator_codec)
  add_executable(translator_isa a64_proptest.c)
  configure_DynamoRIO_decoder(translator_isa)
  install(TARGETS translator_progress translator_codec translator_isa RUNTIME DESTINATION bin LIBRARY DESTINATION lib)
endif()
if(SEEDBOX_JDK25_HOME)
  set(Java_JAVA_EXECUTABLE "${SEEDBOX_JDK25_HOME}/bin/java")
  set(Java_JAVAC_EXECUTABLE "${SEEDBOX_JDK25_HOME}/bin/javac")
  set(Java_JAR_EXECUTABLE "${SEEDBOX_JDK25_HOME}/bin/jar")
endif()
find_package(Java 25 REQUIRED COMPONENTS Development Runtime)
set(jvm_dir "${CMAKE_CURRENT_BINARY_DIR}/jvm")
add_custom_command(OUTPUT "${jvm_dir}/jitstress.jar"
  COMMAND "${CMAKE_COMMAND}" -E make_directory "${jvm_dir}/classes"
  COMMAND "${Java_JAVAC_EXECUTABLE}" -d "${jvm_dir}/classes" "${CMAKE_CURRENT_SOURCE_DIR}/jvm/JitStress.java"
  COMMAND "${Java_JAR_EXECUTABLE}" cfm "${jvm_dir}/jitstress.jar" "${CMAKE_CURRENT_SOURCE_DIR}/jvm/manifest.txt" -C "${jvm_dir}/classes" .
  DEPENDS jvm/JitStress.java jvm/manifest.txt VERBATIM)
add_custom_target(translator_jvm ALL DEPENDS "${jvm_dir}/jitstress.jar")
install(FILES "${jvm_dir}/jitstress.jar" jvm/run.py jvm/source_oracle.py DESTINATION share/translator/jvm)
install(FILES phase.py codec_vectors.tsv origins.json DESTINATION share/translator)
install(DIRECTORY vectors saved licenses DESTINATION share/translator)
find_program(TRANSLATOR_GO go REQUIRED)
find_program(TRANSLATOR_ERLC erlc REQUIRED)
set(language_dir "${CMAKE_CURRENT_BINARY_DIR}/languages")
file(MAKE_DIRECTORY "${language_dir}/go-cache" "${language_dir}/erl/hot/v1" "${language_dir}/erl/hot/v2")
foreach(cgo IN ITEMS 0 1)
  if(cgo)
    set(go_name translator_gofzcgo)
  else()
    set(go_name translator_gofz)
  endif()
  add_custom_command(OUTPUT "${language_dir}/${go_name}"
    COMMAND "${CMAKE_COMMAND}" -E env CGO_ENABLED=${cgo} GOTOOLCHAIN=local GOPROXY=off GOSUMDB=off
      "GOCACHE=${language_dir}/go-cache" "CC=${CMAKE_C_COMPILER}"
      "${TRANSLATOR_GO}" build -trimpath -o "${language_dir}/${go_name}" .
    WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}/languages/go"
    DEPENDS languages/go/gofz.go languages/go/cgo.go languages/go/nocgo.go languages/go/go.mod VERBATIM)
  add_custom_target(${go_name}_build ALL DEPENDS "${language_dir}/${go_name}")
  install(PROGRAMS "${language_dir}/${go_name}" DESTINATION bin)
endforeach()
add_custom_command(OUTPUT "${language_dir}/erl/beamfz.beam" "${language_dir}/erl/hot/v1/fzhot.beam" "${language_dir}/erl/hot/v2/fzhot.beam"
  COMMAND "${TRANSLATOR_ERLC}" -o "${language_dir}/erl" "${CMAKE_CURRENT_SOURCE_DIR}/languages/erl/beamfz.erl"
  COMMAND "${TRANSLATOR_ERLC}" -o "${language_dir}/erl/hot/v1" "${CMAKE_CURRENT_SOURCE_DIR}/languages/erl/fzhot.erl"
  COMMAND "${TRANSLATOR_ERLC}" -DV2 -o "${language_dir}/erl/hot/v2" "${CMAKE_CURRENT_SOURCE_DIR}/languages/erl/fzhot.erl"
  DEPENDS languages/erl/beamfz.erl languages/erl/fzhot.erl VERBATIM)
add_custom_target(translator_erlang ALL DEPENDS "${language_dir}/erl/beamfz.beam" "${language_dir}/erl/hot/v1/fzhot.beam" "${language_dir}/erl/hot/v2/fzhot.beam")
install(DIRECTORY "${language_dir}/erl" languages/js languages/lua DESTINATION share/translator/languages)
install(FILES languages/run.py DESTINATION share/translator/languages)
add_subdirectory(languages/cs)

add_library(translator_observe SHARED observe_client.c)
configure_DynamoRIO_client(translator_observe)
use_DynamoRIO_extension(translator_observe drmgr)
use_DynamoRIO_extension(translator_observe drreg)
install(TARGETS translator_observe LIBRARY DESTINATION lib)

add_subdirectory(replay)
