find_program(TRANSLATOR_DOTNET dotnet REQUIRED)
set(clr_dir "${CMAKE_CURRENT_BINARY_DIR}/prepared")
# Framework-dependent IL and native bridge are prepared before test execution.
# Restore is offline, from the pinned SDK's framework packs; no NuGet feeds.
add_custom_command(OUTPUT "${clr_dir}/csfz.dll" "${clr_dir}/csfz.runtimeconfig.json" "${clr_dir}/csfz.deps.json"
  COMMAND "${CMAKE_COMMAND}" -E env DOTNET_CLI_TELEMETRY_OPTOUT=1 DOTNET_NOLOGO=1
    "DOTNET_CLI_HOME=${CMAKE_CURRENT_BINARY_DIR}/home"
    "${TRANSLATOR_DOTNET}" publish "${CMAKE_CURRENT_SOURCE_DIR}/csfz.csproj"
    -c Release --self-contained false -p:UseAppHost=false
    "-p:BaseIntermediateOutputPath=${CMAKE_CURRENT_BINARY_DIR}/obj/"
    "-p:BaseOutputPath=${CMAKE_CURRENT_BINARY_DIR}/bin/"
    "-p:RestoreConfigFile=${CMAKE_CURRENT_SOURCE_DIR}/NuGet.Config"
    -o "${clr_dir}" --verbosity quiet
  DEPENDS Program.cs csfz.csproj NuGet.Config VERBATIM)
add_custom_target(translator_clr ALL DEPENDS "${clr_dir}/csfz.dll" "${clr_dir}/csfz.runtimeconfig.json" "${clr_dir}/csfz.deps.json")
add_library(fzclr SHARED fzclr.c)
target_compile_options(fzclr PRIVATE -O2 -ffp-contract=off -Wall -Wextra -Werror)
target_link_libraries(fzclr PRIVATE Threads::Threads)
install(TARGETS fzclr LIBRARY DESTINATION share/translator/languages/cs)
install(FILES "${clr_dir}/csfz.dll" "${clr_dir}/csfz.runtimeconfig.json" "${clr_dir}/csfz.deps.json"
  tier.stdout gc.stdout reversePInvoke.stdout provenance.json DESTINATION share/translator/languages/cs)

# Same public drmgr/drreg observer, restricted to this packaged CLR module.
add_library(translator_clr_observe SHARED observe_client.c)
configure_DynamoRIO_client(translator_clr_observe)
use_DynamoRIO_extension(translator_clr_observe drmgr)
use_DynamoRIO_extension(translator_clr_observe drreg)
install(TARGETS translator_clr_observe LIBRARY DESTINATION lib)
