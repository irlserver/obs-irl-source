# obs-irl-source — IRL streaming source plugin for OBS
# https://irlserver.com
#
# Copyright (C) 2026 Thomas Lekanger
# SPDX-License-Identifier: AGPL-3.0-or-later

cmake_minimum_required(VERSION 3.16)
project(obs-irl-source VERSION 1.5.1 LANGUAGES C)

set(OBS_IRL_SOURCE_BUILD_VERSION "${PROJECT_VERSION}")
find_package(Git QUIET)
if(Git_FOUND AND EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/.git")
    execute_process(
        COMMAND "${GIT_EXECUTABLE}" rev-parse --short=8 HEAD
        WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}"
        OUTPUT_VARIABLE OBS_IRL_SOURCE_GIT_HASH
        OUTPUT_STRIP_TRAILING_WHITESPACE
        ERROR_QUIET
        RESULT_VARIABLE OBS_IRL_SOURCE_GIT_RESULT
    )
    if(OBS_IRL_SOURCE_GIT_RESULT EQUAL 0 AND OBS_IRL_SOURCE_GIT_HASH)
        set(OBS_IRL_SOURCE_BUILD_VERSION
            "${PROJECT_VERSION}+${OBS_IRL_SOURCE_GIT_HASH}")
    endif()
endif()

if(DEFINED OBS_SOURCE_DIR AND EXISTS "${OBS_SOURCE_DIR}/cmake/finders")
    list(APPEND CMAKE_MODULE_PATH "${OBS_SOURCE_DIR}/cmake/finders")
elseif(DEFINED ENV{OBS_SOURCE_DIR} AND EXISTS "$ENV{OBS_SOURCE_DIR}/cmake/finders")
    list(APPEND CMAKE_MODULE_PATH "$ENV{OBS_SOURCE_DIR}/cmake/finders")
endif()

# ── Find OBS ─────────────────────────────────────────────────
# Try find_package first (works with OBS build tree, cmake install, or
# system libobs-dev which ships CMake config files).
find_package(libobs QUIET)

if(libobs_FOUND)
    message(STATUS "Found libobs via find_package")
    set(_OBS_USE_FIND_PACKAGE ON)
else()
    set(_OBS_USE_FIND_PACKAGE OFF)

    # Fall back to manual OBS_SOURCE_DIR approach.
    if(NOT DEFINED OBS_SOURCE_DIR AND DEFINED ENV{OBS_SOURCE_DIR})
        set(OBS_SOURCE_DIR "$ENV{OBS_SOURCE_DIR}")
    endif()

    set(_OBS_INCLUDE_DIRS "")
    set(_OBS_LIB_DIRS "")

    if(OBS_SOURCE_DIR)
        list(APPEND _OBS_INCLUDE_DIRS "${OBS_SOURCE_DIR}/libobs")
        # Generated headers from OBS build (obsconfig.h, libobs_EXPORT.h)
        if(EXISTS "${OBS_SOURCE_DIR}/build/config")
            list(APPEND _OBS_INCLUDE_DIRS "${OBS_SOURCE_DIR}/build/config")
        endif()
        if(EXISTS "${OBS_SOURCE_DIR}/build/libobs")
            list(APPEND _OBS_INCLUDE_DIRS "${OBS_SOURCE_DIR}/build/libobs")
        endif()
        # OBS threading.h includes <pthread.h> from w32-pthreads on Windows
        if(WIN32 AND EXISTS "${OBS_SOURCE_DIR}/deps/w32-pthreads")
            list(APPEND _OBS_INCLUDE_DIRS "${OBS_SOURCE_DIR}/deps/w32-pthreads")
        endif()
        if(NOT EXISTS "${OBS_SOURCE_DIR}/build/config/obsconfig.h" AND
           NOT EXISTS "${OBS_SOURCE_DIR}/build/libobs/libobs_EXPORT.h")
            message(WARNING
                "OBS_SOURCE_DIR is set, but generated OBS headers were not found. "
                "Build libobs in ../obs-studio first so obsconfig.h/libobs_EXPORT.h exist.")
        endif()
        if(WIN32)
            list(APPEND _OBS_LIB_DIRS
                "${OBS_SOURCE_DIR}/build/libobs/Release"
                "${OBS_SOURCE_DIR}/build/libobs/RelWithDebInfo"
                "${OBS_SOURCE_DIR}/build/libobs/Debug"
                "${OBS_SOURCE_DIR}/build/libobs"
                "${OBS_SOURCE_DIR}/build/deps/w32-pthreads/Release"
                "${OBS_SOURCE_DIR}/build/deps/w32-pthreads/RelWithDebInfo"
                "${OBS_SOURCE_DIR}/build/deps/w32-pthreads/Debug"
            )
        else()
            # On macOS (Xcode) and Linux, search multiple possible locations.
            # The library may be named libobs.dylib, libobs.so, or libobs.0.dylib.
            find_library(_OBS_LIB_PATH
                NAMES obs libobs
                PATHS
                    "${OBS_SOURCE_DIR}/build/libobs/Release"
                    "${OBS_SOURCE_DIR}/build/libobs/RelWithDebInfo"
                    "${OBS_SOURCE_DIR}/build/libobs/Debug"
                    "${OBS_SOURCE_DIR}/build/libobs"
                NO_DEFAULT_PATH
            )
            if(_OBS_LIB_PATH)
                get_filename_component(_OBS_LIB_DIR "${_OBS_LIB_PATH}" DIRECTORY)
                list(APPEND _OBS_LIB_DIRS "${_OBS_LIB_DIR}")
            else()
                list(APPEND _OBS_LIB_DIRS "${OBS_SOURCE_DIR}/build/libobs")
            endif()
        endif()
    else()
        # Try system-installed OBS (e.g. libobs-dev on Ubuntu)
        find_path(_SYSTEM_OBS_INCLUDE obs-module.h
            PATHS /usr/include/obs /usr/local/include/obs
            NO_DEFAULT_PATH
        )
        if(_SYSTEM_OBS_INCLUDE)
            list(APPEND _OBS_INCLUDE_DIRS "${_SYSTEM_OBS_INCLUDE}")
        endif()
    endif()
