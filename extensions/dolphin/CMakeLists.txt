cmake_minimum_required(VERSION 3.20)
project(onedrive-overlay-plugin)

set(CMAKE_CXX_STANDARD 17)

find_package(ECM REQUIRED NO_MODULE)
set(CMAKE_MODULE_PATH ${ECM_MODULE_PATH})

include(KDEInstallDirs)
include(KDECMakeSettings)
include(KDECompilerSettings)
include(KDEClangFormat OPTIONAL)

find_package(Qt6 REQUIRED COMPONENTS Core)
find_package(KF6KIO REQUIRED)
find_package(KF6CoreAddons REQUIRED)

kcoreaddons_add_plugin(onedrive-overlay
    SOURCES onedrive-overlay.cpp
    INSTALL_NAMESPACE "kf6/overlayicon"
)

target_link_libraries(onedrive-overlay Qt6::Core KF6::KIOCore)
