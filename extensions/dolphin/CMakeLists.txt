cmake_minimum_required(VERSION 3.20)
project(onedrive-overlay-plugin)

set(CMAKE_CXX_STANDARD 17)

find_package(ECM REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})

include(KDEInstallDirs)
include(KDECMakeSettings)
include(KDECompilerSettings)
include(KDEClangFormat OPTIONAL)

# kcoreaddons_add_plugin() hard-errors when CMAKE_LIBRARY_OUTPUT_DIRECTORY is
# unset. KDECMakeSettings above is meant to provide it, but does not on every
# combination — ECM 6.28 with CMake 4 leaves it empty and the configure step
# fails with "set it explicitly or include KDECMakeSettings" even though it is
# included. Setting it is the remedy that error message recommends, and it is
# harmless where ECM already did the job.
if(NOT CMAKE_LIBRARY_OUTPUT_DIRECTORY)
    set(CMAKE_LIBRARY_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/bin)
endif()
if(NOT CMAKE_RUNTIME_OUTPUT_DIRECTORY)
    set(CMAKE_RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/bin)
endif()
if(NOT CMAKE_ARCHIVE_OUTPUT_DIRECTORY)
    set(CMAKE_ARCHIVE_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR}/lib)
endif()

find_package(Qt6 REQUIRED COMPONENTS Core)
find_package(KF6KIO REQUIRED)
find_package(KF6CoreAddons REQUIRED)

kcoreaddons_add_plugin(onedrive-overlay
    SOURCES onedrive-overlay.cpp
    INSTALL_NAMESPACE "kf6/overlayicon"
)

target_link_libraries(onedrive-overlay Qt6::Core KF6::KIOCore)