endif()

# ── OBS dependency headers when using an OBS source tree ─────────────────────
set(_OBS_EXTRA_INCLUDE_DIRS "")
set(_OBS_EXTRA_LIBS "")

find_package(SIMDe QUIET)
if(SIMDe_FOUND)
    list(APPEND _OBS_EXTRA_INCLUDE_DIRS "${SIMDe_INCLUDE_DIR}")
else()
    find_path(SIMDE_FALLBACK_INCLUDE_DIR
        NAMES simde/simde-common.h
        PATHS /usr/include /usr/local/include
    )
    if(SIMDE_FALLBACK_INCLUDE_DIR)
        list(APPEND _OBS_EXTRA_INCLUDE_DIRS "${SIMDE_FALLBACK_INCLUDE_DIR}")
    endif()
endif()

# Headers only.  The plugin never calls jansson; it is reachable from the OBS
# headers, so the include path is needed and the library is not.  Linking it
# baked an absolute /opt/homebrew/opt/jansson/... path into the macOS artifact,
# which made the shipped plugin require Homebrew on the user's machine.
find_package(jansson QUIET)
if(jansson_FOUND)
    list(APPEND _OBS_EXTRA_INCLUDE_DIRS "${jansson_INCLUDE_DIR}")
else()
    find_path(JANSSON_FALLBACK_INCLUDE_DIR
        NAMES jansson.h
        PATHS /usr/include /usr/local/include
    )
    if(JANSSON_FALLBACK_INCLUDE_DIR)
        list(APPEND _OBS_EXTRA_INCLUDE_DIRS "${JANSSON_FALLBACK_INCLUDE_DIR}")
    endif()
endif()

find_package(Uthash QUIET)
if(Uthash_FOUND)
    list(APPEND _OBS_EXTRA_INCLUDE_DIRS "${Uthash_INCLUDE_DIR}")
else()
    find_path(UTHASH_FALLBACK_INCLUDE_DIR
        NAMES uthash.h
        PATHS /usr/include /usr/local/include
    )
    if(UTHASH_FALLBACK_INCLUDE_DIR)
        list(APPEND _OBS_EXTRA_INCLUDE_DIRS "${UTHASH_FALLBACK_INCLUDE_DIR}")
    endif()
endif()

# ── Find FFmpeg ──────────────────────────────────────────────
# Preferred: the bundled static stack from deps/build-deps.sh.  It pins
# FFmpeg, libsrt and mbedTLS versions we control, so the plugin's media
# pipeline no longer depends on which FFmpeg major the host OBS happens to
# ship (that split is what forced per-OBS-line release artifacts) and no
# longer inherits obs-deps' 2022-era libsrt.
#
# Set IRL_BUNDLED_FFMPEG=OFF to link a system/obs-deps FFmpeg instead.
option(IRL_BUNDLED_FFMPEG "Link the bundled static FFmpeg/libsrt stack from deps/" ON)
set(IRL_DEPS_PREFIX "${CMAKE_CURRENT_SOURCE_DIR}/deps/.build/prefix"
    CACHE PATH "Install prefix produced by deps/build-deps.sh")

