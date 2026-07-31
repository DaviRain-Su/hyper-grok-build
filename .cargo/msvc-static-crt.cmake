# These variables are ignored by non-MSVC generators. Setting them in a
# toolchain file makes CMP0091 effective before each dependency's project()
# call, including older vendored CMake projects such as Opus.
set(CMAKE_POLICY_DEFAULT_CMP0091 NEW CACHE STRING "" FORCE)
set(CMAKE_MSVC_RUNTIME_LIBRARY MultiThreaded CACHE STRING "" FORCE)

# CMake projects with a pre-CMP0091 minimum can still append /MD or /MDd after
# the base flags. Override their per-configuration defaults for MSVC targets.
if("$ENV{CARGO_CFG_TARGET_ENV}" STREQUAL "msvc")
  foreach(language C CXX)
    foreach(configuration DEBUG MINSIZEREL RELEASE RELWITHDEBINFO)
      set(CMAKE_${language}_FLAGS_${configuration} "/MT" CACHE STRING "" FORCE)
    endforeach()
  endforeach()
endif()