set(_FFMPEG_FOUND OFF)
set(_IRL_BUNDLED OFF)

if(IRL_BUNDLED_FFMPEG)
    if(EXISTS "${IRL_DEPS_PREFIX}/irl-deps.cmake")
        include("${IRL_DEPS_PREFIX}/irl-deps.cmake")
        set(_FFMPEG_FOUND ON)
        set(_IRL_BUNDLED ON)
    else()
        message(FATAL_ERROR
            "Bundled FFmpeg requested but not built.\n"
            "  Run:  ./deps/build-deps.sh\n"
            "  (or pass -DIRL_DEPS_PREFIX=/path/to/prefix if you built it elsewhere,\n"
            "   or -DIRL_BUNDLED_FFMPEG=OFF to link a system FFmpeg instead)")
    endif()
endif()

# Try pkg-config first (Linux, MSYS2, macOS with Homebrew).
# Fall back to find_path/find_library for MSVC + pre-built FFmpeg.
if(NOT _FFMPEG_FOUND)
    find_package(PkgConfig QUIET)
    if(PkgConfig_FOUND)
        pkg_check_modules(LIBAV QUIET IMPORTED_TARGET
            libavformat
            libavcodec
            libswresample
            libswscale
            libavutil
        )
        if(LIBAV_FOUND)
            set(_FFMPEG_FOUND ON)
        endif()
    endif()
endif()

if(NOT _FFMPEG_FOUND)
    # Manual FFmpeg discovery (Windows MSVC with pre-built FFmpeg).
    #
    # Pass -DFFMPEG_DIR=C:/path/to/ffmpeg on the cmake command line,
    # or set the FFMPEG_DIR environment variable.  The directory should
    # contain include/ and lib/ sub-directories.
    if(NOT DEFINED FFMPEG_DIR AND DEFINED ENV{FFMPEG_DIR})
        set(FFMPEG_DIR "$ENV{FFMPEG_DIR}")
    endif()

    set(_FFMPEG_SEARCH_PATHS "")
    if(FFMPEG_DIR)
        list(APPEND _FFMPEG_SEARCH_PATHS "${FFMPEG_DIR}")
    endif()

    find_path(AVFORMAT_INCLUDE_DIR libavformat/avformat.h
        PATHS ${_FFMPEG_SEARCH_PATHS}
        PATH_SUFFIXES include
    )
    find_path(AVCODEC_INCLUDE_DIR libavcodec/avcodec.h
        PATHS ${_FFMPEG_SEARCH_PATHS}
        PATH_SUFFIXES include
    )
    find_path(SWRESAMPLE_INCLUDE_DIR libswresample/swresample.h
        PATHS ${_FFMPEG_SEARCH_PATHS}
        PATH_SUFFIXES include
    )
    find_path(SWSCALE_INCLUDE_DIR libswscale/swscale.h
        PATHS ${_FFMPEG_SEARCH_PATHS}
        PATH_SUFFIXES include
    )
    find_path(AVUTIL_INCLUDE_DIR libavutil/avutil.h
        PATHS ${_FFMPEG_SEARCH_PATHS}
        PATH_SUFFIXES include
    )

    find_library(AVFORMAT_LIB NAMES avformat PATHS ${_FFMPEG_SEARCH_PATHS} PATH_SUFFIXES lib)
    find_library(AVCODEC_LIB  NAMES avcodec  PATHS ${_FFMPEG_SEARCH_PATHS} PATH_SUFFIXES lib)
    find_library(SWRESAMPLE_LIB NAMES swresample PATHS ${_FFMPEG_SEARCH_PATHS} PATH_SUFFIXES lib)
    find_library(SWSCALE_LIB  NAMES swscale  PATHS ${_FFMPEG_SEARCH_PATHS} PATH_SUFFIXES lib)
    find_library(AVUTIL_LIB   NAMES avutil   PATHS ${_FFMPEG_SEARCH_PATHS} PATH_SUFFIXES lib)

    if(AVFORMAT_LIB AND AVCODEC_LIB AND SWRESAMPLE_LIB AND SWSCALE_LIB AND AVUTIL_LIB)
        set(_FFMPEG_FOUND ON)
        set(_FFMPEG_MANUAL_INCLUDE_DIRS
            ${AVFORMAT_INCLUDE_DIR}
            ${AVCODEC_INCLUDE_DIR}
            ${SWRESAMPLE_INCLUDE_DIR}
            ${SWSCALE_INCLUDE_DIR}
            ${AVUTIL_INCLUDE_DIR}
        )
        list(REMOVE_DUPLICATES _FFMPEG_MANUAL_INCLUDE_DIRS)
        set(_FFMPEG_MANUAL_LIBS
            ${AVFORMAT_LIB}
            ${AVCODEC_LIB}
            ${SWRESAMPLE_LIB}
            ${SWSCALE_LIB}
            ${AVUTIL_LIB}
        )
    endif()
endif()

if(NOT _FFMPEG_FOUND)
    message(FATAL_ERROR
        "FFmpeg not found.  Either:\n"
        "  - Build the bundled stack: ./deps/build-deps.sh\n"
        "  - Install FFmpeg dev packages (apt install libavformat-dev libswscale-dev ...)\n"
        "  - Pass -DFFMPEG_DIR=C:/path/to/ffmpeg (pre-built Windows SDK)\n"
    )
endif()

# ── Plugin shared library ────────────────────────────────────
set(PLUGIN_SOURCES
    src/plugin.c
    src/irl-source.c
    src/receiver.c
    src/receiver-audio.c
    src/receiver-decode.c
    src/receiver-stream.c
    src/receiver-video.c
    src/audio-buffer.c
    src/video-handler.c
    src/pts-repair.c
    src/settings.c
    src/websocket-vendor.c
)

add_library(obs-irl-source MODULE ${PLUGIN_SOURCES})
target_compile_definitions(obs-irl-source PRIVATE
    OBS_IRL_SOURCE_VERSION="${OBS_IRL_SOURCE_BUILD_VERSION}"
)

# ── Include directories ──────────────────────────────────────
target_include_directories(obs-irl-source PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}/include
)

if(NOT _OBS_USE_FIND_PACKAGE AND _OBS_INCLUDE_DIRS)
    target_include_directories(obs-irl-source PRIVATE ${_OBS_INCLUDE_DIRS})
    # OBS headers check HAVE_OBSCONFIG_H to include the generated config
    if(OBS_SOURCE_DIR AND EXISTS "${OBS_SOURCE_DIR}/build/config/obsconfig.h")
        target_compile_definitions(obs-irl-source PRIVATE HAVE_OBSCONFIG_H)
    endif()
endif()

if(_OBS_EXTRA_INCLUDE_DIRS)
    list(REMOVE_DUPLICATES _OBS_EXTRA_INCLUDE_DIRS)
    target_include_directories(obs-irl-source PRIVATE ${_OBS_EXTRA_INCLUDE_DIRS})
endif()

if(_IRL_BUNDLED)
    # BEFORE, so the bundled headers win over any FFmpeg dev package that the
    # OBS include paths might also drag in. Mismatched headers and archives is
    # the one way a static link can still fail at runtime.
    target_include_directories(obs-irl-source BEFORE PRIVATE ${IRL_DEPS_INCLUDE_DIRS})
elseif(LIBAV_FOUND)
    target_include_directories(obs-irl-source PRIVATE ${LIBAV_INCLUDE_DIRS})
elseif(_FFMPEG_MANUAL_INCLUDE_DIRS)
    target_include_directories(obs-irl-source PRIVATE ${_FFMPEG_MANUAL_INCLUDE_DIRS})
endif()

# ── Link directories (OBS) ───────────────────────────────────
if(NOT _OBS_USE_FIND_PACKAGE AND _OBS_LIB_DIRS)
    target_link_directories(obs-irl-source PRIVATE ${_OBS_LIB_DIRS})
endif()

# ── Link libraries ───────────────────────────────────────────
if(_OBS_USE_FIND_PACKAGE)
    set(_OBS_LINK_TARGET OBS::libobs)
elseif(_OBS_LIB_PATH)
    # Link against the exact library found by find_library (handles
    # versioned names like libobs.0.dylib that -lobs can't find)
    set(_OBS_LINK_TARGET "${_OBS_LIB_PATH}")
else()
    set(_OBS_LINK_TARGET obs)
endif()

if(_IRL_BUNDLED)
    target_link_libraries(obs-irl-source PRIVATE ${_OBS_LINK_TARGET}
        ${IRL_DEPS_STATIC_LIBRARIES}
        ${IRL_DEPS_SYSTEM_LIBRARIES}
    )
elseif(LIBAV_FOUND)
    target_link_libraries(obs-irl-source PRIVATE ${_OBS_LINK_TARGET} PkgConfig::LIBAV)
else()
    target_link_libraries(obs-irl-source PRIVATE ${_OBS_LINK_TARGET} ${_FFMPEG_MANUAL_LIBS})
endif()

# ── Symbol isolation for the bundled stack ───────────────────
#
# Load-bearing, not cosmetic. The host OBS has already loaded its own
# libavcodec/libavformat, which export the exact same symbol names as the
# archives linked in here. On ELF, an exported avcodec_open2 in this module
# would be resolved through the global scope to *OBS's* copy, so our calls
# would run against a different library's structs. Hiding everything but the
# obs_module_* entry points (the only symbols libobs looks up) binds every
# internal call to our own copy at link time.
if(_IRL_BUNDLED)
    set_target_properties(obs-irl-source PROPERTIES
        C_VISIBILITY_PRESET hidden
        VISIBILITY_INLINES_HIDDEN ON
    )
    if(APPLE)
        target_link_options(obs-irl-source PRIVATE
            "-Wl,-exported_symbols_list,${CMAKE_CURRENT_SOURCE_DIR}/cmake/module-symbols.exp")
        set_property(TARGET obs-irl-source APPEND PROPERTY
            LINK_DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/cmake/module-symbols.exp")
    elseif(UNIX)
        target_link_options(obs-irl-source PRIVATE
            "-Wl,--exclude-libs,ALL"
            "-Wl,--version-script,${CMAKE_CURRENT_SOURCE_DIR}/cmake/module-symbols.map")
        set_property(TARGET obs-irl-source APPEND PROPERTY
            LINK_DEPENDS "${CMAKE_CURRENT_SOURCE_DIR}/cmake/module-symbols.map")
    endif()
endif()

if(_OBS_EXTRA_LIBS)
    target_link_libraries(obs-irl-source PRIVATE ${_OBS_EXTRA_LIBS})
endif()

if(WIN32 AND NOT _OBS_USE_FIND_PACKAGE)
    # Link w32-pthreads from OBS source tree. The plugin's own threading goes
    # through include/irl-threading.h (Win32 primitives, deliberately not
    # pthread — see that header), but libobs headers still declare against
    # w32-pthreads, so the import stays available.
    target_link_libraries(obs-irl-source PRIVATE w32-pthreads)
elseif(NOT WIN32)
    target_link_libraries(obs-irl-source PRIVATE pthread)
endif()

# ── C standard ───────────────────────────────────────────────
target_compile_features(obs-irl-source PRIVATE c_std_11)

# glibc hides PTHREAD_MUTEX_ERRORCHECK and pthread_mutexattr_settype() behind
# __USE_UNIX98, which a strict -std=c11 build does not define.  CMake asks for
# gnu11 by default, so this only bites when C extensions are turned off — pin
# the visibility rather than depend on that default.  macOS and the BSDs
# declare both unconditionally, so they need nothing.
if(UNIX AND NOT APPLE)
    target_compile_definitions(obs-irl-source PRIVATE _GNU_SOURCE)
endif()

# ── Checked locks ────────────────────────────────────────────
# Turn a lock-contract violation (a nested acquire, or an unlock by a thread
# that does not hold the mutex) into an immediate abort naming the offending
# line, instead of a hung stream that only reproduces on POSIX.  See the
# header comment in include/irl-threading.h.  Costs a mutex type change and
# a branch per lock, so it is a development aid rather than a shipping mode.
option(IRL_CHECKED_LOCKS "Abort on lock-contract violations (dev builds)" OFF)
target_compile_definitions(obs-irl-source PRIVATE
    $<$<CONFIG:Debug>:IRL_CHECKED_LOCKS>
)
if(IRL_CHECKED_LOCKS)
    target_compile_definitions(obs-irl-source PRIVATE IRL_CHECKED_LOCKS)
    message(STATUS "obs-irl-source: checked locks enabled")
endif()

# ── Compiler warnings ────────────────────────────────────────
if(MSVC)
    target_compile_options(obs-irl-source PRIVATE /W4 /wd4100)
    # Export nothing by default (OBS loads the module entry points)
    set_target_properties(obs-irl-source PROPERTIES WINDOWS_EXPORT_ALL_SYMBOLS OFF)
else()
    target_compile_options(obs-irl-source PRIVATE
        -Wall -Wextra -Wpedantic
        -Wno-unused-parameter
    )
endif()

# ── Output name ──────────────────────────────────────────────
# Remove "lib" prefix so output is obs-irl-source.so/.dll everywhere.
set_target_properties(obs-irl-source PROPERTIES PREFIX "")

# ── Windows-specific ─────────────────────────────────────────
if(WIN32 AND NOT _IRL_BUNDLED)
    # Copy FFmpeg DLLs next to the plugin for convenience.  The bundled stack
    # is static, so there is nothing to copy there.
    if(FFMPEG_DIR AND EXISTS "${FFMPEG_DIR}/bin")
        add_custom_command(TARGET obs-irl-source POST_BUILD
            COMMAND ${CMAKE_COMMAND} -E copy_directory
                "${FFMPEG_DIR}/bin"
                "$<TARGET_FILE_DIR:obs-irl-source>"
            COMMENT "Copying FFmpeg DLLs to output directory"
        )
    endif()
endif()

# ── Install ──────────────────────────────────────────────────
include(GNUInstallDirs)

if(APPLE)
    # OBS on macOS only scans .plugin bundles (%module%.plugin/Contents/MacOS
    # with an extensionless binary), not the bin/ layout the other platforms
    # use, so the install must build that bundle structure.
    set(OBS_PLUGIN_BUNDLE
        "$ENV{HOME}/Library/Application Support/obs-studio/plugins/obs-irl-source.plugin")
    set(PLUGIN_VERSION "${PROJECT_VERSION}")
    configure_file(cmake/macos/Info.plist.in "${CMAKE_CURRENT_BINARY_DIR}/Info.plist" @ONLY)
    install(PROGRAMS "$<TARGET_FILE:obs-irl-source>"
        DESTINATION "${OBS_PLUGIN_BUNDLE}/Contents/MacOS"
        RENAME "obs-irl-source")
    install(FILES "${CMAKE_CURRENT_BINARY_DIR}/Info.plist"
        DESTINATION "${OBS_PLUGIN_BUNDLE}/Contents")
    # obs_module_file() resolves to Contents/Resources inside a .plugin bundle.
    install(DIRECTORY data/locale
        DESTINATION "${OBS_PLUGIN_BUNDLE}/Contents/Resources")
elseif(WIN32)
    install(TARGETS obs-irl-source
        DESTINATION "$ENV{APPDATA}/obs-studio/plugins/obs-irl-source/bin/64bit")
    install(DIRECTORY data/locale
        DESTINATION "$ENV{APPDATA}/obs-studio/plugins/obs-irl-source/data")
else()
    install(TARGETS obs-irl-source DESTINATION "${CMAKE_INSTALL_LIBDIR}/obs-plugins")
    install(DIRECTORY data/locale
        DESTINATION "${CMAKE_INSTALL_DATAROOTDIR}/obs/obs-plugins/obs-irl-source")
endif()

# ── Summary ──────────────────────────────────────────────────
message(STATUS "")
message(STATUS "obs-irl-source v${OBS_IRL_SOURCE_BUILD_VERSION}")
message(STATUS "  OS:        ${CMAKE_SYSTEM_NAME}")
message(STATUS "  Compiler:  ${CMAKE_C_COMPILER_ID} ${CMAKE_C_COMPILER_VERSION}")
if(_OBS_USE_FIND_PACKAGE)
    message(STATUS "  OBS:       find_package (libobs)")
else()
    message(STATUS "  OBS dir:   ${OBS_SOURCE_DIR}")
endif()
if(_IRL_BUNDLED)
    message(STATUS "  FFmpeg:    bundled static ${IRL_DEPS_FFMPEG_VERSION}")
    message(STATUS "  libsrt:    bundled static ${IRL_DEPS_SRT_VERSION}")
    message(STATUS "  librist:   bundled static ${IRL_DEPS_LIBRIST_VERSION}")
    message(STATUS "  mbedTLS:   bundled static ${IRL_DEPS_MBEDTLS_VERSION}")
    message(STATUS "  Deps:      ${IRL_DEPS_PREFIX}")
elseif(LIBAV_FOUND)
    message(STATUS "  FFmpeg:    pkg-config (${LIBAV_VERSION}) — host-provided")
else()
    message(STATUS "  FFmpeg:    manual (${FFMPEG_DIR}) — host-provided")
endif()
message(STATUS "  Install:   ${OBS_PLUGIN_DIR}")
message(STATUS "")
